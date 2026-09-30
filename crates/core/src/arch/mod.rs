//! Architecture ports: the portable half (`vibeos-core`) of subsystem `arch` (DESIGN §1.3).
//!
//! The architecture seam (PORTABILITY §11.1): one trait for each row of the
//! seam table whose Seam is a trait, and the umbrella trait [`Port`] over all
//! ten. Portable code takes the port as one type parameter, bounded by the
//! traits it calls or by `Port`, never as `dyn`. Each port implements the
//! traits on one zero-sized type: the kernel's `arch::x86_64::Arch`, and the
//! stub port `stub::Arch` that host builds test against.

pub mod x86_64;

use crate::thread::Tcb;

/// Declared once, in `crate::trap` (ROADMAP §10.3, §10.6).
pub use crate::trap::SyscallAbi;

/// Declared once, in `crate::paging`, beside the `Mapper` that walks through
/// it, so the page-table module uses no port type (ROADMAP §10.3).
pub use crate::paging::PageTable;

/// The four inter-processor interrupts (SMP §7.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ipi {
    Call,
    Shootdown,
    Reschedule,
    Halt,
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for u8 {}
    impl Sealed for u16 {}
    impl Sealed for u32 {}
    impl Sealed for u64 {}
}

/// A width an MMIO register access takes: `u8`, `u16`, `u32` or `u64`.
pub trait MmioWidth: Copy + Into<u64> + sealed::Sealed {}
impl MmioWidth for u8 {}
impl MmioWidth for u16 {}
impl MmioWidth for u32 {}
impl MmioWidth for u64 {}

/// The machine state the boot handshake hands over, normalized.
pub trait BootHandover {
    type Info: 'static;
    /// The boot protocol revision the port asks its loader for (Limine's
    /// base revision).
    const BASE_REVISION: u64;
    /// The handover record. Valid only after entry has captured it, which
    /// every caller outside the entry path is.
    fn info() -> &'static Self::Info;
    /// The physical address of a firmware table the loader handed over as
    /// `raw`: a physical address, or an address in the loader's direct map
    /// at `hhdm_offset`, as the port's revision gives it.
    fn table_phys(raw: u64, hhdm_offset: u64) -> u64;
}

/// This CPU's interrupt mask.
///
/// `Saved` is a move-only token. `restore` unmasks only if the token found
/// interrupts enabled, so saves nest. Portable code calls `restore` and does
/// not rely on what dropping a token does.
pub trait InterruptMask {
    type Saved;
    /// Mask interrupts on this CPU and return what the mask was.
    fn save_disable() -> Self::Saved;
    /// Put the mask back as `s` found it.
    fn restore(s: Self::Saved);
    /// Whether interrupts are unmasked on this CPU.
    fn enabled() -> bool;
}

/// Sending an inter-processor interrupt.
pub trait IpiSend {
    type Error: Copy + core::fmt::Debug;
    /// Send `ipi` to `cpu`. Stores before the call are visible to `cpu` in
    /// its handler.
    fn send(cpu: u32, ipi: Ipi) -> Result<(), Self::Error>;
    /// Send `ipi` to every online CPU but this one, with the same ordering.
    fn send_others(ipi: Ipi) -> Result<(), Self::Error>;
}

/// The free-running cycle counter.
pub trait CycleCounter {
    /// The counter now.
    fn now() -> u64;
    /// Counts per second; `None` until measured.
    fn freq_hz() -> Option<u64>;
}

/// DMA ordering, MMIO accessors, and cache maintenance for DMA (DESIGN §4.7).
pub trait Barriers {
    /// Orders earlier stores to memory a device reads before later stores.
    fn dma_wmb();
    /// Orders earlier loads of memory a device wrote before later loads.
    fn dma_rmb();
    /// Orders earlier stores before later loads, as well as both of the above.
    fn dma_mb();
    /// Read the register at `reg`.
    ///
    /// # Safety
    ///
    /// `reg` is an aligned register of a mapped device, and reading it has no
    /// effect the caller has not accounted for.
    unsafe fn mmio_read<W: MmioWidth>(reg: *const W) -> W;
    /// Write `v` to the register at `reg`.
    ///
    /// # Safety
    ///
    /// `reg` is an aligned register of a mapped device, and the write is one
    /// the caller's driver owns.
    unsafe fn mmio_write<W: MmioWidth>(reg: *mut W, v: W);
    /// Make the CPU's writes to `start..start + len` visible to a device
    /// (a clean; a no-op on a coherent port).
    ///
    /// # Safety
    ///
    /// The range is mapped memory the caller owns for this DMA.
    unsafe fn sync_for_device(start: *const u8, len: usize);
    /// Make a device's writes to `start..start + len` visible to the CPU
    /// (an invalidate; a no-op on a coherent port).
    ///
    /// # Safety
    ///
    /// The range is mapped memory the caller owns for this DMA, and no CPU
    /// store to it is pending that the invalidate would discard.
    unsafe fn sync_for_cpu(start: *const u8, len: usize);
}

/// The per-CPU base register.
pub trait PerCpuBase {
    /// This CPU's id: exact while interrupts are masked, and 0 before the
    /// per-CPU area is live.
    fn cpu_id() -> u32;
    /// The running thread's TCB, read in one instruction that preemption
    /// cannot split (DESIGN §2.9 rule 5); null before the per-CPU area and
    /// the bootstrap thread are live.
    fn current_tcb() -> *mut Tcb;
}

/// Raw user-memory copies, inside the port's user-access window (SMAP, PAN).
/// Each returns the bytes it did not copy.
pub trait UserAccess {
    /// Copy `len` bytes from user address `src` to `dst`.
    ///
    /// # Safety
    ///
    /// `dst..dst + len` is kernel memory the caller may write.
    unsafe fn copy_in(dst: *mut u8, src: u64, len: usize) -> usize;
    /// Copy `len` bytes from `src` to user address `dst`.
    ///
    /// # Safety
    ///
    /// `src..src + len` is kernel memory the caller may read.
    unsafe fn copy_out(dst: u64, src: *const u8, len: usize) -> usize;
}

/// Switching kernel threads.
pub trait ContextSwitch {
    type Context;
    /// Save this thread's context in `old` and resume `new`.
    ///
    /// # Safety
    ///
    /// The caller holds the interrupt mask; `old` is this thread's context,
    /// writable; `new` is a context that `prepare` or an earlier `switch`
    /// filled, whose stack is live.
    unsafe fn switch(old: *mut Self::Context, new: *const Self::Context);
    /// Fill `ctx` so its first switch enters `entry` on the stack whose
    /// 16-byte aligned top is `stack_top`, with interrupts masked.
    fn prepare(ctx: &mut Self::Context, stack_top: u64, entry: u64);
    /// Set whether `ctx` resumes with interrupts unmasked.
    fn resume_with_irqs(ctx: &mut Self::Context, enabled: bool);
}

/// A complete port: every seam trait. Each port implements `Port` for its
/// type with an empty impl, so the compiler names any missing supertrait there.
pub trait Port:
    BootHandover
    + InterruptMask
    + IpiSend
    + CycleCounter
    + PageTable
    + Barriers
    + PerCpuBase
    + SyscallAbi
    + UserAccess
    + ContextSwitch
{
}

#[cfg(any(test, feature = "std"))]
pub mod stub;
