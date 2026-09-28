//! Portable trap kinds and their ring-3 actions: each port decodes a trap into
//! a `TrapKind`, and one table gives each kind its signal and `si_code`, or
//! marks it as not a ring-3 fault (AGENTS.md rule 3, DESIGN §5.2 and §11.5).
//!
//! `decode` reports what the CPU said. It leaves `FpCause` and `DebugCause`
//! unrefined (`Unknown`, `Other`); the fault path refines them from the saved
//! FSW/MXCSR and DR6 before it asks `ring3_action` (ROADMAP §10.6, F005).

use crate::proc::{SIGBUS, SIGFPE, SIGILL, SIGSEGV, SIGTRAP};

/// What the CPU reported, in port-neutral terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrapKind {
    DivideError,
    Debug(DebugCause),
    Nmi,
    Breakpoint,
    Overflow,
    BoundRange,
    InvalidOpcode,
    DeviceNotAvailable,
    DoubleFault,
    InvalidTss,
    SegmentNotPresent,
    StackFault,
    GeneralProtection,
    PageFault(PageFaultCause),
    FloatingPoint(FpUnit, FpCause),
    AlignmentCheck,
    MachineCheck,
    /// x86_64 vectors 9, 15 and 20-31 (`#VE`, `#CP`, `#HV`, `#VC`, `#SX`, reserved).
    Reserved(u8),
    /// x86_64 vectors 32-255: device IRQs and IPIs.
    Interrupt(u8),
}

/// Why a debug trap fired. `decode` gives `Other`; the fault path refines it from DR6.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebugCause {
    SingleStep,
    HwBreakpoint,
    Other,
}

/// The `#PF` error code's present (bit 0), write (bit 1) and fetch (bit 4) bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageFaultCause {
    pub protection: bool,
    pub write: bool,
    pub fetch: bool,
}

/// Which floating-point unit raised the error: x87 (`#MF`) or SIMD (`#XM`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FpUnit {
    X87,
    Simd,
}

/// The first unmasked floating-point exception flag. `decode` gives `Unknown`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FpCause {
    Invalid,
    DivideByZero,
    Overflow,
    Underflow,
    Precision,
    Unknown,
}

/// What a trap raised by ring-3 code does to the process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ring3Action {
    Signal {
        sig: u32,
        si_code: i32,
    },
    /// Not a ring-3 fault: the Ring 0 column of DESIGN §5.2 applies.
    NotRing3,
}

/// `si_code` values, as Linux defines them in
/// `include/uapi/asm-generic/siginfo.h`.
pub mod si_code {
    pub const SI_KERNEL: i32 = 0x80;
    pub const FPE_INTDIV: i32 = 1;
    pub const FPE_FLTDIV: i32 = 3;
    pub const FPE_FLTOVF: i32 = 4;
    pub const FPE_FLTUND: i32 = 5;
    pub const FPE_FLTRES: i32 = 6;
    pub const FPE_FLTINV: i32 = 7;
    pub const ILL_ILLOPN: i32 = 2;
    pub const SEGV_MAPERR: i32 = 1;
    pub const SEGV_ACCERR: i32 = 2;
    pub const BUS_ADRALN: i32 = 1;
    pub const TRAP_BRKPT: i32 = 1;
    pub const TRAP_TRACE: i32 = 2;
    pub const TRAP_HWBKPT: i32 = 4;
}

const fn sig(sig: u32, si_code: i32) -> Ring3Action {
    Ring3Action::Signal { sig, si_code }
}

