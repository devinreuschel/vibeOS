//! Run queue and timeout ordering. ROADMAP §3.3–§3.4, DESIGN §6.5, §7.8.
//!
//! Portable: FIFO ready queue, one sorted timeout list.
//! Wait queues are per-object (`wait::WaitQueue`); this file does not
//! keep a global blocked list. A timing wheel can replace `TimeoutQueue`
//! without changing callers. Kernel `schedule` / idle live in the binary crate.

pub mod fpu;
pub mod irqoff;
pub mod stack_depth;
pub mod thread;
pub mod wait;
pub mod work;

use crate::kalloc::{AllocError, TryVec};
use crate::limits;
use crate::thread::{ThreadId, ThreadState};
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
///
/// A heap ring of a fixed capacity, `limits::MAX_THREADS` for a CPU's run
/// queue, built once by [`ReadyQueue::try_new`] and never grown (ROADMAP
/// §10.4, D1). [`ReadyQueue::empty`] has no room at all: a `const` value
/// for a static until its table is allocated.
#[repr(C)]
pub struct ReadyQueue {
    /// The address of `buf`'s first id, 0 for none, and its capacity: the
    /// words the core tool reads the ring through (docs/VMCOREINFO.md),
    /// since `buf`'s own layout is `alloc`'s. `buf` never moves once built.
    ids: usize,
    cap: usize,
    head: usize,
    len: usize,
    buf: TryVec<ThreadId>,
}

// The layout the core tool reads (docs/VMCOREINFO.md, "Types the core tool
// reads"): the ring's address and capacity, then `head` and `len`.
#[cfg(not(loom))]
const _: () = {
    use core::mem::{align_of, offset_of};
    assert!(offset_of!(ReadyQueue, ids) == 0);
    assert!(offset_of!(ReadyQueue, cap) == 8);
    assert!(offset_of!(ReadyQueue, head) == 16);
    assert!(offset_of!(ReadyQueue, len) == 24);
    assert!(align_of::<ReadyQueue>() == 8);
};

impl ReadyQueue {
    /// The offsets of `ids`, `cap`, `head` and `len`, which the core tool
    /// (`log::vmcore`) reads a dumped queue through: the layout the
    /// assertions above fix, named here because the fields are private.
    pub const CORE_OFFSETS: [usize; 4] = [
        core::mem::offset_of!(Self, ids),
        core::mem::offset_of!(Self, cap),
        core::mem::offset_of!(Self, head),
        core::mem::offset_of!(Self, len),
    ];

    /// A queue with no room, for a `const` initializer.
    pub const fn empty() -> Self {
        Self {
            ids: 0,
            cap: 0,
            head: 0,
            len: 0,
            buf: TryVec::new(),
        }
    }

    /// A queue with room for `cap` ids.
    pub fn try_new(cap: usize) -> Result<Self, AllocError> {
        let buf = limits::table(cap, || ThreadId::NONE)?;
        Ok(Self {
            ids: buf.as_ptr().addr(),
            cap: buf.len(),
            head: 0,
            len: 0,
            buf,
        })
    }

    /// Ids the queue can hold.
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = ThreadId> + '_ {
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

    /// The ring index of the `i`th id from the front. Only called with a
    /// queue that holds at least one id, so the capacity is not 0.
    fn index(&self, i: usize) -> usize {
        (self.head + i) % self.buf.len()
    }

    pub fn at(&self, i: usize) -> ThreadId {
        assert!(i < self.len, "ready: index");
        self.buf[self.index(i)]
    }

    pub fn front(&self) -> Option<ThreadId> {
        if self.len == 0 {
            None
        } else {
            Some(self.buf[self.head])
        }
    }

    pub fn contains(&self, id: ThreadId) -> bool {
        self.iter().any(|x| x == id)
    }

    pub fn push_back(&mut self, id: ThreadId) {
        assert!(!id.is_none(), "ready: push NONE");
        if self.contains(id) {
            return;
        }
        assert!(self.len < self.buf.len(), "ready: full");
        let i = self.index(self.len);
        self.buf[i] = id;
        self.len += 1;
    }

