//! Run queue and timeout ordering. ROADMAP §3.3–§3.4, DESIGN §6.5, §7.8.
//!
//! Portable: FIFO ready queue, one sorted timeout list.
//! Wait queues are per-object (`wait::WaitQueue`); this file does not
//! keep a global blocked list. A timing wheel can replace `TimeoutQueue`
//! without changing callers. Kernel `schedule` / idle live in the binary crate.

use crate::thread::{MAX_THREADS, ThreadId, ThreadState};
use crate::time::Instant;

/// Local timer ticks per slice. DESIGN §6.1. PIT is ~1 kHz, so ~10 ms.
pub const QUANTUM_TICKS: u64 = 10;

/// No deadline given: still a deadline, so nothing blocks forever.
pub const FAR_DEADLINE: Instant = Instant { ns: u64::MAX };

/// Log if still blocked this far past the deadline. 5 s.
pub const OVERDUE_NS: u64 = 5_000_000_000;

/// How often `schedule` scans for overdue waiters, in ticks.
pub const SWEEP_TICKS: u64 = 1_000;

pub fn effective_deadline(deadline: Option<Instant>) -> Instant {
    deadline.unwrap_or(FAR_DEADLINE)
}

/// FIFO round-robin of ready `ThreadId`s. Idle stays off this list.
#[derive(Clone, Copy)]
pub struct ReadyQueue {
    buf: [ThreadId; MAX_THREADS],
    head: usize,
    len: usize,
}

impl ReadyQueue {
    pub const fn empty() -> Self {
        Self {
            buf: [ThreadId::NONE; MAX_THREADS],
            head: 0,
            len: 0,
        }
    }

    pub fn len(self) -> usize {
        self.len
    }

    pub fn is_empty(self) -> bool {
        self.len == 0
    }

    pub fn iter(self) -> impl Iterator<Item = ThreadId> {
        let mut i = 0usize;
        core::iter::from_fn(move || {
            if i >= self.len {
                return None;
            }
            let id = self.at(i);
            i += 1;
            Some(id)
        })
    }

    pub fn at(self, i: usize) -> ThreadId {
        assert!(i < self.len, "ready: index");
        self.buf[(self.head + i) % MAX_THREADS]
    }

    pub fn front(self) -> Option<ThreadId> {
        if self.len == 0 {
            None
        } else {
            Some(self.buf[self.head])
        }
    }

    pub fn contains(self, id: ThreadId) -> bool {
        let mut i = 0;
        while i < self.len {
            if self.at(i) == id {
                return true;
            }
            i += 1;
        }
        false
    }

    pub fn push_back(&mut self, id: ThreadId) {
        assert!(!id.is_none(), "ready: push NONE");
        if self.contains(id) {
            return;
        }
        assert!(self.len < MAX_THREADS, "ready: full");
        let i = (self.head + self.len) % MAX_THREADS;
        self.buf[i] = id;
        self.len += 1;
    }

    pub fn pop_front(&mut self) -> Option<ThreadId> {
        if self.len == 0 {
            return None;
        }
        let id = self.buf[self.head];
        self.buf[self.head] = ThreadId::NONE;
        self.head = (self.head + 1) % MAX_THREADS;
        self.len -= 1;
        Some(id)
    }

