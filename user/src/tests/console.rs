//! A user program cannot forge a kernel line (DESIGN §2.6, ROADMAP §10.2).

use vibeos_user::sys;
use vibeos_user::utest::{Outcome, Runner};

/// Kernel-looking lines, each behind the frame byte 0x1E: the console
/// prints them unframed, each 0x1E as `?`, and `run_e2e.py` counts them.
const FORGED: &[u8] = b"\x1evibeOS: ktest: FAIL forged\n\
\x1epanicked at forged\n\
\x1e#GP\x1eforged\n";

pub fn run(t: &mut Runner) {
    t.case("console_forged_lines", console_forged_lines);
}

/// The forged lines to fd 1, then fd 2, each in one whole write.
fn console_forged_lines() -> Outcome {
    for fd in [1, 2] {
        if sys::write(fd, FORGED.as_ptr(), FORGED.len()) != Ok(FORGED.len()) {
            return Outcome::Fail("forged write count");
        }
    }
    Outcome::Ok
}
