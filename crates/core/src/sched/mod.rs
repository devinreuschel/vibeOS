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

/// Run time a thread gets before a timer tick preempts it. DESIGN §6.1, §7.8.
pub const QUANTUM_MS: u64 = 10;

/// No deadline given: still a deadline, so nothing blocks forever.
pub const FAR_DEADLINE: Instant = Instant { ns: u64::MAX };

/// Log if still blocked this far past the deadline. 5 s.
pub const OVERDUE_NS: u64 = 5_000_000_000;

/// How often CPU 0's `schedule` scans for overdue waiters, in ticks.
pub const SWEEP_TICKS: u64 = 1_000;

/// Most overdue threads one sweep reports; [`find_overdue`]'s cursor
/// reaches the rest on later sweeps.
pub const OVERDUE_REPORT: usize = 16;

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
}

/// Whether a thread in `state` is overdue at `now`: `Blocked` or
/// `Sleeping` with a recorded deadline at least `OVERDUE_NS` behind `now`.
/// The timeout path wakes such a thread at its deadline, so it is overdue
/// only when its timeout entry was lost or never queued. A wait with no
/// deadline (`FAR_DEADLINE`) never is.
pub fn is_overdue(state: ThreadState, now: Instant) -> bool {
    let deadline = match state {
        ThreadState::Sleeping { deadline } | ThreadState::Blocked { deadline, .. } => deadline,
        ThreadState::Ready | ThreadState::Running | ThreadState::Dead => return false,
    };
    deadline != FAR_DEADLINE && now.ns.saturating_sub(deadline.ns) >= OVERDUE_NS
}

/// The blocked-thread sweep's check (DESIGN §6.5, ROADMAP §10.7): the
/// overdue threads ([`is_overdue`]) among `threads`, in tid order starting
/// at tid `from` and wrapping, written to `out` until it is full. Returns
/// how many it wrote and the cursor for the next sweep, one past the last
/// tid written (`from` when none), so successive sweeps reach every
/// overdue thread however many there are. Allocates nothing: `threads` is
/// walked twice, and each pass keeps the smallest tids in `out`.
pub fn find_overdue<I>(
    threads: I,
    now: Instant,
    from: ThreadId,
    out: &mut [ThreadId],
) -> (usize, ThreadId)
where
    I: Iterator<Item = (ThreadId, ThreadState)> + Clone,
{
    let f = from.raw();
    let n = smallest_overdue(threads.clone(), now, |id| id >= f, out);
    let n = match out.get_mut(n..) {
        Some(rest) if !rest.is_empty() => n + smallest_overdue(threads, now, |id| id < f, rest),
        _ => n,
    };
    let next = match n.checked_sub(1).and_then(|i| out.get(i)) {
        Some(last) => ThreadId(last.raw().wrapping_add(1)),
        None => from,
    };
    (n, next)
}

