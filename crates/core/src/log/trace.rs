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
        self.cap.store(N as u32, Ordering::Relaxed);
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
        let pos = self.head.load(Ordering::Relaxed);
        let Some(slot) = self.records.get((pos % N as u64) as usize) else {
            return;
        };
        slot.seq.store(0, Ordering::Relaxed);
        // Release: a reader that sees any field below also sees `seq == 0`
        // on its second `seq` read, so a slot caught mid-write is dropped.
        fence(Ordering::Release);
        slot.tsc.store(tsc, Ordering::Relaxed);
        slot.a.store(a, Ordering::Relaxed);
        slot.b.store(b, Ordering::Relaxed);
        slot.cpu.store(cpu, Ordering::Relaxed);
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
        self.version.store(TRACE_VERSION, Ordering::Relaxed);
        self.record_size
            .store(RECORD_SIZE as u32, Ordering::Relaxed);
        self.cap.store(N as u32, Ordering::Relaxed);
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
        self.freq_hz.store(c.freq_hz, Ordering::Relaxed);
        self.max_skew.store(c.max_skew, Ordering::Relaxed);
        // Release: pairs with the Acquire load in `flags`.
        self.flags.store(c.flags(), Ordering::Release);
    }

    /// The published clock, or `None` before [`Trace::publish_clock`].
    pub fn clock(&self) -> Option<ClockInfo> {
        let flags = self.flags();
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
        self.last.store(0, Ordering::Relaxed);
        self.left.store(0, Ordering::Relaxed);
        // Release: pairs with the Acquire loads in `arrive`.
        self.arrived.store(0, Ordering::Release);
    }

    /// The barrier: true once both sides have arrived, false when this
    /// side waited `timeout` cycles of `now` alone and withdrew. Spins on
    /// the counter, never on a timer interrupt.
    pub fn arrive(&self, now: impl Fn() -> u64, timeout: u64) -> bool {
        // AcqRel: the side that arrives second sees the first's `reset`.
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
pub fn trap_enter(v: u8, cr2: u64, error_code: u64) {
    match traced_vector(v) {
        Some(Event::PageFault) => emit(Event::PageFault, cr2, error_code),
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
mod tests {
    extern crate std;

    use std::sync::Mutex;
    use std::vec::Vec;

    use super::*;

    #[test]
    fn record_layout_is_fixed() {
        assert_eq!(size_of::<Record>(), 40);
        assert_eq!(offset_of!(Record, seq), 0);
        assert_eq!(offset_of!(Record, tsc), 8);
        assert_eq!(offset_of!(Record, a), 16);
        assert_eq!(offset_of!(Record, b), 24);
        assert_eq!(offset_of!(Record, cpu), 32);
        assert_eq!(offset_of!(Record, event), 36);
        assert_eq!(offset_of!(Ring<4>, records), 16);
        assert_eq!(offset_of!(KernelTrace, rings), 48);
        assert_eq!(
            size_of::<KernelTrace>(),
            48 + MAX_CPUS * (16 + RECORDS_PER_CPU * 40)
        );
        assert_eq!(MAX_CPUS, 64);
        assert_eq!(RECORDS_PER_CPU, 256);
    }

    #[test]
    fn ring_push_assigns_positions() {
        let r: Ring<8> = Ring::new();
        for i in 0..5u64 {
            r.push(3, Event::Wake, 100 + i, i, i * 2);
        }
        assert_eq!(r.head(), 5);
        for pos in 0..5u64 {
            let d = r.get(pos).unwrap();
            assert_eq!(d.seq, pos + 1);
            assert_eq!(d.tsc, 100 + pos);
            assert_eq!(d.a, pos);
            assert_eq!(d.b, pos * 2);
            assert_eq!(d.cpu, 3);
            assert_eq!(d.event(), Some(Event::Wake));
        }
        assert_eq!(r.get(5), None);
    }

    #[test]
    fn ring_wraps_keeping_newest() {
        let r: Ring<4> = Ring::new();
        for i in 0..10u64 {
            r.push(0, Event::Switch, i, i, 0);
        }
        // Positions 0 to 5 were overwritten; their slots hold 6 to 9.
        for pos in 0..6u64 {
            assert_eq!(r.get(pos), None, "pos {pos}");
        }
        for pos in 6..10u64 {
            assert_eq!(r.get(pos).unwrap().a, pos);
        }
        let mut snap = [RecordData::default(); 4];
        let head = r.snapshot(&mut snap);
        let got: Vec<u64> = ordered(head, &snap).map(|d| d.a).collect();
        assert_eq!(got, [6, 7, 8, 9]);
    }

    #[test]
    fn torn_last_record_dropped() {
        let r: Ring<4> = Ring::new();
        for i in 0..3u64 {
            r.push(1, Event::IrqEnter, i, 32, 0);
        }
        // A writer stopped between its `seq = 0` store and its last store.
        r.records[2].seq.store(0, Ordering::Relaxed);
        r.records[2].a.store(0xdead, Ordering::Relaxed);
        assert_eq!(r.get(2), None);
        assert!(r.get(1).is_some());
        let mut snap = [RecordData::default(); 4];
        let head = r.snapshot(&mut snap);
        assert_eq!(ordered(head, &snap).count(), 2);
        // The same slot as a dump holds it: seq 0, fields half-written.
        let mut dump = [RecordData::default(); 4];
        for (i, d) in dump.iter_mut().enumerate().take(3) {
            *d = RecordData {
                seq: i as u64 + 1,
                tsc: i as u64,
                a: 32,
                b: 0,
                cpu: 1,
                event: Event::IrqEnter.as_u32(),
            };
        }
        dump[2].seq = 0;
        let seqs: Vec<u64> = ordered(3, &dump).map(|d| d.seq).collect();
        assert_eq!(seqs, [1, 2]);
        // A stale slot: its seq names another lap.
        dump[2].seq = 7;
        assert_eq!(ordered(3, &dump).count(), 2);
        assert!(!valid_at(2, &dump[2]));
    }

    #[test]
    fn record_data_le_bytes_round_trip() {
        let d = RecordData {
            seq: 0x0102_0304_0506_0708,
            tsc: 0x1112_1314_1516_1718,
            a: 0x2122_2324_2526_2728,
            b: 0x3132_3334_3536_3738,
            cpu: 0x4142_4344,
            event: 0x5152_5354,
        };
        let b = d.to_le_bytes();
        assert_eq!(b[0], 0x08);
        assert_eq!(b[8], 0x18);
        assert_eq!(b[16], 0x28);
        assert_eq!(b[24], 0x38);
        assert_eq!(b[32], 0x44);
        assert_eq!(b[36], 0x54);
        assert_eq!(RecordData::from_le_bytes(&b), d);
        // The bytes a live `Record` holds are the same layout.
        let r: Ring<1> = Ring::new();
        r.push(7, Event::BlockSubmit, 99, 5, 6);
        let raw = &r.records[0] as *const Record as *const [u8; RECORD_SIZE];
        // SAFETY: `Record` is `#[repr(C)]`, 40 bytes of integers with no
        // padding (the const assertions above), established here.
        let got = RecordData::from_le_bytes(unsafe { &*raw });
        assert_eq!(got, r.get(0).unwrap());
    }

    #[test]
    fn traced_vector_skips_ist_vectors() {
        for v in [1u8, 2, 8, 18] {
            assert_eq!(traced_vector(v), None, "vector {v}");
        }
        for v in 0u8..32 {
            let want = if v == 14 {
                Some(Event::PageFault)
            } else {
                None
            };
            assert_eq!(traced_vector(v), want, "vector {v}");
        }
        for v in 32u8..=255 {
            assert_eq!(traced_vector(v), Some(Event::IrqEnter), "vector {v}");
        }
    }

    #[test]
    fn event_encoding_round_trips() {
        assert_eq!(Event::from_u32(0), None);
        assert_eq!(Event::from_u32(12), None);
        for (i, ev) in Event::ALL.iter().enumerate() {
            assert_eq!(ev.as_u32(), i as u32 + 1);
            assert_eq!(Event::from_u32(ev.as_u32()), Some(*ev));
            assert!(!ev.name().is_empty());
        }
        assert_eq!(Event::SyscallEnter.as_u32(), 1);
        assert_eq!(Event::BlockComplete.as_u32(), 11);
    }

    static SEEN: Mutex<Vec<(Event, u64, u64)>> = Mutex::new(Vec::new());

    fn capture(ev: Event, a: u64, b: u64) {
        SEEN.lock().unwrap().push((ev, a, b));
    }

    #[test]
    fn emit_reaches_installed_sink() {
        // The sink is global and other tests' code may emit, so look for
        // this test's own arguments only.
        const A: u64 = 0x5eed_0000_1234_5678;
        set_sink(capture);
        crate::trace!(IpiSend, A, 3);
        trap_enter(14, A, 7);
        trap_enter(2, A, 9);
        trap_enter(0x40, A, 0);
        trap_exit(0x40);
        trap_exit(18);
        let seen = SEEN.lock().unwrap();
        assert!(seen.contains(&(Event::IpiSend, A, 3)));
        assert!(seen.contains(&(Event::PageFault, A, 7)));
        assert!(!seen.iter().any(|e| e.1 == A && e.2 == 9));
        assert!(seen.contains(&(Event::IrqEnter, 0x40, 0)));
        assert!(seen.contains(&(Event::IrqExit, 0x40, 0)));
        assert!(!seen.contains(&(Event::IrqExit, 18, 0)));
    }

    #[test]
    fn trace_init_writes_header() {
        let t: Trace<2, 4> = Trace::new();
        assert!(!t.is_live());
        t.init();
        assert!(t.is_live());
        assert_eq!(t.ring(1).unwrap().cap.load(Ordering::Relaxed), 4);
        assert!(t.ring(2).is_none());
    }

    #[test]
    fn warp_step_scripted_backward() {
        let w = WarpLine::new();
        assert_eq!(w.step(|| 100), (100, 0));
        assert_eq!(w.step(|| 90), (90, 10));
        // The largest read stays published.
        assert_eq!(w.step(|| 95), (95, 5));
        assert_eq!(w.step(|| 120), (120, 0));
        let reads = [200u64, 210, 150, 220, 230];
        let i = std::cell::Cell::new(0usize);
        let w = WarpLine::new();
        let worst = w.run(
            || {
                let v = reads[i.get().min(reads.len() - 1)];
                i.set(i.get() + 1);
                v
            },
            25,
            100,
        );
        assert_eq!(worst, 60);
    }

    #[test]
    fn warp_step_monotonic_sees_none() {
        let w = WarpLine::new();
        let t = std::cell::Cell::new(0u64);
        let clock = || {
            t.set(t.get() + 3);
            t.get()
        };
        assert_eq!(w.run(clock, 3000, WARP_MAX_ITERS), 0);
        // It stops at the span, well before the iteration cap.
        assert!(t.get() < 3100);
        // And at the cap when the span is never reached.
        let n = std::cell::Cell::new(0u32);
        let w = WarpLine::new();
        w.run(
            || {
                n.set(n.get() + 1);
                1
            },
            u64::MAX,
            50,
        );
        assert_eq!(n.get(), 51);
    }

    #[test]
    fn warp_threads_synced_see_none() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicU64 as StdU64;
        // One shared counter both sides read: a synchronized clock.
        let counter = Arc::new(StdU64::new(1));
        let line = Arc::new(WarpLine::new());
        let side = |c: Arc<StdU64>, l: Arc<WarpLine>| {
            std::thread::spawn(move || {
                let read = || c.fetch_add(1, Ordering::Relaxed);
                assert!(l.arrive(read, 1 << 40));
                let back = l.run(read, 20_000, WARP_MAX_ITERS);
                l.leave();
                back
            })
        };
        let a = side(counter.clone(), line.clone());
        let b = side(counter.clone(), line.clone());
        assert_eq!(a.join().unwrap(), 0);
        assert_eq!(b.join().unwrap(), 0);
        assert!(line.wait_left(|| 0, 1));
        line.reset();
        assert_eq!(line.arrived.load(Ordering::Relaxed), 0);
        assert_eq!(line.left.load(Ordering::Relaxed), 0);
        assert_eq!(line.last.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn warp_arrive_times_out_alone() {
        let w = WarpLine::new();
        let t = std::cell::Cell::new(0u64);
        let clock = || {
            t.set(t.get() + 10);
            t.get()
        };
        assert!(!w.arrive(clock, 1000));
        assert!(t.get() >= 1000);
        // It withdrew, so the next side waits for a partner of its own.
        assert_eq!(w.arrived.load(Ordering::Relaxed), 0);
        assert!(!w.wait_left(clock, 100));
        // A partner already there: the second arrival meets it at once.
        w.arrived.store(1, Ordering::Relaxed);
        assert!(w.arrive(|| 0, 0));
    }

    #[test]
    fn order_rule_matrix() {
        for invariant in [false, true] {
            for warp_measured in [false, true] {
                for max_skew in [0u64, 1, 5000] {
                    let c = ClockInfo {
                        freq_hz: 1_000_000_000,
                        invariant,
                        warp_measured,
                        max_skew,
                    };
                    let global = invariant && warp_measured && max_skew == 0;
                    assert_eq!(c.order() == Order::Global, global, "{c:?}");
                    let f = c.flags();
                    assert_eq!(f & CLOCK_PUBLISHED, CLOCK_PUBLISHED);
                    assert_eq!(f & TSC_INVARIANT != 0, invariant);
                    assert_eq!(f & WARP_MEASURED != 0, warp_measured);
                    assert_eq!(f & WARP_BACKWARD != 0, max_skew != 0);
                    assert_eq!(ClockInfo::from_header(c.freq_hz, max_skew, f), Some(c));
                }
            }
        }
        assert_eq!(ClockInfo::from_header(1, 0, 0), None);
        let t: Trace<1, 1> = Trace::new();
        assert_eq!(t.clock(), None);
        let c = ClockInfo {
            freq_hz: 3,
            invariant: true,
            warp_measured: false,
            max_skew: 0,
        };
        t.publish_clock(&c);
        assert_eq!(t.clock(), Some(c));
        assert_eq!(c.order(), Order::PerCpu("tsc warp test did not run"));
    }
}
