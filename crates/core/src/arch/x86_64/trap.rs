//! The x86_64 decode: an IDT vector and its error code to a `TrapKind`,
//! and the user frame every entry from ring 3 saves.

use super::syscall;
use crate::desc::{USER_CS_RPL, USER_DS_RPL};
use crate::paging::USER_MAP_END;
use crate::proc::syscall_table::{Handlers, NrTable, SysResult};
use crate::trap::{DebugCause, FpCause, FpUnit, PageFaultCause, SyscallAbi, TrapKind};

/// A ring-3 register frame: the 21 words of Linux's x86_64
/// `struct user_regs_struct`, in its order, low address first (layout
/// as `arch/x86/include/asm/user_64.h` defines it). The last five are
/// the hardware `iretq` frame. Every entry from ring 3 saves one at the
/// top of the thread's kernel stack, and every return to ring 3 leaves
/// from it (DESIGN §5.10, §7.5).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UserFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rax: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    /// The syscall number at a syscall entry, -1 at any other entry.
    pub orig_rax: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

/// RFLAGS bits the first return to ring 3 sets: IF and reserved bit 1.
const RFLAGS_USER: u64 = 0x202;
const RFLAGS_TF: u64 = 1 << 8;
const RFLAGS_RF: u64 = 1 << 16;
const RFLAGS_VM: u64 = 1 << 17;

impl UserFrame {
    pub const fn zeroed() -> Self {
        Self {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            rbp: 0,
            rbx: 0,
            r11: 0,
            r10: 0,
            r9: 0,
            r8: 0,
            rax: 0,
            rcx: 0,
            rdx: 0,
            rsi: 0,
            rdi: 0,
            orig_rax: 0,
            rip: 0,
            cs: 0,
            rflags: 0,
            rsp: 0,
            ss: 0,
        }
    }

    /// A new program's first frame: every GPR zero, IF set, the user
    /// selectors, and RCX/R11 equal to RIP/RFLAGS as a `syscall` leaves
    /// them, so the exit takes `sysretq`.
    pub const fn new_user(rip: u64, rsp: u64) -> Self {
        let mut f = Self::zeroed();
        f.rip = rip;
        f.rcx = rip;
        f.rsp = rsp;
        f.rflags = RFLAGS_USER;
        f.r11 = RFLAGS_USER;
        f.cs = USER_CS_RPL as u64;
        f.ss = USER_DS_RPL as u64;
        f.orig_rax = u64::MAX;
        f
    }
}

/// Whether the syscall exit may leave `f` through `sysretq`, as Linux
/// decides: `sysretq` reloads RIP from RCX and RFLAGS from R11, loads
/// fixed selectors, and cannot return with RF, TF or VM set or to a RIP
/// at or above `USER_MAP_END`. Otherwise the exit uses `iretq`.
pub const fn sysret_ok(f: &UserFrame) -> bool {
    f.rip == f.rcx
        && f.rflags == f.r11
        && f.cs == USER_CS_RPL as u64
        && f.ss == USER_DS_RPL as u64
        && f.rip < USER_MAP_END
        && f.rflags & (RFLAGS_RF | RFLAGS_TF | RFLAGS_VM) == 0
}

/// The x86_64 Linux syscall ABI over [`UserFrame`].
pub struct Abi;

impl SyscallAbi for Abi {
    type Frame = UserFrame;

    fn nr(f: &UserFrame) -> u64 {
        f.orig_rax
    }

    fn arg(f: &UserFrame, i: usize) -> u64 {
        match i {
            0 => f.rdi,
            1 => f.rsi,
            2 => f.rdx,
            3 => f.r10,
            4 => f.r8,
            5 => f.r9,
            _ => 0,
        }
    }

    fn set_ret(f: &mut UserFrame, v: u64) {
        f.rax = v;
    }

    fn ip(f: &UserFrame) -> u64 {
        f.rip
    }

    fn set_ip(f: &mut UserFrame, v: u64) {
        f.rip = v;
    }

    fn sp(f: &UserFrame) -> u64 {
        f.rsp
    }

    fn set_sp(f: &mut UserFrame, v: u64) {
        f.rsp = v;
    }

    fn table() -> &'static NrTable {
        &syscall::TABLE
    }

    fn dispatch<H: Handlers + ?Sized>(h: &mut H, raw_nr: u64, regs: &[u64; 6]) -> SysResult {
        syscall::dispatch(h, raw_nr, regs)
    }

    /// `syscall` is two bytes (`0F 05`).
    fn restart(f: &mut UserFrame) {
        f.rax = f.orig_rax;
        f.rip = f.rip.wrapping_sub(2);
    }
}

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

#[cfg(test)]
mod tests {
    use core::mem::{offset_of, size_of};

