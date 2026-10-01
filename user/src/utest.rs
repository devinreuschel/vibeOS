//! The `/bin/tests` runner and the utest protocol (ROADMAP §10.5,
//! C-USERTESTS).
//!
//! A suite is a `fn(&mut Runner)` that calls [`Runner::case`] once per case.
//! [`Runner::run_suites`] runs the suites twice: a count pass, in which
//! `case` only counts, then the run, which prints the ktest protocol's form
//! with `utest:` (DESIGN §8.2), each line one `write` of at most
//! [`LINE_MAX`] bytes:
//!
//! ```text
//! vibeOS: utest: begin <n>
//! vibeOS: utest: run <name> <deadline_ms>
//! vibeOS: utest: ok <name>
//! vibeOS: utest: FAIL <name>: <why>
//! vibeOS: utest: skip <name>: <reason>
//! vibeOS: utest: end
//! ```
//!
//! So a suite does its work only inside its cases' closures. Nothing in the
//! guest enforces a case's deadline. A case that forks ends its child in
//! `sys::exit`, never back in the runner.

use core::fmt::{self, Write};

use crate::sys::{self, Errno};

/// A case's result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Every check passed.
    Ok,
    /// A check failed; the text says which.
    Fail(&'static str),
    /// The case cannot run here; the text says why.
    Skip(&'static str),
}

/// The deadline every case's `run` line names, in milliseconds.
pub const DEFAULT_DEADLINE_MS: u32 = 10_000;

/// The longest protocol line, newline included: a longer one is cut.
pub const LINE_MAX: usize = 160;

/// Runs the suites and counts the failed cases.
#[derive(Debug, Default)]
pub struct Runner {
    counting: bool,
    count: u32,
    failed: u32,
}

impl Runner {
    /// A runner with nothing run yet.
    pub const fn new() -> Self {
        Self {
            counting: false,
            count: 0,
            failed: 0,
        }
    }

    /// Count the cases of `suites`, then print `begin <n>`, run every case,
    /// and print `end`.
    pub fn run_suites(&mut self, suites: &[fn(&mut Runner)]) {
        self.counting = true;
        self.count = 0;
        for suite in suites {
            suite(self);
        }
        self.counting = false;
        line(format_args!("vibeOS: utest: begin {}", self.count));
        for suite in suites {
            suite(self);
        }
        line(format_args!("vibeOS: utest: end"));
    }

    /// One case: in the count pass, count it; otherwise print its `run`
    /// line, run `f`, and print its result.
    pub fn case(&mut self, name: &'static str, f: impl FnOnce() -> Outcome) {
        if self.counting {
            self.count += 1;
            return;
        }
        line(format_args!(
            "vibeOS: utest: run {name} {DEFAULT_DEADLINE_MS}"
        ));
        match f() {
            Outcome::Ok => line(format_args!("vibeOS: utest: ok {name}")),
            Outcome::Fail(why) => {
                self.failed += 1;
                line(format_args!("vibeOS: utest: FAIL {name}: {why}"));
            }
            Outcome::Skip(reason) => line(format_args!("vibeOS: utest: skip {name}: {reason}")),
        }
    }

    /// How many cases failed.
    pub fn failed(&self) -> u32 {
        self.failed
    }
}

/// `fork`, out of line: the call's clobbers then cover every vector
/// register, so no XMM value is live across it while a child still starts
/// from the boot FPU state (F069).
#[inline(never)]
pub fn fork() -> Result<usize, Errno> {
    sys::fork()
}

/// A line of at most [`LINE_MAX`] bytes, built on the stack.
struct Line {
    buf: [u8; LINE_MAX],
    len: usize,
}

impl Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        // Room for the newline stays; the rest of a long line is cut.
        let room = LINE_MAX - 1 - self.len;
        let n = s.len().min(room);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

/// Write `args` and a newline to fd 1 in one `write`.
fn line(args: fmt::Arguments<'_>) {
    let mut l = Line {
        buf: [0; LINE_MAX],
        len: 0,
    };
    // `Line::write_str` never fails, so neither does the format.
    #[expect(
        clippy::let_underscore_must_use,
        reason = "DESIGN §2.5: Line cuts a long line instead of failing"
    )]
    let _ = l.write_fmt(args);
    l.buf[l.len] = b'\n';
    l.len += 1;
    // One write, so the line is whole on the console; a lost protocol line
    // shows as a missing result, which the reader reports.
    #[expect(
        clippy::let_underscore_must_use,
        reason = "DESIGN §2.5: the line's reader reports a missing line"
    )]
    let _ = sys::write(1, l.buf.as_ptr(), l.len);
}
