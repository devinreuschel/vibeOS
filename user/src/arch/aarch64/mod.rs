//! aarch64: the entry point and the `svc` instruction (ROADMAP §11.6).
//!
//! The kernel enters `_start` with SP at `argc` (the psABI's initial process
//! stack). A system call takes its number in `x8` and its arguments in
//! `x0`–`x5`, returns in `x0`, and is `svc #0`.

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
        "adrp x0, {fp}",
        "add x0, x0, :lo12:{fp}",
        "mrs x1, fpcr",
        "str w1, [x0, #4]",
        "mrs x1, fpsr",
        "strh w1, [x0]",
        "str q0, [x0, #16]",
        "mov x29, xzr",
        "mov x0, sp",
        "and sp, x0, #0xfffffffffffffff0",
        "bl {start}",
        "brk #0",
        fp = sym fp::ENTRY_FP,
        start = sym crate::rt::start,
    )
}

/// The `e_machine` of this architecture's ELF images (`EM_AARCH64`).
pub const ELF_MACHINE: u16 = 183;

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
    // SAFETY: the kernel's `svc` convention, stated in the module docs
    // here; the caller's contract covers the call's own effects.
    unsafe {
        asm!("svc #0", in("x8") n, lateout("x0") ret, options(nostack));
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
    // SAFETY: the kernel's `svc` convention, stated in the module docs
    // here; the caller's contract covers the call's own effects.
    unsafe {
        asm!("svc #0", in("x8") n, inlateout("x0") a as isize => ret, options(nostack));
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
    // SAFETY: the kernel's `svc` convention, stated in the module docs
    // here; the caller's contract covers the call's own effects.
    unsafe {
        asm!("svc #0", in("x8") n, inlateout("x0") a as isize => ret, in("x1") b,
             options(nostack));
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
    // SAFETY: the kernel's `svc` convention, stated in the module docs
    // here; the caller's contract covers the call's own effects.
    unsafe {
        asm!("svc #0", in("x8") n, inlateout("x0") a as isize => ret, in("x1") b,
             in("x2") c, options(nostack));
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
    // SAFETY: the kernel's `svc` convention, stated in the module docs
    // here; the caller's contract covers the call's own effects.
    unsafe {
        asm!("svc #0", in("x8") n, inlateout("x0") a as isize => ret, in("x1") b,
             in("x2") c, in("x3") d, options(nostack));
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
    // SAFETY: the kernel's `svc` convention, stated in the module docs
    // here; the caller's contract covers the call's own effects.
    unsafe {
        asm!("svc #0", in("x8") n, inlateout("x0") a as isize => ret, in("x1") b,
             in("x2") c, in("x3") d, in("x4") e, options(nostack));
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
    // SAFETY: the kernel's `svc` convention, stated in the module docs
    // here; the caller's contract covers the call's own effects.
    unsafe {
        asm!("svc #0", in("x8") n, inlateout("x0") a as isize => ret, in("x1") b,
             in("x2") c, in("x3") d, in("x4") e, in("x5") f, options(nostack));
    }
    ret
}