/// The smallest overdue tids among `threads` that `keep` accepts, sorted,
/// into `out`; how many.
fn smallest_overdue<I>(
    threads: I,
    now: Instant,
    keep: impl Fn(u32) -> bool,
    out: &mut [ThreadId],
) -> usize
where
    I: Iterator<Item = (ThreadId, ThreadState)>,
{
    let mut n = 0usize;
    for (id, state) in threads {
        if !keep(id.raw()) || !is_overdue(state, now) {
            continue;
        }
        let pos = out
            .iter()
            .take(n)
            .position(|o| o.raw() > id.raw())
            .unwrap_or(n);
        if n < out.len() {
            n += 1;
        } else if pos == n {
            continue;
        }
        // Shift `out[pos..n - 1]` up one, dropping the largest when full.
        let mut i = n - 1;
        while i > pos {
            out[i] = out[i - 1];
            i -= 1;
        }
        out[pos] = id;
    }
    n
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

/// Whether a timer tick preempts the running thread: the idle thread at
/// every tick, so a sleeper can displace `sti; hlt`, and any other thread
/// once it has run `quantum` TSC cycles (`ran`) since its quantum began.
/// Run time, not a tick count, so ticks that a host delivers late lengthen
/// a quantum by one tick's lateness at most (DESIGN §7.8).
pub fn should_preempt(ran: u64, quantum: u64, current_is_idle: bool) -> bool {
    current_is_idle || ran >= quantum
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
    fn preempt_quantum_is_run_time() {
        let q = 10_000;
        assert!(!should_preempt(0, q, false));
        assert!(!should_preempt(q - 1, q, false));
        assert!(should_preempt(q, q, false));
        // One late tick after a 10x quantum still preempts: no tick count.
        assert!(should_preempt(10 * q, q, false));
        assert!(should_preempt(0, q, true));
        assert_eq!(QUANTUM_MS, 10);
    }

    #[test]
    fn effective_deadline_far_when_none() {
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

    fn blocked(deadline: Instant) -> ThreadState {
        ThreadState::Blocked { wq: 0, deadline }
    }

    fn sleeping(deadline: Instant) -> ThreadState {
        ThreadState::Sleeping { deadline }
    }

    #[test]
    fn find_overdue_reports_blocked_and_sleeping() {
        let now = at(10 + OVERDUE_NS);
        let t = [
            (tid(1), blocked(at(10))),
            (tid(2), ThreadState::Ready),
            (tid(3), sleeping(at(5))),
            (tid(4), ThreadState::Running),
            (tid(5), ThreadState::Dead),
        ];
        assert!(is_overdue(blocked(at(10)), now));
        assert!(is_overdue(sleeping(at(10)), now));
        let mut out = [ThreadId::NONE; OVERDUE_REPORT];
        let (n, next) = find_overdue(t.iter().copied(), now, tid(0), &mut out);
        assert_eq!(&out[..n], &[tid(1), tid(3)]);
        assert_eq!(next, tid(4));
    }

    #[test]
    fn find_overdue_skips_far_and_recent() {
        let now = at(10 + OVERDUE_NS);
        let t = [
            (tid(1), blocked(FAR_DEADLINE)),
            (tid(2), sleeping(FAR_DEADLINE)),
            (tid(3), blocked(at(11))),
            (tid(4), sleeping(now)),
            (tid(5), blocked(at(u64::MAX - 1))),
        ];
        assert!(!is_overdue(blocked(FAR_DEADLINE), at(u64::MAX)));
        assert!(!is_overdue(ThreadState::Ready, now));
        let mut out = [ThreadId::NONE; OVERDUE_REPORT];
        let (n, next) = find_overdue(t.iter().copied(), now, tid(3), &mut out);
        assert_eq!(n, 0);
        assert_eq!(next, tid(3));
        let (n, _) = find_overdue(t.iter().copied(), now, tid(0), &mut []);
        assert_eq!(n, 0);
    }

    #[test]
    fn find_overdue_wraps_from_cursor() {
        let now = at(OVERDUE_NS + 100);
        // Table order is not tid order.
        let t = [
            (tid(9), blocked(at(1))),
            (tid(2), sleeping(at(1))),
            (tid(7), blocked(at(1))),
            (tid(4), blocked(at(1))),
            (tid(3), ThreadState::Ready),
        ];
        let mut out = [ThreadId::NONE; 2];
        let (n, next) = find_overdue(t.iter().copied(), now, tid(5), &mut out);
        assert_eq!(&out[..n], &[tid(7), tid(9)]);
        assert_eq!(next, tid(10));
        let (n, next) = find_overdue(t.iter().copied(), now, next, &mut out);
        assert_eq!(&out[..n], &[tid(2), tid(4)]);
        assert_eq!(next, tid(5));
        // Past the end of the ids wraps to the smallest.
        let mut one = [ThreadId::NONE; 1];
        let (n, next) = find_overdue(t.iter().copied(), now, tid(8), &mut one);
        assert_eq!(&one[..n], &[tid(9)]);
        let (n, next) = find_overdue(t.iter().copied(), now, next, &mut one);
        assert_eq!(&one[..n], &[tid(2)]);
        assert_eq!(next, tid(3));
        // Every overdue thread fits: all reported, from the cursor on.
        let mut all = [ThreadId::NONE; OVERDUE_REPORT];
        let (n, next) = find_overdue(t.iter().copied(), now, tid(5), &mut all);
        assert_eq!(&all[..n], &[tid(7), tid(9), tid(2), tid(4)]);
        assert_eq!(next, tid(5));
    }
}
