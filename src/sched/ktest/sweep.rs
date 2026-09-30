//! The blocked-thread sweep's tests (ROADMAP §10.7, DESIGN §6.5).

use crate::ktest::Outcome;
use crate::thread_init;
use crate::time_init;

use super::fill_threads;

/// Scans [`sched_sweep_cost`] times.
const COST_RUNS: usize = 16;

/// The sweep's scan over a full thread table: fill the table with threads
/// blocked on a semaphore, run the scan `COST_RUNS` times as the sweep runs
/// it (under SCHED, IF off), and print the largest and the median time.
/// No threshold: ROADMAP §10.7 moves the scan off `schedule_inner` if it
/// measures above 20 µs at 1,024 threads under TCG.
pub(crate) fn sched_sweep_cost() -> Outcome {
    let fill = fill_threads(0);
    // Let the fillers reach their wait.
    thread_init::sleep_ms(100);
    let per_ms = time_init::tsc_per_ms().max(1);
    let mut ns = [0u64; COST_RUNS];
    let mut threads = 0usize;
    for slot in ns.iter_mut() {
        let (cycles, n) = thread_init::testing::time_sweep_scan();
        *slot = cycles.saturating_mul(1_000_000) / per_ms;
        threads = n;
    }
    let spawned = fill.spawned;
    if !fill.release() {
        return Outcome::Fail("fillers did not exit");
    }
    ns.sort_unstable();
    let max = ns.last().copied().unwrap_or(0);
    let median = ns.get(COST_RUNS / 2).copied().unwrap_or(0);
    crate::ktest_info!("max {max} ns median {median} ns at {threads} threads");
    if spawned == 0 {
        return Outcome::Fail("no filler spawned");
    }
    Outcome::Ok
}
