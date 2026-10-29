//! Flight recorder: per-CPU rings of fixed-size trace records (ROADMAP
//! §10.7, DESIGN §2.2).
//!
//! Portable half. Each CPU pushes only to its own [`Ring`], with interrupts
//! off for the few stores a record takes, so no lock is taken; a reader on
//! another CPU, or the core tool reading a dump, checks each slot's `seq`
//! and drops a slot that is torn, stale or overwritten ([`valid_at`]). The
//! NMI, `#MC` and `#DB` bodies never record, since `cli` does not mask them
//! and one could land between another record's stores ([`traced_vector`]).
//!
//! This is not `log::Ring` or the keyboard ring (AGENTS.md rule 10): both
//! are `&mut self` single-writer rings behind a lock, while this one is
//! pushed through `&self` over atomics, from IRQ and IPI context too, and
//! read by other CPUs while its writer runs. ROADMAP §19.1's live drain
//! reuses it.
//!
//! The kernel keeps one [`KernelTrace`] (`trace_init::VIBEOS_TRACE`),
//! exported unmangled so the core tool finds it; every layout it reads is
//! `#[repr(C)]` and fixed by the const assertions below.

use core::fmt;
use core::mem::{offset_of, size_of};

use crate::atomic::statics::{AtomicPtr, AtomicU32, AtomicU64};
use crate::atomic::{Ordering, fence};

/// Rings in the kernel's trace: one per bit of the online mask.
pub const MAX_CPUS: usize = crate::irq::ipi::MAX_IPI_CPUS;
/// Records each CPU's ring keeps; older ones are overwritten.
pub const RECORDS_PER_CPU: usize = 256;
/// First eight bytes of a live [`Trace`] header.
pub const TRACE_MAGIC: [u8; 8] = *b"VBTRACE1";
/// Header layout version.
pub const TRACE_VERSION: u32 = 1;
/// Bytes per [`Record`].
pub const RECORD_SIZE: usize = 40;

/// Header flag: the clock fields below were published.
pub const CLOCK_PUBLISHED: u32 = 1;
/// Header flag: CPUID reports the TSC invariant.
pub const TSC_INVARIANT: u32 = 2;
/// Header flag: at least one AP ran the bring-up warp test.
pub const WARP_MEASURED: u32 = 4;
/// Header flag: the warp test saw a backward step.
pub const WARP_BACKWARD: u32 = 8;

/// A tracepoint. Value 0 is never an event, so a zeroed slot is not one.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    SyscallEnter = 1,
    SyscallExit = 2,
    Switch = 3,
    Wake = 4,
    IrqEnter = 5,
    IrqExit = 6,
    IpiSend = 7,
    IpiAck = 8,
    PageFault = 9,
    BlockSubmit = 10,
    BlockComplete = 11,
}

impl Event {
    /// Every event, in value order.
    pub const ALL: [Event; 11] = [
        Event::SyscallEnter,
        Event::SyscallExit,
        Event::Switch,
        Event::Wake,
        Event::IrqEnter,
        Event::IrqExit,
        Event::IpiSend,
        Event::IpiAck,
        Event::PageFault,
        Event::BlockSubmit,
        Event::BlockComplete,
    ];

    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    pub const fn from_u32(v: u32) -> Option<Event> {
        match v {
            1 => Some(Event::SyscallEnter),
            2 => Some(Event::SyscallExit),
            3 => Some(Event::Switch),
            4 => Some(Event::Wake),
            5 => Some(Event::IrqEnter),
            6 => Some(Event::IrqExit),
            7 => Some(Event::IpiSend),
            8 => Some(Event::IpiAck),
            9 => Some(Event::PageFault),
            10 => Some(Event::BlockSubmit),
            11 => Some(Event::BlockComplete),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Event::SyscallEnter => "syscall_enter",
            Event::SyscallExit => "syscall_exit",
            Event::Switch => "switch",
            Event::Wake => "wake",
            Event::IrqEnter => "irq_enter",
            Event::IrqExit => "irq_exit",
            Event::IpiSend => "ipi_send",
            Event::IpiAck => "ipi_ack",
            Event::PageFault => "page_fault",
            Event::BlockSubmit => "block_submit",
            Event::BlockComplete => "block_complete",
        }
    }

