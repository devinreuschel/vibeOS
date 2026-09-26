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

/// # Safety
/// `sel` is a valid data selector; clobbers DS/ES/SS/FS/GS.
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
