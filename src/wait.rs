//! Wait queues and blocking-primitive models. ROADMAP §3.5, DESIGN §9.4.
//!
//! Predicate check and enqueue happen under the same lock that serializes
//! the queue. Kernel that lock is SCHED. Wake under it too.

use core::mem::MaybeUninit;

use crate::sched::{ReadyQueue, TimeoutQueue, effective_deadline};
use crate::thread::ThreadId;
use crate::time::Instant;

/// FIFO of blocked `ThreadId`s. One per mutex / rwlock / chan end / etc.
pub struct WaitQueue {
    waiters: ReadyQueue,
}

impl WaitQueue {
    pub const fn new() -> Self {
        Self {
            waiters: ReadyQueue::empty(),
        }
    }

    pub fn len(&self) -> usize {
        self.waiters.len()
    }

    pub fn is_empty(&self) -> bool {
        self.waiters.is_empty()
    }

    pub fn contains(&self, id: ThreadId) -> bool {
        self.waiters.contains(id)
    }

    pub fn enqueue(&mut self, id: ThreadId) {
        self.waiters.push_back(id);
    }

    pub fn dequeue(&mut self) -> Option<ThreadId> {
        self.waiters.pop_front()
    }

    pub fn remove(&mut self, id: ThreadId) -> bool {
        self.waiters.remove(id)
    }

    pub fn cookie(&self) -> usize {
        self as *const WaitQueue as usize
    }
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Enqueue, leave the ready FIFO, arm the deadline. Not runnable.
pub fn begin_wait(
    wq: &mut WaitQueue,
    ready: &mut ReadyQueue,
    timeouts: &mut TimeoutQueue,
    id: ThreadId,
    deadline: Instant,
) {
    wq.enqueue(id);
    ready.remove(id);
    timeouts.insert(id, deadline);
}

/// Dequeue one waiter and drop its timeout. Caller places it (local
/// ready or remote inbox). Host tests use [`wake_one`] which also
/// pushes the global FIFO.
pub fn take_one(wq: &mut WaitQueue, timeouts: &mut TimeoutQueue) -> Option<ThreadId> {
    let id = wq.dequeue()?;
    timeouts.remove(id);
    Some(id)
}

/// Dequeue one waiter, drop its timeout, put it on ready.
pub fn wake_one(
    wq: &mut WaitQueue,
    ready: &mut ReadyQueue,
    timeouts: &mut TimeoutQueue,
) -> Option<ThreadId> {
    let id = take_one(wq, timeouts)?;
    ready.push_back(id);
    Some(id)
}

pub fn wake_all(wq: &mut WaitQueue, ready: &mut ReadyQueue, timeouts: &mut TimeoutQueue) -> usize {
    let mut n = 0usize;
    while wake_one(wq, ready, timeouts).is_some() {
        n += 1;
    }
    n
}

pub fn deadline_of(deadline: Option<Instant>) -> Instant {
    effective_deadline(deadline)
}

pub struct MutexModel {
    pub held: bool,
    pub owner: ThreadId,
    pub wq: WaitQueue,
}

impl MutexModel {
    pub const fn new() -> Self {
        Self {
            held: false,
            owner: ThreadId::NONE,
            wq: WaitQueue::new(),
        }
    }

    pub fn try_acquire(&mut self, id: ThreadId) -> bool {
        if self.held {
            return false;
        }
        self.held = true;
        self.owner = id;
        true
    }

    pub fn release(&mut self) {
        self.held = false;
        self.owner = ThreadId::NONE;
    }
}

impl Default for MutexModel {
    fn default() -> Self {
        Self::new()
    }
}

pub struct RwLockModel {
    pub readers: u32,
    pub writer: ThreadId,
    pub read_wq: WaitQueue,
    pub write_wq: WaitQueue,
}

impl RwLockModel {
    pub const fn new() -> Self {
        Self {
            readers: 0,
            writer: ThreadId::NONE,
            read_wq: WaitQueue::new(),
            write_wq: WaitQueue::new(),
        }
    }

    /// Writer preference: new readers wait if a writer is queued.
    pub fn try_read(&mut self) -> bool {
        if !self.writer.is_none() || !self.write_wq.is_empty() {
            return false;
        }
        self.readers += 1;
        true
    }

    pub fn try_write(&mut self, id: ThreadId) -> bool {
        if !self.writer.is_none() || self.readers != 0 {
            return false;
        }
        self.writer = id;
        true
    }

    pub fn drop_read(&mut self) {
        assert!(self.readers > 0, "rwlock: drop_read");
        self.readers -= 1;
    }

