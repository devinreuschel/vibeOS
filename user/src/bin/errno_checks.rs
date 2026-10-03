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
//! 7. `open` of a symbolic link with `O_NOFOLLOW` returns `ELOOP`, and
//!    `ENOTDIR` with `O_DIRECTORY` too; `O_CREAT` of a directory returns
//!    `EISDIR`; `O_CREAT | O_DIRECTORY` returns `EINVAL` and creates
//!    nothing; the empty path returns `ENOENT`.
//! 8. `execve` of a directory and of a device returns `EACCES`.
//! 9. `read` and `write` check the descriptor's access mode before the
//!    buffer and the count: a 0-byte `read` of an `O_WRONLY` descriptor,
//!    and a `write` of an `O_RDONLY` one with a kernel-half buffer or a
//!    count of 0, return `EBADF`.
//! 10. `read` of a directory returns `EISDIR` for a count of 0, and
//!     `EFAULT` for a kernel-half buffer: the buffer, then the directory.
//! 11. `lseek` on the console with an unknown `whence` returns `EINVAL`:
//!     `whence` before the descriptor's kind.
//! 12. `execve` opens its file before it reads `argv`: with a kernel-half
//!     `argv`, a missing path returns `ENOENT` and a directory `EACCES`.
//! 13. `getdents64` of `/dev/console` opened by path returns `ENOTDIR`, as
//!     any descriptor that is not a directory does.
//! 14. `kill` of a zombie, a child that exited and is not yet reaped,
//!     returns 0 for `SIGKILL`, `SIGSTOP`, and `SIGCONT`, as on Linux, not
//!     `ESRCH`, and delivers nothing: `psinfo` still says `zombie`, and
//!     `wait4` reaps the child with the exit status it left.

#![no_std]
#![no_main]

use vibeos_user::env::Env;
use vibeos_user::sys::{self, Errno};
use vibeos_user::utest;

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
/// A symbolic link (case 7).
const LINK: &core::ffi::CStr = c"/proc/self";
/// A name case 7 must not create.
const DIR_NEW: &core::ffi::CStr = c"/tmp/errno_dir";
/// A kernel-half address: never a user buffer (cases 9 and 10).
const KERNEL_PTR: usize = 0xFFFF_8000_0000_1000;
/// A `whence` past Linux's last, `SEEK_HOLE` (case 11).
const WHENCE_BAD: u32 = 99;
/// The status case 14's child exits with.
const ZOMBIE_EXIT: i32 = 7;
// From signal(7) (case 14).
const SIGKILL: i32 = 9;
const SIGSTOP: i32 = 19;
/// Yields case 14 waits, at most, for its child to become a zombie.
const ZOMBIE_TRIES: u32 = 20_000;

