//! The stub port: `vibeos-core`'s host tests use it as their architecture
//! (PORTABILITY §11.1, ROADMAP §10.3).
//!
//! Compiled in host builds only. Its state is per host thread, so parallel
//! tests never share it, and settable through the free functions below. Each
//! host thread starts as its own CPU: `cpu_id` differs between live threads,
//! so an `IrqCell` two threads contend is taken in turn, not read as
//! re-entry. Each
//! seam call that has an effect records an [`Event`] in a bounded log that a
//! test reads with [`take_events`]. The stub holds no assembly: `switch`
//! records the switch and returns, and user addresses are host pointers.

use core::cell::RefCell;
use core::mem::size_of;

use super::x86_64::{paging, syscall};
use super::{
    Barriers, BootHandover, ContextSwitch, CycleCounter, InterruptMask, Ipi, IpiSend, MmioWidth,
    PageTable, PerCpuBase, Port, SyscallAbi, UserAccess,
};
use crate::atomic::statics::AtomicU32;
use crate::atomic::{Ordering, fence};
use crate::paging::{PageFlags, PhysAddr, VirtAddr};
use crate::proc::syscall_table::{Handlers, NrTable, SysResult};

/// Events the log keeps; later ones are counted in [`EventLog::dropped`].
pub const LOG_CAP: usize = 256;

/// How far `restart` moves `ip` back: one syscall instruction.
pub const SYSCALL_INSN_LEN: u64 = 4;

/// The stub port.
pub struct Arch;

/// One recorded seam call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Interrupts went from on to off.
    IrqsOff,
    /// Interrupts went from off to on.
    IrqsOn,
    Ipi {
        cpu: u32,
        ipi: Ipi,
    },
    IpiOthers(Ipi),
    SetRoot(u64),
    FlushPage(u64),
    FlushAll,
    Wmb,
    Rmb,
    Mb,
    MmioRead {
        addr: u64,
        width: u8,
    },
    MmioWrite {
        addr: u64,
        width: u8,
        value: u64,
    },
    SyncForDevice {
        addr: u64,
        len: usize,
    },
    SyncForCpu {
        addr: u64,
        len: usize,
    },
    CopyIn {
        src: u64,
        len: usize,
        left: usize,
    },
    CopyOut {
        dst: u64,
        len: usize,
        left: usize,
    },
    Switch {
        old: u64,
        new: u64,
    },
    /// A test's own event ([`note`]).
    Note {
        tag: &'static str,
        value: u64,
    },
}

/// The first [`LOG_CAP`] events and a count of the rest: a bounded list,
/// not a ring.
#[derive(Clone, Copy, Debug)]
pub struct EventLog {
    events: [Event; LOG_CAP],
    len: usize,
    dropped: u64,
}

impl EventLog {
    const EMPTY: EventLog = EventLog {
        events: [Event::FlushAll; LOG_CAP],
        len: 0,
        dropped: 0,
    };

    /// The kept events, oldest first.
    pub fn as_slice(&self) -> &[Event] {
        self.events.get(..self.len).unwrap_or(&[])
    }

    /// Events recorded after the log was full.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    fn push(&mut self, e: Event) {
        match self.events.get_mut(self.len) {
            Some(slot) => {
                *slot = e;
                self.len += 1;
            }
            None => self.dropped = self.dropped.saturating_add(1),
        }
    }
}

/// `BootHandover::Info` of the stub.
#[derive(Debug, PartialEq, Eq)]
pub struct Boot {
    pub hhdm_offset: u64,
}

static BOOT: Boot = Boot {
    hhdm_offset: 0xFFFF_8000_0000_0000,
};

/// `SyscallAbi::Frame` of the stub.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    pub nr: u64,
    pub args: [u64; 6],
    pub ret: u64,
    pub ip: u64,
    pub sp: u64,
}

/// `ContextSwitch::Context` of the stub.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Context {
    pub sp: u64,
    pub ip: u64,
    pub irqs_on: bool,
}

/// `IpiSend::Error` of the stub: [`refuse_ipis`] is set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpiRefused;

/// A refused IPI is a delivery failure, an I/O error, as `IpiError`'s
/// timeout.
impl From<IpiRefused> for crate::kerror::KError {
    fn from(_: IpiRefused) -> Self {
        Self::Io
    }
}

