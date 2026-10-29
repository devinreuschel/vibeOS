//! Hooks sched's in-guest tests arm, re-exported from `sched::ktest`
//! (kernel_tests only). Their state stays in the production files.

use core::sync::atomic::Ordering;

use crate::sched::thread_init::testing::REQUEUES;
use crate::work_init;

pub(crate) use crate::sched::thread_init::testing::{RequeueGuard, set_requeue_next_cpu};

/// Moves the requeue hook has made since boot.
pub(crate) fn requeues() -> u64 {
    REQUEUES.load(Ordering::Relaxed)
}

/// Whether the workqueue workers have started.
pub(crate) fn work_live() -> bool {
    work_init::LIVE.load(Ordering::Acquire)
}
