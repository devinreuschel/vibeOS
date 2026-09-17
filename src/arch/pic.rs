//! 8259 PIC: remap, mask, spurious. DESIGN §5.5 / ROADMAP §2.3.
//!
//! FADT `iapc_boot_arch` bit 0: skip the ICW sequence when the legacy
//! 8259 is absent. Missing FADT still remaps+masks. `pic: remapped` is
//! emitted after this step either way (programmed or skipped).

use vibeos::fmt_util;
use vibeos::pic::{
    self, irq15_eoi, irq7_eoi, irq_port_bit, should_program, Eoi, IRQ15, IRQ7, PIC1_CMD, PIC1_DATA,
    PIC2_CMD, PIC2_DATA, PIC_EOI, REMAP_WRITES,
};
use vibeos::vectors;

use crate::serial::Serial;
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
        unsafe { x86::outb(port, val) };
    }
}

#[allow(dead_code)] // keyboard unmask in phase 5
pub fn mask(irq: u8) {
    let Some((port, bit)) = irq_port_bit(irq) else {
        return;
    };
    unsafe {
        let cur = x86::inb(port);
        x86::outb(port, cur | (1 << bit));
    }
}

pub fn unmask(irq: u8) {
    let Some((port, bit)) = irq_port_bit(irq) else {
        return;
    };
    unsafe {
        let cur = x86::inb(port);
        x86::outb(port, cur & !(1 << bit));
    }
}

#[allow(dead_code)]
pub fn disable_all() {
    unsafe {
        x86::outb(PIC1_DATA, 0xFF);
        x86::outb(PIC2_DATA, 0xFF);
    }
}

/// IRQ vector `0x20 + irq`. Spurious 7/15 never send a bogus EOI.
pub fn handle(vec: u8) {
    let irq = vec.wrapping_sub(vectors::IRQ_BASE);
    match irq {
        IRQ7 => {
            let isr = read_isr(PIC1_CMD);
            eoi(irq7_eoi(isr));
        }
        IRQ15 => {
            let isr = read_isr(PIC2_CMD);
            eoi(irq15_eoi(isr));
        }
        0..=15 => unexpected(irq),
        _ => unexpected(irq),
    }
}

fn read_isr(cmd: u16) -> u8 {
    unsafe {
        x86::outb(cmd, pic::OCW3_ISR);
        x86::inb(cmd)
    }
}

fn eoi(kind: Eoi) {
    match kind {
        Eoi::None => {}
        Eoi::Master => unsafe { x86::outb(PIC1_CMD, PIC_EOI) },
        Eoi::SlaveThenMaster => unsafe {
            x86::outb(PIC2_CMD, PIC_EOI);
            x86::outb(PIC1_CMD, PIC_EOI);
        },
    }
}

fn unexpected(irq: u8) -> ! {
    Serial::write_bytes(b"vibeOS: irq: unexpected ");
    let mut buf = [0u8; 4];
    Serial::write_bytes(fmt_util::write_dec(irq as u64, &mut buf));
    Serial::write_bytes(b"\n");
    x86::halt();
}
