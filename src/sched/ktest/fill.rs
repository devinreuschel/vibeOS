//! A thread table filled with parked kernel threads, for the tests of a
//! full table (ROADMAP §10.4, F037).

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::ktest::spin_until_ns;
use crate::sync::blocking_init::Semaphore;
use crate::thread_init::{self, SpawnError};

/// The one test-owned wait queue the fillers park on: a semaphore at 0,
/// which [`Fill::release`] raises once per filler.
static PARK: Semaphore = Semaphore::new(0);
/// Fillers spawned and not yet past [`PARK`].
static PARKED: AtomicUsize = AtomicUsize::new(0);

/// How long [`Fill::release`] waits for the fillers' slots to come back.
const RELEASE_NS: u64 = 20_000_000_000;

fn filler() {
    PARK.acquire();
    PARKED.fetch_sub(1, Ordering::AcqRel);
}

/// What [`fill_threads`] did. Dropping it without [`Fill::release`] leaves
/// the fillers parked, so every test releases it.
#[must_use]
pub(crate) struct Fill {
    /// Fillers spawned.
    pub(crate) spawned: usize,
    /// The error the spawn after the last filler got; `None` when the fill
    /// stopped with slots to spare.
    pub(crate) last: Option<SpawnError>,
    /// The table's use before the fill, which `release` waits to see again.
    base: usize,
}

/// Spawn kernel threads parked on [`PARK`] until a spawn fails or, for a
/// `leave` above 0, only `leave` slots remain free by
/// `thread_init::table_usage`, which counts a slot free under
/// `spawn_inner`'s own reuse test.
pub(crate) fn fill_threads(leave: usize) -> Fill {
    let (base, _) = thread_init::table_usage();
    let mut spawned = 0usize;
    let mut last = None;
    loop {
        // With `leave` 0 the spawn that finds the table full ends the fill,
        // so `last` holds its error.
        let (used, cap) = thread_init::table_usage();
        if leave > 0 && cap.saturating_sub(used) <= leave {
            break;
        }
        PARKED.fetch_add(1, Ordering::AcqRel);
        match thread_init::spawn("fill", filler) {
            Ok(_) => spawned += 1,
            Err(e) => {
                PARKED.fetch_sub(1, Ordering::AcqRel);
                last = Some(e);
                break;
            }
        }
    }
    Fill {
        spawned,
        last,
        base,
    }
}

impl Fill {
    /// Wake every filler and wait until the table's use is back at its
    /// value before the fill. False if it did not come back in time.
    pub(crate) fn release(self) -> bool {
        let mut i = 0usize;
        while i < self.spawned {
            PARK.release();
            i += 1;
        }
        let base = self.base;
        spin_until_ns(
            || PARKED.load(Ordering::Acquire) == 0 && thread_init::table_usage().0 <= base,
            RELEASE_NS,
        )
    }
}
