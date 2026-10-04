//! aarch64 trap decode: vector slot and `ESR_EL1` to a `TrapKind`
//! (DESIGN §11.5). Compiles on every host.

use crate::trap::{DebugCause, FpCause, FpUnit, PageFaultCause, TrapKind};

/// Sixteen VBAR entries, 0x80 bytes apart. Index is `offset / 0x80`.
pub const SLOT_CURRENT_SP0_SYNC: u8 = 0;
pub const SLOT_CURRENT_SP0_IRQ: u8 = 1;
pub const SLOT_CURRENT_SP0_FIQ: u8 = 2;
pub const SLOT_CURRENT_SP0_SERROR: u8 = 3;
pub const SLOT_CURRENT_SPX_SYNC: u8 = 4;
pub const SLOT_CURRENT_SPX_IRQ: u8 = 5;
pub const SLOT_CURRENT_SPX_FIQ: u8 = 6;
pub const SLOT_CURRENT_SPX_SERROR: u8 = 7;
pub const SLOT_LOWER_A64_SYNC: u8 = 8;
pub const SLOT_LOWER_A64_IRQ: u8 = 9;
pub const SLOT_LOWER_A64_FIQ: u8 = 10;
pub const SLOT_LOWER_A64_SERROR: u8 = 11;
pub const SLOT_LOWER_A32_SYNC: u8 = 12;
pub const SLOT_LOWER_A32_IRQ: u8 = 13;
pub const SLOT_LOWER_A32_FIQ: u8 = 14;
pub const SLOT_LOWER_A32_SERROR: u8 = 15;

const ESR_EC_SHIFT: u32 = 26;
const ESR_EC_MASK: u64 = 0x3F;
const ESR_ISS_MASK: u64 = 0x01FF_FFFF;
const ISS_FSC_MASK: u32 = 0x3F;
const ISS_WNR: u32 = 1 << 6;

/// Exception class from `ESR_EL1`.
pub const fn esr_ec(esr: u64) -> u8 {
    ((esr >> ESR_EC_SHIFT) & ESR_EC_MASK) as u8
}

const fn esr_iss(esr: u64) -> u32 {
    (esr & ESR_ISS_MASK) as u32
}

/// Slot kind: bits [1:0] of the 16-entry index.
const fn slot_group(slot: u8) -> u8 {
    slot & 3
}

/// Decode a vector slot and `ESR_EL1` (0 on IRQ/FIQ/SError) into a
/// portable [`TrapKind`]. DESIGN §11.5.
pub const fn decode(slot: u8, esr: u64) -> TrapKind {
    match slot_group(slot) {
        1 => TrapKind::Interrupt(0),
        2 => TrapKind::Fiq,
        3 => TrapKind::SError,
        _ => decode_sync(esr_ec(esr), esr_iss(esr)),
    }
}

const fn decode_sync(ec: u8, iss: u32) -> TrapKind {
    match ec {
        0x01 => TrapKind::WaitTrap,
        0x15 => TrapKind::Syscall,
        0x1C => TrapKind::PacFail,
        0x20 | 0x21 => decode_abort(iss, true),
        0x22 | 0x26 => TrapKind::AlignmentCheck,
        0x24 | 0x25 => decode_abort(iss, false),
        0x2C => TrapKind::FloatingPoint(FpUnit::Simd, FpCause::Unknown),
        0x30 | 0x31 | 0x34 | 0x35 => TrapKind::Debug(DebugCause::HwBreakpoint),
        0x32 | 0x33 => TrapKind::Debug(DebugCause::SingleStep),
        0x3C => TrapKind::SoftwareBreak,
        0x00 | 0x07 | 0x0D | 0x0E | 0x18 | 0x19 | 0x1D => TrapKind::Undef,
        _ => TrapKind::Undef,
    }
}

