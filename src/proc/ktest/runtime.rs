//! In-guest test for the Rust user runtime (kernel_tests only, ROADMAP §10.5):
//! `ktest_rt`, embedded from `make user` (C-USERBINS), run in ring 3.
//! Rows: the list in crate::ktest.

use vibeos::fs::FsError;
use vibeos::proc::{wexitstatus, wifexited};

use crate::ktest::Outcome;
use crate::ktest::user::{self, Image};
use crate::user_init::LoadError;

/// The runtime's test program.
const KTEST_RT: Image = Image::UserBin("ktest_rt");

/// The exit status in `wait4` status word `st`, or `None` when the process
/// did not exit (a signal ended it).
fn exit_status(st: u32) -> Option<u32> {
    wifexited(st).then(|| wexitstatus(st))
}

/// Each `ktest_rt` mode: its argv and the exit status it must end with.
/// `args` passes an empty argument, one with a space, and non-ASCII bytes.
const CASES: &[(&str, &[&str], u32)] = &[
    (
        "args",
        &["ktest_rt", "args", "", "a b", "\u{e9}\u{2603}"],
        0,
    ),
    ("mem", &["ktest_rt", "mem"], 0),
    ("fp", &["ktest_rt", "fp"], 0),
    ("panic-capture", &["ktest_rt", "panic-capture"], 0),
    // Its `panicked at` line reaches serial unframed (DESIGN §2.6), so the
    // harness's kernel panic signatures do not match it.
    ("panic", &["ktest_rt", "panic"], 101),
];

pub(crate) fn user_runtime() -> Outcome {
    for &(case, argv, want) in CASES {
        let st = match user::run(&KTEST_RT, argv) {
            Ok(st) => st,
            Err(e) => return crate::fail_fmt!("{case}: spawn: {}", e.as_str()),
        };
        if exit_status(st) != Some(want) {
            return crate::fail_fmt!("{case}: status {st:#x}, want exited {want}");
        }
    }
    match user::spawn(&Image::UserBin("ktest_no_such_bin"), &["x"]) {
        Err(LoadError::Fs(FsError::NotFound)) => Outcome::Ok,
        Err(e) => crate::fail_fmt!("unknown UserBin: {}, want fs not found", e.as_str()),
        Ok(pid) => {
            let st = user::wait(pid);
            crate::fail_fmt!("unknown UserBin: spawned, status {st:#x}")
        }
    }
}