    /// Names of the record's two arguments, `a` then `b`.
    pub const fn arg_names(self) -> (&'static str, &'static str) {
        match self {
            Event::SyscallEnter => ("nr", "arg0"),
            Event::SyscallExit => ("nr", "ret"),
            Event::Switch => ("from_tid", "to_tid"),
            Event::Wake => ("tid", "cpu"),
            Event::IrqEnter | Event::IrqExit => ("vector", "zero"),
            Event::IpiSend => ("vector", "target"),
            Event::IpiAck => ("vector", "sender"),
            Event::PageFault => ("cr2", "error_code"),
            Event::BlockSubmit => ("seq", "lba"),
            Event::BlockComplete => ("seq", "aborted"),
        }
    }
}

/// One slot of a [`Ring`]. `seq` is the slot's position plus one once its
/// fields are whole, and 0 while its writer is between stores.
#[repr(C)]
pub struct Record {
    seq: AtomicU64,
    tsc: AtomicU64,
    a: AtomicU64,
    b: AtomicU64,
    cpu: AtomicU32,
    event: AtomicU32,
}

const _: () = {
    assert!(size_of::<Record>() == RECORD_SIZE);
    assert!(offset_of!(Record, seq) == 0);
    assert!(offset_of!(Record, tsc) == 8);
    assert!(offset_of!(Record, a) == 16);
    assert!(offset_of!(Record, b) == 24);
    assert!(offset_of!(Record, cpu) == 32);
    assert!(offset_of!(Record, event) == 36);
};

impl Record {
    #[allow(
        clippy::new_without_default,
        reason = "a const constructor for statics"
    )]
    pub const fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            tsc: AtomicU64::new(0),
            a: AtomicU64::new(0),
            b: AtomicU64::new(0),
            cpu: AtomicU32::new(0),
            event: AtomicU32::new(0),
        }
    }
}

/// A plain copy of a [`Record`], as a reader or the core tool sees it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecordData {
    pub seq: u64,
    pub tsc: u64,
    pub a: u64,
    pub b: u64,
    pub cpu: u32,
    pub event: u32,
}

fn le_u64(b: &[u8; RECORD_SIZE], at: usize) -> u64 {
    let mut w = [0u8; 8];
    let mut i = 0;
    while i < 8 {
        w[i] = b[at + i];
        i += 1;
    }
    u64::from_le_bytes(w)
}

fn le_u32(b: &[u8; RECORD_SIZE], at: usize) -> u32 {
    let mut w = [0u8; 4];
    let mut i = 0;
    while i < 4 {
        w[i] = b[at + i];
        i += 1;
    }
    u32::from_le_bytes(w)
}

impl RecordData {
    /// A record as it lies in memory on a little-endian CPU (a core dump).
    pub fn from_le_bytes(b: &[u8; RECORD_SIZE]) -> Self {
        Self {
            seq: le_u64(b, offset_of!(Record, seq)),
            tsc: le_u64(b, offset_of!(Record, tsc)),
            a: le_u64(b, offset_of!(Record, a)),
            b: le_u64(b, offset_of!(Record, b)),
            cpu: le_u32(b, offset_of!(Record, cpu)),
            event: le_u32(b, offset_of!(Record, event)),
        }
    }

    /// The inverse of [`RecordData::from_le_bytes`].
    pub fn to_le_bytes(&self) -> [u8; RECORD_SIZE] {
        let mut out = [0u8; RECORD_SIZE];
        let fields: [(usize, &[u8]); 6] = [
            (offset_of!(Record, seq), &self.seq.to_le_bytes()),
            (offset_of!(Record, tsc), &self.tsc.to_le_bytes()),
            (offset_of!(Record, a), &self.a.to_le_bytes()),
            (offset_of!(Record, b), &self.b.to_le_bytes()),
            (offset_of!(Record, cpu), &self.cpu.to_le_bytes()),
            (offset_of!(Record, event), &self.event.to_le_bytes()),
        ];
        for (at, bytes) in fields {
            for (i, v) in bytes.iter().enumerate() {
                out[at + i] = *v;
            }
        }
        out
    }

    /// The record's event, or `None` for a zeroed or foreign slot.
    pub fn event(&self) -> Option<Event> {
        Event::from_u32(self.event)
    }
}

/// The one validity rule both readers use: slot data read for position
/// `pos` is that record, whole, when its `seq` is `pos + 1` and it names an
/// event. A torn slot reads `seq == 0`; a stale or overwritten one has the
/// `seq` of another position.
pub fn valid_at(pos: u64, r: &RecordData) -> bool {
    r.seq != 0 && r.seq == pos.wrapping_add(1) && r.event().is_some()
}