/// `InterruptMask::Saved` of the stub. Restores on drop, as x86_64's
/// `InterruptGuard` does.
#[must_use]
#[derive(Debug)]
pub struct Saved {
    was_on: bool,
}

impl Drop for Saved {
    fn drop(&mut self) {
        if self.was_on {
            with(|s| s.set_irqs(true));
        }
    }
}

struct State {
    cycles: u64,
    step: u64,
    freq_hz: Option<u64>,
    irqs_on: bool,
    cpu_id: u32,
    /// This thread's own CPU id, which `reset` restores.
    home_cpu: u32,
    root: u64,
    refuse_ipis: bool,
    user_fault: Option<u64>,
    log: EventLog,
}

impl State {
    const DEFAULT: State = State {
        cycles: 0,
        step: 1,
        freq_hz: Some(1_000_000_000),
        irqs_on: true,
        cpu_id: 0,
        home_cpu: 0,
        root: 0,
        refuse_ipis: false,
        user_fault: None,
        log: EventLog::EMPTY,
    };

    fn set_irqs(&mut self, on: bool) {
        if self.irqs_on != on {
            self.irqs_on = on;
            self.log
                .push(if on { Event::IrqsOn } else { Event::IrqsOff });
        }
    }

    /// Bytes a user copy of `len` bytes at `addr` copies before the fault
    /// address, if one is set inside the range.
    fn user_copy_len(&self, addr: u64, len: usize) -> usize {
        match self.user_fault {
            Some(f) if f >= addr => usize::try_from(f - addr).map_or(len, |n| n.min(len)),
            _ => len,
        }
    }
}

/// The CPU id the next host thread's state starts with. A `static`, so
/// `core`'s atomic (C-ATOMICS). Ids are never reused: no portable code
/// indexes a per-CPU table by the stub's `cpu_id`.
static NEXT_CPU: AtomicU32 = AtomicU32::new(0);

/// A new thread's state: the defaults, on a CPU id no other thread started
/// with.
fn thread_state() -> State {
    // Relaxed: the counter only hands out distinct ids; pairs with nothing.
    let id = NEXT_CPU.fetch_add(1, Ordering::Relaxed);
    State {
        cpu_id: id,
        home_cpu: id,
        ..State::DEFAULT
    }
}

crate::atomic::thread_local! {
    static STATE: RefCell<State> = RefCell::new(thread_state());
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|c| f(&mut c.borrow_mut()))
}

fn record(e: Event) {
    with(|s| s.log.push(e));
}

fn addr<T>(p: *const T) -> u64 {
    p as usize as u64
}

/// Restore the defaults: cycles 0, step 1, 1 GHz, IRQs on, this thread's
/// own CPU id, root 0, IPIs accepted, no user fault, and an empty log.
pub fn reset() {
    with(|s| {
        *s = State {
            cpu_id: s.home_cpu,
            home_cpu: s.home_cpu,
            ..State::DEFAULT
        }
    });
}

/// Set the counter's next read.
pub fn set_cycles(v: u64) {
    with(|s| s.cycles = v);
}

/// Set what each read adds to the counter, with wrapping.
pub fn set_cycle_step(v: u64) {
    with(|s| s.step = v);
}

/// Set what `freq_hz` returns.
pub fn set_freq_hz(v: Option<u64>) {
    with(|s| s.freq_hz = v);
}

/// Set what `cpu_id` returns on this thread, until `reset`.
pub fn set_cpu_id(v: u32) {
    with(|s| s.cpu_id = v);
}

/// Make every IPI send fail with [`IpiRefused`], recording nothing.
pub fn refuse_ipis(on: bool) {
    with(|s| s.refuse_ipis = on);
}

/// Make a user copy stop at `at` and return the bytes it left.
pub fn fault_user_at(at: Option<u64>) {
    with(|s| s.user_fault = at);
}

/// Record a test's own event, such as a ring store or load.
pub fn note(tag: &'static str, value: u64) {
    record(Event::Note { tag, value });
}

/// Copy the log and clear it.
pub fn take_events() -> EventLog {
    with(|s| core::mem::replace(&mut s.log, EventLog::EMPTY))
}

impl BootHandover for Arch {
    type Info = Boot;
    fn info() -> &'static Boot {
        &BOOT
    }
}

