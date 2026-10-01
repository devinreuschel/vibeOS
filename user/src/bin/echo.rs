//! `/bin/echo [-n] [arg...]` (ROADMAP §10.5): the arguments joined by single
//! spaces, then a newline unless `-n` comes first. Status 0, or 1 when the
//! write fails.

#![no_std]
#![no_main]

use vibeos_user::cmd::{self, Out};
use vibeos_user::env::Env;
use vibeos_user::sys::Errno;

vibeos_user::main!(main);

fn main(env: &Env) -> i32 {
    match echo(env) {
        Ok(()) => 0,
        Err(e) => {
            cmd::err(b"echo", b"write", e);
            1
        }
    }
}

fn echo(env: &Env) -> Result<(), Errno> {
    let newline = env.arg(1) != Some(b"-n");
    let first = if newline { 1 } else { 2 };
    let mut out = Out::new(1);
    for (i, arg) in env.args().skip(first).enumerate() {
        if i != 0 {
            out.put(b" ")?;
        }
        out.put(arg)?;
    }
    if newline {
        out.put(b"\n")?;
    }
    out.flush()
}
