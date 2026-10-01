//! `/bin/yes [arg...]` (ROADMAP §10.5): `y`, or the arguments joined by
//! single spaces, then a newline, repeated with one `write` a line until
//! killed. Status 1 when a write fails.

#![no_std]
#![no_main]

use vibeos_user::cmd::{self, Out};
use vibeos_user::env::Env;
use vibeos_user::sys::Errno;

vibeos_user::main!(main);

fn main(env: &Env) -> i32 {
    loop {
        if let Err(e) = line(env) {
            return cmd::fail(b"yes", b"write", e, 1);
        }
    }
}

/// One line.
fn line(env: &Env) -> Result<(), Errno> {
    let mut out = Out::new(1);
    if env.argc() < 2 {
        out.put(b"y")?;
    }
    for (i, arg) in env.args().skip(1).enumerate() {
        out.put(if i == 0 { b"" } else { b" " })?.put(arg)?;
    }
    out.put(b"\n")?.flush()
}
