//! `/bin/argcheck` (ROADMAP §10.5): checks its own `argv` against the mode
//! its environment's `ARGCHECK` names, for `/bin/tests`' `exec_*` argument
//! cases. It prints nothing; the status is the verdict:
//!
//! - `ARGCHECK=empty`: `argc` is 1 and `argv[0]` is empty.
//! - `ARGCHECK=<n>:<m>`: `argc` is `n`, and every `argv[i]`, 1 ≤ i < n, is
//!   `m` bytes long.
//!
//! Exit 0 when the check holds; 2 for a missing or bad mode, 3 for a wrong
//! `argc`, 4 for a non-empty `argv[0]`, 5 for a wrong length.

#![no_std]
#![no_main]

use vibeos_user::env::Env;

vibeos_user::main!(main);

/// `s` as a decimal number, or `None` when it is empty, holds anything but
/// digits, or overflows.
fn decimal(s: &[u8]) -> Option<usize> {
    if s.is_empty() {
        return None;
    }
    s.iter().try_fold(0usize, |n, &b| {
        let d = b.checked_sub(b'0').filter(|&d| d <= 9)?;
        n.checked_mul(10)?.checked_add(usize::from(d))
    })
}

fn main(env: &Env) -> i32 {
    let Some(mode) = env.var(b"ARGCHECK") else {
        return 2;
    };
    if mode == b"empty" {
        if env.argc() != 1 {
            return 3;
        }
        return if env.arg(0) == Some(b"") { 0 } else { 4 };
    }
    let Some(colon) = mode.iter().position(|&b| b == b':') else {
        return 2;
    };
    let (Some(n), Some(m)) = (
        mode.get(..colon).and_then(decimal),
        mode.get(colon + 1..).and_then(decimal),
    ) else {
        return 2;
    };
    if env.argc() != n {
        return 3;
    }
    if env.args().skip(1).any(|a| a.len() != m) {
        return 5;
    }
    0
}
