//! `/bin/envcheck` (ROADMAP §10.5): exits 0 when its environment holds the
//! entry `K=v`, else 1. `/bin/tests`' `exec_env_*` cases exec it with an
//! environment of their own and read its status. It prints nothing.

#![no_std]
#![no_main]

use vibeos_user::env::Env;

vibeos_user::main!(main);

fn main(env: &Env) -> i32 {
    if env.vars().any(|v| v == b"K=v") {
        0
    } else {
        1
    }
}
