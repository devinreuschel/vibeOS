//! PSCI 0.2/1.0 through the device-tree conduit (ROADMAP §11.4).

use core::arch::asm;

use vibeos::arch::aarch64::psci::{self, Conduit};

/// The conduit from the device tree. No machine description, or one with no
/// `/psci` node, names no conduit. `machine_init::init_from_dtb` publishes the
/// description before any PSCI call, and QEMU's `virt` always passes a DTB.
fn conduit() -> Option<Conduit> {
    psci::conduit(crate::machine_init::info().map(|d| d.enable))
}

/// Whether PSCI calls have a conduit. False with no machine description or
/// no `/psci` node, and then no call is made.
pub fn psci_available() -> bool {
    conduit().is_some()
}

/// The conduit a PSCI call takes, for the secondary stub's `CPU_OFF` path.
pub fn conduit_is_hvc() -> bool {
    conduit() == Some(Conduit::Hvc)
}

/// One PSCI call. `fid` is the function id in `x0`. Without a conduit the
/// call is not made and returns `NOT_SUPPORTED`.
///
/// # Safety
/// The firmware implements PSCI on this conduit; a `SYSTEM_*` call may
/// not return.
unsafe fn psci_call(fid: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let Some(conduit) = conduit() else {
        return psci::NOT_SUPPORTED;
    };
    let mut ret: i64;
    // SAFETY: this fn's `# Safety`; established here.
    unsafe {
        match conduit {
            // SMCCC 1.0 lets the firmware clobber x4-x17 and returns results in
            // x1-x3. x18 is the platform register and is not touched here.
            Conduit::Hvc => asm!(
                "hvc #0",
                inout("x0") fid as i64 => ret,
                inlateout("x1") a1 => _,
                inlateout("x2") a2 => _,
                inlateout("x3") a3 => _,
                lateout("x4") _,
                lateout("x5") _,
                lateout("x6") _,
                lateout("x7") _,
                lateout("x8") _,
                lateout("x9") _,
                lateout("x10") _,
                lateout("x11") _,
                lateout("x12") _,
                lateout("x13") _,
                lateout("x14") _,
                lateout("x15") _,
                lateout("x16") _,
                lateout("x17") _,
                options(nostack, preserves_flags),
            ),
            Conduit::Smc => asm!(
                "smc #0",
                inout("x0") fid as i64 => ret,
                inlateout("x1") a1 => _,
                inlateout("x2") a2 => _,
                inlateout("x3") a3 => _,
                lateout("x4") _,
                lateout("x5") _,
                lateout("x6") _,
                lateout("x7") _,
                lateout("x8") _,
                lateout("x9") _,
                lateout("x10") _,
                lateout("x11") _,
                lateout("x12") _,
                lateout("x13") _,
                lateout("x14") _,
                lateout("x15") _,
                lateout("x16") _,
                lateout("x17") _,
                options(nostack, preserves_flags),
            ),
        }
    }
    ret
}

fn psci_halt(fid: u32) -> ! {
    if psci_available() {
        // SAFETY: SYSTEM_OFF / SYSTEM_RESET; halt if firmware returns.
        // established here.
        let _ = unsafe { psci_call(u64::from(fid), 0, 0, 0) };
    } else {
        crate::klog!(
            vibeos::log::Level::Warn,
            "vibeOS: psci: no /psci node, halting"
        );
    }
    // SAFETY: `wfi` touches no memory; established here.
    unsafe {
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
///
/// # Safety
/// `entry_pa` is the physical address of an EL1 entry stub the kernel
/// keeps mapped, and `context` is the parameter page the stub reads, so
/// the CPU that firmware starts runs code and data this kernel owns.
pub unsafe fn cpu_on(target: u64, entry_pa: u64, context: u64) -> i64 {
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
