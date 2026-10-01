//! The in-guest test `cwd_per_process`'s program (ROADMAP §10.4): a
//! relative `open` resolves from the process's own working directory,
//! which the kernel gave it as `/`, and so does its forked child's,
//! whatever the kernel shell's `cd` set. Only `kernel_tests` kernels embed
//! it.
//!
//! It exits 0 when every case passes, else with the number of the first
//! that fails:
//!
//! 1. `open("cwdprobe")` fails.
//! 2. it does not read `root`, what the test wrote to `/cwdprobe`.
//! 3. `fork` fails.
//! 4. `wait4` does not reap the child.
//! 5. the child did not exit 0: it ran cases 1 and 2 itself, and exits
//!    with the failing one's number.

#![no_std]
#![no_main]

use vibeos_user::env::Env;
use vibeos_user::{rt, sys};

vibeos_user::main!(main);

const PROBE: &[u8] = b"cwdprobe\0";
const WANT: &[u8] = b"root";

fn main(_env: &Env) -> i32 {
    match run() {
        Ok(()) => 0,
        Err(case) => case,
    }
}

/// Open `cwdprobe` relative to the working directory and read it back.
fn probe() -> Result<(), i32> {
    let fd = sys::open(PROBE.as_ptr(), sys::O_RDONLY, 0).map_err(|_| 1)?;
    let fd = u32::try_from(fd).map_err(|_| 1)?;
    let mut buf = [0u8; 16];
    // SAFETY: the kernel writes at most `buf.len()` bytes into `buf`,
    // which no other reference covers during the call; established here.
    let n = unsafe { sys::read(fd, buf.as_mut_ptr(), buf.len()) };
    // A failed close changes no case this program checks.
    let _closed = sys::close(fd);
    match n {
        Ok(n) if buf.get(..n) == Some(WANT) => Ok(()),
        _ => Err(2),
    }
}

fn run() -> Result<(), i32> {
    probe()?;
    let child = match sys::fork() {
        Ok(0) => match probe() {
            Ok(()) => rt::exit(0),
            Err(case) => rt::exit(case),
        },
        Ok(pid) => i32::try_from(pid).map_err(|_| 3)?,
        Err(_) => return Err(3),
    };
    let mut status = 0i32;
    // SAFETY: the kernel writes 4 bytes into `status`, which no other
    // reference covers during the call; established here.
    let waited = unsafe { sys::wait4(child, &raw mut status, 0, core::ptr::null_mut()) };
    if waited != Ok(child as usize) {
        return Err(4);
    }
    match sys::exit_code(status as u32) {
        Some(0) => Ok(()),
        _ => Err(5),
    }
}
