//! Workqueue threads and softirq-equivalent. ROADMAP §6.6.
//!
//! Hard IRQ / MSI: enqueue only. Workers may allocate and block.
//! High-prio ring is the softirq stand-in.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use vibeos::acpi::MAX_CPUS;
use vibeos::ipi::MAX_IPI_CPUS;
use vibeos::kalloc::{DeferList, Deferred};
use vibeos::lock::RANK_SCHED;
use vibeos::sched::FAR_DEADLINE;
use vibeos::thread::ThreadId;
use vibeos::wait::WaitQueue;
use vibeos::work::{WorkClass, WorkItem, WorkQueues};

use crate::irq_init;
use crate::kva_init;
use crate::per_cpu_init;
use crate::sync_init::{self, SpinMutex};
use crate::thread_init::{self, SpawnError};

struct State {
    q: WorkQueues,
    /// One wait queue per CPU id: each CPU's worker waits on its own, so
    /// [`kick_dead_stacks`] wakes only this CPU's.
    wq: [WaitQueue; MAX_IPI_CPUS],
}

static ST: SpinMutex<State> = SpinMutex::with_rank(
    State {
        q: WorkQueues::new(),
        wq: [const { WaitQueue::new() }; MAX_IPI_CPUS],
    },
    RANK_SCHED,
);

/// Run `f` on the work queues. Callers hold SCHED, which serializes their wait queues.
fn with_st<R>(f: impl FnOnce(&mut State) -> R) -> R {
    // pair order: thread_init::SCHED, then ST
    let mut g = ST.lock_nested(1);
    f(&mut g)
}
/// Set once `init` has started the workers; `sched::ktest::work_live`
/// reads it.
pub(super) static LIVE: AtomicBool = AtomicBool::new(false);

fn push(hi: bool, item: WorkItem) -> bool {
    thread_init::with_sched(|s| {
        with_st(|st| {
            let ok = if hi {
                st.q.push_hi(item)
            } else {
                st.q.push(item)
            };
            if ok {
                for wq in st.wq.iter_mut() {
                    s.wake_all(wq);
                }
            }
            ok
        })
    })
}

/// Queue `func(arg)` on the normal ring. May fail if the ring is full.
/// Takes SCHED, so only where `sync_init::may_take_sched` allows.
pub fn enqueue(func: fn(usize), arg: usize) -> bool {
    push(false, WorkItem::new(func, arg))
}

/// Softirq-equivalent. Safe from hard IRQ: no alloc, no block.
pub fn raise_softirq(func: fn(usize), arg: usize) -> bool {
    push(true, WorkItem::new(func, arg))
}

/// This CPU's worker wait queue index. A worker is pinned to its CPU.
fn my_queue() -> usize {
    (thread_init::current_cpu() as usize).min(MAX_IPI_CPUS - 1)
}

/// Wake this CPU's worker to free its dead stacks. IF=0 or IF=1; takes
/// SCHED, so never under it.
pub(crate) fn kick_dead_stacks() {
    let me = my_queue();
    thread_init::with_sched(|s| {
        with_st(|st| {
            s.wake_all(&mut st.wq[me]);
        })
    });
}

/// Each CPU's deferred-release list (DESIGN §2.11 rule 6): counted objects
/// whose last put came where they may not be released in place. A push is
/// safe from any CPU; one release item at a time drains each list.
static RELEASE: [DeferList; MAX_CPUS] = [const { DeferList::new() }; MAX_CPUS];

/// Release items the full ring refused, since boot. A statistic, so Relaxed.
static RELEASE_RING_FULL: AtomicU64 = AtomicU64::new(0);

/// This CPU's list and its index: CPU 0's before per-CPU data is live, and
/// for an id past the table.
fn release_list_of(cpu: usize) -> (usize, &'static DeferList) {
    match RELEASE.get(cpu) {
        Some(l) => (cpu, l),
        None => (0, &RELEASE[0]),
    }
}

fn this_release_list() -> (usize, &'static DeferList) {
    release_list_of(per_cpu_init::try_current().map_or(0, |c| c.cpu_id as usize))
}

/// The deferral sink `kalloc::set_deferral` installs: link `d` onto this
/// CPU's list, allocating nothing, and queue the list's release item if
/// none is queued. Any context.
pub(crate) fn defer_release(d: Deferred) {
    let (cpu, list) = this_release_list();
    if list.push(d) {
        queue_release(cpu, list);
    }
}

