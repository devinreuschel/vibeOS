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

/// What the 8259 handler does with line `line` (0-15), which no driver
/// claims, given the in-service register of the PIC that owns it (`isr`:
/// the master's for 0-7, the slave's for 8-15), DESIGN §5.5.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineAction {
    /// A spurious IRQ7 or IRQ15: the ISR bit is clear. Count it and send
    /// this EOI (none for IRQ7; the master's cascade for IRQ15).
    Spurious(Eoi),
    /// A real interrupt on an unclaimed line: mask the line, send this EOI,
    /// count it, and log it.
    Unclaimed(Eoi),
}

/// [`LineAction`] for `line` with in-service register `isr`.
pub const fn unclaimed_line(line: u8, isr: u8) -> LineAction {
    if line == IRQ7 && is_spurious_irq7(isr) {
        LineAction::Spurious(Eoi::None)
    } else if line == IRQ15 && is_spurious_irq15(isr) {
        // The slave did not service it; still ACK the master's cascade.
        LineAction::Spurious(Eoi::Master)
    } else if line < 8 {
        LineAction::Unclaimed(Eoi::Master)
    } else {
        LineAction::Unclaimed(Eoi::SlaveThenMaster)
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
    fn unclaimed_line_decision() {
        for line in 0..16u8 {
            let real = if line < 8 {
                Eoi::Master
            } else {
                Eoi::SlaveThenMaster
            };
            // In service: every line is a real, unclaimed interrupt.
            assert_eq!(
                unclaimed_line(line, 0x80 | (1 << (line % 8))),
                LineAction::Unclaimed(real)
            );
            assert_eq!(unclaimed_line(line, 0xFF), LineAction::Unclaimed(real));
            // ISR clear: only IRQ7 and IRQ15 are spurious.
            let want = match line {
                7 => LineAction::Spurious(Eoi::None),
                15 => LineAction::Spurious(Eoi::Master),
                _ => LineAction::Unclaimed(real),
            };
            assert_eq!(unclaimed_line(line, 0x00), want, "line {line}");
            assert_eq!(unclaimed_line(line, 0x7F), want, "line {line}");
        }
    }

    #[test]
    fn fadt_skip_only_when_bit0_clear() {
        assert!(should_program(None));
        assert!(should_program(Some(true)));
        assert!(!should_program(Some(false)));
    }
}
