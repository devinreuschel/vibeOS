//! Blocking locks: `BlockingMutex`, `RwLock`, `Semaphore`, `Condvar` and
//! `Channel`. ROADMAP §3.5, DESIGN §2.3 / §9.4.
//!
//! Each sits on `WaitQueue`s serialized by SCHED: enqueue → Blocked →
//! drop → schedule. Every wait takes an optional deadline. They live apart
//! from `sync_init`'s `SpinMutex` because they call the scheduler, which
//! itself takes `SpinMutex`es (DESIGN §1.1 constraint 6).
//!
//! Each primitive's model (`state`, `inner`) is touched only inside
//! `thread_init::with_sched`, which holds SCHED; that lock is what makes
//! each `&mut` to a model unique.

use core::cell::UnsafeCell;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};

use vibeos::thread::{ThreadId, WaitOutcome};
use vibeos::time::Instant;
use vibeos::wait::{
    ChannelModel, CondModel, MutexModel, RwLockModel, SemaModel, WriterTimeoutWake, deadline_of,
};

use vibeos::sync::OpGate;

use crate::sync_init;
use crate::thread_init;
use crate::time_init;

fn past(deadline: Instant) -> bool {
    deadline.ns != u64::MAX && time_init::now_ns() >= deadline.ns
}

fn me() -> ThreadId {
    thread_init::current_id()
}

/// Sleep until woken. `WaitOutcome::Timeout` only when the wait's deadline
/// passed, so a wait with no deadline (`deadline_of(None)`) never returns it.
fn wait_resume() -> WaitOutcome {
    #[cfg(feature = "kernel_tests")]
    thread_init::testing::wait_window();
    thread_init::schedule();
    thread_init::last_wait_outcome()
}

// ----- BlockingMutex -----

pub struct BlockingMutex<T> {
    state: UnsafeCell<MutexModel>,
    data: UnsafeCell<T>,
}

// SAFETY: invariant I232: one guard at a time reaches `data`, so sharing
// the mutex hands one holder at a time `&mut T` (AGENTS.md rule 6), and
// `state` is touched only under SCHED; established by
// `sync::blocking_init::BlockingMutex::lock_until`.
unsafe impl<T: Send> Sync for BlockingMutex<T> {}
// SAFETY: moving the mutex moves the `T` it owns, which `T: Send` allows;
// established here.
unsafe impl<T: Send> Send for BlockingMutex<T> {}

pub struct BlockingMutexGuard<'a, T> {
    mutex: &'a BlockingMutex<T>,
}

impl<T> BlockingMutex<T> {
    pub const fn new(v: T) -> Self {
        Self {
            state: UnsafeCell::new(MutexModel::new()),
            data: UnsafeCell::new(v),
        }
    }

    #[allow(
        clippy::panic,
        reason = "a wait with no deadline never returns Timeout (`sync::blocking_init::wait_resume`)"
    )]
    pub fn lock(&self) -> BlockingMutexGuard<'_, T> {
        match self.lock_until(None) {
            Some(g) => g,
            None => panic!("blocking mutex: far deadline fired"),
        }
    }

    pub fn try_lock(&self) -> Option<BlockingMutexGuard<'_, T>> {
        let got = thread_init::with_sched(|_| {
            // SAFETY: this primitive's model is touched only under SCHED, which
            // `with_sched` holds here, so this `&mut` is the only reference to it;
            // established by `thread_init::with_sched`.
            let st = unsafe { &mut *self.state.get() };
            st.try_acquire(me())
        });
        if got {
            Some(BlockingMutexGuard { mutex: self })
        } else {
            None
        }
    }

    pub fn lock_until(&self, deadline: Option<Instant>) -> Option<BlockingMutexGuard<'_, T>> {
        sync_init::might_sleep();
        let d = deadline_of(deadline);
        loop {
            let got = thread_init::with_sched(|s| {
                // SAFETY: this primitive's model is touched only under SCHED, which
                // `with_sched` holds here, so this `&mut` is the only reference to it;
                // established by `thread_init::with_sched`.
                let st = unsafe { &mut *self.state.get() };
                if st.try_acquire(me()) {
                    return Ok(());
                }
                assert!(st.owner != me(), "blocking mutex: recursive lock");
                if past(d) {
                    return Err(false);
                }
                s.begin_wait(&mut st.wq, d);
                Err(true)
            });
            match got {
                Ok(()) => return Some(BlockingMutexGuard { mutex: self }),
                Err(false) => return None,
                Err(true) => {
                    if wait_resume() == WaitOutcome::Timeout {
                        return None;
                    }
                }
            }
        }
    }
}