impl InterruptMask for Arch {
    type Saved = Saved;
    fn save_disable() -> Saved {
        with(|s| {
            let was_on = s.irqs_on;
            s.set_irqs(false);
            Saved { was_on }
        })
    }
    fn restore(s: Saved) {
        drop(s);
    }
    fn enabled() -> bool {
        with(|s| s.irqs_on)
    }
}

impl IpiSend for Arch {
    type Error = IpiRefused;
    fn send(cpu: u32, ipi: Ipi) -> Result<(), IpiRefused> {
        with(|s| {
            if s.refuse_ipis {
                return Err(IpiRefused);
            }
            s.log.push(Event::Ipi { cpu, ipi });
            Ok(())
        })
    }
    fn send_others(ipi: Ipi) -> Result<(), IpiRefused> {
        with(|s| {
            if s.refuse_ipis {
                return Err(IpiRefused);
            }
            s.log.push(Event::IpiOthers(ipi));
            Ok(())
        })
    }
}

impl CycleCounter for Arch {
    fn now() -> u64 {
        with(|s| {
            let v = s.cycles;
            s.cycles = v.wrapping_add(s.step);
            v
        })
    }
    fn freq_hz() -> Option<u64> {
        with(|s| s.freq_hz)
    }
}

/// The x86_64 pure half's format, so host tests walk the tables the kernel
/// builds; the root register and TLB calls are recorded.
impl PageTable for Arch {
    const LEVELS: u8 = paging::LEVELS;
    const ENTRIES: usize = paging::PTES_PER_TABLE;
    const KERNEL_ROOT_FIRST: usize = paging::KERNEL_PML4_FIRST;
    const KERNEL_VA_START: u64 = 0xFFFF_8000_0000_0000;
    const KERNEL_UXN: u64 = 0;
    fn index(va: VirtAddr, level: u8) -> usize {
        paging::index(va, level)
    }
    fn make_entry(_va: VirtAddr, pa: PhysAddr, flags: PageFlags) -> u64 {
        paging::make_pte(pa, flags)
    }
    fn make_table(pa: PhysAddr) -> u64 {
        paging::make_pte(
            pa,
            PageFlags(PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER),
        )
    }
    fn va_ok(va: u64) -> bool {
        crate::paging::is_canonical(va)
    }
    fn is_kernel_va(va: VirtAddr) -> bool {
        va.as_u64() >= Self::KERNEL_VA_START
    }
    fn entry_phys(entry: u64) -> PhysAddr {
        paging::pte_phys(entry)
    }
    fn entry_flags(entry: u64) -> PageFlags {
        paging::pte_flags(entry)
    }
    fn root() -> PhysAddr {
        with(|s| PhysAddr::new(s.root))
    }
    unsafe fn set_root(root: PhysAddr) {
        with(|s| {
            s.root = root.as_u64();
            s.log.push(Event::SetRoot(root.as_u64()));
        });
    }
    fn flush_local(va: VirtAddr) {
        record(Event::FlushPage(va.as_u64()));
    }
    fn flush_local_all() {
        record(Event::FlushAll);
    }
}

impl Barriers for Arch {
    fn dma_wmb() {
        // Release: pairs with the Acquire fence in the reading side's
        // `dma_rmb`, which a host test runs in the device's place.
        fence(Ordering::Release);
        record(Event::Wmb);
    }
    fn dma_rmb() {
        // Acquire: pairs with the Release fence in the writing side's
        // `dma_wmb`.
        fence(Ordering::Acquire);
        record(Event::Rmb);
    }
    fn dma_mb() {
        // SeqCst: orders a store before a later load, pairing with the
        // other side's `dma_mb`.
        fence(Ordering::SeqCst);
        record(Event::Mb);
    }
    unsafe fn mmio_read<W: MmioWidth>(reg: *const W) -> W {
        // SAFETY: `reg` is aligned and readable, the `# Safety` contract of
        // `vibeos::arch::Barriers::mmio_read`; established here by the caller's
        // unsafe call.
        let v = unsafe { core::ptr::read_volatile(reg) };
        record(Event::MmioRead {
            addr: addr(reg),
            width: size_of::<W>() as u8,
        });
        v
    }
    unsafe fn mmio_write<W: MmioWidth>(reg: *mut W, v: W) {
        // SAFETY: `reg` is aligned and writable, the `# Safety` contract of
        // `vibeos::arch::Barriers::mmio_write`; established here by the
        // caller's unsafe call.
        unsafe { core::ptr::write_volatile(reg, v) };
        record(Event::MmioWrite {
            addr: addr(reg),
            width: size_of::<W>() as u8,
            value: v.into(),
        });
    }
    unsafe fn sync_for_device(start: *const u8, len: usize) {
        record(Event::SyncForDevice {
            addr: addr(start),
            len,
        });
    }
    unsafe fn sync_for_cpu(start: *const u8, len: usize) {
        record(Event::SyncForCpu {
            addr: addr(start),
            len,
        });
    }
}