    use super::*;
    use crate::proc::{SIGBUS, SIGFPE, SIGILL, SIGSEGV, SIGTRAP};
    use crate::trap::si_code::*;
    use crate::trap::{Ring3Action, ring3_action};

    #[test]
    fn user_frame_offsets() {
        let want = [
            offset_of!(UserFrame, r15),
            offset_of!(UserFrame, r14),
            offset_of!(UserFrame, r13),
            offset_of!(UserFrame, r12),
            offset_of!(UserFrame, rbp),
            offset_of!(UserFrame, rbx),
            offset_of!(UserFrame, r11),
            offset_of!(UserFrame, r10),
            offset_of!(UserFrame, r9),
            offset_of!(UserFrame, r8),
            offset_of!(UserFrame, rax),
            offset_of!(UserFrame, rcx),
            offset_of!(UserFrame, rdx),
            offset_of!(UserFrame, rsi),
            offset_of!(UserFrame, rdi),
            offset_of!(UserFrame, orig_rax),
            offset_of!(UserFrame, rip),
            offset_of!(UserFrame, cs),
            offset_of!(UserFrame, rflags),
            offset_of!(UserFrame, rsp),
            offset_of!(UserFrame, ss),
        ];
        for (i, off) in want.iter().enumerate() {
            assert_eq!(*off, i * 8, "word {i}");
        }
        assert_eq!(size_of::<UserFrame>(), 168);
    }

    fn sample() -> UserFrame {
        UserFrame {
            rdi: 10,
            rsi: 11,
            rdx: 12,
            r10: 13,
            r8: 14,
            r9: 15,
            rcx: 16,
            r11: 17,
            rax: 18,
            orig_rax: 39,
            rip: 0x40_1002,
            rsp: 0x7000,
            ..UserFrame::zeroed()
        }
    }

    #[test]
    fn abi_linux_syscall_registers() {
        let mut f = sample();
        assert_eq!(Abi::nr(&f), 39);
        let args: [u64; 7] = core::array::from_fn(|i| Abi::arg(&f, i));
        assert_eq!(args, [10, 11, 12, 13, 14, 15, 0]);
        assert_eq!(Abi::arg(&f, usize::MAX), 0);
        Abi::set_ret(&mut f, (-38i64) as u64);
        assert_eq!(f.rax, (-38i64) as u64);
        assert_eq!(Abi::ip(&f), 0x40_1002);
        Abi::set_ip(&mut f, 0x40_2000);
        assert_eq!(f.rip, 0x40_2000);
        assert_eq!(Abi::sp(&f), 0x7000);
        Abi::set_sp(&mut f, 0x8000);
        assert_eq!(f.rsp, 0x8000);
        // Nothing else moved.
        assert_eq!((f.rcx, f.r11, f.orig_rax), (16, 17, 39));
    }

    #[test]
    fn abi_restart_rewinds() {
        let mut f = sample();
        Abi::restart(&mut f);
        assert_eq!(f.rax, 39);
        assert_eq!(f.rip, 0x40_1000);
        assert_eq!(f.orig_rax, 39);
    }

    #[test]
    fn new_user_frame_takes_sysret() {
        let f = UserFrame::new_user(0x40_0000, 0x7fff_f000);
        assert!(sysret_ok(&f));
        assert_eq!((f.rip, f.rcx, f.rsp), (0x40_0000, 0x40_0000, 0x7fff_f000));
        assert_eq!((f.rflags, f.r11), (0x202, 0x202));
        assert_eq!(f.cs, u64::from(USER_CS_RPL));
        assert_eq!(f.ss, u64::from(USER_DS_RPL));
        assert_eq!(f.orig_rax, u64::MAX);
        assert_eq!((f.rax, f.rdi, f.r15), (0, 0, 0));
    }

    #[test]
    fn sysret_ok_rule() {
        let base = UserFrame::new_user(0x40_0000, 0x7fff_f000);
        assert!(sysret_ok(&base));
        let bad: [fn(&mut UserFrame); 8] = [
            |f| f.rcx = f.rip + 1,
            |f| f.r11 = f.rflags | 1,
            |f| f.cs = 0x08,
            |f| f.ss = 0x10,
            |f| {
                f.rip = USER_MAP_END;
                f.rcx = USER_MAP_END;
            },
            |f| {
                f.rflags |= 1 << 16;
                f.r11 = f.rflags;
            },
            |f| {
                f.rflags |= 1 << 8;
                f.r11 = f.rflags;
            },
            |f| {
                f.rflags |= 1 << 17;
                f.r11 = f.rflags;
            },
        ];
        for (i, change) in bad.iter().enumerate() {
            let mut f = base;
            change(&mut f);
            assert!(!sysret_ok(&f), "case {i}");
        }
        let mut f = base;
        f.rip = USER_MAP_END - 1;
        f.rcx = f.rip;
        assert!(sysret_ok(&f));
    }

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
