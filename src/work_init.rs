//! Workqueue threads and softirq-equivalent. ROADMAP §6.6.
//!
//! Hard IRQ / MSI: enqueue only. Workers may allocate and block.
//! High-prio ring is the softirq stand-in.

use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::ipi::MAX_IPI_CPUS;
use vibeos::sched::FAR_DEADLINE;
use vibeos::wait::WaitQueue;
use vibeos::work::{WorkItem, WorkQueues};

use crate::cell::IrqCell;
use crate::irq_init;
use crate::per_cpu_init;
use crate::thread_init;

struct State {
    q: WorkQueues,
    /// One wait queue per CPU id: each CPU's worker waits on its own, so
    /// [`kick_dead_stacks`] wakes only this CPU's.
    wq: [WaitQueue; MAX_IPI_CPUS],
}

static ST: IrqCell<State> = IrqCell::new(State {
    q: WorkQueues::new(),
    wq: [const { WaitQueue::new() }; MAX_IPI_CPUS],
});
static LIVE: AtomicBool = AtomicBool::new(false);

fn push(hi: bool, item: WorkItem) -> bool {
    thread_init::with_sched(|s| {
        ST.with(|st| {
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

/// Process context. May fail if the ring is full.
#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
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

/// Wake this CPU's worker. IF=0 or IF=1; takes SCHED.
#[allow(
    dead_code,
    reason = "the switch tail's dead-stack list calls it from ROADMAP §10.10"
)]
pub(crate) fn kick_dead_stacks() {
    let me = my_queue();
    thread_init::with_sched(|s| {
        ST.with(|st| {
            s.wake_all(&mut st.wq[me]);
        })
    });
}

fn worker() {
    let me = my_queue();
    loop {
        let item = thread_init::with_sched(|s| {
            ST.with(|st| {
                if let Some(w) = st.q.pop() {
                    return Some(w);
                }
                s.begin_wait(&mut st.wq[me], FAR_DEADLINE);
                None
            })
        });
        match item {
            Some(w) => w.run(),
            None => thread_init::schedule(),
        }
    }
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn live() -> bool {
    LIVE.load(Ordering::Acquire)
}

/// One worker per online CPU, then the threaded-IRQ bottom half.
pub fn init() {
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
