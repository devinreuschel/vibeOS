//! In-guest tests of P10-S02, In-guest registry in production interrupt context (DESIGN §8.2).

use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::kva::DEFAULT_STACK_PAGES;

use super::{CPU_HITS, IRQ_CPU, IRQ_HITS, OBS_STALL_NS, Outcome};
use crate::thread_init;
use crate::time_init;

/// How long `msix_cpu_publish_last` stalls `record_irq_cpu`.
const OBS_STALL_FOR_NS: u64 = 20_000_000;
/// Bound on each of `msix_cpu_publish_last`'s waits.
const OBS_WAIT_NS: u64 = 2_000_000_000;

static OBS_DONE: AtomicBool = AtomicBool::new(false);

fn obs_publisher() {
    super::record_irq_cpu();
    OBS_DONE.store(true, Ordering::Release);
}

/// `record_irq_cpu` publishes `IRQ_HITS` last: once a waiter sees a hit, the
/// hitting CPU's `CPU_HITS` count is already there, even when the observer
/// stalls before its last store (ROADMAP §10.2, F021).
pub(super) fn msix_cpu_publish_last() -> Outcome {
    let Some(ap) = super::second_cpu() else {
        return Outcome::Skip("no AP");
    };
    if ap as usize >= CPU_HITS.len() {
        return Outcome::Fail("ap index");
    }
    OBS_STALL_NS.store(
        time_init::now_ns().saturating_add(OBS_STALL_FOR_NS),
        Ordering::Release,
    );
    super::reset_irq_obs();
    OBS_DONE.store(false, Ordering::Relaxed);
    if thread_init::spawn_opts(
        "obs-pub",
        obs_publisher,
        thread_init::SpawnOpts {
            stack_pages: DEFAULT_STACK_PAGES,
            cpu: Some(ap),
        },
    )
    .is_err()
    {
        OBS_STALL_NS.store(0, Ordering::Release);
        return Outcome::Fail("spawn");
    }
    let hit = super::spin_until_ns(|| IRQ_HITS.load(Ordering::SeqCst) != 0, OBS_WAIT_NS);
    let ap_hits = CPU_HITS[ap as usize].load(Ordering::SeqCst);
    let cpu = IRQ_CPU.load(Ordering::SeqCst);
    let done = super::spin_until_ns(|| OBS_DONE.load(Ordering::Acquire), OBS_WAIT_NS);
    OBS_STALL_NS.store(0, Ordering::Release);
    if !hit || !done {
        return Outcome::Fail("publisher did not run");
    }
    if cpu != ap {
        return Outcome::Fail("wrong cpu");
    }
    if ap_hits == 0 {
        return Outcome::Fail("ap counter");
    }
    Outcome::Ok
}
