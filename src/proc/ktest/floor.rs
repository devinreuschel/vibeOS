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

/// `reboot`'s argument checks from ring 3: bad magic numbers, an unknown
/// command and `HALT` are `EINVAL`, `RESTART2` with an unmapped `arg` is
/// `EFAULT`, and `CAD_ON` and `CAD_OFF` return 0 (SYSCALL.md §3.1).
pub(crate) fn reboot_bad_args_einval() -> Outcome {
    match run_case("reboot-einval") {
        Ok(()) => Outcome::Ok,
        Err(o) => o,
    }
}

/// `floorcheck <case>`, whose `reboot` call ends the machine: its return
/// is the failure. Opt-in; `make test-e2e-power` runs it and checks the
/// line and QEMU's exit.
fn reboot_ends(case: &str) -> Outcome {
    match user::run(&FLOORCHECK, &["floorcheck", case]) {
        Ok(st) => crate::fail_fmt!("reboot returned: {case}: status {st:#x}"),
        Err(e) => crate::fail_fmt!("{case}: spawn: {}", e.as_str()),
    }
}

/// `reboot(POWER_OFF)` turns the machine off.
pub(crate) fn reboot_power_off() -> Outcome {
    reboot_ends("poweroff")
}

/// `reboot(RESTART)` resets the machine.
pub(crate) fn reboot_restart() -> Outcome {
    reboot_ends("restart")
}
