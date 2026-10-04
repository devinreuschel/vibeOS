//! The aarch64 port: its kernel half (DESIGN §1.3).

pub(crate) mod boot;
#[cfg(feature = "kernel_tests")]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::let_underscore_must_use,
    clippy::unused_result_ok,
    clippy::disallowed_types,
    clippy::disallowed_macros,
    reason = "kernel_tests-only in-guest tests: a failure ends a test, not the kernel"
)]
pub mod catch;
pub mod cpu;
pub mod gdt;
pub(crate) mod gic;
pub mod gs;
pub mod idt;
pub(crate) mod ipi;
pub(crate) mod irqchip;
#[cfg(feature = "kernel_tests")]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::let_underscore_must_use,
    clippy::unused_result_ok,
    clippy::disallowed_types,
    clippy::disallowed_macros,
    reason = "kernel_tests-only in-guest tests: a failure ends a test, not the kernel"
)]
pub mod ktest;
#[cfg(feature = "kernel_tests")]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::let_underscore_must_use,
    clippy::unused_result_ok,
    clippy::disallowed_types,
    clippy::disallowed_macros,
    reason = "kernel_tests-only in-guest tests: a failure ends a test, not the kernel"
)]
pub(crate) mod ktest_el0;
pub(crate) mod mmu;
pub mod percpu;
pub mod pic;
pub mod power;
pub(crate) mod secondary;

/// `switch.rs`'s IRQ mask: the kernel sets DAIF.I across the GPR shuffle.
macro_rules! switch_cli {
    () => {
        "msr daifset, #2"
    };
}

/// `switch.rs`'s delayed unmask before return to a thread that resumes
/// with IRQs on (DESIGN §7.5).
macro_rules! switch_sti {
    () => {
        "msr daifclr, #2"
    };
}

/// Read DAIF into `x2` for the outgoing context.
macro_rules! switch_read_daif {
    () => {
        "mrs x2, daif"
    };
}

mod switch;
pub(crate) mod timer;
pub(crate) mod uaccess;
pub(crate) mod vectors;

use core::sync::atomic::{compiler_fence, fence};

use vibeos::arch::aarch64::trap::Abi;
use vibeos::arch::{
    Barriers, ContextSwitch, CycleCounter, InterruptMask, MmioWidth, PerCpuBase, Port, SyscallAbi,
};
use vibeos::atomic::statics::{AtomicU64, Ordering};
use vibeos::proc::syscall_table::{Handlers, NrTable, SysResult};
use vibeos::sched::thread::{CpuContext, Tcb};

/// The aarch64 port's hardware half.
pub struct Arch;

static CNTFRQ: AtomicU64 = AtomicU64::new(0);

pub fn publish_cntfrq(hz: u64) {
    // Release: pairs with the Acquire load in `freq_hz`.
    CNTFRQ.store(hz, Ordering::Release);
}

impl InterruptMask for Arch {
    type Saved = cpu::InterruptGuard;

    #[inline]
    #[track_caller]
    fn save_disable() -> cpu::InterruptGuard {
        cpu::InterruptGuard::enter()
    }

    #[inline]
    fn restore(s: cpu::InterruptGuard) {
        drop(s);
    }

    #[inline]
    fn enabled() -> bool {
        cpu::interrupts_enabled()
    }
}

impl CycleCounter for Arch {
    #[inline]
    fn now() -> u64 {
        cpu::cntvct()
    }

    #[inline]
    fn freq_hz() -> Option<u64> {
        // Acquire: pairs with the Release store in `publish_cntfrq`.
        match CNTFRQ.load(Ordering::Acquire) {
            0 => None,
            v => Some(v),
        }
    }
}

impl ContextSwitch for Arch {
    type Context = CpuContext;

    #[inline]
    unsafe fn switch(old: *mut CpuContext, new: *const CpuContext) {
        // SAFETY: the caller meets `ContextSwitch::switch`; established by
        // `thread_init::switch_now`.
        unsafe { switch::switch_context(old, new) };
    }

    #[inline]
    fn prepare(ctx: &mut CpuContext, stack_top: u64, entry: u64) {
        *ctx = CpuContext::empty();
        ctx.rip = entry;
        ctx.rsp = stack_top;
        ctx.rflags = vibeos::sched::thread::DAIF_D
            | vibeos::sched::thread::DAIF_A
            | vibeos::sched::thread::DAIF_I
            | vibeos::sched::thread::DAIF_F;
    }

