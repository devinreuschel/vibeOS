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
//! vibeOS: utest: info <name>: <text>
//! vibeOS: utest: end
//! ```
//!
//! So a suite does its work only inside its cases' closures. Nothing in the
//! guest enforces a case's deadline: the harness's progress deadline, from
//! the one each `run` line prints, is the only one (`tests/harness/utest.py`).
//! A case that forks ends its child in `sys::exit`, never back in the
//! runner ([`fork_child`]). An `info` line is a case's detail, never a
//! result ([`info`]).

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write};

use crate::rt;
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

/// The deadline a case's `run` line names unless [`Runner::case_ms`] gives
/// another, in milliseconds.
pub const DEFAULT_DEADLINE_MS: u32 = 10_000;

/// The longest protocol line, newline included: a longer one is cut.
pub const LINE_MAX: usize = 160;

/// Runs the suites and counts the failed cases.
#[derive(Debug, Default)]
pub struct Runner {
    counting: bool,
    count: u32,
    failed: u32,
    // Every case's name, from the count pass.
    names: Vec<&'static str>,
    /// When set, only this case name runs.
    only: Option<&'static [u8]>,
}

impl Runner {
    /// A runner with nothing run yet.
    pub const fn new() -> Self {
        Self {
            counting: false,
            count: 0,
            failed: 0,
            names: Vec::new(),
            only: None,
        }
    }

    /// Run only the case whose name is `name`.
    pub fn only_case(&mut self, name: &'static [u8]) {
        self.only = Some(name);
    }

    /// Count the cases of `suites`, then print `begin <n>`, run every case,
    /// and print `end`.
    pub fn run_suites(&mut self, suites: &[fn(&mut Runner)]) {
        self.counting = true;
        self.count = 0;
        self.names.clear();
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
        self.case_ms(name, DEFAULT_DEADLINE_MS, f);
    }

    /// [`Runner::case`] with a deadline of `deadline_ms` on its `run` line,
    /// for a case that needs longer than [`DEFAULT_DEADLINE_MS`].
    pub fn case_ms(&mut self, name: &'static str, deadline_ms: u32, f: impl FnOnce() -> Outcome) {
        if self.only.is_some_and(|only| name.as_bytes() != only) {
            return;
        }
        if self.counting {
            self.count += 1;
            // A failed push only makes `is_registered` miss the name.
            if self.names.try_reserve(1).is_ok() {
                self.names.push(name);
            }
            return;
        }
        line(format_args!("vibeOS: utest: run {name} {deadline_ms}"));
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

    /// How many cases the count pass registered.
    pub fn counted(&self) -> u32 {
        self.count
    }

    /// Whether a case named `name` runs in this run: complete once the
    /// count pass is done, so a suite asks it outside its cases' closures.
    pub fn is_registered(&self, name: &str) -> bool {
        self.names.contains(&name)
    }
}

/// Print `vibeOS: utest: info <name>: <args>`: a case's detail, never a
/// result.
pub fn info(name: &str, args: fmt::Arguments<'_>) {
    line(format_args!("vibeOS: utest: info {name}: {args}"));
}

/// A failure whose text is formatted: the text lives until the process
/// exits, which a failing case's run reaches soon.
pub fn fail(args: fmt::Arguments<'_>) -> Outcome {
    let mut s = String::new();
    if s.write_fmt(args).is_err() {
        return Outcome::Fail("a failure message did not format");
    }
    Outcome::Fail(s.leak())
}

/// What a call returned, for a failure message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Got(pub Result<usize, Errno>);

impl fmt::Display for Got {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Ok(v) => write!(f, "{v}"),
            Err(e) => write!(f, "-{}", e.0),
        }
    }
}

/// `Ok` when `r` is the error `want`; else what it was.
pub fn expect_err(r: Result<usize, Errno>, want: Errno) -> Result<(), Got> {
    if r == Err(want) { Ok(()) } else { Err(Got(r)) }
}

/// The value `r` succeeded with; else the error.
pub fn expect_ok(r: Result<usize, Errno>) -> Result<usize, Got> {
    r.map_err(|e| Got(Err(e)))
}

/// Fork a child that runs `f` and exits with its return value: it never
/// returns to the caller, so it never touches the [`Runner`]. The child's
/// pid, in the parent.
pub fn fork_child(f: impl FnOnce() -> i32) -> Result<usize, Errno> {
    match fork()? {
        0 => rt::exit(f()),
        pid => Ok(pid),
    }
}

/// Wait for child `pid` (`wait4(pid, &status, 0)`): its status word.
pub fn wait_status(pid: usize) -> Result<u32, Errno> {
    let mut status = 0i32;
    // SAFETY: `wait4` writes 4 bytes through `&raw mut status`, a local no
    // reference covers, and nothing through the null rusage; established here.
    let r = unsafe { sys::wait4(pid as i32, &raw mut status, 0, core::ptr::null_mut()) }?;
    if r != pid {
        return Err(Errno::ECHILD);
    }
    Ok(status as u32)
}

/// The exit code a status word holds (`WIFEXITED`, `WEXITSTATUS`).
pub fn exited(status: u32) -> Option<u8> {
    sys::exit_code(status)
}

/// The signal that ended the process (`WIFSIGNALED`, `WTERMSIG`).
pub fn signaled(status: u32) -> Option<u8> {
    let sig = (status & 0x7f) as u8;
    (sig != 0 && sig != 0x7f).then_some(sig)
}

/// `SIGCONT`, from signal(7): continues a stopped process, and is harmless
/// to a running one.
pub const SIGCONT: i32 = 18;

/// `psinfo`'s line for `pid`, `<pid> <ppid> <state> <name> <syscalls>`:
/// its `<ppid>`, and its state (`run`, `stop`, or `zombie`) padded with
/// NULs; `None` when there is no such line. `psinfo` lists only the
/// lowest pids whose lines fit in its 512 bytes (SYSCALL.md §3.1), so a
/// table of more than about 20 processes can leave `pid` out.
pub fn ps_state(pid: usize) -> Option<(usize, [u8; 8])> {
    let mut buf = [0u8; 512];
    // SAFETY: the kernel writes at most 512 bytes into `buf`, a local no
    // other reference covers; established here.
    let n = unsafe { sys::psinfo(buf.as_mut_ptr(), buf.len()) }.ok()?;
    let dec = |s: &[u8]| crate::cmd::parse_dec(s).and_then(|v| usize::try_from(v).ok());
    for line in buf.get(..n)?.split(|&b| b == b'\n') {
        let mut f = line.split(|&b| b == b' ');
        let (Some(p), Some(pp), Some(st)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        if dec(p) != Some(pid) {
            continue;
        }
        let mut state = [0u8; 8];
        let k = st.len().min(state.len());
        state.get_mut(..k)?.copy_from_slice(st.get(..k)?);
        return Some((dec(pp)?, state));
    }
    None
}

/// Whether `state` (from [`ps_state`]) is `word`.
pub fn state_is(state: &[u8; 8], word: &[u8]) -> bool {
    state.get(..word.len()) == Some(word) && state.get(word.len()).is_none_or(|&b| b == 0)
}

/// Whether child `pid` is a zombie: `psinfo` says `zombie` for it. `kill`
/// cannot tell, since it returns 0 for a zombie, as on Linux (SYSCALL.md
/// §3.1). [`ps_state`] says which pids `psinfo` lists.
pub fn zombie(pid: usize) -> bool {
    ps_state(pid).is_some_and(|(_, s)| state_is(&s, b"zombie"))
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