/// The valid records of a ring whose next position is `head` and whose
/// slots are `records`, oldest first. The core tool calls it on a dump.
pub fn ordered(head: u64, records: &[RecordData]) -> impl Iterator<Item = &RecordData> {
    let n = records.len() as u64;
    let start = head.saturating_sub(n);
    (start..head).filter_map(move |pos| {
        // `n` is nonzero here: the range is empty when it is 0.
        let r = records.get((pos % n) as usize)?;
        valid_at(pos, r).then_some(r)
    })
}

/// One CPU's ring of `N` records. Only its own CPU pushes, with IF=0.
#[repr(C)]
pub struct Ring<const N: usize> {
    head: AtomicU64,
    cap: AtomicU32,
    record_size: AtomicU32,
    records: [Record; N],
}

const _: () = {
    assert!(offset_of!(Ring<RECORDS_PER_CPU>, head) == 0);
    assert!(offset_of!(Ring<RECORDS_PER_CPU>, cap) == 8);
    assert!(offset_of!(Ring<RECORDS_PER_CPU>, record_size) == 12);
    assert!(offset_of!(Ring<RECORDS_PER_CPU>, records) == 16);
    assert!(size_of::<Ring<RECORDS_PER_CPU>>() == 16 + RECORDS_PER_CPU * RECORD_SIZE);
};

impl<const N: usize> Ring<N> {
    /// An all-zero ring, so a static of it lies in `.bss`; [`Trace::init`]
    /// fills in `cap` and `record_size`.
    #[allow(
        clippy::new_without_default,
        reason = "a const constructor for statics"
    )]
    pub const fn new() -> Self {
        Self {
            head: AtomicU64::new(0),
            cap: AtomicU32::new(0),
            record_size: AtomicU32::new(0),
            records: [const { Record::new() }; N],
        }
    }

    fn init(&self) {
        // Relaxed: `Trace::init`'s Release store of `magic` publishes it; pairs with nothing.
        self.cap.store(N as u32, Ordering::Relaxed);
        // Relaxed: as `cap`; pairs with nothing.
        self.record_size
            .store(RECORD_SIZE as u32, Ordering::Relaxed);
    }

    /// The next position a push fills; the ring holds positions up to it.
    pub fn head(&self) -> u64 {
        // Acquire: pairs with the Release store in `push`, so the slots
        // below it are whole or rewritten after it.
        self.head.load(Ordering::Acquire)
    }

    /// Append one record. The caller is this ring's CPU, with interrupts
    /// off, so pushes to one ring never overlap (DESIGN §2.2).
    pub fn push(&self, cpu: u32, ev: Event, tsc: u64, a: u64, b: u64) {
        if N == 0 {
            return;
        }
        // Relaxed: only this CPU stores `head`; pairs with nothing.
        let pos = self.head.load(Ordering::Relaxed);
        let Some(slot) = self.records.get((pos % N as u64) as usize) else {
            return;
        };
        // Relaxed: the Release fence below orders it before the fields; pairs with nothing.
        slot.seq.store(0, Ordering::Relaxed);
        // Release: pairs with the Acquire fence in `get`; a reader that sees
        // any field below also sees `seq == 0` on its second `seq` read, so a
        // slot caught mid-write is dropped.
        fence(Ordering::Release);
        // Relaxed: the fence above and the `seq` store below order it; pairs with nothing.
        slot.tsc.store(tsc, Ordering::Relaxed);
        // Relaxed: as `tsc`; pairs with nothing.
        slot.a.store(a, Ordering::Relaxed);
        // Relaxed: as `tsc`; pairs with nothing.
        slot.b.store(b, Ordering::Relaxed);
        // Relaxed: as `tsc`; pairs with nothing.
        slot.cpu.store(cpu, Ordering::Relaxed);
        // Relaxed: as `tsc`; pairs with nothing.
        slot.event.store(ev.as_u32(), Ordering::Relaxed);
        let next = pos.wrapping_add(1);
        // Release: pairs with the Acquire `seq` load in `get`; the fields
        // above are whole before `seq` names this position.
        slot.seq.store(next, Ordering::Release);
        // Release: pairs with the Acquire load in `head`.
        self.head.store(next, Ordering::Release);
    }

    /// A seqlock read of the slot for position `pos`: the record, or
    /// `None` when it is torn, stale or overwritten ([`valid_at`]).
    pub fn get(&self, pos: u64) -> Option<RecordData> {
        if N == 0 {
            return None;
        }
        let slot = self.records.get((pos % N as u64) as usize)?;
        // Acquire: pairs with the Release `seq` store in `push`.
        let seq = slot.seq.load(Ordering::Acquire);
        // Relaxed: the `seq` loads and the fence order the fields; pairs with nothing.
        let r = RecordData {
            seq,
            tsc: slot.tsc.load(Ordering::Relaxed),
            a: slot.a.load(Ordering::Relaxed),
            b: slot.b.load(Ordering::Relaxed),
            cpu: slot.cpu.load(Ordering::Relaxed),
            event: slot.event.load(Ordering::Relaxed),
        };
        // Acquire: pairs with the Release fence in `push`, so a field
        // stored by a later push makes the re-read below see its `seq = 0`.
        fence(Ordering::Acquire);
        // Relaxed: the Acquire fence above orders it after the fields; pairs with nothing.
        if slot.seq.load(Ordering::Relaxed) != seq {
            return None;
        }
        valid_at(pos, &r).then_some(r)
    }

    /// Copy the whole ring for [`ordered`]: its head and every slot, each
    /// read as [`Ring::get`] does for the position it holds.
    pub fn snapshot(&self, out: &mut [RecordData; N]) -> u64 {
        let head = self.head();
        let start = head.saturating_sub(N as u64);
        for pos in start..head {
            if let (Some(r), Some(o)) = (self.get(pos), out.get_mut((pos % N as u64) as usize)) {
                *o = r;
            }
        }
        head
    }
}