    #[inline]
    fn resume_with_irqs(ctx: &mut CpuContext, enabled: bool) {
        if enabled {
            *ctx.irq_word_mut() &= !vibeos::sched::thread::DAIF_I;
        } else {
            *ctx.irq_word_mut() |= vibeos::sched::thread::DAIF_I;
        }
    }
}

impl Barriers for Arch {
    #[inline]
    fn dma_wmb() {
        // Release: pairs with the Acquire fence in `dma_rmb`.
        fence(Ordering::Release);
        // SAFETY: `dmb oshst` orders stores to device-visible memory. established here.
        unsafe {
            core::arch::asm!("dmb oshst", options(nostack, preserves_flags));
        }
        // Release: pairs with nothing.
        compiler_fence(Ordering::Release);
    }

    #[inline]
    fn dma_rmb() {
        // Acquire: pairs with the Release fence in `dma_wmb`.
        fence(Ordering::Acquire);
        // SAFETY: `dmb oshld` orders loads from device-visible memory. established here.
        unsafe {
            core::arch::asm!("dmb oshld", options(nostack, preserves_flags));
        }
        // Acquire: pairs with nothing.
        compiler_fence(Ordering::Acquire);
    }

    #[inline]
    fn dma_mb() {
        // SAFETY: `dmb osh` is a full outer-shareable barrier. established here.
        unsafe {
            core::arch::asm!("dmb osh", options(nostack, preserves_flags));
        }
    }

    #[inline]
    unsafe fn mmio_read<W: MmioWidth>(reg: *const W) -> W {
        compiler_fence(Ordering::SeqCst);
        // SAFETY: `reg` is a mapped device register, the `# Safety`
        // contract of `vibeos::arch::Barriers::mmio_read`; established
        // here by the caller's unsafe call.
        let v = unsafe { core::ptr::read_volatile(reg) };
        compiler_fence(Ordering::SeqCst);
        v
    }

    #[inline]
    unsafe fn mmio_write<W: MmioWidth>(reg: *mut W, v: W) {
        compiler_fence(Ordering::SeqCst);
        // SAFETY: `reg` is a mapped device register the caller owns. established here.
        unsafe { core::ptr::write_volatile(reg, v) };
        compiler_fence(Ordering::SeqCst);
    }

    #[inline]
    unsafe fn sync_for_device(_start: *const u8, _len: usize) {}

    #[inline]
    unsafe fn sync_for_cpu(_start: *const u8, _len: usize) {}
}

impl PerCpuBase for Arch {
    #[inline(always)]
    fn cpu_id() -> u32 {
        percpu::cpu_id_hint()
    }

    #[inline(always)]
    fn current_tcb() -> *mut Tcb {
        percpu::current_tcb()
    }
}

impl Port for Arch {}

impl SyscallAbi for Arch {
    type Frame = <Abi as SyscallAbi>::Frame;

    #[inline]
    fn nr(f: &Self::Frame) -> u64 {
        Abi::nr(f)
    }

    #[inline]
    fn arg(f: &Self::Frame, i: usize) -> u64 {
        Abi::arg(f, i)
    }

    #[inline]
    fn set_ret(f: &mut Self::Frame, v: u64) {
        Abi::set_ret(f, v);
    }

    #[inline]
    fn ip(f: &Self::Frame) -> u64 {
        Abi::ip(f)
    }

    #[inline]
    fn set_ip(f: &mut Self::Frame, v: u64) {
        Abi::set_ip(f, v);
    }

    #[inline]
    fn sp(f: &Self::Frame) -> u64 {
        Abi::sp(f)
    }

    #[inline]
    fn set_sp(f: &mut Self::Frame, v: u64) {
        Abi::set_sp(f, v);
    }

    #[inline]
    fn restart(f: &mut Self::Frame) {
        Abi::restart(f);
    }

    #[inline]
    fn table() -> &'static NrTable {
        Abi::table()
    }

    #[inline]
    fn dispatch<H: Handlers + ?Sized>(h: &mut H, raw_nr: u64, regs: &[u64; 6]) -> SysResult {
        Abi::dispatch(h, raw_nr, regs)
    }
}
