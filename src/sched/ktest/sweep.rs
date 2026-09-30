//! The blocked-thread sweep's tests (ROADMAP §10.7, DESIGN §6.5).

use crate::ktest::Outcome;
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