/// The whole flight recorder: a header, then one ring per CPU.
#[repr(C)]
pub struct Trace<const CPUS: usize, const N: usize> {
    magic: AtomicU64,
    version: AtomicU32,
    record_size: AtomicU32,
    cap: AtomicU32,
    cpus: AtomicU32,
    freq_hz: AtomicU64,
    max_skew: AtomicU64,
    flags: AtomicU32,
    reserved: AtomicU32,
    rings: [Ring<N>; CPUS],
}

/// The kernel's trace: [`MAX_CPUS`] rings of [`RECORDS_PER_CPU`] records.
pub type KernelTrace = Trace<MAX_CPUS, RECORDS_PER_CPU>;

const _: () = {
    assert!(offset_of!(KernelTrace, magic) == 0);
    assert!(offset_of!(KernelTrace, version) == 8);
    assert!(offset_of!(KernelTrace, record_size) == 12);
    assert!(offset_of!(KernelTrace, cap) == 16);
    assert!(offset_of!(KernelTrace, cpus) == 20);
    assert!(offset_of!(KernelTrace, freq_hz) == 24);
    assert!(offset_of!(KernelTrace, max_skew) == 32);
    assert!(offset_of!(KernelTrace, flags) == 40);
    assert!(offset_of!(KernelTrace, reserved) == 44);
    assert!(offset_of!(KernelTrace, rings) == 48);
    assert!(size_of::<KernelTrace>() == 48 + MAX_CPUS * size_of::<Ring<RECORDS_PER_CPU>>());
};

/// Offsets in a [`KernelTrace`] and in one of its rings, which the core
/// tool (`log::vmcore`) decodes from a dump: the layout the assertions
/// above fix, named here because the fields are private.
pub const TRACE_MAGIC_OFF: usize = offset_of!(KernelTrace, magic);
pub const TRACE_FREQ_OFF: usize = offset_of!(KernelTrace, freq_hz);
pub const TRACE_SKEW_OFF: usize = offset_of!(KernelTrace, max_skew);
pub const TRACE_FLAGS_OFF: usize = offset_of!(KernelTrace, flags);
pub const TRACE_RINGS_OFF: usize = offset_of!(KernelTrace, rings);
pub const RING_HEAD_OFF: usize = offset_of!(Ring<RECORDS_PER_CPU>, head);
pub const RING_RECORDS_OFF: usize = offset_of!(Ring<RECORDS_PER_CPU>, records);
pub const RING_SIZE: usize = size_of::<Ring<RECORDS_PER_CPU>>();

