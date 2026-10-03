//! In-guest test of the MSRs `syscall_init::init_cpu` writes (kernel_tests
//! only). Rows: the parent `ktest.rs`'s `TESTS`.

use crate::ktest::Outcome;
use crate::x86::{IA32_SYSENTER_CS, IA32_SYSENTER_EIP, IA32_SYSENTER_ESP};

/// `syscall_init::init_cpu` writes the SYSENTER MSRs 0 whatever they held
/// (DESIGN §11.4): values planted as firmware might leave them are gone
/// after it runs, so a CPL-3 `sysenter` faults instead of entering ring 0.
pub(crate) fn sysenter_msrs_zero() -> Outcome {
    const MSRS: [u32; 3] = [IA32_SYSENTER_CS, IA32_SYSENTER_ESP, IA32_SYSENTER_EIP];
    // IF off, so the plant and the rewrite happen on one CPU.
    let _g = crate::arch::current::InterruptGuard::enter();
    for (msr, v) in MSRS
        .into_iter()
        .zip([0x08, 0xFFFF_8000_0000_1000, 0xFFFF_8000_0000_2000])
    {
        // SAFETY: the SYSENTER MSRs are architectural; ring 3 reaches them
        // only through `sysenter`, which cannot run on this CPU while IF
        // is off here, and `init_cpu` below rewrites them; established
        // here.
        unsafe { crate::x86::wrmsr(msr, v) };
    }
    // SAFETY: `init_cpu`'s contract: the GDT is loaded and `GS_BASE` is
    // this CPU's `PerCpu`, as bring-up left them before `smp: done`;
    // established at `proc::syscall_init::init_bsp` and
    // `proc::syscall_init::init_ap`.
    unsafe { crate::syscall_init::init_cpu() };
    let got = MSRS.map(crate::x86::rdmsr);
    if got != [0; 3] {
        return crate::fail_fmt!("SYSENTER CS/ESP/EIP {got:x?} after init_cpu, want 0");
    }
    Outcome::Ok
}
