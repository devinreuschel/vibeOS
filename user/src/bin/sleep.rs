//! `/bin/sleep <seconds>` (ROADMAP §10.5): sleeps that many whole seconds,
//! a decimal integer, through `nanosleep`, resuming with the remaining time
//! after `EINTR`. Status 0, or 1 on a bad operand or a failed call.

#![no_std]
#![no_main]

use core::ffi::c_void;

use vibeos_user::cmd;
use vibeos_user::env::Env;
use vibeos_user::sys::{self, Errno};

vibeos_user::main!(main);

/// `struct __kernel_timespec`.
#[repr(C)]
struct Timespec {
    sec: i64,
    nsec: i64,
}

fn main(env: &Env) -> i32 {
    let arg = env.arg(1).unwrap_or(b"");
    let Some(sec) = cmd::parse_dec(arg).and_then(|s| i64::try_from(s).ok()) else {
        return cmd::fail(b"sleep", arg, Errno::EINVAL, 1);
    };
    let mut req = Timespec { sec, nsec: 0 };
    loop {
        let mut rem = Timespec { sec: 0, nsec: 0 };
        let rq = (&raw const req).cast::<c_void>();
        match sys::nanosleep(rq, (&raw mut rem).cast::<c_void>()) {
            Err(e) if cmd::interrupted(e) => req = rem,
            Err(e) => return cmd::fail(b"sleep", b"nanosleep", e, 1),
            Ok(_) => return 0,
        }
    }
}