impl PerCpuBase for Arch {
    fn cpu_id() -> u32 {
        with(|s| s.cpu_id)
    }
    fn current_tcb() -> *mut crate::thread::Tcb {
        core::ptr::null_mut()
    }
}

impl SyscallAbi for Arch {
    type Frame = Frame;
    fn nr(f: &Frame) -> u64 {
        f.nr
    }
    fn arg(f: &Frame, i: usize) -> u64 {
        f.args.get(i).copied().unwrap_or(0)
    }
    fn set_ret(f: &mut Frame, v: u64) {
        f.ret = v;
    }
    fn ip(f: &Frame) -> u64 {
        f.ip
    }
    fn set_ip(f: &mut Frame, v: u64) {
        f.ip = v;
    }
    fn sp(f: &Frame) -> u64 {
        f.sp
    }
    fn set_sp(f: &mut Frame, v: u64) {
        f.sp = v;
    }
    /// Moves `ip` back over the syscall instruction; `nr` is kept apart from
    /// `ret`, so it needs no restoring.
    fn restart(f: &mut Frame) {
        f.ip = f.ip.wrapping_sub(SYSCALL_INSN_LEN);
    }
    /// x86_64's numbers, so host tests dispatch the kernel's table.
    fn table() -> &'static NrTable {
        &syscall::TABLE
    }
    fn dispatch<H: Handlers + ?Sized>(h: &mut H, raw_nr: u64, regs: &[u64; 6]) -> SysResult {
        syscall::dispatch(h, raw_nr, regs)
    }
}

impl UserAccess for Arch {
    unsafe fn copy_in(dst: *mut u8, src: u64, len: usize) -> usize {
        let n = with(|s| s.user_copy_len(src, len));
        // SAFETY: the stub's user addresses are host pointers to `len`
        // readable bytes, and `dst` has `len` writable ones, the `# Safety`
        // contract of `vibeos::arch::UserAccess::copy_in`; established here by
        // the caller's unsafe call.
        unsafe { core::ptr::copy_nonoverlapping(src as usize as *const u8, dst, n) };
        let left = len - n;
        record(Event::CopyIn { src, len, left });
        left
    }
    unsafe fn copy_out(dst: u64, src: *const u8, len: usize) -> usize {
        let n = with(|s| s.user_copy_len(dst, len));
        // SAFETY: the stub's user addresses are host pointers to `len`
        // writable bytes, and `src` has `len` readable ones, the `# Safety`
        // contract of `vibeos::arch::UserAccess::copy_out`; established here by
        // the caller's unsafe call.
        unsafe { core::ptr::copy_nonoverlapping(src, dst as usize as *mut u8, n) };
        let left = len - n;
        record(Event::CopyOut { dst, len, left });
        left
    }
}

impl ContextSwitch for Arch {
    type Context = Context;
    unsafe fn switch(old: *mut Context, new: *const Context) {
        record(Event::Switch {
            old: addr(old),
            new: addr(new),
        });
    }
    fn prepare(ctx: &mut Context, stack_top: u64, entry: u64) {
        *ctx = Context {
            sp: stack_top,
            ip: entry,
            irqs_on: false,
        };
    }
    fn resume_with_irqs(ctx: &mut Context, enabled: bool) {
        ctx.irqs_on = enabled;
    }
}

impl Port for Arch {}

const _: () = super::assert_port::<Arch>();

#[cfg(test)]
mod tests {
    use super::*;