    pub fn remove(&mut self, id: ThreadId) -> bool {
        let mut i = 0;
        while i < self.len {
            if self.at(i) == id {
                break;
            }
            i += 1;
        }
        if i == self.len {
            return false;
        }
        while i + 1 < self.len {
            let nxt = self.at(i + 1);
            let slot = (self.head + i) % MAX_THREADS;
            self.buf[slot] = nxt;
            i += 1;
        }
        let last = (self.head + self.len - 1) % MAX_THREADS;
        self.buf[last] = ThreadId::NONE;
        self.len -= 1;
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeout {
    pub id: ThreadId,
    pub deadline: Instant,
}

/// Sorted by deadline, then id. Sleep and wait share this list.
#[derive(Clone, Copy)]
pub struct TimeoutQueue {
    items: [Timeout; MAX_THREADS],
    len: usize,
}

impl TimeoutQueue {
    pub const fn empty() -> Self {
        Self {
            items: [Timeout {
                id: ThreadId::NONE,
                deadline: FAR_DEADLINE,
            }; MAX_THREADS],
            len: 0,
        }
    }

    pub fn len(self) -> usize {
        self.len
    }

    pub fn is_empty(self) -> bool {
        self.len == 0
    }

    pub fn next_deadline(self) -> Option<Instant> {
        if self.len == 0 {
            None
        } else {
            Some(self.items[0].deadline)
        }
    }

    pub fn get(self, i: usize) -> Timeout {
        assert!(i < self.len, "timeout: index");
        self.items[i]
    }

    pub fn insert(&mut self, id: ThreadId, deadline: Instant) {
        assert!(!id.is_none(), "timeout: insert NONE");
        self.remove(id);
        assert!(self.len < MAX_THREADS, "timeout: full");
        let mut i = 0;
        while i < self.len {
            let t = self.items[i];
            if deadline.ns < t.deadline.ns || (deadline.ns == t.deadline.ns && id.0 < t.id.0) {
                break;
            }
            i += 1;
        }
        let mut j = self.len;
        while j > i {
            self.items[j] = self.items[j - 1];
            j -= 1;
        }
        self.items[i] = Timeout { id, deadline };
        self.len += 1;
    }

    pub fn remove(&mut self, id: ThreadId) -> bool {
        let mut i = 0;
        while i < self.len {
            if self.items[i].id == id {
                break;
            }
            i += 1;
        }
        if i == self.len {
            return false;
        }
        while i + 1 < self.len {
            self.items[i] = self.items[i + 1];
            i += 1;
        }
        self.len -= 1;
        self.items[self.len] = Timeout {
            id: ThreadId::NONE,
            deadline: FAR_DEADLINE,
        };
        true
    }

    pub fn pop_expired(&mut self, now: Instant) -> Option<ThreadId> {
        if self.len == 0 || self.items[0].deadline.ns > now.ns {
            return None;
        }
        let id = self.items[0].id;
        let _ = self.remove(id);
        Some(id)
    }

    pub fn pop_expired_into(&mut self, now: Instant, out: &mut [ThreadId]) -> usize {
        let mut n = 0;
        while n < out.len() {
            match self.pop_expired(now) {
                Some(id) => {
                    out[n] = id;
                    n += 1;
                }
                None => break,
            }
        }
        n
    }

    /// Ids whose deadline is at least `OVERDUE_NS` behind `now`.
    pub fn overdue(self, now: Instant) -> impl Iterator<Item = Timeout> {
        let mut i = 0usize;
        core::iter::from_fn(move || {
            while i < self.len {
                let t = self.items[i];
                i += 1;
                if now.ns.saturating_sub(t.deadline.ns) >= OVERDUE_NS {
                    return Some(t);
                }
            }
            None
        })
    }
}

/// Enqueue current if it still wants the CPU. Idle is never on the FIFO.
pub fn enqueue_runnable(ready: &mut ReadyQueue, id: ThreadId, idle: ThreadId, state: ThreadState) {
    if id == idle || id.is_none() {
        return;
    }
    match state {
        ThreadState::Running | ThreadState::Ready => ready.push_back(id),
        ThreadState::Sleeping { .. } | ThreadState::Blocked { .. } | ThreadState::Dead => {}
    }
}

pub fn take_next(ready: &mut ReadyQueue, idle: ThreadId) -> ThreadId {
    ready.pop_front().unwrap_or(idle)
}

/// Move expired timeouts onto the ready FIFO. `on_wake` unlinks wait queues
/// and sets TCB state.
pub fn wake_expired(
    timeouts: &mut TimeoutQueue,
    ready: &mut ReadyQueue,
    now: Instant,
    mut on_wake: impl FnMut(ThreadId),
) -> usize {
    let mut n = 0usize;
    while let Some(id) = timeouts.pop_expired(now) {
        on_wake(id);
        ready.push_back(id);
        n += 1;
    }
    n
}

pub fn should_preempt(ticks: u64, current_is_idle: bool) -> bool {
    current_is_idle || ticks % QUANTUM_TICKS == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tid(n: u32) -> ThreadId {
        ThreadId(n)
    }

    fn at(ns: u64) -> Instant {
        Instant { ns }
    }

    #[test]
    fn ready_fifo_round_robin() {
        let mut q = ReadyQueue::empty();
        assert!(q.is_empty());
        q.push_back(tid(1));
        q.push_back(tid(2));
        q.push_back(tid(3));
        assert_eq!(q.len(), 3);
        assert_eq!(q.front(), Some(tid(1)));
        assert_eq!(q.pop_front(), Some(tid(1)));
        q.push_back(tid(1));
        assert_eq!(q.pop_front(), Some(tid(2)));
        assert_eq!(q.pop_front(), Some(tid(3)));
        assert_eq!(q.pop_front(), Some(tid(1)));
        assert_eq!(q.pop_front(), None);
        q.push_back(tid(4));
        q.push_back(tid(4));
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn ready_remove_middle() {
        let mut q = ReadyQueue::empty();
        q.push_back(tid(1));
        q.push_back(tid(2));
        q.push_back(tid(3));
        assert!(q.remove(tid(2)));
        assert!(!q.remove(tid(2)));
        assert_eq!(q.pop_front(), Some(tid(1)));
        assert_eq!(q.pop_front(), Some(tid(3)));
        assert!(q.is_empty());
    }

    #[test]
    fn timeout_orders_by_deadline() {
        let mut t = TimeoutQueue::empty();
        t.insert(tid(3), at(30));
        t.insert(tid(1), at(10));
        t.insert(tid(2), at(20));
        assert_eq!(t.next_deadline(), Some(at(10)));
        assert_eq!(t.pop_expired(at(15)), Some(tid(1)));
        assert_eq!(t.pop_expired(at(15)), None);
        assert_eq!(t.pop_expired(at(20)), Some(tid(2)));
        assert_eq!(t.next_deadline(), Some(at(30)));
        assert_eq!(t.pop_expired(at(30)), Some(tid(3)));
        assert!(t.is_empty());
    }

    #[test]
    fn timeout_equal_deadlines_by_id() {
        let mut t = TimeoutQueue::empty();
        t.insert(tid(5), at(10));
        t.insert(tid(2), at(10));
        t.insert(tid(9), at(10));
        assert_eq!(t.pop_expired(at(10)), Some(tid(2)));
        assert_eq!(t.pop_expired(at(10)), Some(tid(5)));
        assert_eq!(t.pop_expired(at(10)), Some(tid(9)));
    }

    #[test]
    fn timeout_rearm_moves_entry() {
        let mut t = TimeoutQueue::empty();
        t.insert(tid(1), at(50));
        t.insert(tid(1), at(10));
        assert_eq!(t.len(), 1);
        assert_eq!(t.next_deadline(), Some(at(10)));
        assert!(t.remove(tid(1)));
        assert!(t.is_empty());
    }

    #[test]
    fn timeout_far_future_stays() {
        let mut t = TimeoutQueue::empty();
        t.insert(tid(1), FAR_DEADLINE);
        t.insert(tid(2), at(1));
        assert_eq!(t.pop_expired(at(1_000_000)), Some(tid(2)));
        assert_eq!(t.pop_expired(at(1_000_000)), None);
        assert_eq!(t.next_deadline(), Some(FAR_DEADLINE));
    }

    #[test]
    fn overdue_scan() {
        let mut t = TimeoutQueue::empty();
        t.insert(tid(1), at(10));
        t.insert(tid(2), at(10 + OVERDUE_NS));
        let v: Vec<_> = t.overdue(at(10 + OVERDUE_NS)).collect();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id, tid(1));
    }

    #[test]
    fn state_machine_yield_and_idle() {
        let idle = tid(0);
        let a = tid(1);
        let b = tid(2);
        let mut ready = ReadyQueue::empty();
        enqueue_runnable(&mut ready, a, idle, ThreadState::Running);
        enqueue_runnable(&mut ready, idle, idle, ThreadState::Running);
        ready.push_back(b);
        let next = take_next(&mut ready, idle);
        assert_eq!(next, a);
        let next = take_next(&mut ready, idle);
        assert_eq!(next, b);
        enqueue_runnable(&mut ready, b, idle, ThreadState::Dead);
        assert_eq!(take_next(&mut ready, idle), idle);
    }

    #[test]
    fn state_machine_sleep_timeout() {
        let idle = tid(0);
        let a = tid(1);
        let mut ready = ReadyQueue::empty();
        let mut timeouts = TimeoutQueue::empty();
        enqueue_runnable(
            &mut ready,
            a,
            idle,
            ThreadState::Sleeping { deadline: at(50) },
        );
        assert!(ready.is_empty());
        timeouts.insert(a, at(50));
        let mut woke = 0u32;
        let n = wake_expired(&mut timeouts, &mut ready, at(50), |_| {
            woke += 1;
        });
        assert_eq!(n, 1);
        assert_eq!(woke, 1);
        assert_eq!(take_next(&mut ready, idle), a);
        assert_eq!(take_next(&mut ready, idle), idle);
    }

    #[test]
    fn blocked_timeout_uses_same_queue() {
        let idle = tid(0);
        let a = tid(1);
        let mut ready = ReadyQueue::empty();
        let mut timeouts = TimeoutQueue::empty();
        let mut wq = crate::wait::WaitQueue::new();
        wq.enqueue(a);
        timeouts.insert(a, effective_deadline(None));
        let n = wake_expired(&mut timeouts, &mut ready, at(1), |_| {});
        assert_eq!(n, 0);
        assert!(wq.contains(a));
        timeouts.remove(a);
        timeouts.insert(a, at(5));
        let n = wake_expired(&mut timeouts, &mut ready, at(5), |id| {
            wq.remove(id);
        });
        assert_eq!(n, 1);
        assert!(!wq.contains(a));
        assert_eq!(take_next(&mut ready, idle), a);
    }

    #[test]
    fn preempt_quantum_and_idle() {
        assert!(!should_preempt(1, false));
        assert!(should_preempt(10, false));
        assert!(should_preempt(20, false));
        assert!(should_preempt(1, true));
        assert_eq!(QUANTUM_TICKS, 10);
        assert_eq!(effective_deadline(None), FAR_DEADLINE);
        assert_eq!(effective_deadline(Some(at(3))).ns, 3);
    }
}
