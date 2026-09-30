//! The `IoWaiter` completion's loom model (ROADMAP §10.8): `DoneWord`'s
//! publish against `wait`'s lock-free poll and return.

extern crate std;

use super::*;
use crate::sync::variant::{Bound, check};
use loom::cell::UnsafeCell;
use loom::sync::Arc;
use loom::thread;

/// The status the completer publishes.
const STATUS: u32 = 5;

/// `block_init::IoWaiter` as the model sees it: the done word and, for
/// its wait queue, a witness both sides write. The waiter lives on the
/// submitter's stack, so once `wait` returns the frame is reused.
struct Frame {
    done: DoneWord,
    queue: UnsafeCell<u32>,
}

// SAFETY: `queue` is written by the completer before it publishes
// `done`, and by the waiter only after its poll sees the status; the
// models check, through loom, that `done` orders the two. Established
// here.
unsafe impl Sync for Frame {}

/// The completer (`block_init::IoWaiter::finish`) wakes the queue, a
/// write standing for `wake_all` under SCHED, then publishes the
/// status as its last access. Main (`IoWaiter::wait`) polls, yielding
/// between polls, until the status shows, then writes the frame as its
/// reuse after `wait` returns. The poll returns the published status.
/// Bound: 2 threads (the completer 1 wake and 1 publish, main polls
/// until done and 1 reuse), 3 preemptions.
fn io_done_model(v: Option<Site>) {
    let bound = Bound {
        threads: 2,
        preemptions: 3,
    };
    check(v, bound, || {
        let f = Arc::new(Frame {
            done: DoneWord::new(),
            queue: UnsafeCell::new(0),
        });
        let completer = {
            let f = f.clone();
            thread::spawn(move || {
                // SAFETY: the waiter touches `queue` only after it sees
                // the status, which is published below as the last
                // access; established by `block::DoneWord::publish`.
                f.queue.with_mut(|p| unsafe { *p = 1 });
                f.done.publish(STATUS);
            })
        };
        let status = loop {
            if let Some(s) = f.done.poll() {
                break s;
            }
            thread::yield_now();
        };
        assert_eq!(status, STATUS);
        // SAFETY: the status is visible, so the completer has made its
        // last access; established by `block::DoneWord::publish`.
        f.queue.with_mut(|p| unsafe { *p = 2 });
        completer.join().unwrap();
    });
}

#[test]
fn loom_io_done_publish_last() {
    io_done_model(None);
}

#[test]
#[should_panic(expected = "Causality violation")]
fn loom_io_done_relaxed_races_fails() {
    io_done_model(Some(Site::IoDoneRelaxed));
}