    /// What [`drive`] read back through the seam.
    struct Driven<P: Port> {
        info: &'static <P as BootHandover>::Info,
        irqs: [bool; 4],
        cycles: [u64; 2],
        freq_hz: Option<u64>,
        cpu_id: u32,
        send: Result<(), <P as IpiSend>::Error>,
        send_others: Result<(), <P as IpiSend>::Error>,
        root: PhysAddr,
        mmio: u32,
        left_out: usize,
        left_in: usize,
        copied: [u8; 4],
        frame: [u64; 5],
        ctx: [<P as ContextSwitch>::Context; 2],
    }

    /// Portable code's view of a port: every method of every seam trait,
    /// reached only through `P: Port`. `reg` stands in for a device register
    /// and `user` for user memory.
    fn drive<P: Port>(reg: *mut u32, user: &mut [u8; 4]) -> Driven<P>
    where
        <P as SyscallAbi>::Frame: Default,
        <P as ContextSwitch>::Context: Default,
    {
        let info = P::info();

        let before = P::enabled();
        let outer = P::save_disable();
        let inner = P::save_disable();
        let masked = P::enabled();
        P::restore(inner);
        let inner_restored = P::enabled();
        P::restore(outer);
        let after = P::enabled();

        let cycles = [P::now(), P::now()];
        let freq_hz = P::freq_hz();
        let cpu_id = P::cpu_id();

        let send = P::send(1, Ipi::Reschedule);
        let send_others = P::send_others(Ipi::Shootdown);

        // SAFETY: the stub's root register is a plain value, which any root
        // may replace; established here.
        unsafe { P::set_root(PhysAddr::new(0x1000)) };
        let root = P::root();
        P::flush_local(VirtAddr::new(0x4000));
        P::flush_local_all();

        P::dma_wmb();
        P::dma_rmb();
        P::dma_mb();
        // SAFETY: `reg` is the caller's aligned, live `u32`, standing in for a
        // register; established here.
        let mmio = unsafe {
            P::mmio_write(reg, 0xDEAD_BEEF);
            P::mmio_read(reg.cast_const())
        };
        // SAFETY: `user` is live memory this fn borrows mutably; established
        // here.
        unsafe {
            P::sync_for_device(user.as_ptr(), user.len());
            P::sync_for_cpu(user.as_ptr(), user.len());
        }

        let src = [1u8, 2, 3, 4];
        let mut copied = [0u8; 4];
        let uaddr = user.as_mut_ptr() as usize as u64;
        // SAFETY: `src` and `copied` hold 4 bytes each, and `uaddr` is
        // `user`'s 4 bytes, which the stub reads as a host pointer;
        // established here.
        let (left_out, left_in) = unsafe {
            (
                P::copy_out(uaddr, src.as_ptr(), 4),
                P::copy_in(copied.as_mut_ptr(), uaddr, 4),
            )
        };

        let mut f = <P as SyscallAbi>::Frame::default();
        P::set_ip(&mut f, 0x400);
        P::set_sp(&mut f, 0x7000);
        P::set_ret(&mut f, 7);
        P::restart(&mut f);
        let frame = [
            P::nr(&f),
            P::arg(&f, 0),
            P::arg(&f, 6),
            P::ip(&f),
            P::sp(&f),
        ];

        let mut ctx: [<P as ContextSwitch>::Context; 2] = Default::default();
        P::prepare(&mut ctx[1], 0x8000, 0x1234);
        P::resume_with_irqs(&mut ctx[1], true);
        let p = ctx.as_mut_ptr();
        // SAFETY: both pointers are elements of `ctx`, live and aligned, and
        // the second was filled by `prepare`; established here.
        unsafe { P::switch(p, p.add(1).cast_const()) };

        Driven {
            info,
            irqs: [before, masked, inner_restored, after],
            cycles,
            freq_hz,
            cpu_id,
            send,
            send_others,
            root,
            mmio,
            left_out,
            left_in,
            copied,
            frame,
            ctx,
        }
    }

