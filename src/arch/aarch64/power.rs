//! PSCI `SYSTEM_OFF` / `SYSTEM_RESET` (ROADMAP §11.4).

use core::arch::asm;

use vibeos::machine::EnableMethod;

const PSCI_SYSTEM_OFF: u32 = 0x8400_0008;
const PSCI_SYSTEM_RESET: u32 = 0x8400_0009;

fn conduit_hvc() -> bool {
    crate::machine_init::info()
        .map(|d| matches!(d.enable, EnableMethod::Psci { hvc: true }))
        .unwrap_or(true)
}

fn psci(fid: u32) -> ! {
    let hvc = conduit_hvc();
    // SAFETY: PSCI SMC/HVC; the firmware ends the VM or returns, and we
    // halt if it returns; established here.
    unsafe {
        if hvc {
            asm!("hvc #0", in("x0") u64::from(fid), options(nostack, preserves_flags));
        } else {
            asm!("smc #0", in("x0") u64::from(fid), options(nostack, preserves_flags));
        }
        loop {
            asm!("wfi", options(nomem, nostack, preserves_flags));
        }
    }
}

pub fn power_off() -> ! {
    psci(PSCI_SYSTEM_OFF);
}

pub fn restart() -> ! {
    psci(PSCI_SYSTEM_RESET);
}

/// PSCI `SYSTEM_OFF` for the ktest pass verdict (QEMU exits 0).
#[cfg(feature = "kernel_tests")]
pub fn qemu_exit(code: u32) -> ! {
    let _ = code;
    power_off();
}
