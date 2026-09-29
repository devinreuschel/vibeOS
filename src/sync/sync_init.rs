//! Kernel locks. ROADMAP §3.5, DESIGN §2.3 / §9.4.
//!
//! `SpinMutex` is IRQ-aware (the only spinlock), with the lock-rank
//! tracker. The blocking primitives, which call the scheduler, are in
//! `sync::blocking_init`.
#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::ptr;
#[cfg(feature = "kernel_tests")]
use core::sync::atomic::AtomicU64;
use core::sync::atomic::{AtomicPtr, Ordering};

use vibeos::lock::{acquire_mask, can_acquire, release_mask};
use vibeos::sync::SpinLock;

use crate::per_cpu_init;
use crate::x86::InterruptGuard;

/// What `SpinMutex::lock` runs on each failed try: the IPI inbox's
/// `service_incoming`, which the IPI module's `init` sets before the first
/// AP starts (DESIGN §1.2, SMP.md §7.9). Unset, the spin only spins.
static SPIN_POLL: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Install the spin-poll hook.
pub fn set_spin_poll(f: fn()) {
    // Release: pairs with the Acquire load in `spin_poll`.
    SPIN_POLL.store(f as *mut (), Ordering::Release);
}

/// Whether the spin-poll hook is set.
#[cfg(feature = "kernel_tests")]
pub fn spin_poll_installed() -> bool {
    !SPIN_POLL.load(Ordering::Acquire).is_null()
}

#[inline]
fn spin_poll() {
    // Acquire: pairs with the Release store in `set_spin_poll`.
    let p = SPIN_POLL.load(Ordering::Acquire);
    if p.is_null() {
        return;
    }
    // SAFETY: invariant: a non-null `SPIN_POLL` holds a `fn()`; established
    // by `sync_init::set_spin_poll`, its only store.
    let f = unsafe { core::mem::transmute::<*mut (), fn()>(p) };
    f();
}

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
            #[cfg(feature = "kernel_tests")]
            record_spin(self.rank);
            spin_poll();
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

#[cfg(feature = "kernel_tests")]
const SPIN_RANKS: usize = 7;

#[cfg(feature = "kernel_tests")]
static SPINS: [AtomicU64; SPIN_RANKS] = [const { AtomicU64::new(0) }; SPIN_RANKS];

#[cfg(feature = "kernel_tests")]
fn record_spin(rank: u8) {
    let i = rank as usize;
    if i < SPIN_RANKS {
        SPINS[i].fetch_add(1, Ordering::Relaxed);
    }
}

/// Spin iterations per lock rank (index by rank; 0 unused). Phase 19 baseline.
#[cfg(feature = "kernel_tests")]
pub fn spin_counts() -> [u64; SPIN_RANKS] {
    core::array::from_fn(|i| SPINS[i].load(Ordering::Relaxed))
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
