//! `/bin/grep pattern [file...]` (ROADMAP §10.5): every line that holds the
//! fixed string `pattern`, prefixed `<file>:` when there are several files;
//! no file reads fd 0. Status 0 on a match, 1 on none, 2 on an error.

#![no_std]
#![no_main]
#![allow(
    clippy::disallowed_types,
    reason = "the line Vec grows only through try_reserve"
)]

extern crate alloc;

use alloc::vec::Vec;

use vibeos_user::cmd::{self, Out};
use vibeos_user::env::Env;
use vibeos_user::sys::Errno;

vibeos_user::main!(main);

fn main(env: &Env) -> i32 {
    let Some(pat) = env.arg(1) else {
        return cmd::fail(b"grep", b"no pattern", Errno::EINVAL, 2);
    };
    let (mut matched, mut failed, mut line) = (false, false, Vec::new());
    let mut out = Out::new(1);
    for path in cmd::operands(env, 2, &[b"-"]) {
        let found = cmd::with_reader(path, |r| {
            let mut any = false;
            while r.line(&mut line)? {
                if pat.is_empty() || line.windows(pat.len()).any(|w| w == pat) {
                    any = true;
                    if env.argc() > 3 {
                        out.put(path)?.put(b":")?;
                    }
                    out.put(&line)?.put(b"\n")?.flush()?;
                }
            }
            Ok(any)
        });
        match found {
            Ok(m) => matched |= m,
            Err(e) => failed = cmd::fail(b"grep", path, e, 2) != 0,
        }
    }
    if failed { 2 } else { i32::from(!matched) }
}
