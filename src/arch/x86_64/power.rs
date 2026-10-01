//! Power-off and reset: the x86_64 port's "Power-off and reset" module
//! (PORTABILITY §11.1). The kernel shell's `poweroff` and `reboot` and the
//! `reboot` syscall call it, and §11.4 gives aarch64 the same two
//! functions over PSCI.
//!
//! Power-off writes ACPI S5 through the FADT's sleep-control register with
//! a hard-coded `SLP_TYP` of 5, then QEMU's and Bochs's PM1a ports `0x604`
//! and `0xB004`. Restart writes the FADT's reset register, then pulses the
//! 8042's reset line, then writes `0xCF9`. A memory-space register is
//! written through the physmap without being mapped first, and the ports
//! are QEMU's; ROADMAP §20.2 (F097) reads `_S5` and the PM1 control block.
//!
//! The module prints nothing and takes no lock, so ROADMAP §22.2's panic
//! reset can call it.

use vibeos::acpi::{GAS_SYSTEM_IO, GAS_SYSTEM_MEMORY, Gas};

use super::cpu as x86;
use crate::acpi_init;
use crate::paging_init;

/// Turn the machine off: ACPI S5, then QEMU's power-off ports. Halts this
/// CPU if none of them worked.
pub fn power_off() -> ! {
    try_acpi_sleep_s5();
    // SAFETY: 0x604 and 0xB004 are QEMU's and Bochs's ACPI power-off ports,
    // which no module but this one writes (invariant I50 gives them no
    // owner), and a power-off is this function's purpose; established here.
    unsafe {
        x86::outw(0x604, 0x2000);
        x86::outw(0xB004, 0x2000);
    }
    x86::halt();
}

/// Reset the machine: the FADT's reset register, the 8042, then `0xCF9`.
/// Halts this CPU if none of them worked.
pub fn restart() -> ! {
    try_acpi_reset();
    pulse_8042();
    // SAFETY: 0xCF9 is the chipset's reset control, which no module but
    // this one writes (invariant I50 gives it no owner), and a reset is
    // this function's purpose; established here.
    unsafe { x86::outb(0xCF9, 0x06) };
    x86::halt();
}

fn try_acpi_reset() {
    let Some(info) = acpi_init::info() else {
        return;
    };
    let Some(fadt) = info.fadt else {
        return;
    };
    if fadt.reset.is_empty() {
        return;
    }
    write_gas(fadt.reset, fadt.reset_value);
}

fn try_acpi_sleep_s5() {
    let Some(info) = acpi_init::info() else {
        return;
    };
    let Some(fadt) = info.fadt else {
        return;
    };
    if fadt.sleep_control.is_empty() {
        return;
    }
    // SLP_TYPx = 5 in bits 2..4, SLP_EN bit 5.
    write_gas(fadt.sleep_control, (5 << 2) | (1 << 5));
}

fn write_gas(gas: Gas, val: u8) {
    match gas.space_id {
        GAS_SYSTEM_IO => {
            let port = gas.address as u16;
            // SAFETY: the port is the reset or sleep register the FADT names,
            // which no module but this one writes (invariant I50), and the
            // write is the one ACPI defines for it, which ends the machine;
            // the firmware's address is trusted as the ACPI tables are (DESIGN
            // §2.10); established here.
            unsafe {
                match gas.access_size {
                    2 => x86::outw(port, val as u16),
                    3 => x86::outl(port, val as u32),
                    _ => x86::outb(port, val),
                }
            }
        }
        GAS_SYSTEM_MEMORY => {
            if gas.address == 0 {
                return;
            }
            let va = paging_init::HHDM_BASE.wrapping_add(gas.address);
            // SAFETY: the FADT names this register for the reset or sleep
            // write ACPI defines, and the HHDM maps physical memory at
            // `paging_init::HHDM_BASE`; the firmware's address is trusted
            // here as the ACPI tables are (DESIGN §2.10).
            unsafe { (va as *mut u8).write_volatile(val) };
        }
        _ => {}
    }
}

fn pulse_8042() {
    let mut n = 100_000u32;
    while n > 0 {
        // SAFETY: the 8042's owner is `console::kbd_init` (invariant I50);
        // this reads its status to pulse its reset line, which ends the
        // machine, so the owner's state no longer matters; established here.
        if unsafe { x86::inb(vibeos::kbd::STATUS) } & vibeos::kbd::STAT_IBF == 0 {
            // SAFETY: as for the status read above (invariant I50); the
            // 0xFE command pulses the CPU reset line; established here.
            unsafe { x86::outb(vibeos::kbd::CMD, 0xFE) };
            return;
        }
        n -= 1;
    }
}
