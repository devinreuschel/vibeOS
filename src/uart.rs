//! 16550 UART register offsets and the DLAB init recipe, DESIGN §0.4.
//!
//! Pure constants and small helpers. Actual port I/O lives in the binary
//! crate so this module is host-testable.

pub const COM1_BASE: u16 = 0x3F8;

// Offsets from base.
pub const REG_DATA: u16 = 0;
pub const REG_IER: u16 = 1;
pub const REG_FCR: u16 = 2;
pub const REG_LCR: u16 = 3;
pub const REG_MCR: u16 = 4;
pub const REG_LSR: u16 = 5;

// DLAB overlays (when LCR bit 7 is set).
pub const REG_DLL: u16 = 0;
pub const REG_DLM: u16 = 1;

// LSR bits.
pub const LSR_DR: u8 = 1 << 0;
pub const LSR_THRE: u8 = 1 << 5;

// Divisor for 115200 baud (input clock 1.8432 MHz / 16 = 115200).
pub const BAUD_115200_DIVISOR: u16 = 1;

// LCR: 8N1, DLAB clear.
pub const LCR_8N1: u8 = 0b0000_0011;
// LCR: DLAB set + 8N1 (used briefly during divisor programming).
pub const LCR_DLAB: u8 = 0b1000_0011;

// FCR: enable + clear RX/TX, 14-byte trigger.
pub const FCR_ENABLE: u8 = 0b1100_0111;

// MCR: DTR, RTS, OUT2 (OUT2 gates the UART's IRQ into the PIC; harmless off).
pub const MCR_READY: u8 = 0b0000_1011;

/// Iteration cap on the transmit-holding-register-empty poll.
/// DESIGN §9.6: unbounded polls in the panic handler wedge the machine.
/// A dead UART drops the byte instead.
pub const TX_POLL_CAP: u32 = 100_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_and_offsets_dont_alias() {
        // Guard against a copy-paste rename.
        assert_ne!(REG_DATA, REG_IER);
        assert_ne!(REG_LCR, REG_LSR);
        assert_ne!(REG_FCR, REG_MCR);
    }

    #[test]
    fn dlab_bit_is_high() {
        assert_eq!(LCR_DLAB & 0x80, 0x80);
        assert_eq!(LCR_8N1 & 0x80, 0);
        assert_ne!(LSR_DR, LSR_THRE);
        assert_eq!(LSR_DR, 1);
    }

    #[test]
    fn divisor_matches_115200() {
        // 1.8432 MHz / (16 * 115200) == 1
        assert_eq!(BAUD_115200_DIVISOR, 1);
    }
}
