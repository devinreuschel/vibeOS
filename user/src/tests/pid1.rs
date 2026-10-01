//! No process can kill or stop init (ROADMAP §10.5, F068).

use vibeos_user::sys;
use vibeos_user::utest::{Outcome, Runner};

// From signal(7).
const SIGKILL: i32 = 9;
const SIGTERM: i32 = 15;
const SIGSTOP: i32 = 19;

pub fn run(t: &mut Runner) {
    t.case("kill_init_ignored", kill_init_ignored);
}

/// `SIGSTOP`, `SIGTERM` and `SIGKILL` to pid 1 each return 0, and init is
/// still running after each. Skipped when this process is not init's child,
/// as in `kernel_tests`' `user_syscalls` run, where no init exists.
fn kill_init_ignored() -> Outcome {
    if sys::getppid() != Ok(1) {
        return Outcome::Skip("no init");
    }
    let sigs: [(i32, &'static str, &'static str); 3] = [
        (
            SIGSTOP,
            "kill(1, SIGSTOP) not 0",
            "pid 1 not run after SIGSTOP",
        ),
        (
            SIGTERM,
            "kill(1, SIGTERM) not 0",
            "pid 1 not run after SIGTERM",
        ),
        (
            SIGKILL,
            "kill(1, SIGKILL) not 0",
            "pid 1 not run after SIGKILL",
        ),
    ];
    for (sig, kill_why, state_why) in sigs {
        if sys::kill(1, sig) != Ok(0) {
            return Outcome::Fail(kill_why);
        }
        for _ in 0..4 {
            #[expect(
                clippy::let_underscore_must_use,
                reason = "a yield has no failure the case can act on (DESIGN §2.5)"
            )]
            let _ = sys::sched_yield();
        }
        match init_state() {
            Some(true) => {}
            Some(false) => return Outcome::Fail(state_why),
            None => return Outcome::Fail("no psinfo line for pid 1"),
        }
    }
    Outcome::Ok
}

/// Whether `psinfo`'s line for pid 1 (`<pid> <ppid> <state> ...`, fields
/// split on spaces, extra ones ignored) names state `run`; `None` when
/// there is no such line.
fn init_state() -> Option<bool> {
    let mut buf = [0u8; 512];
    // SAFETY: `psinfo` writes at most `buf.len()` bytes into `buf`, a local
    // no reference covers; established here.
    let n = unsafe { sys::psinfo(buf.as_mut_ptr(), buf.len()) }.ok()?;
    buf.get(..n)?
        .split(|&b| b == b'\n')
        .map(|line| {
            let mut f = line.split(|&b| b == b' ').filter(|f| !f.is_empty());
            (f.next(), f.nth(1))
        })
        .find(|(pid, _)| *pid == Some(b"1".as_slice()))
        .map(|(_, state)| state == Some(b"run".as_slice()))
}