/// The ring-3 action for each trap kind: DESIGN §5.2's Ring 3 column, with the
/// `si_code` Linux sends.
pub const fn ring3_action(kind: TrapKind) -> Ring3Action {
    use si_code::*;
    match kind {
        TrapKind::DivideError => sig(SIGFPE, FPE_INTDIV),
        TrapKind::Debug(DebugCause::SingleStep) => sig(SIGTRAP, TRAP_TRACE),
        TrapKind::Debug(DebugCause::HwBreakpoint) => sig(SIGTRAP, TRAP_HWBKPT),
        TrapKind::Debug(DebugCause::Other) => sig(SIGTRAP, TRAP_BRKPT),
        TrapKind::Nmi | TrapKind::DoubleFault | TrapKind::MachineCheck | TrapKind::Interrupt(_) => {
            Ring3Action::NotRing3
        }
        TrapKind::Breakpoint => sig(SIGTRAP, SI_KERNEL),
        TrapKind::Overflow
        | TrapKind::BoundRange
        | TrapKind::DeviceNotAvailable
        | TrapKind::InvalidTss
        | TrapKind::GeneralProtection
        | TrapKind::Reserved(_) => sig(SIGSEGV, SI_KERNEL),
        TrapKind::InvalidOpcode => sig(SIGILL, ILL_ILLOPN),
        TrapKind::SegmentNotPresent | TrapKind::StackFault => sig(SIGBUS, SI_KERNEL),
        TrapKind::PageFault(c) => {
            if c.protection {
                sig(SIGSEGV, SEGV_ACCERR)
            } else {
                sig(SIGSEGV, SEGV_MAPERR)
            }
        }
        TrapKind::FloatingPoint(_, cause) => sig(
            SIGFPE,
            match cause {
                FpCause::Invalid => FPE_FLTINV,
                FpCause::DivideByZero => FPE_FLTDIV,
                FpCause::Overflow => FPE_FLTOVF,
                FpCause::Underflow => FPE_FLTUND,
                FpCause::Precision => FPE_FLTRES,
                FpCause::Unknown => SI_KERNEL,
            },
        ),
        TrapKind::AlignmentCheck => sig(SIGBUS, BUS_ADRALN),
    }
}

/// The x86_64 decode: an IDT vector and its error code to a `TrapKind`.
pub mod x86_64 {
    use super::{DebugCause, FpCause, FpUnit, PageFaultCause, TrapKind};

    const PF_PRESENT: u64 = 1 << 0;
    const PF_WRITE: u64 = 1 << 1;
    const PF_FETCH: u64 = 1 << 4;

    /// Exception flag bits, at bit 0 in the FSW and FCW, and in MXCSR's low
    /// six bits (masks at MXCSR bits 7-12).
    const FP_IE: u32 = 1 << 0;
    const FP_DE: u32 = 1 << 1;
    const FP_ZE: u32 = 1 << 2;
    const FP_OE: u32 = 1 << 3;
    const FP_UE: u32 = 1 << 4;
    const FP_PE: u32 = 1 << 5;
    const FP_ALL: u32 = 0x3F;

    pub const fn decode(vector: u8, error_code: u64) -> TrapKind {
        match vector {
            0 => TrapKind::DivideError,
            1 => TrapKind::Debug(DebugCause::Other),
            2 => TrapKind::Nmi,
            3 => TrapKind::Breakpoint,
            4 => TrapKind::Overflow,
            5 => TrapKind::BoundRange,
            6 => TrapKind::InvalidOpcode,
            7 => TrapKind::DeviceNotAvailable,
            8 => TrapKind::DoubleFault,
            10 => TrapKind::InvalidTss,
            11 => TrapKind::SegmentNotPresent,
            12 => TrapKind::StackFault,
            13 => TrapKind::GeneralProtection,
            14 => TrapKind::PageFault(PageFaultCause {
                protection: error_code & PF_PRESENT != 0,
                write: error_code & PF_WRITE != 0,
                fetch: error_code & PF_FETCH != 0,
            }),
            16 => TrapKind::FloatingPoint(FpUnit::X87, FpCause::Unknown),
            17 => TrapKind::AlignmentCheck,
            18 => TrapKind::MachineCheck,
            19 => TrapKind::FloatingPoint(FpUnit::Simd, FpCause::Unknown),
            9 | 15 | 20..=31 => TrapKind::Reserved(vector),
            _ => TrapKind::Interrupt(vector),
        }
    }

