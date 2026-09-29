//! System calls (ROADMAP §10.5, C-USERRT).
//!
//! `sys::<name>(…)` returns `Ok` with the call's non-negative result, or the
//! `Errno` Linux returns as `-errno`. The calls themselves live in the `arch`
//! module; the portable types are here.

pub use crate::arch::sys::*;

/// A Linux error number, as a failed system call returns it negated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Errno(pub i32);

/// Turn a raw system call return into a result: Linux returns `-errno` in
/// `[-4095, -1]`.
pub fn result(ret: isize) -> Result<usize, Errno> {
    if (-4095..0).contains(&ret) {
        Err(Errno(-(ret as i32)))
    } else {
        Ok(ret as usize)
    }
}

// Open flags, from Linux `include/uapi/asm-generic/fcntl.h`.
/// Open for reading only.
pub const O_RDONLY: i32 = 0o0;
/// Open for writing only.
pub const O_WRONLY: i32 = 0o1;
/// Create the file if it does not exist.
pub const O_CREAT: i32 = 0o100;
/// Truncate the file to length 0.
pub const O_TRUNC: i32 = 0o1000;

/// The exit code in a `wait4` status word, or `None` when the process did
/// not exit normally (`WIFEXITED` and `WEXITSTATUS`).
pub fn exit_code(status: u32) -> Option<u8> {
    if status & 0x7f == 0 {
        Some(((status >> 8) & 0xff) as u8)
    } else {
        None
    }
}