impl<T> Drop for BlockingMutexGuard<'_, T> {
    fn drop(&mut self) {
        thread_init::with_sched(|s| {
            // SAFETY: this primitive's model is touched only under SCHED, which
            // `with_sched` holds here, so this `&mut` is the only reference to it;
            // established by `thread_init::with_sched`.
            let st = unsafe { &mut *self.mutex.state.get() };
            st.release();
            s.wake_one(&mut st.wq);
        });
    }
}

impl<T> Deref for BlockingMutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: invariant I232: this guard owns the mutex, so no other
        // reference to `data` exists; established by
        // `sync::blocking_init::BlockingMutex::lock_until`.
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T> DerefMut for BlockingMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: invariant I232: this guard owns the mutex and `&mut self`
        // borrows it uniquely; established by
        // `sync::blocking_init::BlockingMutex::lock_until`.
        unsafe { &mut *self.mutex.data.get() }
    }
}

// ----- RwLock -----

pub struct RwLock<T> {
    state: UnsafeCell<RwLockModel>,
    data: UnsafeCell<T>,
}

// SAFETY: invariant I232: readers share `&T` and a writer gets `&mut T`
// alone, which `T: Send + Sync` covers (AGENTS.md rule 6), and `state` is
// touched only under SCHED; established by `sync::blocking_init::RwLock::write_until`.
unsafe impl<T: Send + Sync> Sync for RwLock<T> {}
// SAFETY: moving the lock moves the `T` it owns, which `T: Send` allows;
// established here.
unsafe impl<T: Send> Send for RwLock<T> {}

pub struct RwLockReadGuard<'a, T> {
    lock: &'a RwLock<T>,
}

pub struct RwLockWriteGuard<'a, T> {
    lock: &'a RwLock<T>,
}

impl<T> RwLock<T> {
    pub const fn new(v: T) -> Self {
        Self {
            state: UnsafeCell::new(RwLockModel::new()),
            data: UnsafeCell::new(v),
        }
    }

    #[allow(
        clippy::expect_used,
        reason = "a wait with no deadline never returns Timeout (`sync::blocking_init::wait_resume`)"
    )]
    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        self.read_until(None).expect("rwlock: far deadline fired")
    }

    #[allow(
        clippy::expect_used,
        reason = "a wait with no deadline never returns Timeout (`sync::blocking_init::wait_resume`)"
    )]
    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        self.write_until(None).expect("rwlock: far deadline fired")
    }

    pub fn read_until(&self, deadline: Option<Instant>) -> Option<RwLockReadGuard<'_, T>> {
        sync_init::might_sleep();
        let d = deadline_of(deadline);
        loop {
            let got = thread_init::with_sched(|s| {
                // SAFETY: this primitive's model is touched only under SCHED, which
                // `with_sched` holds here, so this `&mut` is the only reference to it;
                // established by `thread_init::with_sched`.
                let st = unsafe { &mut *self.state.get() };
                if st.try_read() {
                    return Ok(());
                }
                if past(d) {
                    return Err(false);
                }
                s.begin_wait(&mut st.read_wq, d);
                Err(true)
            });
            match got {
                Ok(()) => return Some(RwLockReadGuard { lock: self }),
                Err(false) => return None,
                Err(true) => {
                    if wait_resume() == WaitOutcome::Timeout {
                        return None;
                    }
                }
            }
        }
    }

    pub fn write_until(&self, deadline: Option<Instant>) -> Option<RwLockWriteGuard<'_, T>> {
        sync_init::might_sleep();
        let d = deadline_of(deadline);
        loop {
            let got = thread_init::with_sched(|s| {
                // SAFETY: this primitive's model is touched only under SCHED, which
                // `with_sched` holds here, so this `&mut` is the only reference to it;
                // established by `thread_init::with_sched`.
                let st = unsafe { &mut *self.state.get() };
                if st.try_write(me()) {
                    return Ok(());
                }
                assert!(st.writer != me(), "rwlock: recursive write");
                if past(d) {
                    return Err(false);
                }
                s.begin_wait(&mut st.write_wq, d);
                Err(true)
            });
            match got {
                Ok(()) => return Some(RwLockWriteGuard { lock: self }),
                Err(false) => return None,
                Err(true) => {
                    if wait_resume() == WaitOutcome::Timeout {
                        thread_init::with_sched(|s| {
                            // SAFETY: this primitive's model is touched only under SCHED, which
                            // `with_sched` holds here, so this `&mut` is the only reference to it;
                            // established by `thread_init::with_sched`.
                            let st = unsafe { &mut *self.state.get() };
                            match st.after_writer_wait_timeout() {
                                WriterTimeoutWake::Readers => {
                                    s.wake_all(&mut st.read_wq);
                                }
                                WriterTimeoutWake::NextWriter => {
                                    s.wake_one(&mut st.write_wq);
                                }
                                WriterTimeoutWake::None => {}
                            }
                        });
                        return None;
                    }
                }
            }
        }
    }
}

