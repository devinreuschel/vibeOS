//! Kernel `SpinMutex<T>`: CAS + IRQ-aware. ROADMAP §3.5.
//!
//! One implementation. There is no non-IRQ-aware variant (DESIGN §2.3).
//! Misuse is `assert!`, not `debug_assert!`.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};

use vibeos::sync::SpinLock;

use crate::per_cpu_init;
use crate::x86::InterruptGuard;

pub struct SpinMutex<T> {
    lock: SpinLock,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for SpinMutex<T> {}
unsafe impl<T: Send> Send for SpinMutex<T> {}

pub struct SpinMutexGuard<'a, T> {
    mutex: &'a SpinMutex<T>,
    owner: usize,
    _irq: InterruptGuard,
}

impl<T> SpinMutex<T> {
    pub const fn new(v: T) -> Self {
        Self {
            lock: SpinLock::new(),
            data: UnsafeCell::new(v),
        }
    }

    pub fn lock(&self) -> SpinMutexGuard<'_, T> {
        let irq = InterruptGuard::enter();
        let owner = owner_token();
        self.lock.acquire(owner);
        SpinMutexGuard {
            mutex: self,
            owner,
            _irq: irq,
        }
    }
}

impl<T> Drop for SpinMutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.lock.release(self.owner);
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