impl<const CPUS: usize, const N: usize> Trace<CPUS, N> {
    /// An all-zero trace, so the kernel's static lies in `.bss`. It reads
    /// as not live (no magic) until [`Trace::init`].
    #[allow(
        clippy::new_without_default,
        reason = "a const constructor for statics"
    )]
    pub const fn new() -> Self {
        Self {
            magic: AtomicU64::new(0),
            version: AtomicU32::new(0),
            record_size: AtomicU32::new(0),
            cap: AtomicU32::new(0),
            cpus: AtomicU32::new(0),
            freq_hz: AtomicU64::new(0),
            max_skew: AtomicU64::new(0),
            flags: AtomicU32::new(0),
            reserved: AtomicU32::new(0),
            rings: [const { Ring::new() }; CPUS],
        }
    }

    /// Fill in the header and every ring's geometry, magic last.
    pub fn init(&self) {
        for r in &self.rings {
            r.init();
        }
        // Relaxed: the Release store of `magic` below publishes it; pairs with nothing.
        self.version.store(TRACE_VERSION, Ordering::Relaxed);
        // Relaxed: as `version`; pairs with nothing.
        self.record_size
            .store(RECORD_SIZE as u32, Ordering::Relaxed);
        // Relaxed: as `version`; pairs with nothing.
        self.cap.store(N as u32, Ordering::Relaxed);
        // Relaxed: as `version`; pairs with nothing.
        self.cpus.store(CPUS as u32, Ordering::Relaxed);
        // Release: pairs with the Acquire load in `is_live`.
        self.magic
            .store(u64::from_le_bytes(TRACE_MAGIC), Ordering::Release);
    }

    pub fn is_live(&self) -> bool {
        // Acquire: pairs with the Release store in `init`.
        self.magic.load(Ordering::Acquire) == u64::from_le_bytes(TRACE_MAGIC)
    }

    /// CPU `cpu`'s ring, or `None` past the last one.
    pub fn ring(&self, cpu: u32) -> Option<&Ring<N>> {
        self.rings.get(cpu as usize)
    }

    /// The header's flag bits.
    pub fn flags(&self) -> u32 {
        // Acquire: pairs with the Release store in `publish_clock`.
        self.flags.load(Ordering::Acquire)
    }

    /// Publish the calibration and warp result for the core tool: the
    /// fields, then the flags that say they are there.
    pub fn publish_clock(&self, c: &ClockInfo) {
        // Relaxed: the Release store of `flags` below publishes it; pairs with nothing.
        self.freq_hz.store(c.freq_hz, Ordering::Relaxed);
        // Relaxed: as `freq_hz`; pairs with nothing.
        self.max_skew.store(c.max_skew, Ordering::Relaxed);
        // Release: pairs with the Acquire load in `flags`.
        self.flags.store(c.flags(), Ordering::Release);
    }

    /// The published clock, or `None` before [`Trace::publish_clock`].
    pub fn clock(&self) -> Option<ClockInfo> {
        let flags = self.flags();
        // Relaxed: the Acquire load in `flags` above orders them; pairs with nothing.
        ClockInfo::from_header(
            self.freq_hz.load(Ordering::Relaxed),
            self.max_skew.load(Ordering::Relaxed),
            flags,
        )
    }
}

/// Milliseconds each side of the warp test reads its counter for.
pub const WARP_MS: u64 = 2;
/// Ceiling on one side's warp-test iterations.
pub const WARP_MAX_ITERS: u32 = 200_000;

/// The cache line the bring-up TSC warp test shares between the BSP and
/// one AP (DESIGN §7.4). Each side reads its own counter and compares the
/// read with the largest one either side has published; a read below it
/// is a backward step, which makes the counter unfit to order a trace
/// across CPUs (Linux's `check_tsc_warp` rule).
#[repr(C, align(64))]
pub struct WarpLine {
    last: AtomicU64,
    arrived: AtomicU32,
    left: AtomicU32,
}

