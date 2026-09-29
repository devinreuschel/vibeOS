//! In-guest test of C-REQUEUE-HOOK (`thread_init::requeue_next_cpu`), the
//! move the migration tests use. Row: the list in crate::ktest.

use core::sync::atomic::{AtomicU32, Ordering};

use vibeos::kva::DEFAULT_STACK_PAGES;

use super::{RequeueGuard, set_requeue_next_cpu};
use crate::ktest::{Outcome, second_cpu, sleep_until};
use crate::thread_init;
use crate::x86;

/// The CPU each `requeue_mover_*` ran on, `u32::MAX` before it runs.
static MOVER_CPU: [AtomicU32; 2] = [const { AtomicU32::new(u32::MAX) }; 2];

fn requeue_mover_0() {
    MOVER_CPU[0].store(thread_init::current_cpu(), Ordering::Release);
}

fn requeue_mover_1() {
    MOVER_CPU[1].store(thread_init::current_cpu(), Ordering::Release);
}

/// C-REQUEUE-HOOK moves every movable thread its CPU dequeues, not only
/// the first: two unpinned threads queued back to back on this CPU both
/// run elsewhere. `user_tf_repin` failed `cpus 0 -> 0` when the thread
/// dequeued after a move ran where it was queued.
pub(crate) fn test_requeue_moves_each_dequeue() -> Outcome {
    if second_cpu().is_none() {
        return Outcome::Skip("needs 2 CPUs");
    }
    let me = thread_init::current_cpu();
    for c in MOVER_CPU.iter() {
        c.store(u32::MAX, Ordering::Release);
    }
    {
        // IF off until the hook is on: no dequeue here runs either thread
        // before then.
        let _irq = x86::InterruptGuard::enter();
        let opts = thread_init::SpawnOpts {
            stack_pages: DEFAULT_STACK_PAGES,
            cpu: Some(me),
        };
        for entry in [requeue_mover_0 as fn(), requeue_mover_1] {
            match thread_init::spawn_opts("requeue-mover", entry, opts) {
                Ok(h) => thread_init::testing::unpin(h.id()),
                Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
            }
        }
        set_requeue_next_cpu(true);
    }
    let ran = {
        let _g = RequeueGuard;
        sleep_until(
            || {
                MOVER_CPU
                    .iter()
                    .all(|c| c.load(Ordering::Acquire) != u32::MAX)
            },
            5_000,
        )
    };
    let cpus = MOVER_CPU.each_ref().map(|c| c.load(Ordering::Acquire));
    if !ran || cpus.contains(&me) {
        return crate::fail_fmt!(
            "movers ran on cpus {} and {}; want two CPUs other than {me}",
            cpus[0] as i64,
            cpus[1] as i64
        );
    }
    Outcome::Ok
}
