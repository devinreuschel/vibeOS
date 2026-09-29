//! Workqueue threads and softirq-equivalent. ROADMAP §6.6.
//!
//! Hard IRQ / MSI: enqueue only. Workers may allocate and block.
//! High-prio ring is the softirq stand-in.

use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::ipi::MAX_IPI_CPUS;
use vibeos::lock::RANK_SCHED;
use vibeos::sched::FAR_DEADLINE;
use vibeos::wait::WaitQueue;
use vibeos::work::{WorkItem, WorkQueues};

use crate::irq_init;
use crate::kva_init;
use crate::per_cpu_init;
use crate::sync_init::SpinMutex;
use crate::thread_init;

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

/// Process context. May fail if the ring is full. No production caller
/// yet; the in-guest tests use it.
#[cfg(feature = "kernel_tests")]
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

enum Next {
    /// This CPU's dead list, taken whole.
    DeadStacks(u64),
    Item(WorkItem),
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
                if let Some(w) = st.q.pop() {
                    return Next::Item(w);
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
            Next::Item(w) => w.run(),
            Next::Wait => thread_init::schedule(),
        }
    }
}

/// One worker per online CPU, then the threaded-IRQ bottom half.
pub fn init() {
    thread_init::set_kick_hook(kick_dead_stacks);
    let n = per_cpu_init::cpu_count().max(1);
    let mut cpu = 0u32;
    while cpu < n as u32 {
        if per_cpu_init::is_online(cpu)
            && let Err(e) = thread_init::spawn_on("wq", worker, cpu)
        {
            crate::klog!(
                vibeos::log::Level::Error,
                "work: cpu{cpu} worker not started: {}",
                e.as_str()
            );
        }
        cpu += 1;
    }
    irq_init::start_threaded();
    LIVE.store(true, Ordering::Release);
    crate::marker!("vibeOS: work: ready");
}