    #[test]
    fn portable_core_on_stub_port() {
        reset();
        set_cpu_id(0);
        let mut reg = 0u32;
        let mut user = [0u8; 4];
        let reg_p: *mut u32 = &mut reg;
        let ua = user.as_ptr() as usize as u64;
        let d = drive::<Arch>(reg_p, &mut user);

        assert_eq!(
            d.info,
            &Boot {
                hhdm_offset: 0xFFFF_8000_0000_0000
            }
        );
        assert_eq!(d.irqs, [true, false, false, true]);
        assert_eq!(d.cycles, [0, 1]);
        assert_eq!(d.freq_hz, Some(1_000_000_000));
        assert_eq!(d.cpu_id, 0);
        assert_eq!(d.send, Ok(()));
        assert_eq!(d.send_others, Ok(()));
        assert_eq!(d.root, PhysAddr::new(0x1000));
        assert_eq!(d.mmio, 0xDEAD_BEEF);
        assert_eq!((d.left_out, d.left_in), (0, 0));
        assert_eq!(d.copied, [1, 2, 3, 4]);
        assert_eq!(user, [1, 2, 3, 4]);
        assert_eq!(d.frame, [0, 0, 0, 0x400 - SYSCALL_INSN_LEN, 0x7000]);
        assert_eq!(d.ctx[0], Context::default());
        assert_eq!(
            d.ctx[1],
            Context {
                sp: 0x8000,
                ip: 0x1234,
                irqs_on: true,
            }
        );

        let log = take_events();
        assert_eq!(log.dropped(), 0);
        let reg_a = reg_p as usize as u64;
        let ev = log.as_slice();
        // The switch's pointers name `drive`'s own frame, so only their
        // distance is checked.
        let Some(Event::Switch { old, new }) = ev.last().copied() else {
            panic!("last event {:?}", ev.last());
        };
        assert_eq!(new - old, size_of::<Context>() as u64);
        assert_eq!(
            &ev[..ev.len() - 1],
            &[
                Event::IrqsOff,
                Event::IrqsOn,
                Event::Ipi {
                    cpu: 1,
                    ipi: Ipi::Reschedule,
                },
                Event::IpiOthers(Ipi::Shootdown),
                Event::SetRoot(0x1000),
                Event::FlushPage(0x4000),
                Event::FlushAll,
                Event::Wmb,
                Event::Rmb,
                Event::Mb,
                Event::MmioWrite {
                    addr: reg_a,
                    width: 4,
                    value: 0xDEAD_BEEF,
                },
                Event::MmioRead {
                    addr: reg_a,
                    width: 4,
                },
                Event::SyncForDevice { addr: ua, len: 4 },
                Event::SyncForCpu { addr: ua, len: 4 },
                Event::CopyOut {
                    dst: ua,
                    len: 4,
                    left: 0,
                },
                Event::CopyIn {
                    src: ua,
                    len: 4,
                    left: 0,
                },
            ][..]
        );
    }

    #[test]
    fn stub_cycle_counter_steps_and_wraps() {
        reset();
        set_cycles(u64::MAX);
        assert_eq!(Arch::now(), u64::MAX);
        assert_eq!(Arch::now(), 0);
        set_cycle_step(10);
        assert_eq!(Arch::now(), 1);
        assert_eq!(Arch::now(), 11);
        set_freq_hz(None);
        assert_eq!(Arch::freq_hz(), None);
        set_freq_hz(Some(24_000_000));
        assert_eq!(Arch::freq_hz(), Some(24_000_000));
    }

    #[test]
    fn stub_interrupt_mask_nests() {
        reset();
        let outer = Arch::save_disable();
        let inner = Arch::save_disable();
        Arch::restore(inner);
        assert!(!Arch::enabled());
        Arch::restore(outer);
        assert!(Arch::enabled());
        {
            let _dropped = Arch::save_disable();
            assert!(!Arch::enabled());
        }
        assert!(Arch::enabled());
        assert_eq!(
            take_events().as_slice(),
            &[Event::IrqsOff, Event::IrqsOn, Event::IrqsOff, Event::IrqsOn][..]
        );
    }

    #[test]
    fn stub_state_is_per_thread() {
        reset();
        set_cpu_id(3);
        let held = Arch::save_disable();
        let (cpu, on) = std::thread::spawn(|| {
            let fresh = (Arch::cpu_id(), Arch::enabled());
            set_cpu_id(7);
            fresh
        })
        .join()
        .unwrap();
        assert!(on);
        assert_ne!(cpu, 3);
        assert_eq!(Arch::cpu_id(), 3);
        assert!(!Arch::enabled());
        Arch::restore(held);
    }