impl<T> Drop for RwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        thread_init::with_sched(|s| {
            // SAFETY: this primitive's model is touched only under SCHED, which
            // `with_sched` holds here, so this `&mut` is the only reference to it;
            // established by `thread_init::with_sched`.
            let st = unsafe { &mut *self.lock.state.get() };
            st.drop_read();
            if st.readers == 0 {
                s.wake_one(&mut st.write_wq);
            }
        });
    }
}

impl<T> Drop for RwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        thread_init::with_sched(|s| {
            // SAFETY: this primitive's model is touched only under SCHED, which
            // `with_sched` holds here, so this `&mut` is the only reference to it;
            // established by `thread_init::with_sched`.
            let st = unsafe { &mut *self.lock.state.get() };
            st.drop_write();
            if !st.write_wq.is_empty() {
                s.wake_one(&mut st.write_wq);
            } else {
                s.wake_all(&mut st.read_wq);
            }
        });
    }
}

impl<T> Deref for RwLockReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: invariant I232: a read guard exists only while no writer
        // holds the lock, so every reference to `data` is shared;
        // established by `sync::blocking_init::RwLock::read_until`.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: invariant I232: the write guard excludes every other
        // guard; established by `sync::blocking_init::RwLock::write_until`.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: invariant I232: the write guard excludes every other
        // guard and `&mut self` borrows it uniquely; established by
        // `sync::blocking_init::RwLock::write_until`.
        unsafe { &mut *self.lock.data.get() }
    }
}

// ----- Semaphore -----

pub struct Semaphore {
    state: UnsafeCell<SemaModel>,
}

// SAFETY: `state` holds only a count and a wait queue, touched only under
// SCHED; established by `thread_init::with_sched`.
unsafe impl Sync for Semaphore {}
// SAFETY: `state` owns no thread-bound data; established here.
unsafe impl Send for Semaphore {}

impl Semaphore {
    pub const fn new(count: usize) -> Self {
        Self {
            state: UnsafeCell::new(SemaModel::new(count)),
        }
    }

    #[allow(
        clippy::expect_used,
        reason = "a wait with no deadline never returns Timeout (`sync::blocking_init::wait_resume`)"
    )]
    pub fn acquire(&self) {
        self.acquire_until(None).expect("sema: far deadline fired");
    }

    pub fn acquire_until(&self, deadline: Option<Instant>) -> Option<()> {
        sync_init::might_sleep();
        let d = deadline_of(deadline);
        loop {
            let got = thread_init::with_sched(|s| {
                // SAFETY: this primitive's model is touched only under SCHED, which
                // `with_sched` holds here, so this `&mut` is the only reference to it;
                // established by `thread_init::with_sched`.
                let st = unsafe { &mut *self.state.get() };
                if st.try_acquire() {
                    return Ok(());
                }
                if past(d) {
                    return Err(false);
                }
                s.begin_wait(&mut st.wq, d);
                Err(true)
            });
            match got {
                Ok(()) => return Some(()),
                Err(false) => return None,
                Err(true) => {
                    if wait_resume() == WaitOutcome::Timeout {
                        return None;
                    }
                }
            }
        }
    }

    pub fn release(&self) {
        thread_init::with_sched(|s| {
            // SAFETY: this primitive's model is touched only under SCHED, which
            // `with_sched` holds here, so this `&mut` is the only reference to it;
            // established by `thread_init::with_sched`.
            let st = unsafe { &mut *self.state.get() };
            st.release();
            s.wake_one(&mut st.wq);
        });
    }
}

// ----- Condvar -----

pub struct Condvar {
    state: UnsafeCell<CondModel>,
}

// SAFETY: `state` holds only a wait queue, touched only under SCHED;
// established by `thread_init::with_sched`.
unsafe impl Sync for Condvar {}
// SAFETY: `state` owns no thread-bound data; established here.
unsafe impl Send for Condvar {}