    pub fn pop_front(&mut self) -> Option<ThreadId> {
        if self.len == 0 {
            return None;
        }
        let id = self.buf[self.head];
        self.buf[self.head] = ThreadId::NONE;
        self.head = self.index(1);
        self.len -= 1;
        Some(id)
    }

    pub fn remove(&mut self, id: ThreadId) -> bool {
        let Some(mut i) = self.iter().position(|x| x == id) else {
            return false;
        };
        while i + 1 < self.len {
            let nxt = self.at(i + 1);
            let slot = self.index(i);
            self.buf[slot] = nxt;
            i += 1;
        }
        let last = self.index(self.len - 1);
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

const NO_TIMEOUT: Timeout = Timeout {
    id: ThreadId::NONE,
    deadline: FAR_DEADLINE,
};

/// Sorted by deadline, then id. Sleep and wait share this list. A heap
/// table of a fixed capacity, `limits::MAX_THREADS` for the scheduler's,
/// built by [`TimeoutQueue::try_new`]; [`TimeoutQueue::empty`] has no room.
pub struct TimeoutQueue {
    items: TryVec<Timeout>,
    len: usize,
}

impl TimeoutQueue {
    /// A queue with no room, for a `const` initializer.
    pub const fn empty() -> Self {
        Self {
            items: TryVec::new(),
            len: 0,
        }
    }

    /// A queue with room for `cap` timeouts.
    pub fn try_new(cap: usize) -> Result<Self, AllocError> {
        Ok(Self {
            items: limits::table(cap, || NO_TIMEOUT)?,
            len: 0,
        })
    }

    /// Timeouts the queue can hold.
    pub fn capacity(&self) -> usize {
        self.items.len()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        if self.len == 0 {
            None
        } else {
            Some(self.items[0].deadline)
        }
    }

    pub fn get(&self, i: usize) -> Timeout {
        assert!(i < self.len, "timeout: index");
        self.items[i]
    }

    pub fn insert(&mut self, id: ThreadId, deadline: Instant) {
        assert!(!id.is_none(), "timeout: insert NONE");
        self.remove(id);
        assert!(self.len < self.items.len(), "timeout: full");
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
        self.items[self.len] = NO_TIMEOUT;
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

    /// Pop up to `out.len()` expired ids into `out`; how many.
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

    /// Ids whose deadline is at least `OVERDUE_NS` behind `now`. Borrows
    /// the queue: `schedule_inner` runs on top of any preempted syscall
    /// body's stack, where a copy of the whole queue does not fit.
    pub fn overdue(&self, now: Instant) -> impl Iterator<Item = Timeout> + '_ {
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
    current_is_idle || ticks.is_multiple_of(QUANTUM_TICKS)
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
        let mut q = ReadyQueue::try_new(8).unwrap();
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
        let mut q = ReadyQueue::try_new(8).unwrap();
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
        let mut t = TimeoutQueue::try_new(8).unwrap();
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
        let mut t = TimeoutQueue::try_new(8).unwrap();
        t.insert(tid(5), at(10));
        t.insert(tid(2), at(10));
        t.insert(tid(9), at(10));
        assert_eq!(t.pop_expired(at(10)), Some(tid(2)));
        assert_eq!(t.pop_expired(at(10)), Some(tid(5)));
        assert_eq!(t.pop_expired(at(10)), Some(tid(9)));
    }

    #[test]
    fn timeout_rearm_moves_entry() {
        let mut t = TimeoutQueue::try_new(8).unwrap();
        t.insert(tid(1), at(50));
        t.insert(tid(1), at(10));
        assert_eq!(t.len(), 1);
        assert_eq!(t.next_deadline(), Some(at(10)));
        assert!(t.remove(tid(1)));
        assert!(t.is_empty());
    }

    #[test]
    fn timeout_far_future_stays() {
        let mut t = TimeoutQueue::try_new(8).unwrap();
        t.insert(tid(1), FAR_DEADLINE);
        t.insert(tid(2), at(1));
        assert_eq!(t.pop_expired(at(1_000_000)), Some(tid(2)));
        assert_eq!(t.pop_expired(at(1_000_000)), None);
        assert_eq!(t.next_deadline(), Some(FAR_DEADLINE));
    }

    #[test]
    fn overdue_scan() {
        let mut t = TimeoutQueue::try_new(8).unwrap();
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
        let mut ready = ReadyQueue::try_new(8).unwrap();
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
        let mut ready = ReadyQueue::try_new(8).unwrap();
        let mut timeouts = TimeoutQueue::try_new(8).unwrap();
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
        let mut ready = ReadyQueue::try_new(8).unwrap();
        let mut timeouts = TimeoutQueue::try_new(8).unwrap();
        let mut wq = crate::wait::WaitQueue::new();
        let mut links = [crate::wait::WaitLink::NONE; 2];
        wq.enqueue(a, &mut Links(&mut links));
        timeouts.insert(a, effective_deadline(None));
        let n = wake_expired(&mut timeouts, &mut ready, at(1), |_| {});
        assert_eq!(n, 0);
        assert!(wq.contains(a, &mut Links(&mut links)));
        timeouts.remove(a);
        timeouts.insert(a, at(5));
        let n = wake_expired(&mut timeouts, &mut ready, at(5), |id| {
            wq.remove(id, &mut Links(&mut links));
        });
        assert_eq!(n, 1);
        assert!(!wq.contains(a, &mut Links(&mut links)));
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

    /// Links for thread ids `0..len`, for a host test's wait queue.
    struct Links<'a>(&'a mut [crate::wait::WaitLink]);

    impl crate::wait::WaitLinks for Links<'_> {
        fn link(&mut self, id: ThreadId) -> Option<&mut crate::wait::WaitLink> {
            self.0.get_mut(id.0 as usize)
        }
    }

    #[test]
    fn queues_hold_their_capacity() {
        let mut r = ReadyQueue::try_new(3).unwrap();
        assert_eq!(r.capacity(), 3);
        for n in 1..=3 {
            r.push_back(tid(n));
        }
        assert_eq!(r.len(), 3);
        // Wrap the ring: the head moves and ids still come out in order.
        assert_eq!(r.pop_front(), Some(tid(1)));
        r.push_back(tid(4));
        assert!(r.remove(tid(3)));
        assert_eq!(r.iter().collect::<Vec<_>>(), [tid(2), tid(4)]);
        let mut t = TimeoutQueue::try_new(2).unwrap();
        t.insert(tid(1), at(1));
        t.insert(tid(2), at(2));
        assert_eq!(t.capacity(), 2);
        assert_eq!(t.len(), 2);
        assert_eq!(ReadyQueue::empty().capacity(), 0);
        assert!(ReadyQueue::empty().pop_front().is_none());
    }

    /// The heap queues the kernel sizes from `limits` (ROADMAP §10.4, D1).
    #[test]
    fn fixed_tables_match_limits() {
        use crate::limits::MAX_THREADS;
        assert_eq!(
            ReadyQueue::try_new(MAX_THREADS).unwrap().capacity(),
            MAX_THREADS
        );
        assert_eq!(
            TimeoutQueue::try_new(MAX_THREADS).unwrap().capacity(),
            MAX_THREADS
        );
    }

    #[test]
    #[should_panic(expected = "ready: full")]
    fn ready_full_asserts() {
        let mut r = ReadyQueue::try_new(1).unwrap();
        r.push_back(tid(1));
        r.push_back(tid(2));
    }

    #[test]
    #[should_panic(expected = "timeout: full")]
    fn timeout_full_asserts() {
        let mut t = TimeoutQueue::try_new(1).unwrap();
        t.insert(tid(1), at(1));
        t.insert(tid(2), at(2));
    }
}
