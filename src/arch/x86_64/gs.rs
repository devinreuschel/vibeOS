//! Single `swapgs` policy. DESIGN §7.5.
//!
//! After ring 3 exists, `GS_BASE` is the user base at CPL=3 and
//! `KERNEL_GS_BASE` holds `PerCpu`. The `swapgs` instructions live in two
//! places that implement one policy:
//!
//! 1. `syscall_init`: `vibeos_syscall_entry`'s first instruction (it
//!    cannot `call`; the user RSP is live), and the exits' `swapgs` before
//!    `sysretq` and before `iretq`.
//! 2. `arch::idt`'s generated entry paths: the entry `swapgs` when the
//!    interrupted CS.RPL is 3, and the exit's matching one.
//!
//! Do not add another. [`force_kernel`] writes GS MSRs; it is not a
//! `swapgs` site.

use vibeos::desc::KERNEL_DS;

use crate::per_cpu_init;
use crate::x86::{self, IA32_GS_BASE, IA32_KERNEL_GS_BASE};

#[inline]
pub fn from_user(cs: u64) -> bool {
    cs & 3 == 3
}

/// Reload kernel data segs and both GS bases to `PerCpu`.
///
/// After a user exception `longjmp`s into `catch`, GS_BASE is already
/// kernel (swapped on entry) but KERNEL_GS_BASE still holds the user base
/// and DS/ES may be user.
///
/// `mov gs` zeroes `GS_BASE`, and an interrupt taken at CPL 0 does not
/// swap, so IF stays 0 from before the segment loads until `GS_BASE` is
/// back. `KERNEL_GS_BASE` is written first, so an NMI in the window, which
/// swaps by the sign of `GS_BASE`, also lands on `PerCpu`.
pub fn force_kernel() {
    let _irq = x86::InterruptGuard::enter();
    let Some(cpu) = per_cpu_init::try_current() else {
        return;
    };
    let ptr = cpu.self_ptr as u64;
    // SAFETY: invariant I4, established here: both GS MSRs get this CPU's
    // `PerCpu` (`per_cpu_init::try_current`), with IF=0 until `GS_BASE` is
    // written back, and `KERNEL_DS` is the kernel data selector of the GDT
    // `arch::x86_64::gdt::init_bsp` (or `smp_init`'s AP path) loaded.
    unsafe {
        x86::wrmsr(IA32_KERNEL_GS_BASE, ptr);
        x86::load_data_segs(KERNEL_DS);
    }
    #[cfg(feature = "kernel_tests")]
    crate::arch::x86_64::catch::force_kernel_window();
    // SAFETY: invariant I4, as above; established here.
    unsafe { x86::wrmsr(IA32_GS_BASE, ptr) };
}
