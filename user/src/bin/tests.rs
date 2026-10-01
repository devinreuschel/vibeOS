//! `/bin/tests` (ROADMAP §10.5): init's first child. It prints
//! `user: tests begin`, runs the suites of `src/tests/` through the utest
//! runner (C-USERTESTS), and ends with `user: tests ok` and status 0, or
//! `user: tests fail` and status 1.
//!
//! `/bin/tests --exec-step <path>` instead `execve`s `<path>` at once, and
//! exits 126 if that fails: the middle step of `lifecycle`'s exec chain.

#![no_std]
#![no_main]

use core::sync::atomic::Ordering;

use vibeos_user::env::Env;
use vibeos_user::sys;
use vibeos_user::utest::Runner;

#[path = "../tests/mod.rs"]
mod tests;

vibeos_user::main!(main);

const OK: &[u8] = b"user: tests ok\n";
const FAIL: &[u8] = b"user: tests fail\n";

/// The flag of an exec chain's middle step.
const EXEC_STEP: &[u8] = b"--exec-step";

fn main(env: &Env) -> i32 {
    if env.arg(1) == Some(EXEC_STEP) {
        return exec_step(env);
    }
    let n = sys::write(1, tests::BANNER.as_ptr(), tests::BANNER.len()).unwrap_or(usize::MAX);
    tests::BANNER_WRITE.store(n, Ordering::Relaxed);
    let mut t = Runner::new();
    t.run_suites(tests::SUITES);
    let (line, code) = if t.failed() == 0 { (OK, 0) } else { (FAIL, 1) };
    #[expect(
        clippy::let_underscore_must_use,
        reason = "DESIGN §2.5: the exit status carries the verdict too"
    )]
    let _ = sys::write(1, line.as_ptr(), line.len());
    code
}

/// `execve(argv[2], [argv[2]], NULL)`; 126 when it returns. `argv[2]` is
/// NUL-terminated in the initial stack, so its start is a C string.
fn exec_step(env: &Env) -> i32 {
    let Some(path) = env.arg(2) else {
        return 126;
    };
    let argv = [path.as_ptr(), core::ptr::null()];
    #[expect(
        clippy::let_underscore_must_use,
        reason = "an execve that returns failed; status 126 reports it (DESIGN §2.5)"
    )]
    let _ = sys::execve(path.as_ptr(), argv.as_ptr(), core::ptr::null());
    126
}