impl WarpLine {
    #[allow(
        clippy::new_without_default,
        reason = "a const constructor for statics"
    )]
    pub const fn new() -> Self {
        Self {
            last: AtomicU64::new(0),
            arrived: AtomicU32::new(0),
            left: AtomicU32::new(0),
        }
    }

    /// Ready the line for the next AP. Only once neither side runs.
    pub fn reset(&self) {
        // Relaxed: the Release store of `arrived` below publishes it; pairs with nothing.
        self.last.store(0, Ordering::Relaxed);
        // Relaxed: as `last`; pairs with nothing.
        self.left.store(0, Ordering::Relaxed);
        // Release: pairs with the Acquire loads in `arrive`.
        self.arrived.store(0, Ordering::Release);
    }

    /// The barrier: true once both sides have arrived, false when this
    /// side waited `timeout` cycles of `now` alone and withdrew. Spins on
    /// the counter, never on a timer interrupt.
    pub fn arrive(&self, now: impl Fn() -> u64, timeout: u64) -> bool {
        // AcqRel: pairs with the Release store in `reset` and the other
        // side's `fetch_add`, so the side that arrives second sees the
        // first's `reset`.
        if self.arrived.fetch_add(1, Ordering::AcqRel) >= 1 {
            return true;
        }
        let t0 = now();
        loop {
            // Acquire: pairs with the other side's AcqRel `fetch_add`.
            if self.arrived.load(Ordering::Acquire) >= 2 {
                return true;
            }
            if now().saturating_sub(t0) >= timeout {
                // Withdraw, unless the other side arrived meanwhile.
                // AcqRel, Acquire on failure: pairs with the other side's AcqRel `fetch_add`.
                return self
                    .arrived
                    .compare_exchange(1, 0, Ordering::AcqRel, Ordering::Acquire)
                    .is_err();
            }
            crate::atomic::spin_loop();
        }
    }

    /// One iteration: load the largest published read, take a read, and
    /// publish it. Returns the read and how far it fell below the loaded
    /// value (0 for none).
    pub fn step(&self, read: impl FnOnce() -> u64) -> (u64, u64) {
        // Acquire: pairs with the AcqRel `fetch_max` below on the other
        // side, so the read taken next comes after the one published.
        let prev = self.last.load(Ordering::Acquire);
        let now = read();
        // AcqRel: pairs with the Acquire load and `fetch_max` in the other side's `step`.
        self.last.fetch_max(now, Ordering::AcqRel);
        (now, prev.saturating_sub(now))
    }

    /// Step for `span` cycles of `read`, at most `max_iters` times; the
    /// largest backward step seen, in cycles, 0 for none.
    pub fn run(&self, read: impl Fn() -> u64, span: u64, max_iters: u32) -> u64 {
        let t0 = read();
        let mut worst = 0u64;
        let mut i = 0u32;
        while i < max_iters {
            let (now, back) = self.step(&read);
            worst = worst.max(back);
            if now.saturating_sub(t0) >= span {
                break;
            }
            i += 1;
        }
        worst
    }

    /// The AP side is done with the line.
    pub fn leave(&self) {
        // Release: pairs with the Acquire load in `wait_left`.
        self.left.fetch_add(1, Ordering::Release);
    }

    /// Wait up to `timeout` cycles of `now` for the other side to
    /// [`leave`](WarpLine::leave). False on a timeout.
    pub fn wait_left(&self, now: impl Fn() -> u64, timeout: u64) -> bool {
        let t0 = now();
        loop {
            // Acquire: pairs with the Release `fetch_add` in `leave`.
            if self.left.load(Ordering::Acquire) != 0 {
                return true;
            }
            if now().saturating_sub(t0) >= timeout {
                return false;
            }
            crate::atomic::spin_loop();
        }
    }
}

const _: () = {
    assert!(size_of::<WarpLine>() == 64);
    assert!(core::mem::align_of::<WarpLine>() == 64);
};

/// How a trace's timestamps may be ordered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    /// One timeline across CPUs, merged by timestamp.
    Global,
    /// Each CPU on its own, in ring order; the reason says why.
    PerCpu(&'static str),
}

/// The calibration and warp result the BSP publishes into the trace's
/// header once bring-up is done.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClockInfo {
    /// Counter frequency; 0 when uncalibrated.
    pub freq_hz: u64,
    /// CPUID reports the TSC invariant.
    pub invariant: bool,
    /// At least one AP ran the warp test.
    pub warp_measured: bool,
    /// The largest backward step the warp test saw, in cycles; 0 for none.
    pub max_skew: u64,
}

impl ClockInfo {
    /// DESIGN §6.4: records order across CPUs only when the counter is
    /// invariant and the warp test ran and saw no backward step.
    pub const fn order(&self) -> Order {
        if !self.invariant {
            Order::PerCpu("tsc not invariant")
        } else if !self.warp_measured {
            Order::PerCpu("tsc warp test did not run")
        } else if self.max_skew != 0 {
            Order::PerCpu("tsc warp test saw a backward step")
        } else {
            Order::Global
        }
    }

    /// The header flag bits for this clock.
    pub const fn flags(&self) -> u32 {
        let mut f = CLOCK_PUBLISHED;
        if self.invariant {
            f |= TSC_INVARIANT;
        }
        if self.warp_measured {
            f |= WARP_MEASURED;
        }
        if self.max_skew != 0 {
            f |= WARP_BACKWARD;
        }
        f
    }

