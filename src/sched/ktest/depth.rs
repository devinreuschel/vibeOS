//! Kernel stack depth (kernel_tests only), re-exported from `sched::ktest`:
//! the table every scan records into, the end-of-run report, and the tests
//! of the measurement (DESIGN §4.5, TESTING §8.2).

use vibeos::lock::RANK_DEVICE;
use vibeos::sched::stack_depth::{self, Deepest, DepthTable, SIZES};

use crate::ktest::{Outcome, sleep_until};
use crate::sync_init::SpinMutex;
use crate::thread_init;

/// The deepest use per stack size. Ranks above `SCHED`, which a scan may
/// hold as it records.
static DEPTH: SpinMutex<DepthTable> = SpinMutex::with_rank(DepthTable::new(), RANK_DEVICE);

/// Keep one measurement (`thread_init::testing`'s scans).
pub(crate) fn record(d: Deepest) {
    DEPTH.lock().record(d);
}

/// The depth recorded for thread `tid` when it exited, if the table's
/// ring still holds it.
pub(crate) fn exit_depth(tid: u32) -> Option<usize> {
    DEPTH.lock().recent(tid).map(|d| d.used)
}

/// Scan every live thread's stack, then print one line per stack size and
/// the report line, outside the lock (TESTING §8.2). Runs just before
/// `vibeOS: ktest: end`.
pub(crate) fn report() {
    thread_init::testing::scan_live_stacks(record);
    let mut sizes: [Option<Deepest>; SIZES] = [None; SIZES];
    let (n, lost) = {
        let g = DEPTH.lock();
        for (slot, d) in sizes.iter_mut().zip(g.deepest()) {
            *slot = Some(d);
        }
        (g.len(), g.lost())
    };
    for d in sizes.iter().flatten() {
        crate::marker!(
            "vibeOS: stack: {} used {} of {} by tid {} {}",
            d.size,
            d.used,
            stack_depth::budget(d.size),
            d.tid,
            d.name
        );
    }
    crate::marker!("vibeOS: stack: report {} sizes {} lost", n, lost);
}

/// Wait up to 2 s for the exit scan of thread `tid`.
fn wait_exit_depth(tid: u32) -> Option<usize> {
    if !sleep_until(|| exit_depth(tid).is_some(), 2_000) {
        return None;
    }
    exit_depth(tid)
}

/// Bytes [`depth_worker`] puts on its stack.
const EXIT_SCAN_BYTES: usize = 4096;

fn depth_worker() {
    let mut a = [0u8; EXIT_SCAN_BYTES];
    core::hint::black_box(&mut a);
}

/// A 16 KiB worker that puts 4 KiB on its stack and exits: the switch
/// tail's scan records between 4 KiB and the 12 KiB budget for it.
pub(crate) fn stack_depth_exit_scan() -> Outcome {
    let h = match thread_init::spawn("stack-exit", depth_worker) {
        Ok(h) => h,
        Err(_) => return Outcome::Fail("spawn"),
    };
    let Some(used) = wait_exit_depth(h.id().0) else {
        return Outcome::Fail("no exit record within 2 s");
    };
    crate::ktest_info!("stack-exit used {} bytes", used);
    if used < EXIT_SCAN_BYTES {
        return crate::fail_fmt!("used {} < {}", used, EXIT_SCAN_BYTES);
    }
    if used > stack_depth::budget(16 * 1024) {
        return crate::fail_fmt!("used {} over budget", used);
    }
    Outcome::Ok
}