impl Condvar {
    pub const fn new() -> Self {
        Self {
            state: UnsafeCell::new(CondModel::new()),
        }
    }

    pub fn wait<'a, T>(&self, guard: BlockingMutexGuard<'a, T>) -> BlockingMutexGuard<'a, T> {
        self.wait_until(guard, None).0
    }

    /// Enqueue on the CV (lost-wakeup), unlock the mutex under the same
    /// SCHED, then schedule. Mesa: caller rechecks the predicate.
    pub fn wait_until<'a, T>(
        &self,
        guard: BlockingMutexGuard<'a, T>,
        deadline: Option<Instant>,
    ) -> (BlockingMutexGuard<'a, T>, WaitOutcome) {
        sync_init::might_sleep();
        let guard = ManuallyDrop::new(guard);
        let mutex = guard.mutex;
        let d = deadline_of(deadline);
        thread_init::with_sched(|s| {
            // SAFETY: this primitive's model is touched only under SCHED, which
            // `with_sched` holds here, so this `&mut` is the only reference to it;
            // established by `thread_init::with_sched`.
            let st = unsafe { &mut *self.state.get() };
            s.begin_wait(&mut st.wq, d);
            // SAFETY: this primitive's model is touched only under SCHED, which
            // `with_sched` holds here, so this `&mut` is the only reference to it;
            // established by `thread_init::with_sched`.
            let mst = unsafe { &mut *mutex.state.get() };
            mst.release();
            s.wake_one(&mut mst.wq);
        });
        let outcome = wait_resume();
        (mutex.lock(), outcome)
    }

    pub fn notify_one(&self) {
        thread_init::with_sched(|s| {
            // SAFETY: this primitive's model is touched only under SCHED, which
            // `with_sched` holds here, so this `&mut` is the only reference to it;
            // established by `thread_init::with_sched`.
            let st = unsafe { &mut *self.state.get() };
            s.wake_one(&mut st.wq);
        });
    }

    /// Under SCHED, begin a wait on this condvar's queue unless `done()`
    /// holds, then sleep until woken or `deadline`. `None` at once if
    /// `done()` held. Checking under the same SCHED as the enqueue loses no
    /// wake-up from a waker that changes the state before it notifies.
    pub(crate) fn wait_unless(
        &self,
        done: impl FnOnce() -> bool,
        deadline: Instant,
    ) -> Option<WaitOutcome> {
        let waiting = thread_init::with_sched(|s| {
            if done() {
                return false;
            }
            // SAFETY: this primitive's model is touched only under SCHED, which
            // `with_sched` holds here, so this `&mut` is the only reference to it;
            // established by `thread_init::with_sched`.
            let st = unsafe { &mut *self.state.get() };
            s.begin_wait(&mut st.wq, deadline);
            true
        });
        waiting.then(wait_resume)
    }

    pub fn notify_all(&self) {
        thread_init::with_sched(|s| {
            // SAFETY: this primitive's model is touched only under SCHED, which
            // `with_sched` holds here, so this `&mut` is the only reference to it;
            // established by `thread_init::with_sched`.
            let st = unsafe { &mut *self.state.get() };
            s.wake_all(&mut st.wq);
        });
    }
}

// ----- Channel -----

pub struct Channel<T, const N: usize> {
    inner: UnsafeCell<ChannelModel<T, N>>,
}

// SAFETY: `inner` is touched only under SCHED, and a value moves in by
// `send` and out by `recv` on possibly different threads, which `T: Send`
// covers; established by `thread_init::with_sched`.
unsafe impl<T: Send, const N: usize> Sync for Channel<T, N> {}
// SAFETY: moving the channel moves the `T`s it holds, which `T: Send`
// allows; established here.
unsafe impl<T: Send, const N: usize> Send for Channel<T, N> {}