    /// The clock a header's fields describe, or `None` before the BSP
    /// published one.
    pub const fn from_header(freq_hz: u64, max_skew: u64, flags: u32) -> Option<ClockInfo> {
        if flags & CLOCK_PUBLISHED == 0 {
            return None;
        }
        Some(ClockInfo {
            freq_hz,
            invariant: flags & TSC_INVARIANT != 0,
            warp_measured: flags & WARP_MEASURED != 0,
            max_skew,
        })
    }
}

/// `cycles` of a counter running at `freq_hz`, in nanoseconds, or `None`
/// when the counter is uncalibrated or the result does not fit.
pub fn cycles_to_ns(cycles: u64, freq_hz: u64) -> Option<u64> {
    if freq_hz == 0 {
        return None;
    }
    let ns = u128::from(cycles)
        .checked_mul(1_000_000_000)?
        .checked_div(u128::from(freq_hz))?;
    u64::try_from(ns).ok()
}

/// A timestamp field: `delta` cycles as microseconds with three decimals,
/// or as raw cycles when the clock is uncalibrated.
fn write_ts<W: fmt::Write>(out: &mut W, delta: u64, freq_hz: u64) -> fmt::Result {
    match cycles_to_ns(delta, freq_hz) {
        Some(ns) => write!(out, "{}.{:03}", ns / 1000, ns % 1000),
        None => write!(out, "{delta}.000"),
    }
}

/// One tracepoint as a Chrome trace-event instant event.
fn write_event<W: fmt::Write>(
    out: &mut W,
    first: &mut bool,
    r: &RecordData,
    ev: Event,
    delta: u64,
    freq_hz: u64,
    pid: u32,
) -> fmt::Result {
    if !*first {
        out.write_char(',')?;
    }
    *first = false;
    let (an, bn) = ev.arg_names();
    write!(
        out,
        "{{\"name\":\"{}\",\"ph\":\"i\",\"s\":\"t\",\"ts\":",
        ev.name()
    )?;
    write_ts(out, delta, freq_hz)?;
    write!(
        out,
        ",\"pid\":{pid},\"tid\":{},\"args\":{{\"{an}\":\"{:#x}\",\"{bn}\":\"{:#x}\",\"seq\":\"{:#x}\"}}}}",
        r.cpu, r.a, r.b, r.seq
    )
}

/// A `ph:"M"` name event for a process or a thread.
fn write_name<W: fmt::Write>(
    out: &mut W,
    first: &mut bool,
    what: &str,
    pid: u32,
    tid: u32,
    name: fmt::Arguments<'_>,
) -> fmt::Result {
    if !*first {
        out.write_char(',')?;
    }
    *first = false;
    write!(
        out,
        "{{\"name\":\"{what}\",\"ph\":\"M\",\"ts\":0,\"pid\":{pid},\"tid\":{tid},\"args\":{{\"name\":\"{name}\"}}}}"
    )
}

