//! The blocked-thread sweep's tests (ROADMAP §10.7, DESIGN §6.5).

use core::sync::atomic::{AtomicU32, Ordering};

use vibeos::thread::ThreadState;
use vibeos::time::Instant;

use crate::ktest::{Outcome, sleep_until};
use crate::sync::blocking_init::Semaphore;
use crate::thread_init;
use crate::time_init;

use super::fill_threads;

/// Scans [`sched_sweep_cost`] times.
const COST_RUNS: usize = 16;
/// SCHED holds one scan takes: `MAX_THREADS` in `SWEEP_CHUNK` slots each.
const HOLDS: usize = vibeos::thread::MAX_THREADS.div_ceil(thread_init::testing::SWEEP_CHUNK);

/// The sweep's scan over a full thread table: fill the table with threads
/// blocked on a semaphore, run the scan `COST_RUNS` times as the sweep
/// thread runs it (a `SWEEP_CHUNK`-slot chunk per SCHED hold, IF off), and
/// print the largest and the median hold over every run. No threshold:
/// ROADMAP §10.7's bound is 20 µs at 1,024 threads under TCG (TIME.md §6.5
/// records the figure).
pub(crate) fn sched_sweep_cost() -> Outcome {
    let fill = fill_threads(0);
    // Let the fillers reach their wait.
    thread_init::sleep_ms(100);
    let per_ms = time_init::tsc_per_ms().max(1);
    let mut ns = [0u64; COST_RUNS * HOLDS];
    let mut n = 0usize;
    let mut threads = 0usize;
    for _ in 0..COST_RUNS {
        threads = thread_init::testing::time_sweep_scan(|cycles| {
            if let Some(slot) = ns.get_mut(n) {
                *slot = cycles.saturating_mul(1_000_000) / per_ms;
                n += 1;
            }
        });
    }
    let spawned = fill.spawned;
    if !fill.release() {
        return Outcome::Fail("fillers did not exit");
    }
    let held = ns.get_mut(..n).unwrap_or(&mut []);
    held.sort_unstable();
    let max = held.last().copied().unwrap_or(0);
    let median = held.get(n / 2).copied().unwrap_or(0);
    crate::ktest_info!("max {max} ns median {median} ns at {threads} threads");
    if spawned == 0 {
        return Outcome::Fail("no filler spawned");
    }
    Outcome::Ok
}

/// The semaphore [`lost_worker`] waits on, at 0.
static LOST_SEM: Semaphore = Semaphore::new(0);
/// [`lost_worker`]'s end: 0 still waiting, 1 woken, 2 timed out.
static LOST_END: AtomicU32 = AtomicU32::new(0);
/// How long [`lost_worker`]'s wait lasts.
const LOST_WAIT_MS: u64 = 100;
/// How long after the block the sweep has to report it: `OVERDUE_NS`
/// past a 100 ms deadline, plus a sweep period and slack.
const LOST_REPORT_MS: u64 = 8_000;

fn lost_worker() {
    let deadline = Instant {
        ns: time_init::now_ns().saturating_add(LOST_WAIT_MS * 1_000_000),
    };
    let end = if LOST_SEM.acquire_until(Some(deadline)).is_some() {
        1
    } else {
        2
    };
    LOST_END.store(end, Ordering::Release);
}

/// A thread blocked with a 100 ms deadline whose timeout entry a
/// `kernel_tests` hook removes is reported as `vibeOS: sched: overdue tid
/// <id>` within 8 s of blocking (F111). The harness registers that line as
/// a failure line; this test declares it (`tests/harness/declared.py`).
pub(crate) fn sched_overdue_lost_timeout() -> Outcome {
    LOST_END.store(0, Ordering::Relaxed);
    let Ok(h) = thread_init::spawn("lost-timeout", lost_worker) else {
        return Outcome::Fail("spawn");
    };
    let id = h.id();
    let blocked = || {
        matches!(
            thread_init::try_state(id),
            Some(ThreadState::Blocked { .. })
        )
    };
    if !sleep_until(blocked, LOST_WAIT_MS / 2) {
        return Outcome::Fail("worker never blocked");
    }
    let t0 = time_init::now_ns();
    if !thread_init::ktest_drop_timeout(id) {
        return Outcome::Fail("timeout fired before removal");
    }
    let reported = sleep_until(|| thread_init::ktest_last_overdue() == id, LOST_REPORT_MS);
    let waited_ms = time_init::now_ns().saturating_sub(t0) / 1_000_000;
    LOST_SEM.release();
    let sweeps = thread_init::ktest_sweeps();
    let woke = sleep_until(
        || LOST_END.load(Ordering::Acquire) != 0 && thread_init::exited(id),
        2_000,
    );
    // Every sweep that saw the worker blocked has printed before the run
    // ends: its line stays inside this test's window.
    let settled = sleep_until(|| thread_init::ktest_sweeps() > sweeps, 3_000);
    if !reported {
        return crate::fail_fmt!("no overdue report within {} ms", waited_ms);
    }
    if !woke {
        return Outcome::Fail("worker not woken by the release");
    }
    if LOST_END.load(Ordering::Acquire) != 1 {
        return Outcome::Fail("worker's wait timed out");
    }
    if !settled {
        return Outcome::Fail("no sweep finished after the release");
    }
    crate::ktest_info!("reported {} ms after the block", waited_ms);
    Outcome::Ok
}
