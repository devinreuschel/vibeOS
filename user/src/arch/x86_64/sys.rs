//! x86_64 system call wrappers (C-USERRT). Numbers are from Linux
//! `arch/x86/entry/syscalls/syscall_64.tbl`. ROADMAP §10.5's generated
//! table replaces this hand-written set.

use core::ffi::CStr;

/// The raw calls, for a call with no wrapper yet.
pub use super::{syscall0, syscall1, syscall2, syscall3, syscall4, syscall5, syscall6};
use crate::sys::{Errno, result};

const READ: usize = 0;
const WRITE: usize = 1;
const OPEN: usize = 2;
const CLOSE: usize = 3;
const DUP2: usize = 33;
const FORK: usize = 57;
const EXIT: usize = 60;
const WAIT4: usize = 61;

/// `read(2)` into `buf`.
pub fn read(fd: i32, buf: &mut [u8]) -> Result<usize, Errno> {
    // SAFETY: `buf` is valid for `buf.len()` byte writes, which is all
    // `read` writes, established here by the `&mut` borrow.
    result(unsafe { syscall3(READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) })
}

/// `write(2)` from `buf`.
pub fn write(fd: i32, buf: &[u8]) -> Result<usize, Errno> {
    // SAFETY: `write` only reads `buf.len()` bytes of `buf`, valid by the
    // borrow established here.
    result(unsafe { syscall3(WRITE, fd as usize, buf.as_ptr() as usize, buf.len()) })
}

/// `open(2)`.
pub fn open(path: &CStr, flags: i32, mode: u32) -> Result<usize, Errno> {
    // SAFETY: `open` only reads the NUL-terminated `path`, valid by the
    // borrow established here.
    result(unsafe { syscall3(OPEN, path.as_ptr() as usize, flags as usize, mode as usize) })
}

/// `close(2)`.
pub fn close(fd: i32) -> Result<usize, Errno> {
    // SAFETY: `close` touches no user memory, established here.
    result(unsafe { syscall1(CLOSE, fd as usize) })
}

/// `dup2(2)`.
pub fn dup2(old: i32, new: i32) -> Result<usize, Errno> {
    // SAFETY: `dup2` touches no user memory, established here.
    result(unsafe { syscall2(DUP2, old as usize, new as usize) })
}

/// `fork(2)`: the child's pid in the parent, 0 in the child.
pub fn fork() -> Result<usize, Errno> {
    // SAFETY: `fork` touches no user memory, established here; the child
    // resumes here with a copy of this address space.
    result(unsafe { syscall0(FORK) })
}

/// `wait4(2)` for `pid`, storing the status word in `status`; no rusage.
pub fn wait4(pid: i32, status: &mut u32, options: i32) -> Result<usize, Errno> {
    // SAFETY: `wait4` writes one `u32` through `status`, valid by the
    // `&mut` borrow established here, and nothing through the NULL rusage.
    result(unsafe {
        syscall4(
            WAIT4,
            pid as isize as usize,
            status as *mut u32 as usize,
            options as usize,
            0,
        )
    })
}

/// `exit(2)` with `code`.
pub fn exit(code: i32) -> ! {
    // SAFETY: `exit` touches no user memory and does not return,
    // established here.
    unsafe { syscall1(EXIT, code as usize) };
    loop {
        core::hint::spin_loop();
    }
}
