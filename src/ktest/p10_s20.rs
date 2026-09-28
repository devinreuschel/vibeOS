//! In-guest tests of P10-S20, Fallible fork, execve and open (DESIGN §8.2).

use core::fmt;

use vibeos::kalloc::{TryBox, TryVec};

use super::{Outcome, Test, test};
use crate::heap_init::fail_after::{self, Scope, Seen};
use crate::proc_init;
use crate::thread_init;
use crate::time_init;

pub(super) const TESTS: &[Test] = &[test("kalloc_fail_after_hook", test_kalloc_fail_after_hook)];

/// Counts the lines written to it.
struct LineCount(usize);

impl fmt::Write for LineCount {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0 += s.bytes().filter(|&b| b == b'\n').count();
        Ok(())
    }
}

/// Wait up to 5 s until no process, zombies included, holds a slot. Call it
/// only while the hook is disarmed: `write_ps` allocates.
fn no_process() -> bool {
    let deadline = time_init::now_ns().saturating_add(5_000_000_000);
    loop {
        let mut w = LineCount(0);
        proc_init::write_ps(&mut w);
        if w.0 == 0 {
            return true;
        }
        if time_init::now_ns() >= deadline {
            return false;
        }
        thread_init::sleep_ms(10);
    }
}

/// Arms the hook; its `Drop` disarms, so a failing case leaves it off.
struct Armed(bool);

impl Armed {
    fn new(budget: usize, scope: Scope) -> Self {
        fail_after::arm(budget, scope);
        Self(true)
    }

    fn disarm(mut self) -> Seen {
        self.0 = false;
        fail_after::disarm()
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        if self.0 {
            // A failed case disarms only to leave the hook off.
            fail_after::disarm();
        }
    }
}

fn test_kalloc_fail_after_hook() -> Outcome {
    if !no_process() {
        return Outcome::Fail("a process is still running");
    }
    let me = thread_init::current_id();

    let armed = Armed::new(1, Scope::Thread(me));
    let first = TryBox::try_new(1u64);
    let second = TryBox::try_new(2u64);
    let seen = armed.disarm();
    if first.is_err() || second.is_ok() {
        return crate::fail_fmt!(
            "TryBox under budget 1: first ok {}, second ok {}",
            first.is_ok(),
            second.is_ok()
        );
    }
    let want = Seen {
        counted: 2,
        refused: 1,
    };
    if seen != want {
        return crate::fail_fmt!("TryBox counts {seen:?}, want {want:?}");
    }
    drop(first);

    let armed = Armed::new(1, Scope::Thread(me));
    let mut v = match TryVec::<u64>::try_with_capacity(8) {
        Ok(v) => v,
        Err(_) => return Outcome::Fail("TryVec::try_with_capacity(8) refused under budget 1"),
    };
    let grown = v.try_reserve(4096);
    let seen = armed.disarm();
    if grown.is_ok() {
        return Outcome::Fail("TryVec::try_reserve(4096) past the budget succeeded");
    }
    if v.capacity() < 8 || !v.is_empty() {
        return Outcome::Fail("a refused try_reserve changed the vector");
    }
    if seen != want {
        return crate::fail_fmt!("TryVec counts {seen:?}, want {want:?}");
    }
    drop(v);

    let armed = Armed::new(0, Scope::Processes { from_syscall: 1 });
    let b = TryBox::try_new(3u64);
    let seen = armed.disarm();
    if b.is_err() {
        return Outcome::Fail("a kernel thread's TryBox was refused under a process scope");
    }
    let none = Seen {
        counted: 0,
        refused: 0,
    };
    if seen != none {
        return crate::fail_fmt!("process scope counted a kernel thread: {seen:?}");
    }
    Outcome::Ok
}
