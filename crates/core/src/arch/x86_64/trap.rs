//! The x86_64 decode: an IDT vector and its error code to a `TrapKind`,
//! and the user frame every entry from ring 3 saves.

use crate::desc::{USER_CS_RPL, USER_DS_RPL};
use crate::paging::USER_MAP_END;
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
