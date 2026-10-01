//! 8259 PIC: remap, mask, spurious. DESIGN §5.5 / ROADMAP §2.3.
//!
//! FADT `iapc_boot_arch` bit 0: skip the ICW sequence when the legacy
//! 8259 is absent. Missing FADT still remaps+masks. `pic: remapped` is
//! emitted after this step either way (programmed or skipped).

use vibeos::pic::{
    self, Eoi, LineAction, PIC_EOI, PIC1_CMD, PIC1_DATA, PIC2_CMD, PIC2_DATA, REMAP_WRITES,
    irq_port_bit, should_program, unclaimed_line,
};
use vibeos::vectors;

use crate::x86;

/// Remap master `0x20` / slave `0x28`, then mask every line.
/// Skips port I/O when FADT says there is no legacy 8259.
///
/// # Safety
/// IRQs should already be off. Call before `lidt`. ACPI walk already ran.
pub unsafe fn remap_and_mask() {
    let legacy = crate::acpi_init::info()
        .and_then(|i| i.fadt)
        .map(|f| f.legacy_8259());
    if !should_program(legacy) {
        return;
    }
    // SAFETY: this fn's `# Safety` (here) is `program`'s: IRQs off, and no
    // IDT loaded yet to route a stray line.
    unsafe { program() };
}

/// Always run the ICW sequence. Slice C needs this even when FADT bit 0
/// skipped the boot remap: QEMU clears that bit (LEGACY_DEVICES) but
/// still has an 8259 parked on vectors 0x08–0x0F. Unmasking IRQ0
/// without a remap turns the PIT into a #DF.
///
/// # Safety
/// IRQs off. IDT already loaded so 0x20 is `pit_irq`, not #DF.
pub unsafe fn program() {
    for &(port, val) in REMAP_WRITES {
        // SAFETY: invariant I229, established at `arch::x86_64::pic::program`:
        // the 8259 ports are this module's, and `REMAP_WRITES` is the ICW sequence
        // plus the delay port's writes.
        unsafe { x86::outb(port, val) };
    }
}

pub fn mask(irq: u8) {
    let Some((port, bit)) = irq_port_bit(irq) else {
        return;
    };
    // SAFETY: invariant I229, established at `arch::x86_64::pic::program`:
    // the 8259 ports are this module's, and `port` is a data port `irq_port_bit` chose.
    unsafe {
        let cur = x86::inb(port);
        x86::outb(port, cur | (1 << bit));
    }
}

pub fn unmask(irq: u8) {
    let Some((port, bit)) = irq_port_bit(irq) else {
        return;
    };
    // SAFETY: invariant I229, established at `arch::x86_64::pic::program`:
    // the 8259 ports are this module's, and `port` is a data port `irq_port_bit` chose.
    unsafe {
        let cur = x86::inb(port);
        x86::outb(port, cur & !(1 << bit));
    }
}

pub fn disable_all() {
    // SAFETY: invariant I229, established at `arch::x86_64::pic::program`:
    // the 8259 ports are this module's.
    unsafe {
        x86::outb(PIC1_DATA, 0xFF);
        x86::outb(PIC2_DATA, 0xFF);
    }
}

/// The body of IRQ vector `0x20 + irq` for every 8259 line no driver
/// claims: `irq_init::unowned` counts it, EOIs it (the LAPIC's when it
/// delivered the vector, else [`unclaimed`]) and logs it (DESIGN §5.5).
pub fn handle(vec: u8) {
    crate::irq_init::unowned(vec);
}

/// An 8259 interrupt on line `irq` (0-15) that no driver claims: read the
/// owning PIC's ISR, and for a spurious IRQ7 or IRQ15 send only the EOI it
/// needs; for any other, mask the line and EOI it (DESIGN §5.5). True when
/// it was spurious.
pub fn unclaimed(irq: u8) -> bool {
    let isr = read_isr(if irq < 8 { PIC1_CMD } else { PIC2_CMD });
    match unclaimed_line(irq, isr) {
        LineAction::Spurious(kind) => {
            eoi(kind);
            true
        }
        LineAction::Unclaimed(kind) => {
            mask(irq);
            eoi(kind);
            false
        }
    }
}

/// Whether line `irq` is masked at its PIC.
#[cfg(feature = "kernel_tests")]
pub fn is_masked(irq: u8) -> bool {
    let Some((port, bit)) = irq_port_bit(irq) else {
        return false;
    };
    // SAFETY: invariant I229, established at `arch::x86_64::pic::program`:
    // the 8259 ports are this module's, and `port` is a data port `irq_port_bit` chose.
    unsafe { x86::inb(port) & (1 << bit) != 0 }
}

/// The IRQ line of 8259 vector `vec`, or `None` outside `0x20`-`0x2F`.
pub fn line_of(vec: u8) -> Option<u8> {
    let irq = vec.wrapping_sub(vectors::IRQ_BASE);
    (irq < 16).then_some(irq)
}

fn read_isr(cmd: u16) -> u8 {
    // SAFETY: invariant I229, established at `arch::x86_64::pic::program`:
    // the 8259 ports are this module's, and `cmd` is a command port.
    unsafe {
        x86::outb(cmd, pic::OCW3_ISR);
        x86::inb(cmd)
    }
}

fn eoi(kind: Eoi) {
    match kind {
        Eoi::None => {}
        // SAFETY: invariant I229, established at `arch::x86_64::pic::program`:
        // the 8259 ports are this module's.
        Eoi::Master => unsafe { x86::outb(PIC1_CMD, PIC_EOI) },
        // SAFETY: invariant I229, as for the master's EOI; established at
        // `arch::x86_64::pic::program`.
        Eoi::SlaveThenMaster => unsafe {
            x86::outb(PIC2_CMD, PIC_EOI);
            x86::outb(PIC1_CMD, PIC_EOI);
        },
    }
}