/// Write every CPU's records as one Chrome trace-event JSON object, which
/// Perfetto opens. `cpus` holds each CPU's valid records in ring order, as
/// [`ordered`] yields them. With [`Order::Global`] the records form one
/// timeline, merged by timestamp and counted from the smallest; otherwise
/// each CPU is a track of its own in ring order, counted from its first
/// record, and `otherData` says why (DESIGN §6.4). Allocates nothing.
pub fn export_chrome<W: fmt::Write>(
    cpus: &[(u32, &[RecordData])],
    clock: &ClockInfo,
    out: &mut W,
) -> fmt::Result {
    if cpus.len() > MAX_CPUS {
        return Err(fmt::Error);
    }
    let order = clock.order();
    out.write_str("{\"traceEvents\":[")?;
    let mut first = true;
    match order {
        Order::Global => {
            write_name(
                out,
                &mut first,
                "process_name",
                0,
                0,
                format_args!("vibeOS"),
            )?;
            for (cpu, _) in cpus {
                write_name(
                    out,
                    &mut first,
                    "thread_name",
                    0,
                    *cpu,
                    format_args!("cpu {cpu}"),
                )?;
            }
            let base = cpus
                .iter()
                .flat_map(|(_, rs)| rs.iter())
                .filter(|r| r.event().is_some())
                .map(|r| r.tsc)
                .min()
                .unwrap_or(0);
            // A k-way merge: each CPU's cursor, advanced past the smallest
            // head each round; ties go to the lower CPU index.
            let mut at = [0usize; MAX_CPUS];
            loop {
                let mut best: Option<(usize, &RecordData)> = None;
                for (i, (_, rs)) in cpus.iter().enumerate() {
                    let Some(r) = at.get(i).and_then(|&k| rs.get(k)) else {
                        continue;
                    };
                    if best.is_none_or(|(_, b)| r.tsc < b.tsc) {
                        best = Some((i, r));
                    }
                }
                let Some((i, r)) = best else {
                    break;
                };
                if let Some(k) = at.get_mut(i) {
                    *k = k.saturating_add(1);
                }
                if let Some(ev) = r.event() {
                    let delta = r.tsc.saturating_sub(base);
                    write_event(out, &mut first, r, ev, delta, clock.freq_hz, 0)?;
                }
            }
        }
        Order::PerCpu(_) => {
            for (cpu, rs) in cpus {
                write_name(
                    out,
                    &mut first,
                    "process_name",
                    *cpu,
                    *cpu,
                    format_args!("cpu {cpu} (own clock)"),
                )?;
                write_name(
                    out,
                    &mut first,
                    "thread_name",
                    *cpu,
                    *cpu,
                    format_args!("cpu {cpu}"),
                )?;
                let base = rs.iter().find(|r| r.event().is_some()).map_or(0, |r| r.tsc);
                for r in rs.iter() {
                    if let Some(ev) = r.event() {
                        let delta = r.tsc.saturating_sub(base);
                        write_event(out, &mut first, r, ev, delta, clock.freq_hz, *cpu)?;
                    }
                }
            }
        }
    }
    let (name, reason) = match order {
        Order::Global => (
            "global",
            "invariant tsc and no backward step in the warp test",
        ),
        Order::PerCpu(why) => ("per-cpu", why),
    };
    let clk = if clock.freq_hz == 0 { "cycles" } else { "tsc" };
    write!(
        out,
        "],\"displayTimeUnit\":\"ns\",\"otherData\":{{\"clock\":\"{clk}\",\"freq_hz\":\"{}\",\"order\":\"{name}\",\"reason\":\"{reason}\",\"skew_cycles\":\"{}\"}}}}",
        clock.freq_hz, clock.max_skew
    )
}

/// Where [`emit`] sends a record: null, or the kernel's
/// `trace_init::record`, stored by [`set_sink`].
static SINK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Install the recorder. Until then [`emit`] does nothing, so nothing
/// reads the per-CPU base before it is live.
pub fn set_sink(f: fn(Event, u64, u64)) {
    // Release: pairs with the Acquire load in `emit`.
    SINK.store(f as *mut (), Ordering::Release);
}

/// Record `ev` through the installed sink, or do nothing before one is.
#[inline]
pub fn emit(ev: Event, a: u64, b: u64) {
    // Acquire: pairs with the Release store in `set_sink`.
    let p = SINK.load(Ordering::Acquire);
    if p.is_null() {
        return;
    }
    // SAFETY: invariant: `SINK` holds null or a `fn(Event, u64, u64)`;
    // established by `log::trace::set_sink`, its only store.
    let f = unsafe { core::mem::transmute::<*mut (), fn(Event, u64, u64)>(p) };
    f(ev, a, b);
}

/// The event an IDT vector records on entry, or `None` when it must not
/// record: the IST vectors (`#DB` 1, NMI 2, `#DF` 8, `#MC` 18), which `cli`
/// does not mask, and every other exception but `#PF` (14).
pub const fn traced_vector(v: u8) -> Option<Event> {
    match v {
        14 => Some(Event::PageFault),
        32..=255 => Some(Event::IrqEnter),
        _ => None,
    }
}

/// The dispatcher's entry tracepoint, after the stub's GS decision.
#[inline]
pub fn trap_enter(v: u8, fault_addr: u64, error_code: u64) {
    match traced_vector(v) {
        Some(Event::PageFault) => emit(Event::PageFault, fault_addr, error_code),
        Some(Event::IrqEnter) => emit(Event::IrqEnter, u64::from(v), 0),
        _ => {}
    }
}

/// The dispatcher's exit tracepoint, after the vector's body.
#[inline]
pub fn trap_exit(v: u8) {
    if let Some(Event::IrqEnter) = traced_vector(v) {
        emit(Event::IrqExit, u64::from(v), 0);
    }
}

/// `trace!(Event, a, b)`: one tracepoint, recorded on this CPU's ring.
/// A no-op until the kernel installs its sink.
#[macro_export]
macro_rules! trace {
    ($ev:ident, $a:expr, $b:expr) => {
        $crate::log::trace::emit($crate::log::trace::Event::$ev, $a, $b)
    };
}

#[cfg(test)]
mod tests;
