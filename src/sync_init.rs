//! Kernel locks. ROADMAP §3.5, DESIGN §2.3 / §9.4.
//!
//! `SpinMutex` is IRQ-aware (the only spinlock). Blocking primitives
//! sit on `WaitQueue`s serialized by SCHED: enqueue → Blocked → drop →
//! schedule. Every wait takes an optional deadline.
#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]

use core::cell::UnsafeCell;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};

use vibeos::lock::{acquire_mask, can_acquire, release_mask};
use vibeos::sync::SpinLock;
use vibeos::thread::{ThreadId, WaitOutcome};
use vibeos::time::Instant;
use vibeos::wait::{
    ChannelModel, CondModel, MutexModel, RwLockModel, SemaModel, WriterTimeoutWake, deadline_of,
};

use crate::per_cpu_init;
use crate::thread_init;
use crate::time_init;
use crate::x86::InterruptGuard;

pub struct SpinMutex<T> {
    lock: SpinLock,
    data: UnsafeCell<T>,
    rank: u8,
}

unsafe impl<T: Send> Sync for SpinMutex<T> {}
unsafe impl<T: Send> Send for SpinMutex<T> {}

pub struct SpinMutexGuard<'a, T> {
    mutex: &'a SpinMutex<T>,
    owner: usize,
    rank: u8,
    _irq: InterruptGuard,
}

impl<T> SpinMutex<T> {
    pub const fn new(v: T) -> Self {
        Self::with_rank(v, 0)
    }

    pub const fn with_rank(v: T, rank: u8) -> Self {
        Self {
            lock: SpinLock::new(),
            data: UnsafeCell::new(v),
            rank,
        }
    }

    pub fn lock(&self) -> SpinMutexGuard<'_, T> {
        let irq = InterruptGuard::enter();
        let owner = owner_token();
        lock_enter(self.rank);
        while !self.lock.try_acquire(owner) {
            crate::ipi_init::service_incoming();
            core::hint::spin_loop();
        }
        SpinMutexGuard {
            mutex: self,
            owner,
            rank: self.rank,
            _irq: irq,
        }
    }

    /// One shot. `None` if held (including by us: recursive would panic
    /// the TAS, so we treat same-owner as fail without a second CAS).
    pub fn try_lock(&self) -> Option<SpinMutexGuard<'_, T>> {
        let irq = InterruptGuard::enter();
        let owner = owner_token();
        if self.lock.is_locked() && self.lock.owner() == owner {
            return None;
        }
        lock_enter(self.rank);
        if self.lock.try_acquire(owner) {
            Some(SpinMutexGuard {
                mutex: self,
                owner,
                rank: self.rank,
                _irq: irq,
            })
        } else {
            lock_leave(self.rank);
            None
        }
    }
}

impl<T> Drop for SpinMutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.lock.release(self.owner);
        lock_leave(self.rank);
    }
}

impl<T> Deref for SpinMutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T> DerefMut for SpinMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.data.get() }
    }
}

fn owner_token() -> usize {
    match per_cpu_init::try_current() {
        Some(c) => c.cpu_id as usize + 1,
        None => 1,
    }
}

fn lock_cpu() -> usize {
    match per_cpu_init::try_current() {
        Some(c) => c.cpu_id as usize,
        None => 0,
    }
}

/// Debug lock-order tracker. Cheap: one byte per CPU, skipped until GS is live.
static HELD: [core::sync::atomic::AtomicU8; 64] =
    [const { core::sync::atomic::AtomicU8::new(0) }; 64];

fn lock_enter(rank: u8) {
    if rank == 0 || !per_cpu_init::is_live() {
        return;
    }
    let i = lock_cpu();
    if i >= 64 {
        return;
    }
    let held = HELD[i].load(core::sync::atomic::Ordering::Relaxed);
    assert!(
        can_acquire(held, rank),
        "lock order: rank {rank} while holding {held:#x}"
    );
    HELD[i].store(
        acquire_mask(held, rank),
        core::sync::atomic::Ordering::Relaxed,
    );
}

fn lock_leave(rank: u8) {
    if rank == 0 || !per_cpu_init::is_live() {
        return;
    }
    let i = lock_cpu();
    if i >= 64 {
        return;
    }
    let held = HELD[i].load(core::sync::atomic::Ordering::Relaxed);
    HELD[i].store(
        release_mask(held, rank),
        core::sync::atomic::Ordering::Relaxed,
    );
}

fn past(deadline: Instant) -> bool {
    deadline.ns != u64::MAX && time_init::now_ns() >= deadline.ns
}

fn me() -> ThreadId {
    thread_init::current_id()
}

fn wait_resume() -> WaitOutcome {
    thread_init::schedule();
    thread_init::last_wait_outcome()
}

// ----- BlockingMutex -----

pub struct BlockingMutex<T> {
    state: UnsafeCell<MutexModel>,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for BlockingMutex<T> {}
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

