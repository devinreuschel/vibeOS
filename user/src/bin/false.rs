//! `/bin/false` (ROADMAP §10.5): ignores its arguments and exits 1.

#![no_std]
#![no_main]

use vibeos_user::env::Env;

vibeos_user::main!(main);

fn main(_env: &Env) -> i32 {
    1
}