    /// Six exception bits (IE, DE, ZE, OE, UE, PE) at bit 0 in both arguments:
    /// FSW and FCW, or MXCSR & 0x3F and (MXCSR >> 7) & 0x3F.
    ///
    /// Reports the first unmasked flag in the order invalid, divide-by-zero,
    /// overflow, underflow or denormal, precision.
    pub const fn fp_cause(status: u32, masks: u32) -> FpCause {
        let live = status & !masks & FP_ALL;
        if live & FP_IE != 0 {
            FpCause::Invalid
        } else if live & FP_ZE != 0 {
            FpCause::DivideByZero
        } else if live & FP_OE != 0 {
            FpCause::Overflow
        } else if live & (FP_UE | FP_DE) != 0 {
            FpCause::Underflow
        } else if live & FP_PE != 0 {
            FpCause::Precision
        } else {
            FpCause::Unknown
        }
    }
}

#[cfg(test)]
mod tests {
    use super::si_code::*;
    use super::x86_64::{decode, fp_cause};
    use super::*;

    const fn s(sig: u32, si_code: i32) -> Ring3Action {
        Ring3Action::Signal { sig, si_code }
    }

    const NOT: Ring3Action = Ring3Action::NotRing3;

    /// DESIGN §5.2's Ring 3 column for vectors 0x00-0x1F with error code 0,
    /// and the si_code table.
    const EXPECTED: [Ring3Action; 32] = [
        s(SIGFPE, FPE_INTDIV),   // 0x00 #DE
        s(SIGTRAP, TRAP_BRKPT),  // 0x01 #DB, unrefined
        NOT,                     // 0x02 NMI
        s(SIGTRAP, SI_KERNEL),   // 0x03 #BP
        s(SIGSEGV, SI_KERNEL),   // 0x04 #OF
        s(SIGSEGV, SI_KERNEL),   // 0x05 #BR
        s(SIGILL, ILL_ILLOPN),   // 0x06 #UD
        s(SIGSEGV, SI_KERNEL),   // 0x07 #NM
        NOT,                     // 0x08 #DF
        s(SIGSEGV, SI_KERNEL),   // 0x09 reserved
        s(SIGSEGV, SI_KERNEL),   // 0x0A #TS
        s(SIGBUS, SI_KERNEL),    // 0x0B #NP
        s(SIGBUS, SI_KERNEL),    // 0x0C #SS
        s(SIGSEGV, SI_KERNEL),   // 0x0D #GP
        s(SIGSEGV, SEGV_MAPERR), // 0x0E #PF, not present
        s(SIGSEGV, SI_KERNEL),   // 0x0F reserved
        s(SIGFPE, SI_KERNEL),    // 0x10 #MF, unrefined
        s(SIGBUS, BUS_ADRALN),   // 0x11 #AC
        NOT,                     // 0x12 #MC
        s(SIGFPE, SI_KERNEL),    // 0x13 #XF, unrefined
        s(SIGSEGV, SI_KERNEL),   // 0x14 #VE
        s(SIGSEGV, SI_KERNEL),   // 0x15 #CP
        s(SIGSEGV, SI_KERNEL),   // 0x16
        s(SIGSEGV, SI_KERNEL),   // 0x17
        s(SIGSEGV, SI_KERNEL),   // 0x18
        s(SIGSEGV, SI_KERNEL),   // 0x19
        s(SIGSEGV, SI_KERNEL),   // 0x1A
        s(SIGSEGV, SI_KERNEL),   // 0x1B
        s(SIGSEGV, SI_KERNEL),   // 0x1C #HV
        s(SIGSEGV, SI_KERNEL),   // 0x1D #VC
        s(SIGSEGV, SI_KERNEL),   // 0x1E #SX
        s(SIGSEGV, SI_KERNEL),   // 0x1F
    ];

    #[test]
    fn ring3_action_covers_vectors_0_to_31() {
        for v in 0u8..=31 {
            let got = ring3_action(decode(v, 0));
            assert_eq!(got, EXPECTED[usize::from(v)], "vector {v:#04x}");
        }
        for (e, want) in [
            (0x1, s(SIGSEGV, SEGV_ACCERR)),
            (0x2, s(SIGSEGV, SEGV_MAPERR)),
            (0x10, s(SIGSEGV, SEGV_MAPERR)),
        ] {
            assert_eq!(
                ring3_action(decode(14, e)),
                want,
                "vector 0x0e error {e:#x}"
            );
        }
    }

