//! The vibeOS user runtime (ROADMAP §10.5, C-USERRT).
//!
//! A program is `src/bin/<name>.rs`, `#![no_std]` and `#![no_main]`, and names
//! its entry with [`main!`]. The runtime's `_start` reads the initial stack
//! into an [`env::Env`], calls the program, and exits with its return value; a
//! panic prints `panicked at <file>:<line>:<col>:` and the message on fd 2 and
//! exits with status 101 ([`rt`]). The heap is [`alloc`]'s `#[global_allocator]`
//! over `brk`: a program that names the `alloc` crate (`extern crate alloc;`)
//! gets `Box`, `Vec` and `String`. Everything that names an architecture lives
//! in the private `arch` module (`scripts/check_user_arch.py`).

#![no_std]
#![allow(
    clippy::disallowed_types,
    clippy::disallowed_macros,
    reason = "user code, not the kernel: a failed allocation ends a user process, so alloc's panicking calls are fine here (DESIGN §4.4)"
)]

extern crate vibeos_user_mem;

pub mod alloc;
mod arch;
pub mod env;
mod errno;
pub mod io;
pub mod rt;
pub mod sys;

/// `vibeos_user::main!(f)` makes `f: fn(&env::Env) -> i32` the program's
/// entry; its return value is the exit status.
#[macro_export]
macro_rules! main {
    ($f:path) => {
        #[unsafe(no_mangle)]
        fn __vibeos_user_main(env: &$crate::env::Env) -> i32 {
            $f(env)
        }
    };
}
