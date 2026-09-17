//! IRQ-aware spinlock core. DESIGN §2.3, ROADMAP §3.5.
//!
//! Portable CAS + owner tracking. The kernel wraps this in
//! [`InterruptGuard`] so every acquire runs with IF off. Host tests
//! drive it with a fake owner token.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// 0 means unlocked. Owners are never 0.
pub const UNLOCKED: usize = 0;

pub struct SpinLock {
    locked: AtomicBool,
    owner: AtomicUsize,
}

impl SpinLock {
    pub const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            owner: AtomicUsize::new(UNLOCKED),
        }
    }

    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }

    pub fn owner(&self) -> usize {
        self.owner.load(Ordering::Relaxed)
    }

    /// CAS acquire. Panics on recursive lock by `owner`.
    pub fn acquire(&self, owner: usize) {
        assert!(owner != UNLOCKED, "spin: owner 0 is reserved");
        loop {
            match self.locked.compare_exchange(
                false,
                true,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    self.owner.store(owner, Ordering::Relaxed);
                    return;
                }
                Err(_) => {
                    let held = self.owner.load(Ordering::Relaxed);
                    assert!(held != owner, "spin: recursive lock");
                    core::hint::spin_loop();
                }
            }
        }
    }

    /// Release. Panics if free or `owner` does not hold it.
    pub fn release(&self, owner: usize) {
        assert!(self.locked.load(Ordering::Relaxed), "spin: unlock of free lock");
        let held = self.owner.load(Ordering::Relaxed);
        assert!(held == owner, "spin: unlock by non-owner");
        self.owner.store(UNLOCKED, Ordering::Relaxed);
        self.locked.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_unlock_roundtrip() {
        let l = SpinLock::new();
        assert!(!l.is_locked());
        l.acquire(1);
        assert!(l.is_locked());
        assert_eq!(l.owner(), 1);
        l.release(1);
        assert!(!l.is_locked());
        assert_eq!(l.owner(), UNLOCKED);
    }

    #[test]
    fn two_owners_serialize() {
        let l = SpinLock::new();
        l.acquire(1);
        l.release(1);
        l.acquire(2);
        assert_eq!(l.owner(), 2);
        l.release(2);
    }

    #[test]
    #[should_panic(expected = "spin: recursive lock")]
    fn recursive_same_owner_asserts() {
        let l = SpinLock::new();
        l.acquire(3);
        l.acquire(3);
    }

    #[test]
    #[should_panic(expected = "spin: unlock of free lock")]
    fn unlock_free_asserts() {
        let l = SpinLock::new();
        l.release(1);
    }

    #[test]
    #[should_panic(expected = "spin: unlock by non-owner")]
    fn unlock_wrong_owner_asserts() {
        let l = SpinLock::new();
        l.acquire(1);
        l.release(2);
    }

    #[test]
    #[should_panic(expected = "spin: owner 0 is reserved")]
    fn owner_zero_asserts() {
        let l = SpinLock::new();
        l.acquire(0);
    }
}