    #[test]
    fn interrupt_vectors_are_not_ring3_faults() {
        for v in 32u8..=255 {
            assert_eq!(decode(v, 0), TrapKind::Interrupt(v), "vector {v:#04x}");
            assert_eq!(ring3_action(decode(v, 0)), NOT, "vector {v:#04x}");
        }
    }

    #[test]
    fn page_fault_error_code_sets_si_code() {
        let pf = |e| match decode(14, e) {
            TrapKind::PageFault(c) => c,
            k => panic!("error {e:#x} decoded to {k:?}"),
        };
        assert_eq!(
            pf(0x13),
            PageFaultCause {
                protection: true,
                write: true,
                fetch: true
            }
        );
        assert_eq!(
            pf(0x4),
            PageFaultCause {
                protection: false,
                write: false,
                fetch: false
            }
        );
        let code = |e| match ring3_action(decode(14, e)) {
            Ring3Action::Signal { sig, si_code } => {
                assert_eq!(sig, SIGSEGV, "error {e:#x}");
                si_code
            }
            Ring3Action::NotRing3 => panic!("error {e:#x} is not a ring-3 fault"),
        };
        assert_eq!(code(0x0), SEGV_MAPERR);
        assert_eq!(code(0x2), SEGV_MAPERR);
        assert_eq!(code(0x4), SEGV_MAPERR);
        assert_eq!(code(0x1), SEGV_ACCERR);
        assert_eq!(code(0x7), SEGV_ACCERR);
        assert_eq!(code(0x11), SEGV_ACCERR);
    }

    #[test]
    fn fp_cause_picks_first_unmasked_flag() {
        // x87: FSW flags against FCW masks. FCW 0x037F masks all six.
        assert_eq!(fp_cause(0x04, 0x3F), FpCause::Unknown);
        assert_eq!(fp_cause(0x04, 0x3B), FpCause::DivideByZero);
        assert_eq!(fp_cause(0x3F, 0x00), FpCause::Invalid);
        assert_eq!(fp_cause(0x3E, 0x00), FpCause::DivideByZero);
        assert_eq!(fp_cause(0x3A, 0x00), FpCause::Overflow);
        assert_eq!(fp_cause(0x32, 0x00), FpCause::Underflow);
        assert_eq!(fp_cause(0x10, 0x00), FpCause::Underflow);
        assert_eq!(fp_cause(0x20, 0x00), FpCause::Precision);
        // A masked flag is ignored in favour of a later unmasked one.
        assert_eq!(fp_cause(0x21, 0x01), FpCause::Precision);
        assert_eq!(fp_cause(0x00, 0x00), FpCause::Unknown);
        // Status bits above the six flags (SF, ES, C0-C3, TOP) are ignored.
        assert_eq!(fp_cause(0xFFC0, 0x00), FpCause::Unknown);
        // SIMD: MXCSR 0x1D84 is ZE set with ZM clear.
        let mxcsr: u32 = 0x1D84;
        assert_eq!(
            fp_cause(mxcsr & 0x3F, (mxcsr >> 7) & 0x3F),
            FpCause::DivideByZero
        );
        // The default MXCSR 0x1F80 masks everything.
        let mxcsr: u32 = 0x1F84;
        assert_eq!(
            fp_cause(mxcsr & 0x3F, (mxcsr >> 7) & 0x3F),
            FpCause::Unknown
        );
        assert_eq!(
            ring3_action(TrapKind::FloatingPoint(FpUnit::Simd, FpCause::DivideByZero)),
            s(SIGFPE, FPE_FLTDIV)
        );
        assert_eq!(
            ring3_action(TrapKind::FloatingPoint(FpUnit::X87, FpCause::Overflow)),
            s(SIGFPE, FPE_FLTOVF)
        );
    }
}