impl<T, const N: usize> Channel<T, N> {
    pub const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(ChannelModel::new()),
        }
    }

    #[allow(
        clippy::panic,
        reason = "a wait with no deadline never returns Timeout (`sync::blocking_init::wait_resume`)"
    )]
    pub fn send(&self, v: T) {
        if self.send_until(v, None).is_err() {
            panic!("channel: far deadline fired");
        }
    }

    #[allow(
        clippy::expect_used,
        reason = "a wait with no deadline never returns Timeout (`sync::blocking_init::wait_resume`)"
    )]
    pub fn recv(&self) -> T {
        self.recv_until(None).expect("channel: far deadline fired")
    }

    pub fn try_send(&self, v: T) -> Result<(), T> {
        thread_init::with_sched(|s| {
            // SAFETY: this primitive's model is touched only under SCHED, which
            // `with_sched` holds here, so this `&mut` is the only reference to it;
            // established by `thread_init::with_sched`.
            let ch = unsafe { &mut *self.inner.get() };
            match ch.try_send(v) {
                Ok(()) => {
                    s.wake_one(&mut ch.recv_wq);
                    Ok(())
                }
                Err(v) => Err(v),
            }
        })
    }

    pub fn try_recv(&self) -> Option<T> {
        thread_init::with_sched(|s| {
            // SAFETY: this primitive's model is touched only under SCHED, which
            // `with_sched` holds here, so this `&mut` is the only reference to it;
            // established by `thread_init::with_sched`.
            let ch = unsafe { &mut *self.inner.get() };
            let v = ch.try_recv()?;
            s.wake_one(&mut ch.send_wq);
            Some(v)
        })
    }

    pub fn send_until(&self, v: T, deadline: Option<Instant>) -> Result<(), T> {
        sync_init::might_sleep();
        let d = deadline_of(deadline);
        let mut v = v;
        loop {
            // `Err((v, true))`: queued to wait; `Err((v, false))`: past `d`.
            let step = thread_init::with_sched(|s| {
                // SAFETY: this primitive's model is touched only under SCHED, which
                // `with_sched` holds here, so this `&mut` is the only reference to it;
                // established by `thread_init::with_sched`.
                let ch = unsafe { &mut *self.inner.get() };
                match ch.try_send(v) {
                    Ok(()) => {
                        s.wake_one(&mut ch.recv_wq);
                        Ok(())
                    }
                    Err(back) => {
                        if past(d) {
                            Err((back, false))
                        } else {
                            s.begin_wait(&mut ch.send_wq, d);
                            Err((back, true))
                        }
                    }
                }
            });
            match step {
                Ok(()) => return Ok(()),
                Err((back, false)) => return Err(back),
                Err((back, true)) => {
                    if wait_resume() == WaitOutcome::Timeout {
                        return Err(back);
                    }
                    v = back;
                }
            }
        }
    }

    pub fn recv_until(&self, deadline: Option<Instant>) -> Option<T> {
        sync_init::might_sleep();
        let d = deadline_of(deadline);
        loop {
            let got = thread_init::with_sched(|s| {
                // SAFETY: this primitive's model is touched only under SCHED, which
                // `with_sched` holds here, so this `&mut` is the only reference to it;
                // established by `thread_init::with_sched`.
                let ch = unsafe { &mut *self.inner.get() };
                if let Some(v) = ch.try_recv() {
                    s.wake_one(&mut ch.send_wq);
                    return Ok(v);
                }
                if past(d) {
                    return Err(false);
                }
                s.begin_wait(&mut ch.recv_wq, d);
                Err(true)
            });
            match got {
                Ok(v) => return Some(v),
                Err(false) => return None,
                Err(true) => {
                    if wait_resume() == WaitOutcome::Timeout {
                        return None;
                    }
                }
            }
        }
    }
}

// ----- Operation gate sleep (DESIGN §2.11 rule 3) -----

/// Where `OpGate::kill` sleeps. Static, so a waiter's queue never lives
/// inside a gate its owner frees (`thread_init::unlink_wait` casts the
/// wait cookie back to the queue).
static GATE_WAITERS: Condvar = Condvar::new();

/// How long a killer sleeps before it rechecks its gate: it covers a wake
/// `gate_wake` skipped under a lock ranked after SCHED.
const GATE_RECHECK_NS: u64 = 10_000_000;

/// The sleep `sync::set_gate_wait` installs for `OpGate::kill`: sleep
/// until woken or 10 ms pass, unless `g` is already empty. `kill` loops on
/// it until the gate is empty.
pub(crate) fn gate_sleep(g: &OpGate) {
    let deadline = Instant {
        ns: time_init::now_ns().saturating_add(GATE_RECHECK_NS),
    };
    // Woken or timed out, `kill` rechecks the gate either way.
    GATE_WAITERS.wait_unless(|| g.inside() == 0, deadline);
}

/// The wake `sync::set_gate_wait` installs: the last operation out of a
/// dead gate wakes every killer, where this CPU may take SCHED. Elsewhere
/// the killer's 10 ms recheck finds the gate empty.
pub(crate) fn gate_wake() {
    if sync_init::may_take_sched() {
        GATE_WAITERS.notify_all();
    }
}
