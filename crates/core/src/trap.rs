//! Portable trap kinds and their ring-3 actions: each port decodes a trap into
//! a `TrapKind`, and one table gives each kind its signal and `si_code`, or
//! marks it as not a ring-3 fault (AGENTS.md rule 3, DESIGN §5.2 and §11.5).
//!
//! `decode` reports what the CPU said. It leaves `FpCause` and `DebugCause`
//! unrefined (`Unknown`, `Other`); the fault path refines them from the saved
//! FSW/MXCSR and DR6 before it asks `ring3_action` (ROADMAP §10.6, F005).

use crate::proc::syscall_table::{Handlers, NrTable, SysResult};
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
    /// x86_64 vectors 32-255: device IRQs and IPIs. aarch64 IRQ slot.
    Interrupt(u8),
    /// `svc` (ESR EC `0x15`). The syscall path, not a signal.
    Syscall,
    /// Trapped `wfi`/`wfe` (EC `0x01`). EL0 steps over; no signal.
    WaitTrap,
    /// SError slot. Not a ring-3 fault (DESIGN §11.5).
    SError,
    /// FIQ slot. Not a ring-3 fault: nothing routes a FIQ to the kernel.
    Fiq,
    /// Unknown, reserved, or a class the kernel leaves off (SIGILL, ILL_ILLOPC).
    Undef,
    /// Pointer-authentication failure (EC `0x1C`).
    PacFail,
    /// `brk` (EC `0x3C`).
    SoftwareBreak,
    /// Synchronous external abort (SIGBUS, BUS_OBJERR).
    BusError,
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
    /// `svc`: the syscall path, not a signal.
    Syscall,
    /// Trapped `wfi`/`wfe`: the handler steps over the instruction.
    StepOver,
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
    pub const ILL_ILLOPC: i32 = 1;
    pub const ILL_ILLOPN: i32 = 2;
    pub const BUS_OBJERR: i32 = 3;
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
        TrapKind::Nmi
        | TrapKind::DoubleFault
        | TrapKind::MachineCheck
        | TrapKind::Interrupt(_)
        | TrapKind::SError
        | TrapKind::Fiq => Ring3Action::NotRing3,
        TrapKind::Syscall => Ring3Action::Syscall,
        TrapKind::WaitTrap => Ring3Action::StepOver,
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
        TrapKind::Undef => sig(SIGILL, ILL_ILLOPC),
        TrapKind::PacFail => sig(SIGILL, ILL_ILLOPN),
        TrapKind::SoftwareBreak => sig(SIGTRAP, TRAP_BRKPT),
        TrapKind::BusError => sig(SIGBUS, BUS_OBJERR),
    }
}

/// How portable code reads and changes a syscall's saved user context: the
/// number, the arguments, the return value, the instruction and stack
/// pointers, and the rewind that restarts the call (ROADMAP §10.3, §10.6).
pub trait SyscallAbi {
    type Frame;
    fn nr(f: &Self::Frame) -> u64;
    /// Argument `i` (0 to 5); 0 for any other `i`.
    fn arg(f: &Self::Frame, i: usize) -> u64;
    fn set_ret(f: &mut Self::Frame, v: u64);
    fn ip(f: &Self::Frame) -> u64;
    fn set_ip(f: &mut Self::Frame, v: u64);
    fn sp(f: &Self::Frame) -> u64;
    fn set_sp(f: &mut Self::Frame, v: u64);
    /// Rewind the frame so the return to user mode runs the syscall again.
    fn restart(f: &mut Self::Frame);
    /// The port's syscall numbers (ROADMAP §10.5).
    fn table() -> &'static NrTable;
    /// Look the number register's `raw_nr` up as the port's ABI reads it and
    /// call its handler with `regs`, each cut to its argument's C type; a
    /// number that names no call is `ENOSYS`.
    fn dispatch<H: Handlers + ?Sized>(h: &mut H, raw_nr: u64, regs: &[u64; 6]) -> SysResult;
}
