//! Single `swapgs` policy. DESIGN §7.5.
//!
//! After ring 3 exists, `GS_BASE` is the user base at CPL=3 and
//! `KERNEL_GS_BASE` holds `PerCpu`. The `swapgs` instruction lives in
//! two sites that implement one policy:
//!
//! 1. `vibeos_syscall_entry` — first insn. Cannot `call` (user RSP).
//! 2. [`do_swapgs`] — IRQ/exception entry that can fire at CPL=3, after
//!    the CPU has already switched to TSS.RSP0.
//!
//! Kernel ISRs do not `swapgs` when CS.RPL=0. Do not add a third site.

#[cfg(feature = "kernel_tests")]
use vibeos::desc::KERNEL_DS;

#[cfg(feature = "kernel_tests")]
use crate::per_cpu_init;
#[cfg(feature = "kernel_tests")]
use crate::x86::{self, IA32_GS_BASE, IA32_KERNEL_GS_BASE};

#[inline]
pub fn from_user(cs: u64) -> bool {
    cs & 3 == 3
}

/// The ISR `swapgs`. Syscall inlines the same insn as its first byte.
#[inline(always)]
pub unsafe fn do_swapgs() {
    unsafe {
        core::arch::asm!("swapgs", options(nomem, nostack, preserves_flags));
    }
}

#[inline(always)]
pub unsafe fn enter(from_user: bool) {
    if from_user {
        unsafe { do_swapgs() };
    }
}

#[inline(always)]
pub unsafe fn leave(to_user: bool) {
    if to_user {
        unsafe { do_swapgs() };
    }
}

/// Reload kernel data segs and both GS bases to `PerCpu`.
///
/// After a user exception `longjmp`s into `catch`, GS_BASE is already
/// kernel (we swapped on entry) but KERNEL_GS_BASE still holds the user
/// base and DS/ES may be the user selectors.
#[cfg(feature = "kernel_tests")]
pub fn force_kernel() {
    let Some(cpu) = per_cpu_init::try_current() else {
        return;
    };
    let ptr = cpu.self_ptr as u64;
    unsafe { load_data_segs(KERNEL_DS) };
    unsafe {
        x86::wrmsr(IA32_GS_BASE, ptr);
        x86::wrmsr(IA32_KERNEL_GS_BASE, ptr);
    }
}

#[cfg(feature = "kernel_tests")]
unsafe fn load_data_segs(sel: u16) {
    unsafe {
        core::arch::asm!(
            "mov ds, {0:x}",
            "mov es, {0:x}",
            "mov ss, {0:x}",
            "mov fs, {0:x}",
            "mov gs, {0:x}",
            in(reg) sel,
            options(nostack, preserves_flags),
        );
    }
}
