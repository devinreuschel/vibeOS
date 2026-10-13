//! The x86_64 port: its kernel half (DESIGN §1.3).

pub(crate) mod apic_init;
mod boot;
#[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
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
pub mod gs;
pub mod idt;
mod ipi;
pub(crate) mod mmu;
pub mod percpu;
pub mod pic;
pub mod power;
mod trampoline;
pub(crate) mod uaccess;

/// `switch.rs`'s `cli`: the kernel masks interrupts across the GPR shuffle.
macro_rules! switch_cli {
    () => {
        "cli"
    };
}

/// `switch.rs`'s delayed `sti` before the jump to a thread that resumes
/// with interrupts on (DESIGN §5.8).
macro_rules! switch_sti {
    () => {
        "sti"
    };
}

pub mod switch;

use core::arch::asm;
use core::sync::atomic::{compiler_fence, fence};

use vibeos::arch::x86_64::trap::Abi;
use vibeos::arch::{
    Barriers, ContextSwitch, CycleCounter, InterruptMask, MmioWidth, PerCpuBase, Port, SyscallAbi,
};
use vibeos::atomic::statics::{AtomicBool, AtomicU64, Ordering};
use vibeos::proc::syscall_table::{Handlers, NrTable, SysResult};
use vibeos::sched::thread::{CpuContext, Tcb, apply_if_on_resume, prepare_thread};

/// The x86_64 port's hardware half: the seam traits (PORTABILITY §11.1) on
/// one zero-sized type. Kernel code names it as `arch::current::Arch`.
pub struct Arch;

/// TSC ticks per millisecond, 0 until `time_init`'s calibration publishes it.
static TSC_PER_MS: AtomicU64 = AtomicU64::new(0);

/// Whether [`CycleCounter::now`] reads with `rdtscp`, which `time_init`
/// sets once it has seen the CPU has it.
static USE_RDTSCP: AtomicBool = AtomicBool::new(false);

/// Publish the calibrated TSC rate for [`CycleCounter::freq_hz`]. The
/// calibration in `time_init` calls it once.
pub fn publish_tsc_per_ms(v: u64) {
    // Release: pairs with the Acquire load in `freq_hz`.
    TSC_PER_MS.store(v, Ordering::Release);
}

/// Make [`CycleCounter::now`] read with `rdtscp` from here on; `time_init`
/// calls it once, when [`has_rdtscp`] holds.
pub fn publish_rdtscp(on: bool) {
    // Relaxed: pairs with nothing. The flag only picks which serialized
    // read of the same counter runs, and either is right.
    USE_RDTSCP.store(on, Ordering::Relaxed);
}

/// CPUID.8000_0001H:EDX[27]: the CPU has `rdtscp`.
pub fn has_rdtscp() -> bool {
    let (max, _, _, _) = cpu::cpuid(0x8000_0000, 0);
    if max < 0x8000_0001 {
        return false;
    }
    let (_, _, _, edx) = cpu::cpuid(0x8000_0001, 0);
    edx & (1 << 27) != 0
}

/// CPUID.8000_0007H:EDX[8]: the TSC runs at a constant rate in every
/// state.
pub fn invariant_tsc() -> bool {
    let (max, _, _, _) = cpu::cpuid(0x8000_0000, 0);
    if max < 0x8000_0007 {
        return false;
    }
    let (_, _, _, edx) = cpu::cpuid(0x8000_0007, 0);
    edx & (1 << 8) != 0
}

/// A serialized TSC read: `rdtscp` when `use_rdtscp`, else `lfence; rdtsc`.
pub fn rdtsc_ser(use_rdtscp: bool) -> u64 {
    if use_rdtscp {
        cpu::rdtscp()
    } else {
        cpu::lfence_rdtsc()
    }
}

impl InterruptMask for Arch {
    /// `InterruptGuard` keeps `irq_nest` and restores on drop.
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
        // Relaxed: as in `publish_rdtscp`; pairs with nothing.
        rdtsc_ser(USE_RDTSCP.load(Ordering::Relaxed))
    }

    #[inline]
    fn freq_hz() -> Option<u64> {
        // Acquire: pairs with the Release store in `publish_tsc_per_ms`.
        match TSC_PER_MS.load(Ordering::Acquire) {
            0 => None,
            v => v.checked_mul(1000),
        }
    }
}

