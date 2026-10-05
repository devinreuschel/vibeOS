//! x86_64: the entry point and the `syscall` instruction (ROADMAP §10.5).
//!
//! The kernel enters `_start` with RSP at `argc` (the psABI's initial process
//! stack). A system call takes its number in RAX and its arguments in RDI,
//! RSI, RDX, R10, R8 and R9, returns in RAX, and clobbers RCX and R11.

pub mod env;
pub mod fp;
pub mod stat;
pub mod sys;

pub use env::user_env;
pub use fp::{FpState, fp_syscall, initial_fp, is_initial};

use core::arch::{asm, naked_asm};

/// The entry point: capture the FP state the program starts with
/// ([`fp::initial_fp`]) before any other instruction touches it, clear the
/// frame pointer, hand the initial stack pointer to the portable start, and
/// align the stack for its call.
///
/// # Safety
///
/// Only the kernel enters it, once, with the initial process stack.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _start() -> ! {
    naked_asm!(
        "lea rax, [rip + {fp}]",
        "fnstcw word ptr [rax]",
        "stmxcsr dword ptr [rax + 4]",
        "movdqa xmmword ptr [rax + 16], xmm0",
        "xor ebp, ebp",
        "mov rdi, rsp",
        "and rsp, -16",
        "call {start}",
        "ud2",
        fp = sym fp::ENTRY_FP,
        start = sym crate::rt::start,
    )
}

/// The `e_machine` of this architecture's ELF images (`EM_X86_64`, from
/// the System V gABI's machine list).
pub const ELF_MACHINE: u16 = 62;

/// The address of [`_start`], the entry point.
pub fn entry_address() -> usize {
    _start as *const () as usize
}

/// System call `n` with no arguments.
///
/// # Safety
///
/// The call's effect on memory is the caller's to allow: a call that writes
/// through an argument needs it valid for that write.
#[inline(always)]
pub unsafe fn syscall0(n: usize) -> isize {
    let ret: isize;
    // SAFETY: the kernel's `syscall` convention, stated in the module docs
    // here; the caller's contract covers the call's own effects.
    unsafe {
        asm!("syscall", inlateout("rax") n as isize => ret,
             out("rcx") _, out("r11") _, options(nostack));
    }
    ret
}

/// System call `n` with one argument.
///
/// # Safety
///
/// As [`syscall0`].
#[inline(always)]
pub unsafe fn syscall1(n: usize, a: usize) -> isize {
    let ret: isize;
    // SAFETY: the kernel's `syscall` convention, stated in the module docs
    // here; the caller's contract covers the call's own effects.
    unsafe {
        asm!("syscall", inlateout("rax") n as isize => ret, in("rdi") a,
             out("rcx") _, out("r11") _, options(nostack));
    }
    ret
}

/// System call `n` with two arguments.
///
/// # Safety
///
/// As [`syscall0`].
#[inline(always)]
pub unsafe fn syscall2(n: usize, a: usize, b: usize) -> isize {
    let ret: isize;
    // SAFETY: the kernel's `syscall` convention, stated in the module docs
    // here; the caller's contract covers the call's own effects.
    unsafe {
        asm!("syscall", inlateout("rax") n as isize => ret, in("rdi") a, in("rsi") b,
             out("rcx") _, out("r11") _, options(nostack));
    }
    ret
}

/// System call `n` with three arguments.
///
/// # Safety
///
/// As [`syscall0`].
#[inline(always)]
pub unsafe fn syscall3(n: usize, a: usize, b: usize, c: usize) -> isize {
    let ret: isize;
    // SAFETY: the kernel's `syscall` convention, stated in the module docs
    // here; the caller's contract covers the call's own effects.
    unsafe {
        asm!("syscall", inlateout("rax") n as isize => ret, in("rdi") a, in("rsi") b,
             in("rdx") c, out("rcx") _, out("r11") _, options(nostack));
    }
    ret
}

/// System call `n` with four arguments.
///
/// # Safety
///
/// As [`syscall0`].
#[inline(always)]
pub unsafe fn syscall4(n: usize, a: usize, b: usize, c: usize, d: usize) -> isize {
    let ret: isize;
    // SAFETY: the kernel's `syscall` convention, stated in the module docs
    // here; the caller's contract covers the call's own effects.
    unsafe {
        asm!("syscall", inlateout("rax") n as isize => ret, in("rdi") a, in("rsi") b,
             in("rdx") c, in("r10") d, out("rcx") _, out("r11") _, options(nostack));
    }
    ret
}

/// System call `n` with five arguments.
///
/// # Safety
///
/// As [`syscall0`].
#[inline(always)]
pub unsafe fn syscall5(n: usize, a: usize, b: usize, c: usize, d: usize, e: usize) -> isize {
    let ret: isize;
    // SAFETY: the kernel's `syscall` convention, stated in the module docs
    // here; the caller's contract covers the call's own effects.
    unsafe {
        asm!("syscall", inlateout("rax") n as isize => ret, in("rdi") a, in("rsi") b,
             in("rdx") c, in("r10") d, in("r8") e, out("rcx") _, out("r11") _,
             options(nostack));
    }
    ret
}

/// System call `n` with six arguments.
///
/// # Safety
///
/// As [`syscall0`].
#[inline(always)]
pub unsafe fn syscall6(
    n: usize,
    a: usize,
    b: usize,
    c: usize,
    d: usize,
    e: usize,
    f: usize,
) -> isize {
    let ret: isize;
    // SAFETY: the kernel's `syscall` convention, stated in the module docs
    // here; the caller's contract covers the call's own effects.
    unsafe {
        asm!("syscall", inlateout("rax") n as isize => ret, in("rdi") a, in("rsi") b,
             in("rdx") c, in("r10") d, in("r8") e, in("r9") f, out("rcx") _, out("r11") _,
             options(nostack));
    }
    ret
}
