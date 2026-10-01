//! Hooks sched's in-guest tests arm, re-exported from `sched::ktest`
//! (kernel_tests only). Their state stays in the production files.

use core::sync::atomic::Ordering;

use crate::sched::thread_init::testing::{ARRIVED, REQUEUE, REQUEUES};
use crate::work_init;

/// C-REQUEUE-HOOK: move each user or `CpuAffinity::Any` thread to the
/// next online CPU when its CPU dequeues it (`thread_init::requeue_next_cpu`).
/// Turning it on forgets the arrivals an earlier use left: a thread
/// moved just before the hook went off keeps its flag, and a later
/// thread in that slot would run where it is dequeued instead of moving.
pub(crate) fn set_requeue_next_cpu(on: bool) {
    if on {
        for a in ARRIVED.try_get().map_or(&[][..], |v| &v[..]) {
            a.store(false, Ordering::Relaxed);
        }
    }
    REQUEUE.store(on, Ordering::Release);
}

/// Moves the requeue hook has made since boot.
pub(crate) fn requeues() -> u64 {
    REQUEUES.load(Ordering::Relaxed)
}

/// Whether the workqueue workers have started.
pub(crate) fn work_live() -> bool {
    work_init::LIVE.load(Ordering::Acquire)
}

/// Turns the requeue hook off when dropped.
pub(crate) struct RequeueGuard;

impl Drop for RequeueGuard {
    fn drop(&mut self) {
        set_requeue_next_cpu(false);
    }
}
