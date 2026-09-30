//! In-guest tests of the ROADMAP §10.5 floor calls (kernel_tests only):
//! `floorcheck`, embedded from `make user` (C-USERBINS), run in ring 3.
//! Rows: the list in crate::ktest.

use vibeos::proc::{wexitstatus, wifexited};

use crate::ktest::Outcome;
use crate::ktest::user::{self, Image};
use crate::time_init;

const MS: u64 = 1_000_000;

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

/// `floorcheck <case>`, and the nanoseconds it took.
fn timed_case(case: &str) -> Result<u64, Outcome> {
    let t0 = time_init::now_ns();
    run_case(case)?;
    Ok(time_init::now_ns().saturating_sub(t0))
}

/// `getdents64`, `fstat` and `nanosleep` from ring 3, each with its errors
/// (SYSCALL.md §3.1). The time checks are one-sided, as TCG's timing is
/// loose: a 50 ms sleep takes at least 50 ms, and a child sleeping 10 s
/// that `SIGKILL` ends after 100 ms takes under 5 s.
pub(crate) fn floor_syscalls_from_user() -> Outcome {
    let r = (|| {
        run_case("getdents")?;
        run_case("fstat")?;
        let t = timed_case("nanosleep")?;
        if t < 50 * MS {
            return Err(crate::fail_fmt!("nanosleep: {t} ns, want at least 50 ms"));
        }
        let t = timed_case("sleepkill")?;
        if !(100 * MS..5_000 * MS).contains(&t) {
            return Err(crate::fail_fmt!(
                "sleepkill: {t} ns, want 100 ms to under 5 s"
            ));
        }
        Ok(())
    })();
    match r {
        Ok(()) => Outcome::Ok,
        Err(o) => o,
    }
}