/// Queue the release item for `list`, whose duty the caller holds. Where
/// this CPU may not take SCHED, or the ring is full, give the duty back:
/// the next timer tick on this CPU claims it again (`kick_deferred`).
fn queue_release(cpu: usize, list: &DeferList) {
    if !sync_init::may_take_sched() {
        list.unclaim();
        return;
    }
    if enqueue(release_list, cpu) {
        return;
    }
    list.unclaim();
    let n = RELEASE_RING_FULL.fetch_add(1, Ordering::Relaxed);
    if n.is_multiple_of(1024) {
        crate::klog!(
            vibeos::log::Level::Warn,
            "work: ring full; cpu{cpu}'s deferred releases wait for a tick ({} times)",
            n.saturating_add(1)
        );
    }
}

/// Timer tick: queue this CPU's release item if its list holds objects
/// and none is queued, as when a put came under SCHED or a lock ranked
/// after it.
pub(crate) fn kick_deferred() {
    let (cpu, list) = this_release_list();
    if list.claim() {
        queue_release(cpu, list);
    }
}

/// The release item: release every object on `cpu`'s list, on a worker
/// with IF=1 that runs no softirq-equivalent item.
fn release_list(cpu: usize) {
    debug_assert!(sync_init::may_release_here());
    let (_, list) = release_list_of(cpu);
    list.release_all();
}

enum Next {
    /// This CPU's dead list, taken whole.
    DeadStacks(u64),
    Item(WorkItem, WorkClass),
    Wait,
}

/// One per CPU, pinned. First this CPU's dead stacks, freed with IF=1,
/// then the shared ring. The switch tail that parks a stack runs on this
/// CPU with IF=0 and wakes this queue, so the check under SCHED loses no
/// wake-up.
fn worker() {
    let me = my_queue();
    loop {
        let next = thread_init::with_sched(|s| {
            let head = thread_init::take_dead_stacks();
            if head != 0 {
                return Next::DeadStacks(head);
            }
            with_st(|st| {
                if let Some((w, c)) = st.q.pop() {
                    return Next::Item(w, c);
                }
                s.begin_wait(&mut st.wq[me], FAR_DEADLINE);
                Next::Wait
            })
        });
        match next {
            Next::DeadStacks(head) => {
                let n = kva_init::free_parked(head);
                thread_init::stacks_reclaimed(n);
            }
            Next::Item(w, WorkClass::Soft) => {
                // A softirq-equivalent item runs as a no-reclaim thread
                // (DESIGN §2.11 rule 6).
                let _nr = sync_init::no_reclaim();
                w.run();
            }
            Next::Item(w, WorkClass::Normal) => w.run(),
            Next::Wait => thread_init::schedule(),
        }
    }
}

/// A CPU's per-CPU kernel threads, parked until the CPU is online: its
/// workqueue worker.
pub(crate) struct CpuWorkers {
    wq: ThreadId,
}

/// Spawn CPU `cpu`'s workers pinned and parked ([`thread_init::spawn_parked_on`]):
/// AP bring-up makes them before it starts the CPU, so a full thread table
/// leaves the CPU offline instead of online with no worker (ROADMAP §10.4,
/// F037). On failure nothing is left behind.
pub(crate) fn spawn_cpu_workers(cpu: u32) -> Result<CpuWorkers, SpawnError> {
    let wq = thread_init::spawn_parked_on("wq", worker, cpu)?;
    Ok(CpuWorkers { wq: wq.id() })
}

/// Make `w`'s workers runnable, once their CPU is online.
pub(crate) fn start_cpu_workers(w: &CpuWorkers) {
    thread_init::make_ready(w.wq);
}

/// Retire workers [`spawn_cpu_workers`] made for a CPU that did not come
/// up. None of them ever ran, so each slot is free again.
pub(crate) fn abandon_cpu_workers(w: CpuWorkers) {
    if let Some(stack) = thread_init::abandon_unstarted(w.wq, false) {
        thread_init::return_stack(stack);
    }
}

/// CPU 0's workers, then the threaded-IRQ bottom half. Each AP's workers
/// come up with it (`smp_init::alloc_ap_resources`).
pub fn init() {
    thread_init::set_kick_hook(kick_dead_stacks);
    match spawn_cpu_workers(0) {
        Ok(w) => start_cpu_workers(&w),
        Err(e) => crate::klog!(
            vibeos::log::Level::Error,
            "work: cpu0 worker not started: {}",
            e.as_str()
        ),
    }
    irq_init::start_threaded();
    LIVE.store(true, Ordering::Release);
    crate::marker!("vibeOS: work: ready");
}

/// Test access to the deferred-release lists (kernel_tests only).
#[cfg(feature = "kernel_tests")]
pub(crate) mod testing {
    /// Whether `cpu`'s deferred-release list holds no object.
    pub(crate) fn release_idle(cpu: usize) -> bool {
        super::release_list_of(cpu).1.is_empty()
    }
}
