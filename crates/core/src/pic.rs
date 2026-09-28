//! 8259 PIC constants and the remap write plan. Port I/O lives in the
//! binary crate. DESIGN §5.5, ROADMAP §2.3.

pub const PIC1_CMD: u16 = 0x20;
pub const PIC1_DATA: u16 = 0x21;
pub const PIC2_CMD: u16 = 0xA0;
pub const PIC2_DATA: u16 = 0xA1;
pub const PIC_EOI: u8 = 0x20;
pub const OCW3_ISR: u8 = 0x0B;

pub const ICW1_INIT: u8 = 0x11;
pub const ICW4_8086: u8 = 0x01;
pub const MASTER_OFFSET: u8 = 0x20;
pub const SLAVE_OFFSET: u8 = 0x28;
pub const MASTER_CASCADE: u8 = 0x04; // slave on IRQ2
pub const SLAVE_ID: u8 = 0x02;
pub const IO_WAIT_PORT: u16 = 0x80;

pub const IRQ_COUNT: u8 = 16;
pub const IRQ7: u8 = 7;
pub const IRQ15: u8 = 15;

/// `(port, value)` pairs for the ICW sequence plus immediate mask-all.
/// `io_wait` is a write to port `0x80` after every PIC write.
pub const REMAP_WRITES: &[(u16, u8)] = &[
    (PIC1_CMD, ICW1_INIT),
    (IO_WAIT_PORT, 0),
    (PIC2_CMD, ICW1_INIT),
    (IO_WAIT_PORT, 0),
    (PIC1_DATA, MASTER_OFFSET),
    (IO_WAIT_PORT, 0),
    (PIC2_DATA, SLAVE_OFFSET),
    (IO_WAIT_PORT, 0),
    (PIC1_DATA, MASTER_CASCADE),
    (IO_WAIT_PORT, 0),
    (PIC2_DATA, SLAVE_ID),
    (IO_WAIT_PORT, 0),
    (PIC1_DATA, ICW4_8086),
    (IO_WAIT_PORT, 0),
    (PIC2_DATA, ICW4_8086),
    (IO_WAIT_PORT, 0),
    (PIC1_DATA, 0xFF),
    (IO_WAIT_PORT, 0),
    (PIC2_DATA, 0xFF),
];

pub const fn irq_port_bit(irq: u8) -> Option<(u16, u8)> {
    if irq < 8 {
        Some((PIC1_DATA, irq))
    } else if irq < 16 {
        Some((PIC2_DATA, irq - 8))
    } else {
        None
    }
}

/// ISR bit 7 clear means the IRQ7/15 delivery was spurious.
pub const fn is_spurious_irq7(master_isr: u8) -> bool {
    master_isr & (1 << 7) == 0
}

pub const fn is_spurious_irq15(slave_isr: u8) -> bool {
    slave_isr & (1 << 7) == 0
}

/// What EOI a spurious/real IRQ7/15 needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eoi {
    None,
    Master,
    SlaveThenMaster,
}

pub const fn irq7_eoi(master_isr: u8) -> Eoi {
    if is_spurious_irq7(master_isr) {
        Eoi::None
    } else {
        Eoi::Master
    }
}

pub const fn irq15_eoi(slave_isr: u8) -> Eoi {
    if is_spurious_irq15(slave_isr) {
        // Slave did not actually service; still ACK the master's cascade.
        Eoi::Master
    } else {
        Eoi::SlaveThenMaster
    }
}

/// FADT `iapc_boot_arch` bit 0. `None` (no FADT) means program the PIC.
pub const fn should_program(legacy_8259: Option<bool>) -> bool {
    match legacy_8259 {
        Some(present) => present,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remap_plan_is_0x20_0x28_then_mask() {
        assert_eq!(MASTER_OFFSET, 0x20);
        assert_eq!(SLAVE_OFFSET, 0x28);
        let mut saw_master_off = false;
        let mut saw_slave_off = false;
        let mut last_pic: Option<(u16, u8)> = None;
        let mut masks = 0usize;
        for &(port, val) in REMAP_WRITES {
            if port == PIC1_DATA && val == MASTER_OFFSET {
                saw_master_off = true;
            }
            if port == PIC2_DATA && val == SLAVE_OFFSET {
                saw_slave_off = true;
            }
            if port != IO_WAIT_PORT {
                last_pic = Some((port, val));
            } else {
                assert!(last_pic.is_some(), "io_wait with no preceding PIC write");
            }
            if (port == PIC1_DATA || port == PIC2_DATA) && val == 0xFF {
                masks += 1;
            }
        }
        assert!(saw_master_off && saw_slave_off);
        assert_eq!(masks, 2, "both PICs masked 0xFF at the end");
        // io_wait between ICWs: more wait pokes than ICW steps.
        let waits = REMAP_WRITES
            .iter()
            .filter(|(p, _)| *p == IO_WAIT_PORT)
            .count();
        assert!(waits >= 8);
        let _ = last_pic;
    }

    #[test]
    fn irq_port_split() {
        assert_eq!(irq_port_bit(0), Some((PIC1_DATA, 0)));
        assert_eq!(irq_port_bit(7), Some((PIC1_DATA, 7)));
        assert_eq!(irq_port_bit(8), Some((PIC2_DATA, 0)));
        assert_eq!(irq_port_bit(15), Some((PIC2_DATA, 7)));
        assert_eq!(irq_port_bit(16), None);
    }

    #[test]
    fn spurious_irq7_skips_eoi() {
        assert_eq!(irq7_eoi(0x00), Eoi::None);
        assert_eq!(irq7_eoi(0x7F), Eoi::None);
        assert_eq!(irq7_eoi(0x80), Eoi::Master);
        assert_eq!(irq7_eoi(0xFF), Eoi::Master);
    }

    #[test]
    fn spurious_irq15_eoi_master_only() {
        assert_eq!(irq15_eoi(0x00), Eoi::Master);
        assert_eq!(irq15_eoi(0x80), Eoi::SlaveThenMaster);
    }

    #[test]
    fn fadt_skip_only_when_bit0_clear() {
        assert!(should_program(None));
        assert!(should_program(Some(true)));
        assert!(!should_program(Some(false)));
    }
}