/// The switch is [`switch::switch_context`]; the portable half fills a first
/// run's frame and the IF-on-resume bit (DESIGN §5.8).
impl ContextSwitch for Arch {
    type Context = CpuContext;

    #[inline]
    unsafe fn switch(old: *mut CpuContext, new: *const CpuContext) {
        // SAFETY: the caller meets `vibeos::arch::ContextSwitch::switch`'s
        // `# Safety` contract, which covers `switch::switch_context`'s;
        // established by the caller's unsafe call, `thread_init::switch_now`.
        unsafe { switch::switch_context(old, new) };
    }

    #[inline]
    fn prepare(ctx: &mut CpuContext, stack_top: u64, entry: u64) {
        prepare_thread(ctx, stack_top, entry);
    }

    #[inline]
    fn resume_with_irqs(ctx: &mut CpuContext, enabled: bool) {
        apply_if_on_resume(&mut ctx.rflags, u32::from(!enabled));
    }
}

/// x86_64 is cache-coherent for DMA, so the syncs do no cache maintenance;
/// the fences order this CPU's accesses to device-visible memory (DESIGN §4.7).
impl Barriers for Arch {
    #[inline]
    fn dma_wmb() {
        // Release: pairs with the Acquire fence in `dma_rmb` on the side that
        // reads what this CPU published.
        fence(Ordering::Release);
        // SAFETY: `sfence` orders stores and touches no memory or register;
        // established here.
        unsafe {
            asm!("sfence", options(nostack, preserves_flags));
        }
        // Release: pairs with nothing. A compiler fence keeps a future port
        // from "optimizing" this into a comment; the atomic fence above is the
        // contract.
        compiler_fence(Ordering::Release);
    }

    #[inline]
    fn dma_rmb() {
        // Acquire: pairs with the Release fence in `dma_wmb` on the side that
        // published what this CPU reads.
        fence(Ordering::Acquire);
        // SAFETY: `lfence` orders loads and touches no memory or register;
        // established here.
        unsafe {
            asm!("lfence", options(nostack, preserves_flags));
        }
        // Acquire: pairs with nothing, as in `dma_wmb`; the fence above is the contract.
        compiler_fence(Ordering::Acquire);
    }

    #[inline]
    fn dma_mb() {
        // SAFETY: `mfence` orders every earlier load and store before every
        // later one and changes no register; without `nomem` it is also a
        // compiler barrier; established here.
        unsafe {
            asm!("mfence", options(nostack, preserves_flags));
        }
    }

    #[inline]
    unsafe fn mmio_read<W: MmioWidth>(reg: *const W) -> W {
        // SeqCst: keeps the compiler from moving memory accesses across the
        // register access (DESIGN §4.7).
        compiler_fence(Ordering::SeqCst);
        // SAFETY: `reg` is an aligned register of a mapped device, the
        // `# Safety` contract of `vibeos::arch::Barriers::mmio_read`;
        // established here by the caller's unsafe call.
        let v = unsafe { core::ptr::read_volatile(reg) };
        // SeqCst: as above, on the far side of the access.
        compiler_fence(Ordering::SeqCst);
        v
    }

    #[inline]
    unsafe fn mmio_write<W: MmioWidth>(reg: *mut W, v: W) {
        // SeqCst: keeps the compiler from moving memory accesses across the
        // register access (DESIGN §4.7).
        compiler_fence(Ordering::SeqCst);
        // SAFETY: `reg` is an aligned register of a mapped device that the
        // caller's driver owns, the `# Safety` contract of
        // `vibeos::arch::Barriers::mmio_write`; established here by the
        // caller's unsafe call.
        unsafe { core::ptr::write_volatile(reg, v) };
        // SeqCst: as above, on the far side of the access.
        compiler_fence(Ordering::SeqCst);
    }

    #[inline]
    unsafe fn sync_for_device(_start: *const u8, _len: usize) {}

    #[inline]
    unsafe fn sync_for_cpu(_start: *const u8, _len: usize) {}
}

/// The fast path: one `gs`-relative load each ([`percpu`]).
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

/// Every seam trait, which `arch::current`'s `assert_port` checks.
impl Port for Arch {}

/// Forwarded to the pure half's `Abi`, where the x86_64 syscall ABI lives.
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
