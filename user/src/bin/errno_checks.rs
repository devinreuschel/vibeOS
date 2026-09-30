//! The in-guest test `syscall_errno_checks`'s program (ROADMAP §10.4, E2).
//! Only `kernel_tests` kernels embed it.
//!
//! Each case checks one of the syscall layer's own errnos against Linux's
//! and, when it fails, exits with its number; the program exits 0 when
//! every case passes:
//!
//! 1. `read` on an `O_WRONLY` descriptor returns `EBADF`.
//! 2. `write` on an `O_RDONLY` descriptor returns `EBADF`.
//! 3. `dup` with a full descriptor table returns `EMFILE`.
//! 4. `open` passes a path's bytes to the filesystem: a name that is not
//!    UTF-8 is created, written, and read back, and a missing one is
//!    `ENOENT`.
//! 5. `execve` accepts an argument that is not UTF-8: a forked child runs
//!    `/hello` (which exits 42) with one, and exits 100 + errno if
//!    `execve` returns.
//! 6. `lseek` on the console returns `ESPIPE`.

#![no_std]
#![no_main]

use vibeos_user::env::Env;
use vibeos_user::sys::{self, Errno};

vibeos_user::main!(main);

/// The file cases 1 and 2 open.
const PLAIN: &core::ffi::CStr = c"/tmp/errno_w";
/// A name that is not UTF-8 (case 4).
const RAW: &core::ffi::CStr = c"/tmp/\xff\xfe";
/// A missing name that is not UTF-8 (case 4).
const RAW_MISSING: &core::ffi::CStr = c"/tmp/\xffx";
/// The program case 5 runs, and its exit status.
const HELLO: &core::ffi::CStr = c"/hello";
const HELLO_EXIT: u8 = 42;
/// Linux `SEEK_CUR`, from `include/uapi/linux/fs.h`.
const SEEK_CUR: u32 = 1;

fn main(_env: &Env) -> i32 {
    let cases: [fn() -> bool; 6] = [
        read_wronly_ebadf,
        write_rdonly_ebadf,
        dup_full_emfile,
        open_raw_bytes,
        execve_raw_arg,
        lseek_console_espipe,
    ];
    for (i, case) in cases.iter().enumerate() {
        if !case() {
            return i as i32 + 1;
        }
    }
    0
}

/// Case 1.
fn read_wronly_ebadf() -> bool {
    let Ok(fd) = sys::open(PLAIN.as_ptr().cast(), sys::O_CREAT | sys::O_WRONLY, 0o644) else {
        return false;
    };
    let mut b = [0u8; 1];
    // SAFETY: `read` writes at most one byte into `b`, a local no other
    // reference covers; established here.
    let r = unsafe { sys::read(fd as u32, b.as_mut_ptr(), 1) };
    close(fd) && r == Err(Errno::EBADF)
}

/// Case 2.
fn write_rdonly_ebadf() -> bool {
    let Ok(fd) = sys::open(PLAIN.as_ptr().cast(), sys::O_RDONLY, 0) else {
        return false;
    };
    let r = sys::write(fd as u32, b"x".as_ptr(), 1);
    close(fd) && r == Err(Errno::EBADF)
}

/// Descriptors a process holds: the kernel's `limits::MAX_FDS` (SYSCALL.md
/// §4), so case 3's copies all fit before `dup` fails.
const FD_SLOTS: usize = 256;

/// Case 3: `dup(0)` until it fails, then close every copy.
fn dup_full_emfile() -> bool {
    let mut fds = [0usize; FD_SLOTS];
    let mut n = 0usize;
    let r = loop {
        match sys::dup(0) {
            Ok(fd) if n < fds.len() => {
                fds[n] = fd;
                n += 1;
            }
            Ok(fd) => {
                close(fd);
                break Ok(fd);
            }
            Err(e) => break Err(e),
        }
    };
    let mut ok = true;
    for &fd in &fds[..n] {
        ok &= close(fd);
    }
    ok && n > 0 && r == Err(Errno::EMFILE)
}

/// Case 4.
fn open_raw_bytes() -> bool {
    let Ok(fd) = sys::open(RAW.as_ptr().cast(), sys::O_CREAT | sys::O_RDWR, 0o644) else {
        return false;
    };
    let wrote = sys::write(fd as u32, b"z".as_ptr(), 1) == Ok(1);
    if !close(fd) || !wrote {
        return false;
    }
    let Ok(fd) = sys::open(RAW.as_ptr().cast(), sys::O_RDONLY, 0) else {
        return false;
    };
    let mut b = [0u8; 2];
    // SAFETY: `read` writes at most two bytes into `b`, a local no other
    // reference covers; established here.
    let r = unsafe { sys::read(fd as u32, b.as_mut_ptr(), b.len()) };
    if !close(fd) || r != Ok(1) || b[0] != b'z' {
        return false;
    }
    sys::open(RAW_MISSING.as_ptr().cast(), sys::O_RDONLY, 0) == Err(Errno::ENOENT)
}

/// Case 5.
fn execve_raw_arg() -> bool {
    let pid = match sys::fork() {
        Ok(0) => {
            let argv: [*const u8; 3] = [
                c"hello".as_ptr().cast(),
                c"\xff\xfe".as_ptr().cast(),
                core::ptr::null(),
            ];
            let envp: [*const u8; 1] = [core::ptr::null()];
            let e = match sys::execve(HELLO.as_ptr().cast(), argv.as_ptr(), envp.as_ptr()) {
                Ok(_) => 0,
                Err(Errno(e)) => e,
            };
            vibeos_user::rt::exit(100 + e);
        }
        Ok(pid) => pid,
        Err(_) => return false,
    };
    let mut status = 0i32;
    // SAFETY: `wait4` writes 4 bytes through `&raw mut status`, a local no
    // reference covers, and nothing through the null rusage; established here.
    let r = unsafe { sys::wait4(pid as i32, &raw mut status, 0, core::ptr::null_mut()) };
    r == Ok(pid) && sys::exit_code(status as u32) == Some(HELLO_EXIT)
}

/// Case 6: fd 1 is the console.
fn lseek_console_espipe() -> bool {
    sys::lseek(1, 0, SEEK_CUR) == Err(Errno::ESPIPE)
}

/// Close `fd`; whether it closed.
fn close(fd: usize) -> bool {
    sys::close(fd as u32).is_ok()
}
