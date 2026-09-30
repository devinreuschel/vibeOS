//! In-guest tests of the ROADMAP §10.5 floor calls (kernel_tests only):
//! `floorcheck`, embedded from `make user` (C-USERBINS), run in ring 3.
//! Rows: the list in crate::ktest.

use vibeos::proc::{wexitstatus, wifexited};

use crate::ktest::Outcome;
use crate::ktest::user::{self, Image};

/// The test program.
const FLOORCHECK: Image = Image::UserBin("floorcheck");

/// Run `floorcheck <case>` and require exit status 0.
fn run_case(case: &str) -> Result<(), Outcome> {
    match user::run(&FLOORCHECK, &["floorcheck", case]) {
        Ok(st) if wifexited(st) && wexitstatus(st) == 0 => Ok(()),
        Ok(st) => Err(crate::fail_fmt!("{case}: status {st:#x}, want exited 0")),
        Err(e) => Err(crate::fail_fmt!("{case}: spawn: {}", e.as_str())),
    }
}

/// `getdents64` and `fstat` from ring 3, each with its errors
/// (SYSCALL.md §3.1).
pub(crate) fn floor_syscalls_from_user() -> Outcome {
    for case in ["getdents", "fstat"] {
        if let Err(o) = run_case(case) {
            return o;
        }
    }
    Outcome::Ok
}