    #[test]
    fn stub_cpu_id_per_thread() {
        use std::sync::mpsc::channel;
        let (ids_tx, ids) = channel();
        let mut stops = std::vec::Vec::new();
        let mut threads = std::vec::Vec::new();
        for _ in 0..2 {
            let ids_tx = ids_tx.clone();
            let (stop_tx, stop) = channel::<()>();
            stops.push(stop_tx);
            threads.push(std::thread::spawn(move || {
                let id = Arch::cpu_id();
                reset();
                ids_tx.send((id, Arch::cpu_id())).unwrap();
                // Stay live until both ids are read.
                stop.recv().unwrap();
            }));
        }
        let (a, b) = (ids.recv().unwrap(), ids.recv().unwrap());
        for s in stops {
            s.send(()).unwrap();
        }
        for t in threads {
            t.join().unwrap();
        }
        assert_ne!(a.0, b.0, "two live host threads share a CPU id");
        assert_eq!((a.0, b.0), (a.1, b.1), "reset keeps the thread's id");
    }

    #[test]
    fn stub_event_log_bounded() {
        reset();
        for i in 0..LOG_CAP + 5 {
            note("n", i as u64);
        }
        let log = take_events();
        assert_eq!(log.as_slice().len(), LOG_CAP);
        assert_eq!(log.dropped(), 5);
        assert_eq!(
            log.as_slice()[LOG_CAP - 1],
            Event::Note {
                tag: "n",
                value: (LOG_CAP - 1) as u64,
            }
        );
        let empty = take_events();
        assert!(empty.as_slice().is_empty());
        assert_eq!(empty.dropped(), 0);
    }

    #[test]
    fn stub_user_copy_stops_at_fault() {
        reset();
        let user = [9u8, 8, 7, 6, 5, 4, 3, 2];
        let src = user.as_ptr() as usize as u64;
        fault_user_at(Some(src + 3));
        let mut dst = [0u8; 8];
        // SAFETY: `user` and `dst` hold 8 bytes each; established here.
        let left = unsafe { Arch::copy_in(dst.as_mut_ptr(), src, 8) };
        assert_eq!(left, 5);
        assert_eq!(dst, [9, 8, 7, 0, 0, 0, 0, 0]);
        assert_eq!(
            take_events().as_slice(),
            &[Event::CopyIn {
                src,
                len: 8,
                left: 5,
            }][..]
        );
    }

    #[test]
    fn stub_ipi_refused() {
        reset();
        refuse_ipis(true);
        assert_eq!(Arch::send(2, Ipi::Call), Err(IpiRefused));
        assert_eq!(Arch::send_others(Ipi::Halt), Err(IpiRefused));
        assert!(take_events().as_slice().is_empty());
        refuse_ipis(false);
        assert_eq!(Arch::send(2, Ipi::Call), Ok(()));
    }

    #[test]
    fn stub_mmio_and_barriers_recorded() {
        reset();
        let mut reg = 0u32;
        let p: *mut u32 = &mut reg;
        // SAFETY: `p` is the live, aligned `reg`; established here.
        let v = unsafe {
            Arch::mmio_write(p, 0x1234_5678);
            Arch::mmio_read(p.cast_const())
        };
        assert_eq!(v, 0x1234_5678);
        Arch::dma_wmb();
        Arch::dma_rmb();
        Arch::dma_mb();
        let a = p as usize as u64;
        assert_eq!(
            take_events().as_slice(),
            &[
                Event::MmioWrite {
                    addr: a,
                    width: 4,
                    value: 0x1234_5678,
                },
                Event::MmioRead { addr: a, width: 4 },
                Event::Wmb,
                Event::Rmb,
                Event::Mb,
            ][..]
        );
    }

    #[test]
    fn stub_context_prepare_and_switch() {
        reset();
        let mut a = Context {
            sp: 1,
            ip: 2,
            irqs_on: true,
        };
        let mut b = Context::default();
        Arch::prepare(&mut b, 0x9000, 0x4242);
        assert_eq!(
            b,
            Context {
                sp: 0x9000,
                ip: 0x4242,
                irqs_on: false,
            }
        );
        let (pa, pb): (*mut Context, *const Context) = (&mut a, &b);
        // SAFETY: both are live locals; the stub only records the pointers;
        // established here.
        unsafe { Arch::switch(pa, pb) };
        assert_eq!(
            take_events().as_slice(),
            &[Event::Switch {
                old: pa as usize as u64,
                new: pb as usize as u64,
            }][..]
        );
        assert_eq!(a.sp, 1);
    }
}
