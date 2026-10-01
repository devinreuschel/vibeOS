//! In-guest test of C-REQUEUE-HOOK (`thread_init::requeue_next_cpu`), the
//! move the migration tests use. Row: the list in crate::ktest.

use core::sync::atomic::{AtomicU32, Ordering};

use vibeos::thread::ThreadId;

use super::{RequeueGuard, set_requeue_next_cpu};
use crate::ktest::{Outcome, second_cpu, sleep_until};
use crate::thread_init;

/// The CPU each `requeue_mover_*` first ran on, `u32::MAX` before it
/// runs. A mover starts with IF off (`testing::spawn_parked_any`), so
/// that is where it was dequeued to run: the hook moves an unpinned
/// kernel thread again each time it is preempted.
static MOVER_CPU: [AtomicU32; 2] = [const { AtomicU32::new(u32::MAX) }; 2];

fn requeue_mover_0() {
    MOVER_CPU[0].store(thread_init::current_cpu(), Ordering::Release);
}

fn requeue_mover_1() {
    MOVER_CPU[1].store(thread_init::current_cpu(), Ordering::Release);
}

/// C-REQUEUE-HOOK moves every movable thread its CPU dequeues, not only
/// the first: two unpinned threads queued back to back on this CPU both
/// first run elsewhere. `user_tf_repin` failed `cpus 0 -> 0` when the thread
/// dequeued after a move ran where it was queued.
pub(crate) fn test_requeue_moves_each_dequeue() -> Outcome {
    if second_cpu().is_none() {
        return Outcome::Skip("needs 2 CPUs");
    }
    let me = thread_init::current_cpu();
    for c in MOVER_CPU.iter() {
        c.store(u32::MAX, Ordering::Release);
    }
    let mut ids = [ThreadId::NONE; 2];
    for (id, entry) in ids
        .iter_mut()
        .zip([requeue_mover_0 as fn(), requeue_mover_1])
    {
        match thread_init::testing::spawn_parked_any("requeue-mover", entry) {
            Ok(h) => *id = h.id(),
            Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
        }
    }
    {
        // IF off from the first queueing until the hook is on: no dequeue
        // here runs either thread before then.
        let _irq = crate::arch::current::InterruptGuard::enter();
        for id in ids {
            thread_init::testing::queue_here(id);
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