    pub fn drop_write(&mut self) {
        self.writer = ThreadId::NONE;
    }

    /// After a queued writer times out and is unlinked. Writer preference
    /// otherwise leaves `read_wq` parked forever.
    pub fn after_writer_wait_timeout(&self) -> WriterTimeoutWake {
        match (
            self.writer.is_none(),
            self.write_wq.is_empty(),
            self.readers == 0,
        ) {
            (true, true, _) => WriterTimeoutWake::Readers,
            (true, false, true) => WriterTimeoutWake::NextWriter,
            (true, false, false) | (false, _, _) => WriterTimeoutWake::None,
        }
    }
}

impl Default for RwLockModel {
    fn default() -> Self {
        Self::new()
    }
}

/// Who to wake when a writer wait returns `Timeout`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriterTimeoutWake {
    None,
    NextWriter,
    Readers,
}

pub struct SemaModel {
    pub count: usize,
    pub wq: WaitQueue,
}

impl SemaModel {
    pub const fn new(count: usize) -> Self {
        Self {
            count,
            wq: WaitQueue::new(),
        }
    }

    pub fn try_acquire(&mut self) -> bool {
        if self.count == 0 {
            return false;
        }
        self.count -= 1;
        true
    }

    pub fn release(&mut self) {
        self.count = self.count.checked_add(1).expect("sema: overflow");
    }
}

pub struct CondModel {
    pub wq: WaitQueue,
}

impl CondModel {
    pub const fn new() -> Self {
        Self {
            wq: WaitQueue::new(),
        }
    }
}

impl Default for CondModel {
    fn default() -> Self {
        Self::new()
    }
}

/// Bounded MPSC ring. Waiters sit on `send_wq` (full) / `recv_wq` (empty).
pub struct ChannelModel<T, const N: usize> {
    slots: [MaybeUninit<T>; N],
    head: usize,
    len: usize,
    pub send_wq: WaitQueue,
    pub recv_wq: WaitQueue,
}