fn main(_env: &Env) -> i32 {
    let cases: [fn() -> bool; 14] = [
        read_wronly_ebadf,
        write_rdonly_ebadf,
        dup_full_emfile,
        open_raw_bytes,
        execve_raw_arg,
        lseek_console_espipe,
        open_links_and_dirs,
        execve_not_regular_eacces,
        access_before_buffer_and_count,
        read_dir_eisdir_after_buffer,
        lseek_whence_before_espipe,
        execve_path_before_argv,
        getdents_device_enotdir,
        kill_zombie_returns_0,
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

/// `open(path, flags)`'s error; `None` when it opened (and is closed).
fn open_err(path: &core::ffi::CStr, flags: i32) -> Option<Errno> {
    match sys::open(path.as_ptr().cast(), flags, 0o644) {
        Ok(fd) => {
            close(fd);
            None
        }
        Err(e) => Some(e),
    }
}

/// Case 7.
fn open_links_and_dirs() -> bool {
    open_err(LINK, sys::O_RDONLY | sys::O_NOFOLLOW) == Some(Errno::ELOOP)
        && open_err(LINK, sys::O_RDONLY | sys::O_NOFOLLOW | sys::O_DIRECTORY)
            == Some(Errno::ENOTDIR)
        && open_err(LINK, sys::O_RDONLY | sys::O_DIRECTORY).is_none()
        && open_err(c"/tmp", sys::O_RDONLY | sys::O_CREAT) == Some(Errno::EISDIR)
        && open_err(DIR_NEW, sys::O_RDONLY | sys::O_CREAT | sys::O_DIRECTORY) == Some(Errno::EINVAL)
        && open_err(DIR_NEW, sys::O_RDONLY) == Some(Errno::ENOENT)
        && open_err(c"", sys::O_RDONLY) == Some(Errno::ENOENT)
}

/// Case 8: each `execve` returns, since neither file can run.
fn execve_not_regular_eacces() -> bool {
    [c"/tmp", c"/dev/null"].iter().all(|p| {
        let argv: [*const u8; 2] = [p.as_ptr().cast(), core::ptr::null()];
        sys::execve(p.as_ptr().cast(), argv.as_ptr(), core::ptr::null()) == Err(Errno::EACCES)
    })
}

/// Case 9.
fn access_before_buffer_and_count() -> bool {
    let mut b = [0u8; 1];
    let Ok(w) = sys::open(PLAIN.as_ptr().cast(), sys::O_CREAT | sys::O_WRONLY, 0o644) else {
        return false;
    };
    // SAFETY: a 0-byte `read` writes nothing into `b`, a local no other
    // reference covers; established here.
    let r = unsafe { sys::read(w as u32, b.as_mut_ptr(), 0) };
    if !close(w) || r != Err(Errno::EBADF) {
        return false;
    }
    let Ok(fd) = sys::open(PLAIN.as_ptr().cast(), sys::O_RDONLY, 0) else {
        return false;
    };
    let ok = sys::write(fd as u32, KERNEL_PTR as *const u8, 1) == Err(Errno::EBADF)
        && sys::write(fd as u32, b"x".as_ptr(), 0) == Err(Errno::EBADF);
    close(fd) && ok
}

/// Case 10.
fn read_dir_eisdir_after_buffer() -> bool {
    let Ok(fd) = sys::open(c"/tmp".as_ptr().cast(), sys::O_RDONLY | sys::O_DIRECTORY, 0) else {
        return false;
    };
    let mut b = [0u8; 8];
    // SAFETY: the kernel writes nothing for a 0-byte `read`, nor through a
    // kernel-half pointer, which its range check refuses with `EFAULT`
    // before any write (SYSCALL.md §5); established here.
    let (zero, kernel) = unsafe {
        (
            sys::read(fd as u32, b.as_mut_ptr(), 0),
            sys::read(fd as u32, KERNEL_PTR as *mut u8, 8),
        )
    };
    close(fd) && zero == Err(Errno::EISDIR) && kernel == Err(Errno::EFAULT)
}

/// Case 11: fd 1 is the console.
fn lseek_whence_before_espipe() -> bool {
    sys::lseek(1, 0, WHENCE_BAD) == Err(Errno::EINVAL)
}

/// Case 12.
fn execve_path_before_argv() -> bool {
    let argv = KERNEL_PTR as *const *const u8;
    sys::execve(c"/errno_none".as_ptr().cast(), argv, core::ptr::null()) == Err(Errno::ENOENT)
        && sys::execve(c"/tmp".as_ptr().cast(), argv, core::ptr::null()) == Err(Errno::EACCES)
}

/// Case 13.
fn getdents_device_enotdir() -> bool {
    let Ok(fd) = sys::open(c"/dev/console".as_ptr().cast(), sys::O_RDONLY, 0) else {
        return false;
    };
    let mut b = [0u8; 512];
    // SAFETY: `b` is a local 512-byte buffer no other reference covers;
    // established here.
    let r = unsafe { sys::getdents64(fd as u32, b.as_mut_ptr().cast(), 512) };
    close(fd) && r == Err(Errno::ENOTDIR)
}

/// Case 14: the child is reaped whatever the checks found.
fn kill_zombie_returns_0() -> bool {
    let pid = match sys::fork() {
        Ok(0) => vibeos_user::rt::exit(ZOMBIE_EXIT),
        Ok(pid) => pid,
        Err(_) => return false,
    };
    let mut zombie = false;
    for _ in 0..ZOMBIE_TRIES {
        zombie = utest::zombie(pid);
        if zombie {
            break;
        }
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a yield has no failure the case can act on (DESIGN §2.5)"
        )]
        let _ = sys::sched_yield();
    }
    let killed = [SIGKILL, SIGSTOP, utest::SIGCONT].map(|sig| sys::kill(pid as i32, sig));
    let still = utest::zombie(pid);
    let mut status = 0i32;
    // SAFETY: `wait4` writes 4 bytes through `&raw mut status`, a local no
    // reference covers, and nothing through the null rusage; established here.
    let r = unsafe { sys::wait4(pid as i32, &raw mut status, 0, core::ptr::null_mut()) };
    zombie
        && killed.iter().all(|k| *k == Ok(0))
        && still
        && r == Ok(pid)
        && sys::exit_code(status as u32) == Some(ZOMBIE_EXIT as u8)
}

/// Close `fd`; whether it closed.
fn close(fd: usize) -> bool {
    sys::close(fd as u32).is_ok()
}
