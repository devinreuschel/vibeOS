//! In-guest tests of P10-S02, In-guest registry in production interrupt context (DESIGN §8.2).

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use vibeos::kva::DEFAULT_STACK_PAGES;
use vibeos::paging::PAGE_SIZE_4K;

use super::{CPU_HITS, IRQ_CPU, IRQ_HITS, OBS_STALL_NS, Outcome, Test, test};
use crate::paging_init;
use crate::per_cpu_init;
use crate::thread_init;
use crate::time_init;
use crate::x86;

pub(super) const TESTS: &[Test] = &[
    test("ktest_context", ktest_context),
    test("msix_cpu_publish_last", msix_cpu_publish_last),
];

/// The registry's stack size ROADMAP §10.2 names.
const REGISTRY_STACK_BYTES: u64 = 64 * 1024;
/// How long [`ktest_context`] yields for its worker.
const WORKER_WAIT_NS: u64 = 1_000_000_000;

const CTX_IF_OFF: u32 = 1 << 0;
const CTX_NEST: u32 = 1 << 1;
const CTX_STACK: u32 = 1 << 2;

static CTX_RESULT: AtomicU32 = AtomicU32::new(0);
static CTX_DONE: AtomicBool = AtomicBool::new(false);

/// The calling thread's context as a [`CTX_IF_OFF`] / [`CTX_NEST`] /
/// [`CTX_STACK`] mask: IF on, `irq_nest` 0, and RSP inside a stack from
/// `alloc_guarded_stack` of `stack_bytes`, whose guard page is unmapped.
fn context_bits(stack_bytes: u64) -> u32 {
    let mut bits = 0;
    if !x86::interrupts_enabled() {
        bits |= CTX_IF_OFF;
    }
    if per_cpu_init::irq_nest() != 0 {
        bits |= CTX_NEST;
    }
    // SAFETY: a running thread's `Tcb` stays in the TCB table
    // (`thread_init::SCHED`) until it exits, and only
    // `thread_init::spawn_inner` and `thread_init::thread_exit` write its
    // `stack`, neither of which runs for this thread while it runs here.
    let stack = unsafe { &(*thread_init::current_tcb()).stack };
    let guarded = match stack {
        Some(ks) => {
            let rsp = x86::read_rsp();
            rsp > ks.guard().as_u64() + PAGE_SIZE_4K
                && rsp <= ks.top().as_u64()
                && paging_init::translate(ks.guard()).is_none()
                && (ks.pages() as u64) * PAGE_SIZE_4K == stack_bytes
        }
        None => false,
    };
    if !guarded {
        bits |= CTX_STACK;
    }
    bits
}

fn ctx_worker() {
    CTX_RESULT.store(
        context_bits(DEFAULT_STACK_PAGES as u64 * PAGE_SIZE_4K),
        Ordering::Relaxed,
    );
    CTX_DONE.store(true, Ordering::Release);
}

/// The registry, and a `spawn_here` worker it starts, run with IF on,
/// `irq_nest` 0, and a guarded KVA stack (ROADMAP §10.2, F075).
fn ktest_context() -> Outcome {
    if !x86::interrupts_enabled() {
        return Outcome::Fail("registry IF off");
    }
    if per_cpu_init::irq_nest() != 0 {
        return Outcome::Fail("registry irq_nest not 0");
    }
    let bits = context_bits(REGISTRY_STACK_BYTES);
    if bits & CTX_STACK != 0 {
        // Tell the two stack failures apart.
        // SAFETY: as in `context_bits`: a running thread's `Tcb` stays in
        // the TCB table, and only `thread_init::spawn_inner` and
        // `thread_init::thread_exit` write its `stack`.
        let stack = unsafe { &(*thread_init::current_tcb()).stack };
        return match stack {
            Some(ks) if (ks.pages() as u64) * PAGE_SIZE_4K != REGISTRY_STACK_BYTES => {
                Outcome::Fail("registry stack not 64 KiB")
            }
            _ => Outcome::Fail("registry stack not guarded"),
        };
    }

    CTX_RESULT.store(0, Ordering::Relaxed);
    CTX_DONE.store(false, Ordering::Relaxed);
    if thread_init::spawn_here("ktest-ctx-w", ctx_worker).is_err() {
        return Outcome::Fail("spawn");
    }
    let t0 = time_init::now_ns();
    while !CTX_DONE.load(Ordering::Acquire) {
        if time_init::now_ns().saturating_sub(t0) > WORKER_WAIT_NS {
            return Outcome::Fail("worker did not run");
        }
        thread_init::yield_now();
    }
    let w = CTX_RESULT.load(Ordering::Relaxed);
    if w & CTX_IF_OFF != 0 {
        return Outcome::Fail("worker IF off");
    }
    if w & CTX_NEST != 0 {
        return Outcome::Fail("worker irq_nest not 0");
    }
    if w & CTX_STACK != 0 {
        return Outcome::Fail("worker stack not guarded");
    }
    Outcome::Ok
}

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
fn msix_cpu_publish_last() -> Outcome {
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