impl<T, const N: usize> ChannelModel<T, N> {
    pub const fn new() -> Self {
        assert!(N > 0);
        Self {
            slots: [const { MaybeUninit::uninit() }; N],
            head: 0,
            len: 0,
            send_wq: WaitQueue::new(),
            recv_wq: WaitQueue::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn cap(&self) -> usize {
        N
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn is_full(&self) -> bool {
        self.len >= N
    }

    pub fn try_send(&mut self, v: T) -> Result<(), T> {
        if self.len >= N {
            return Err(v);
        }
        let i = (self.head + self.len) % N;
        self.slots[i].write(v);
        self.len += 1;
        Ok(())
    }

    pub fn try_recv(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let i = self.head;
        let v = unsafe { self.slots[i].assume_init_read() };
        self.head = (self.head + 1) % N;
        self.len -= 1;
        Some(v)
    }
}

impl<T, const N: usize> Default for ChannelModel<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Drop for ChannelModel<T, N> {
    fn drop(&mut self) {
        while self.try_recv().is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sched::{FAR_DEADLINE, enqueue_runnable, take_next, wake_expired};
    use crate::thread::ThreadState;

    fn tid(n: u32) -> ThreadId {
        ThreadId(n)
    }

    fn at(ns: u64) -> Instant {
        Instant { ns }
    }

    fn assert_not_queued_and_ready(wq: &WaitQueue, ready: &ReadyQueue, id: ThreadId) {
        assert!(
            !(wq.contains(id) && ready.contains(id)),
            "tid {} on wait queue and ready",
            id.0
        );
    }

    #[test]
    fn wait_fifo_wake_one_all() {
        let mut wq = WaitQueue::new();
        let mut ready = ReadyQueue::empty();
        let mut timeouts = TimeoutQueue::empty();
        begin_wait(&mut wq, &mut ready, &mut timeouts, tid(2), at(50));
        begin_wait(&mut wq, &mut ready, &mut timeouts, tid(3), at(50));
        begin_wait(&mut wq, &mut ready, &mut timeouts, tid(4), at(50));
        assert_eq!(wq.len(), 3);
        assert!(!ready.contains(tid(2)));
        assert_eq!(wake_one(&mut wq, &mut ready, &mut timeouts), Some(tid(2)));
        assert!(ready.contains(tid(2)));
        assert!(!wq.contains(tid(2)));
        assert_eq!(wake_all(&mut wq, &mut ready, &mut timeouts), 2);
        assert!(wq.is_empty());
        assert_eq!(ready.len(), 3);
    }

    #[test]
    fn lost_wakeup_enqueue_before_unlock() {
        let mut m = MutexModel::new();
        let mut ready = ReadyQueue::empty();
        let mut timeouts = TimeoutQueue::empty();
        let a = tid(1);
        let b = tid(2);
        assert!(m.try_acquire(a));
        // B fails the predicate under the same lock, then enqueues.
        assert!(!m.try_acquire(b));
        begin_wait(&mut m.wq, &mut ready, &mut timeouts, b, at(100));
        assert_not_queued_and_ready(&m.wq, &ready, b);
        assert!(m.wq.contains(b));
        m.release();
        let woke = wake_one(&mut m.wq, &mut ready, &mut timeouts);
        assert_eq!(woke, Some(b));
        assert!(!m.wq.contains(b));
        assert!(ready.contains(b));
        assert!(!m.held);
        assert!(m.try_acquire(b));
    }

    #[test]
    fn timeout_unlinks_waiter() {
        let mut wq = WaitQueue::new();
        let mut ready = ReadyQueue::empty();
        let mut timeouts = TimeoutQueue::empty();
        let idle = tid(0);
        let a = tid(1);
        begin_wait(&mut wq, &mut ready, &mut timeouts, a, at(10));
        let n = wake_expired(&mut timeouts, &mut ready, at(10), |id| {
            wq.remove(id);
        });
        assert_eq!(n, 1);
        assert!(!wq.contains(a));
        assert_eq!(take_next(&mut ready, idle), a);
        assert_eq!(crate::thread::WaitOutcome::Timeout.name(), "timeout");
        assert_eq!(crate::thread::WaitOutcome::Woken.name(), "woken");
    }

    #[test]
    fn far_deadline_not_expired() {
        let mut wq = WaitQueue::new();
        let mut ready = ReadyQueue::empty();
        let mut timeouts = TimeoutQueue::empty();
        begin_wait(
            &mut wq,
            &mut ready,
            &mut timeouts,
            tid(1),
            deadline_of(None),
        );
        assert_eq!(timeouts.next_deadline(), Some(FAR_DEADLINE));
        let n = wake_expired(&mut timeouts, &mut ready, at(1_000_000), |_| {});
        assert_eq!(n, 0);
        assert!(wq.contains(tid(1)));
    }

    #[test]
    fn mutex_second_locker_waits() {
        let mut m = MutexModel::new();
        let mut ready = ReadyQueue::empty();
        let mut timeouts = TimeoutQueue::empty();
        assert!(m.try_acquire(tid(1)));
        assert!(!m.try_acquire(tid(2)));
        begin_wait(&mut m.wq, &mut ready, &mut timeouts, tid(2), at(9));
        begin_wait(&mut m.wq, &mut ready, &mut timeouts, tid(3), at(9));
        m.release();
        assert_eq!(wake_one(&mut m.wq, &mut ready, &mut timeouts), Some(tid(2)));
        assert!(m.try_acquire(tid(2)));
        m.release();
        assert_eq!(wake_one(&mut m.wq, &mut ready, &mut timeouts), Some(tid(3)));
        assert!(wq_empty_means_unlock_without_waiter(&mut m));
    }

    fn wq_empty_means_unlock_without_waiter(m: &mut MutexModel) -> bool {
        m.wq.is_empty()
    }

    #[test]
    fn rwlock_writer_excludes_readers() {
        let mut r = RwLockModel::new();
        assert!(r.try_read());
        assert!(r.try_read());
        assert!(!r.try_write(tid(1)));
        r.drop_read();
        r.drop_read();
        assert!(r.try_write(tid(1)));
        assert!(!r.try_read());
        r.drop_write();
        assert!(r.try_read());
    }

    #[test]
    fn rwlock_writer_preference() {
        let mut r = RwLockModel::new();
        let mut ready = ReadyQueue::empty();
        let mut timeouts = TimeoutQueue::empty();
        assert!(r.try_read());
        begin_wait(&mut r.write_wq, &mut ready, &mut timeouts, tid(2), at(1));
        assert!(!r.try_read());
        r.drop_read();
        assert_eq!(
            wake_one(&mut r.write_wq, &mut ready, &mut timeouts),
            Some(tid(2))
        );
        assert!(r.try_write(tid(2)));
    }

    #[test]
    fn rwlock_writer_timeout_wakes_readers() {
        let mut r = RwLockModel::new();
        let mut ready = ReadyQueue::empty();
        let mut timeouts = TimeoutQueue::empty();
        assert!(r.try_read());
        begin_wait(&mut r.write_wq, &mut ready, &mut timeouts, tid(2), at(5));
        begin_wait(&mut r.read_wq, &mut ready, &mut timeouts, tid(3), at(50));
        assert_eq!(r.after_writer_wait_timeout(), WriterTimeoutWake::None);
        r.write_wq.remove(tid(2));
        timeouts.remove(tid(2));
        assert_eq!(r.after_writer_wait_timeout(), WriterTimeoutWake::Readers);
        assert_eq!(wake_all(&mut r.read_wq, &mut ready, &mut timeouts), 1);
        assert!(ready.contains(tid(3)));
        r.drop_read();
        assert!(r.try_read());
    }

    #[test]
    fn rwlock_writer_timeout_wakes_next_writer() {
        let mut r = RwLockModel::new();
        let mut ready = ReadyQueue::empty();
        let mut timeouts = TimeoutQueue::empty();
        begin_wait(&mut r.write_wq, &mut ready, &mut timeouts, tid(2), at(5));
        begin_wait(&mut r.write_wq, &mut ready, &mut timeouts, tid(3), at(50));
        r.write_wq.remove(tid(2));
        timeouts.remove(tid(2));
        assert_eq!(r.after_writer_wait_timeout(), WriterTimeoutWake::NextWriter);
        assert_eq!(
            wake_one(&mut r.write_wq, &mut ready, &mut timeouts),
            Some(tid(3))
        );
    }

    #[test]
    fn semaphore_tokens() {
        let mut s = SemaModel::new(2);
        let mut ready = ReadyQueue::empty();
        let mut timeouts = TimeoutQueue::empty();
        assert!(s.try_acquire());
        assert!(s.try_acquire());
        assert!(!s.try_acquire());
        begin_wait(&mut s.wq, &mut ready, &mut timeouts, tid(3), at(1));
        s.release();
        assert_eq!(wake_one(&mut s.wq, &mut ready, &mut timeouts), Some(tid(3)));
        assert!(s.try_acquire());
        assert_eq!(s.count, 0);
    }

    #[test]
    fn cond_notify_uses_same_queue() {
        let mut c = CondModel::new();
        let mut ready = ReadyQueue::empty();
        let mut timeouts = TimeoutQueue::empty();
        begin_wait(&mut c.wq, &mut ready, &mut timeouts, tid(1), at(8));
        begin_wait(&mut c.wq, &mut ready, &mut timeouts, tid(2), at(8));
        assert_eq!(wake_one(&mut c.wq, &mut ready, &mut timeouts), Some(tid(1)));
        assert_eq!(wake_all(&mut c.wq, &mut ready, &mut timeouts), 1);
        assert!(c.wq.is_empty());
    }

    #[test]
    fn channel_bounded_mpsc_order() {
        let mut ch = ChannelModel::<u32, 2>::new();
        let mut ready = ReadyQueue::empty();
        let mut timeouts = TimeoutQueue::empty();
        assert!(ch.try_send(1).is_ok());
        assert!(ch.try_send(2).is_ok());
        assert_eq!(ch.try_send(3), Err(3));
        begin_wait(&mut ch.send_wq, &mut ready, &mut timeouts, tid(9), at(1));
        assert_eq!(ch.try_recv(), Some(1));
        assert_eq!(
            wake_one(&mut ch.send_wq, &mut ready, &mut timeouts),
            Some(tid(9))
        );
        assert!(ch.try_send(3).is_ok());
        assert_eq!(ch.try_recv(), Some(2));
        assert_eq!(ch.try_recv(), Some(3));
        assert!(ch.try_recv().is_none());
        begin_wait(&mut ch.recv_wq, &mut ready, &mut timeouts, tid(4), at(1));
        assert!(ch.try_send(7).is_ok());
        assert_eq!(
            wake_one(&mut ch.recv_wq, &mut ready, &mut timeouts),
            Some(tid(4))
        );
        assert_eq!(ch.try_recv(), Some(7));
    }

    #[test]
    fn take_one_does_not_touch_ready() {
        let mut wq = WaitQueue::new();
        let mut ready = ReadyQueue::empty();
        let mut timeouts = TimeoutQueue::empty();
        begin_wait(&mut wq, &mut ready, &mut timeouts, tid(1), at(1));
        assert_eq!(take_one(&mut wq, &mut timeouts), Some(tid(1)));
        assert!(ready.is_empty());
        assert!(timeouts.is_empty());
    }

    #[test]
    fn cookie_is_address() {
        let wq = WaitQueue::new();
        assert_eq!(wq.cookie(), &wq as *const WaitQueue as usize);
        assert_eq!(deadline_of(None), FAR_DEADLINE);
        enqueue_runnable(
            &mut ReadyQueue::empty(),
            tid(1),
            tid(0),
            ThreadState::Blocked { wq: 0 },
        );
    }
}
