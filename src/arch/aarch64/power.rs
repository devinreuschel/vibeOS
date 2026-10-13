//! PSCI 0.2/1.0 through the device-tree conduit (ROADMAP §11.4).

use core::arch::asm;

use vibeos::arch::aarch64::psci;
use vibeos::machine::EnableMethod;

fn conduit_hvc() -> bool {
    crate::machine_init::info()
        .map(|d| matches!(d.enable, EnableMethod::Psci { hvc: true }))
        .unwrap_or(true)
}

/// One PSCI call. `fid` is the function id in `x0`.
///
/// # Safety
/// The firmware implements PSCI on this conduit; a `SYSTEM_*` call may
/// not return.
unsafe fn psci_call(fid: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let hvc = conduit_hvc();
    let mut ret: i64;
    // SAFETY: this fn's `# Safety`; established here.
    unsafe {
        if hvc {
            asm!(
                "hvc #0",
                inout("x0") fid as i64 => ret,
                in("x1") a1,
                in("x2") a2,
                in("x3") a3,
                options(nostack, preserves_flags),
            );
        } else {
            asm!(
                "smc #0",
                inout("x0") fid as i64 => ret,
                in("x1") a1,
                in("x2") a2,
                in("x3") a3,
                options(nostack, preserves_flags),
            );
        }
    }
    ret
}

fn psci_halt(fid: u32) -> ! {
    // SAFETY: SYSTEM_OFF / SYSTEM_RESET; halt if firmware returns.
    // established here.
    unsafe {
        let _ = psci_call(u64::from(fid), 0, 0, 0);
        loop {
            asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}

pub fn power_off() -> ! {
    psci_halt(psci::SYSTEM_OFF as u32);
}

pub fn restart() -> ! {
    psci_halt(psci::SYSTEM_RESET as u32);
}

/// PSCI `CPU_ON` (64-bit). `target` is the MPIDR affinity.
pub fn cpu_on(target: u64, entry_pa: u64, context: u64) -> i64 {
    // SAFETY: CPU_ON; firmware returns a status. established here.
    unsafe { psci_call(psci::CPU_ON, target, entry_pa, context) }
}

/// PSCI `AFFINITY_INFO` for `target` at level 0.
pub fn affinity_info(target: u64) -> i64 {
    // SAFETY: AFFINITY_INFO; firmware returns ON/OFF/ON_PENDING. established here.
    unsafe { psci_call(psci::AFFINITY_INFO, target, 0, 0) }
}

/// PSCI `CPU_OFF` for the calling core. Success does not return.
pub fn cpu_off() -> i64 {
    // SAFETY: CPU_OFF; success does not return, a failure returns a status.
    // established here.
    unsafe { psci_call(psci::CPU_OFF, 0, 0, 0) }
}

/// ktest pass code (`ktest::EXIT_PASS`). A pass is PSCI `SYSTEM_OFF`
/// (QEMU exits 0). Any other code is a fail: write pvpanic's panicked
/// bit and wait. QEMU with `-action panic=pause` reports
/// `GUEST_PANICKED`. Semihosting is not used (ROADMAP §11.7).
#[cfg(feature = "kernel_tests")]
const EXIT_PASS: u32 = 0x10;

/// End a ktest run. Pass powers off; fail signals `pvpanic-pci` and waits.
#[cfg(feature = "kernel_tests")]
pub fn qemu_exit(code: u32) -> ! {
    if code == EXIT_PASS {
        power_off();
    }
    crate::log::pvpanic_init::signal(vibeos::log::pvpanic::Step::Halt);
    // SAFETY: the fail verdict parks this CPU until QEMU pauses on the
    // pvpanic write; established here.
    unsafe {
        loop {
            asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}
