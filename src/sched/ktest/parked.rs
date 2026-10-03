//! In-guest test that a thread spawned parked waits for `make_ready`
//! (DESIGN §7.6): a stale run-queue or wake-inbox entry for its slot finds
//! it `Blocked` and drops it.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::ktest::Outcome;
use crate::thread_init;

/// Set by [`parked_entry`] when the parked thread runs.
static RAN: AtomicBool = AtomicBool::new(false);

fn parked_entry() {
    RAN.store(true, Ordering::Release);
}

/// A thread spawned parked on this CPU, then queued here by hand as a wake
/// whose push lands after its thread exited and a parked spawn took the
/// slot would queue it, does not run while this thread yields; it runs
/// once `make_ready` makes it runnable.
pub(crate) fn test_parked_waits_for_make_ready() -> Outcome {
    RAN.store(false, Ordering::Release);
    let me = thread_init::current_cpu();
    let h = match thread_init::spawn_parked_on("parked", parked_entry, me) {
        Ok(h) => h,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    // Local: the run queue takes the tid, as a drained inbox bit would.
    crate::ipi_init::place_ready(me, h.id(), 0);
    for _ in 0..4 {
        thread_init::yield_now();
    }
    if RAN.load(Ordering::Acquire) {
        return Outcome::Fail("parked thread ran before make_ready");
    }
    thread_init::make_ready(h.id());
    if !crate::ktest::wait_for(|| thread_init::exited(h.id())) {
        return Outcome::Fail("thread did not exit after make_ready");
    }
    if !RAN.load(Ordering::Acquire) {
        return Outcome::Fail("thread exited without running");
    }
    Outcome::Ok
}
