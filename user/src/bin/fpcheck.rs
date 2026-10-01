//! `/bin/fpcheck` (ROADMAP §10.6): exits 0 when it started with the psABI's
//! initial FP state (FCW `0x037F`, MXCSR `0x1F80`, the first vector register
//! zero), as `_start` captured it, else 1. `/bin/tests`' `fp_execve_initial`
//! execs it with every one of those dirtied. It prints nothing.

#![no_std]
#![no_main]

use vibeos_user::env::Env;

vibeos_user::main!(main);

fn main(_env: &Env) -> i32 {
    i32::from(!vibeos_user::arch::is_initial())
}
