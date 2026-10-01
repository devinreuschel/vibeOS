//! `/hello` (ROADMAP §9.8): writes `hello from ring3` to fd 1, calls
//! `getpid`, and exits 42.

#![no_std]
#![no_main]

use vibeos_user::env::Env;
use vibeos_user::sys;

vibeos_user::main!(main);

const MSG: &[u8] = b"hello from ring3\n";

fn main(_env: &Env) -> i32 {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "DESIGN §2.5: status 42 is the program's whole contract"
    )]
    let _ = (sys::write(1, MSG.as_ptr(), MSG.len()), sys::getpid());
    42
}