const fn decode_abort(iss: u32, fetch: bool) -> TrapKind {
    let fsc = iss & ISS_FSC_MASK;
    if fsc == 0b100001 {
        return TrapKind::AlignmentCheck;
    }
    // Synchronous external abort, or one taken on a table walk.
    if fsc == 0b010000 || (fsc & 0b11_1100) == 0b010100 {
        return TrapKind::BusError;
    }
    let protection = (fsc & 0b11_1100) == 0b001100;
    let write = !fetch && (iss & ISS_WNR) != 0;
    TrapKind::PageFault(PageFaultCause {
        protection,
        write,
        fetch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc::{SIGBUS, SIGFPE, SIGILL, SIGSEGV, SIGTRAP};
    use crate::trap::si_code::*;
    use crate::trap::{Ring3Action, ring3_action};

    fn esr(ec: u8) -> u64 {
        u64::from(ec) << ESR_EC_SHIFT
    }

    fn esr_iss(ec: u8, iss: u32) -> u64 {
        esr(ec) | u64::from(iss) & ESR_ISS_MASK
    }

    fn action(slot: u8, esr: u64) -> Ring3Action {
        ring3_action(decode(slot, esr))
    }

    fn sig(slot: u8, esr: u64) -> (u32, i32) {
        match action(slot, esr) {
            Ring3Action::Signal { sig, si_code } => (sig, si_code),
            other => panic!("slot {slot} esr {esr:#x} -> {other:?}"),
        }
    }

    #[test]
    fn ring3_action_covers_ecs_0x00_to_0x3f_and_slots() {
        for ec in 0u8..=0x3F {
            let _ = action(SLOT_LOWER_A64_SYNC, esr(ec));
            let _ = action(SLOT_CURRENT_SPX_SYNC, esr(ec));
        }
        for slot in [
            SLOT_CURRENT_SP0_IRQ,
            SLOT_CURRENT_SPX_IRQ,
            SLOT_LOWER_A64_IRQ,
            SLOT_LOWER_A32_IRQ,
        ] {
            assert_eq!(action(slot, 0), Ring3Action::NotRing3, "irq slot {slot}");
            assert!(matches!(decode(slot, 0), TrapKind::Interrupt(_)));
        }
        for slot in [
            SLOT_CURRENT_SP0_FIQ,
            SLOT_CURRENT_SPX_FIQ,
            SLOT_LOWER_A64_FIQ,
            SLOT_LOWER_A32_FIQ,
        ] {
            assert_eq!(action(slot, 0), Ring3Action::NotRing3, "fiq slot {slot}");
            assert_eq!(decode(slot, 0), TrapKind::Fiq);
        }
        for slot in [
            SLOT_CURRENT_SP0_SERROR,
            SLOT_CURRENT_SPX_SERROR,
            SLOT_LOWER_A64_SERROR,
            SLOT_LOWER_A32_SERROR,
        ] {
            assert_eq!(action(slot, 0), Ring3Action::NotRing3, "serror slot {slot}");
            assert_eq!(decode(slot, 0), TrapKind::SError);
        }
    }

    #[test]
    fn design_11_5_rows() {
        assert_eq!(sig(SLOT_LOWER_A64_SYNC, esr(0x00)), (SIGILL, ILL_ILLOPC));
        assert_eq!(
            action(SLOT_LOWER_A64_SYNC, esr(0x01)),
            Ring3Action::StepOver
        );
        assert_eq!(sig(SLOT_LOWER_A64_SYNC, esr(0x07)), (SIGILL, ILL_ILLOPC));
        assert_eq!(sig(SLOT_LOWER_A64_SYNC, esr(0x0D)), (SIGILL, ILL_ILLOPC));
        assert_eq!(sig(SLOT_LOWER_A64_SYNC, esr(0x0E)), (SIGILL, ILL_ILLOPC));
        assert_eq!(action(SLOT_LOWER_A64_SYNC, esr(0x15)), Ring3Action::Syscall);
        assert_eq!(sig(SLOT_LOWER_A64_SYNC, esr(0x18)), (SIGILL, ILL_ILLOPC));
        assert_eq!(sig(SLOT_LOWER_A64_SYNC, esr(0x19)), (SIGILL, ILL_ILLOPC));
        assert_eq!(sig(SLOT_LOWER_A64_SYNC, esr(0x1D)), (SIGILL, ILL_ILLOPC));
        assert_eq!(sig(SLOT_LOWER_A64_SYNC, esr(0x1C)), (SIGILL, ILL_ILLOPN));
        assert_eq!(
            sig(SLOT_LOWER_A64_SYNC, esr_iss(0x20, 0b000100)),
            (SIGSEGV, SEGV_MAPERR)
        );
        assert_eq!(
            sig(SLOT_LOWER_A64_SYNC, esr_iss(0x24, 0b001100)),
            (SIGSEGV, SEGV_ACCERR)
        );
        assert_eq!(
            sig(SLOT_LOWER_A64_SYNC, esr_iss(0x24, 0b100001)),
            (SIGBUS, BUS_ADRALN)
        );
        assert_eq!(
            sig(SLOT_LOWER_A64_SYNC, esr_iss(0x24, 0b010000)),
            (SIGBUS, BUS_OBJERR)
        );
        assert_eq!(sig(SLOT_LOWER_A64_SYNC, esr(0x22)), (SIGBUS, BUS_ADRALN));
        assert_eq!(sig(SLOT_LOWER_A64_SYNC, esr(0x26)), (SIGBUS, BUS_ADRALN));
        match action(SLOT_LOWER_A64_SYNC, esr(0x2C)) {
            Ring3Action::Signal { sig, .. } => assert_eq!(sig, SIGFPE),
            other => panic!("{other:?}"),
        }
        assert_eq!(sig(SLOT_LOWER_A64_SYNC, esr(0x30)), (SIGTRAP, TRAP_HWBKPT));
        assert_eq!(sig(SLOT_LOWER_A64_SYNC, esr(0x32)), (SIGTRAP, TRAP_TRACE));
        assert_eq!(sig(SLOT_LOWER_A64_SYNC, esr(0x34)), (SIGTRAP, TRAP_HWBKPT));
        assert_eq!(sig(SLOT_LOWER_A64_SYNC, esr(0x3C)), (SIGTRAP, TRAP_BRKPT));
        assert_eq!(sig(SLOT_LOWER_A64_SYNC, esr(0x3F)), (SIGILL, ILL_ILLOPC));
    }

    #[test]
    fn data_abort_wnr_and_fetch() {
        let pf = |ec, iss| match decode(SLOT_LOWER_A64_SYNC, esr_iss(ec, iss)) {
            TrapKind::PageFault(c) => c,
            k => panic!("{k:?}"),
        };
        assert_eq!(
            pf(0x24, 0b000100 | ISS_WNR),
            PageFaultCause {
                protection: false,
                write: true,
                fetch: false
            }
        );
        assert_eq!(
            pf(0x20, 0b000100 | ISS_WNR),
            PageFaultCause {
                protection: false,
                write: false,
                fetch: true
            }
        );
    }

    #[test]
    fn every_slot_decodes() {
        for slot in 0u8..16 {
            let _ = decode(slot, 0);
        }
    }
}
