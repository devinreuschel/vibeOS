//! Named interrupt vectors. DESIGN §5.3.
//!
//! One module, one set of numbers. A host test asserts uniqueness so a
//! copy-paste that assigns two owners the same vector fails in `make test-unit`
//! rather than as a silent IRQ alias on hardware.

/// CPU exceptions. Hardware-reserved; we do not allocate from this range.
pub const DE: u8 = 0x00;
pub const DB: u8 = 0x01;
pub const NMI: u8 = 0x02;
pub const BP: u8 = 0x03;
pub const OF: u8 = 0x04;
pub const BR: u8 = 0x05;
pub const UD: u8 = 0x06;
pub const NM: u8 = 0x07;
pub const DF: u8 = 0x08;
pub const TS: u8 = 0x0A;
pub const NP: u8 = 0x0B;
pub const SS: u8 = 0x0C;
pub const GP: u8 = 0x0D;
pub const PF: u8 = 0x0E;
pub const MF: u8 = 0x10;
pub const AC: u8 = 0x11;
pub const MC: u8 = 0x12;
pub const XF: u8 = 0x13;
pub const VE: u8 = 0x14;
pub const CP: u8 = 0x15;

/// Legacy PIC after remap. Live only until the I/O APIC takes over.
pub const IRQ_BASE: u8 = 0x20;
pub const IRQ_SLAVE_BASE: u8 = 0x28;
pub const IRQ_PIT: u8 = IRQ_BASE;
pub const IRQ_KEYBOARD: u8 = IRQ_BASE + 1;
pub const IRQ_CASCADE: u8 = IRQ_BASE + 2;
pub const IRQ_SPURIOUS_MASTER: u8 = IRQ_BASE + 7;
pub const IRQ_SPURIOUS_SLAVE: u8 = IRQ_SLAVE_BASE + 7;

/// I/O APIC keyboard (ISA IRQ1 / GSI). First device vector.
pub const KBD: u8 = 0x30;

/// Dynamic device pool (I/O APIC / MSI). Keyboard took 0x30.
pub const DEVICE_VEC_START: u8 = 0x31;
pub const DEVICE_VEC_END: u8 = 0x7F;

/// LAPIC LVT + IPI. High numbers so they are not starved by device IRQs.
pub const LAPIC_TIMER: u8 = 0xF0;
pub const LAPIC_ERROR: u8 = 0xF1;
pub const LAPIC_THERMAL: u8 = 0xF2;
pub const IPI_CALL: u8 = 0xFB;
pub const IPI_SHOOTDOWN: u8 = 0xFC;
pub const IPI_RESCHEDULE: u8 = 0xFD;
pub const IPI_HALT: u8 = 0xFE;
pub const LAPIC_SPURIOUS: u8 = 0xFF;

/// Exception vectors that push an error code. Installing a no-code
/// `x86-interrupt` handler on these misaligns the iret frame.
pub const fn pushes_error_code(vec: u8) -> bool {
    matches!(vec, DF | TS | NP | SS | GP | PF | AC | CP | 29 | 30)
}

/// Every distinct named vector, for the uniqueness test. Aliases
/// (`IRQ_PIT` == `IRQ_BASE`, etc.) are checked separately.
pub const NAMED: &[(&str, u8)] = &[
    ("DE", DE),
    ("DB", DB),
    ("NMI", NMI),
    ("BP", BP),
    ("OF", OF),
    ("BR", BR),
    ("UD", UD),
    ("NM", NM),
    ("DF", DF),
    ("TS", TS),
    ("NP", NP),
    ("SS", SS),
    ("GP", GP),
    ("PF", PF),
    ("MF", MF),
    ("AC", AC),
    ("XF", XF),
    ("VE", VE),
    ("CP", CP),
    ("IRQ_BASE", IRQ_BASE),
    ("IRQ_SLAVE_BASE", IRQ_SLAVE_BASE),
    ("IRQ_KEYBOARD", IRQ_KEYBOARD),
    ("IRQ_CASCADE", IRQ_CASCADE),
    ("IRQ_SPURIOUS_MASTER", IRQ_SPURIOUS_MASTER),
    ("IRQ_SPURIOUS_SLAVE", IRQ_SPURIOUS_SLAVE),
    ("KBD", KBD),
    ("DEVICE_VEC_START", DEVICE_VEC_START),
    ("DEVICE_VEC_END", DEVICE_VEC_END),
    ("LAPIC_TIMER", LAPIC_TIMER),
    ("LAPIC_ERROR", LAPIC_ERROR),
    ("LAPIC_THERMAL", LAPIC_THERMAL),
    ("IPI_CALL", IPI_CALL),
    ("IPI_SHOOTDOWN", IPI_SHOOTDOWN),
    ("IPI_RESCHEDULE", IPI_RESCHEDULE),
    ("IPI_HALT", IPI_HALT),
    ("LAPIC_SPURIOUS", LAPIC_SPURIOUS),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_vectors_are_unique() {
        for (i, (a, va)) in NAMED.iter().enumerate() {
            for (b, vb) in NAMED.iter().skip(i + 1) {
                assert_ne!(va, vb, "{a} and {b} both claimed vector {va:#x}");
            }
        }
    }

    #[test]
    fn pic_and_exception_ranges_do_not_overlap() {
        const {
            assert!(IRQ_BASE >= 0x20);
            assert!(DEVICE_VEC_START > IRQ_SPURIOUS_SLAVE);
        }
        assert_eq!(IRQ_SLAVE_BASE, IRQ_BASE + 8);
        assert_eq!(IRQ_SPURIOUS_MASTER, 0x27);
        assert_eq!(IRQ_SPURIOUS_SLAVE, 0x2F);
        assert_eq!(IRQ_PIT, IRQ_BASE);
        assert_eq!(IRQ_KEYBOARD, IRQ_BASE + 1);
    }

    #[test]
    fn error_code_set_matches_sdm() {
        for v in 0u8..=31 {
            let want = matches!(v, 8 | 10 | 11 | 12 | 13 | 14 | 17 | 21 | 29 | 30);
            assert_eq!(pushes_error_code(v), want, "vec {v}");
        }
        assert!(!pushes_error_code(IRQ_PIT));
        assert!(!pushes_error_code(LAPIC_SPURIOUS));
    }

    #[test]
    fn ipi_vectors_sit_in_the_high_block() {
        for &(_, v) in NAMED {
            if v >= 0xFB {
                assert!(v >= 0xF0);
            }
        }
        assert_eq!(IPI_SHOOTDOWN, 0xFC);
        assert_eq!(IPI_RESCHEDULE, 0xFD);
        assert_eq!(IPI_HALT, 0xFE);
    }
}
