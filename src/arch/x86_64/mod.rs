//! The x86_64 port: its kernel half (DESIGN §1.3).

pub(crate) mod apic_init;
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
pub mod gs;
pub mod idt;
pub mod pic;
mod trampoline;

use core::arch::asm;
use core::mem::offset_of;

use vibeos::arch::x86_64::trap::Abi;
use vibeos::arch::{CycleCounter, InterruptMask, PerCpuBase, SyscallAbi};
use vibeos::atomic::statics::{AtomicU64, Ordering};
use vibeos::smp::per_cpu::PerCpu;

/// The x86_64 port's hardware half: the seam traits (PORTABILITY §11.1) on
/// one zero-sized type. Kernel code names it as `arch::current::Arch`.
pub struct Arch;

/// TSC ticks per millisecond, 0 until `time_init`'s calibration publishes it.
static TSC_PER_MS: AtomicU64 = AtomicU64::new(0);

/// Publish the calibrated TSC rate for [`CycleCounter::freq_hz`]. The
/// calibration in `time_init` calls it once.
pub fn publish_tsc_per_ms(v: u64) {
    // Release: pairs with the Acquire load in `freq_hz`.
    TSC_PER_MS.store(v, Ordering::Release);
}

impl InterruptMask for Arch {
    /// `InterruptGuard` keeps `irq_nest` and restores on drop.
    type Saved = cpu::InterruptGuard;

    #[inline]
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
        cpu::lfence_rdtsc()
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

impl PerCpuBase for Arch {
    #[inline]
    fn cpu_id() -> u32 {
        if cpu::rdmsr(cpu::IA32_GS_BASE) == 0 {
            return 0;
        }
        let id: u32;
        // SAFETY: invariant I4, established at
        // `smp::per_cpu_init::init_bsp` (on an AP,
        // `smp::per_cpu_init::install_gs`): a nonzero `GS_BASE` is this
        // CPU's `PerCpu`, so the load reads its `cpu_id` and touches no stack
        // or flags.
        unsafe {
            asm!(
                "mov {id:e}, dword ptr gs:[{off}]",
                id = out(reg) id,
                off = const offset_of!(PerCpu, cpu_id),
                options(nostack, preserves_flags, readonly),
            );
        }
        id
    }
}

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
}
