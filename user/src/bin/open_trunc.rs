//! The in-guest test `open_trunc_enfile`'s program (ROADMAP §10.4, F057):
//! an `open` that cannot get a descriptor or an open-file slot changes no
//! file. Only `kernel_tests` kernels embed it.
//!
//! `open_trunc <case>` exits 0 when the case passes, else with the number
//! of the first check that fails:
//!
//! - `enfile`, run while the kernel holds every open-file slot: check 1,
//!   `open("/vibe/s60t", O_WRONLY | O_TRUNC)` fails with `ENFILE`; check
//!   2, `open("/vibe/s60n", O_WRONLY | O_CREAT)` fails with `ENFILE`.
//! - `emfile`: `dup(0)` until the descriptor table is full; check 3, the
//!   last `dup` fails with `EMFILE`; check 4, `open("/vibe/s60t",
//!   O_WRONLY | O_TRUNC)` fails with `EMFILE`.
//!
//! An unknown case exits 9. The kernel test checks that `/vibe/s60t`
//! keeps its size and that `/vibe/s60n` was not made.

#![no_std]
#![no_main]

use vibeos_user::env::Env;
use vibeos_user::sys::{self, Errno};

vibeos_user::main!(main);

const TRUNC: &[u8] = b"/vibe/s60t\0";
const NEW: &[u8] = b"/vibe/s60n\0";

/// More `dup`s than any descriptor table holds.
const MAX_DUPS: u32 = 4096;

fn main(env: &Env) -> i32 {
    let r = match env.arg(1) {
        Some(b"enfile") => enfile(),
        Some(b"emfile") => emfile(),
        _ => Err(9),
    };
    match r {
        Ok(()) => 0,
        Err(check) => check,
    }
}

/// `open(path, flags)` fails with `want`; an fd it returned is closed.
fn fails_with(path: &[u8], flags: i32, want: Errno) -> bool {
    match sys::open(path.as_ptr(), flags, 0o644) {
        Err(e) => e == want,
        Ok(fd) => {
            if let Ok(fd) = u32::try_from(fd) {
                // A failed close changes no check this program makes.
                let _closed = sys::close(fd);
            }
            false
        }
    }
}

fn enfile() -> Result<(), i32> {
    if !fails_with(TRUNC, sys::O_WRONLY | sys::O_TRUNC, Errno::ENFILE) {
        return Err(1);
    }
    if !fails_with(NEW, sys::O_WRONLY | sys::O_CREAT, Errno::ENFILE) {
        return Err(2);
    }
    Ok(())
}

fn emfile() -> Result<(), i32> {
    let mut n = 0u32;
    let full = loop {
        match sys::dup(0) {
            Ok(_) if n < MAX_DUPS => n += 1,
            Ok(_) => break false,
            Err(e) => break e == Errno::EMFILE,
        }
    };
    if !full {
        return Err(3);
    }
    if !fails_with(TRUNC, sys::O_WRONLY | sys::O_TRUNC, Errno::EMFILE) {
        return Err(4);
    }
    Ok(())
}