    pub fn lock(&self) -> BlockingMutexGuard<'_, T> {
        match self.lock_until(None) {
            Some(g) => g,
            None => panic!("blocking mutex: far deadline fired"),
        }
    }

    pub fn try_lock(&self) -> Option<BlockingMutexGuard<'_, T>> {
        let got = thread_init::with_sched(|_| {
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
        let d = deadline_of(deadline);
        loop {
            let got = thread_init::with_sched(|s| {
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
            let st = unsafe { &mut *self.mutex.state.get() };
            st.release();
            s.wake_one(&mut st.wq);
        });
    }
}

impl<T> Deref for BlockingMutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T> DerefMut for BlockingMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.data.get() }
    }
}

// ----- RwLock -----

pub struct RwLock<T> {
    state: UnsafeCell<RwLockModel>,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send + Sync> Sync for RwLock<T> {}
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

    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        self.read_until(None).expect("rwlock: far deadline fired")
    }

    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        self.write_until(None).expect("rwlock: far deadline fired")
    }

    pub fn read_until(&self, deadline: Option<Instant>) -> Option<RwLockReadGuard<'_, T>> {
        let d = deadline_of(deadline);
        loop {
            let got = thread_init::with_sched(|s| {
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
        let d = deadline_of(deadline);
        loop {
            let got = thread_init::with_sched(|s| {
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
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

// ----- Semaphore -----

pub struct Semaphore {
    state: UnsafeCell<SemaModel>,
}

unsafe impl Sync for Semaphore {}
unsafe impl Send for Semaphore {}

impl Semaphore {
    pub const fn new(count: usize) -> Self {
        Self {
            state: UnsafeCell::new(SemaModel::new(count)),
        }
    }

    pub fn acquire(&self) {
        self.acquire_until(None).expect("sema: far deadline fired");
    }

    pub fn acquire_until(&self, deadline: Option<Instant>) -> Option<()> {
        let d = deadline_of(deadline);
        loop {
            let got = thread_init::with_sched(|s| {
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

unsafe impl Sync for Condvar {}
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
        let guard = ManuallyDrop::new(guard);
        let mutex = guard.mutex;
        let d = deadline_of(deadline);
        thread_init::with_sched(|s| {
            let st = unsafe { &mut *self.state.get() };
            s.begin_wait(&mut st.wq, d);
            let mst = unsafe { &mut *mutex.state.get() };
            mst.release();
            s.wake_one(&mut mst.wq);
        });
        let outcome = wait_resume();
        (mutex.lock(), outcome)
    }

    pub fn notify_one(&self) {
        thread_init::with_sched(|s| {
            let st = unsafe { &mut *self.state.get() };
            s.wake_one(&mut st.wq);
        });
    }

    pub fn notify_all(&self) {
        thread_init::with_sched(|s| {
            let st = unsafe { &mut *self.state.get() };
            s.wake_all(&mut st.wq);
        });
    }
}

// ----- Channel -----

pub struct Channel<T, const N: usize> {
    inner: UnsafeCell<ChannelModel<T, N>>,
}

unsafe impl<T: Send, const N: usize> Sync for Channel<T, N> {}
unsafe impl<T: Send, const N: usize> Send for Channel<T, N> {}

impl<T, const N: usize> Channel<T, N> {
    pub const fn new() -> Self {
        Self {
            inner: UnsafeCell::new(ChannelModel::new()),
        }
    }

    pub fn send(&self, v: T) {
        if self.send_until(v, None).is_err() {
            panic!("channel: far deadline fired");
        }
    }

    pub fn recv(&self) -> T {
        self.recv_until(None).expect("channel: far deadline fired")
    }

    pub fn try_send(&self, v: T) -> Result<(), T> {
        thread_init::with_sched(|s| {
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
            let ch = unsafe { &mut *self.inner.get() };
            let v = ch.try_recv()?;
            s.wake_one(&mut ch.send_wq);
            Some(v)
        })
    }

    pub fn send_until(&self, v: T, deadline: Option<Instant>) -> Result<(), T> {
        let d = deadline_of(deadline);
        let mut pending = Some(v);
        loop {
            let step = thread_init::with_sched(|s| {
                let ch = unsafe { &mut *self.inner.get() };
                match ch.try_send(pending.take().expect("channel: send pending")) {
                    Ok(()) => {
                        s.wake_one(&mut ch.recv_wq);
                        0u8
                    }
                    Err(back) => {
                        pending = Some(back);
                        if past(d) {
                            1
                        } else {
                            s.begin_wait(&mut ch.send_wq, d);
                            2
                        }
                    }
                }
            });
            match step {
                0 => return Ok(()),
                1 => return Err(pending.take().expect("channel: send timeout val")),
                _ => {
                    if wait_resume() == WaitOutcome::Timeout {
                        return Err(pending.take().expect("channel: send timeout val"));
                    }
                }
            }
        }
    }

    pub fn recv_until(&self, deadline: Option<Instant>) -> Option<T> {
        let d = deadline_of(deadline);
        loop {
            let got = thread_init::with_sched(|s| {
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
