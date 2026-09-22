//! Workqueue threads and softirq-equivalent. ROADMAP §6.6.
//!
//! Hard IRQ / MSI: enqueue only. Workers may allocate and block.
//! High-prio ring is the softirq stand-in.

use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::sched::FAR_DEADLINE;
use vibeos::wait::WaitQueue;
use vibeos::work::{WorkItem, WorkQueues};

use crate::cell::IrqCell;
use crate::irq_init;
use crate::per_cpu_init;
use crate::thread_init;

struct State {
    q: WorkQueues,
    wq: WaitQueue,
}

static ST: IrqCell<State> = IrqCell::new(State {
    q: WorkQueues::new(),
    wq: WaitQueue::new(),
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
                s.wake_all(&mut st.wq);
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

fn worker() {
    loop {
        let item = thread_init::with_sched(|s| {
            ST.with(|st| {
                if let Some(w) = st.q.pop() {
                    return Some(w);
                }
                s.begin_wait(&mut st.wq, FAR_DEADLINE);
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
        if per_cpu_init::is_online(cpu) {
            let _ = thread_init::spawn_on("wq", worker, cpu);
        }
        cpu += 1;
    }
    irq_init::start_threaded();
    LIVE.store(true, Ordering::Release);
    crate::marker!("vibeOS: work: ready");
}
