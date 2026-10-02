//! The in-guest test `walk_path_resolution`'s program (ROADMAP §10.4,
//! F056): path resolution as path_resolution(7) gives it, over the tree
//! the test makes on vibefs at `/vibe` (`f` holding `F`, `a/b/`, `a/x`
//! holding `X`, and `l`, a link to `a/b`). Only `kernel_tests` kernels
//! embed it.
//!
//! It exits 0 when every case passes, else with the number of the first
//! that fails:
//!
//! 1. `/./vibe/f` does not read `F` (`.`).
//! 2. `//vibe/f` does not read `F` (repeated slashes).
//! 3. `/dev/../vibe/f` does not read `F` (`..` out of a mount's root).
//! 4. `/VIBE/f` does not read `F` (FAT's case-insensitive name finds the
//!    dentry the vibefs mount is on).
//! 5. `/vibe/l/../x` does not read `X` (`..` after the link is followed).
//! 6. `open("/vibe/l/", O_RDONLY | O_DIRECTORY)` fails, or a `read` of it
//!    does not return `EISDIR`.
//! 7. `open("/vibe/f/")` does not return `ENOTDIR`.

#![no_std]
#![no_main]

use vibeos_user::env::Env;
use vibeos_user::sys::{self, Errno};

vibeos_user::main!(main);

fn main(_env: &Env) -> i32 {
    match run() {
        Ok(()) => 0,
        Err(case) => case,
    }
}

fn open(path: &[u8], flags: i32) -> Result<u32, Errno> {
    let fd = sys::open(path.as_ptr(), flags, 0)?;
    u32::try_from(fd).map_err(|_| Errno::EBADF)
}

/// One `read` of up to 8 bytes from `fd`.
fn read8(fd: u32, buf: &mut [u8; 8]) -> Result<usize, Errno> {
    // SAFETY: the kernel writes at most `buf.len()` bytes into `buf`,
    // which no other reference covers during the call; established here.
    unsafe { sys::read(fd, buf.as_mut_ptr(), buf.len()) }
}

fn close(fd: u32) {
    // A failed close changes no case this program checks.
    let _closed = sys::close(fd);
}

/// Whether `path` (NUL-terminated) opens and reads exactly `want`.
fn reads(path: &[u8], want: &[u8]) -> bool {
    let Ok(fd) = open(path, sys::O_RDONLY) else {
        return false;
    };
    let mut buf = [0u8; 8];
    let r = read8(fd, &mut buf);
    close(fd);
    matches!(r, Ok(n) if buf.get(..n) == Some(want))
}

fn run() -> Result<(), i32> {
    let cases: [(&[u8], &[u8]); 5] = [
        (b"/./vibe/f\0", b"F"),
        (b"//vibe/f\0", b"F"),
        (b"/dev/../vibe/f\0", b"F"),
        (b"/VIBE/f\0", b"F"),
        (b"/vibe/l/../x\0", b"X"),
    ];
    for (i, (path, want)) in cases.iter().enumerate() {
        if !reads(path, want) {
            return Err(i as i32 + 1);
        }
    }
    let fd = open(b"/vibe/l/\0", sys::O_RDONLY | sys::O_DIRECTORY).map_err(|_| 6)?;
    let mut buf = [0u8; 8];
    let r = read8(fd, &mut buf);
    close(fd);
    if r != Err(Errno::EISDIR) {
        return Err(6);
    }
    match open(b"/vibe/f/\0", sys::O_RDONLY) {
        Err(e) if e == Errno::ENOTDIR => Ok(()),
        Ok(fd) => {
            close(fd);
            Err(7)
        }
        Err(_) => Err(7),
    }
}
