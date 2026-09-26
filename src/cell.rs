//! One write-once cell and one IRQ-off exclusive cell. DESIGN §2.3.
//!
//! Three cells in the kernel:
//! - [`crate::sync_init::SpinMutex`]: shared across CPUs
//! - [`IrqCell`]: CPU-local or boot-only mutable; IRQ-off + same-CPU re-entry panic
//! - [`BootCell`]: write once before `smp: done`, then shared `&T`
//!
//! Kernel `mod cell` in `main.rs`. Host tests: `#[cfg(test)]` in `vibeos-core`.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};

#[cfg(target_os = "none")]
use crate::x86::InterruptGuard;

#[cfg(not(target_os = "none"))]
struct InterruptGuard;

#[cfg(not(target_os = "none"))]
impl InterruptGuard {
    fn enter() -> Self {
        Self
    }
}

const UNSET: u8 = 0;
const SET: u8 = 1;

/// Write-once before SMP, then shared. `get` panics if still unset.
pub struct BootCell<T> {
    data: UnsafeCell<MaybeUninit<T>>,
    state: AtomicU8,
}

// After `set`, `&T` is shared. Caller puts a `Sync` `T` in the cell.
unsafe impl<T> Sync for BootCell<T> {}
unsafe impl<T> Send for BootCell<T> {}

// Write-once; Default would look like a normal cell.
#[allow(clippy::new_without_default)]
impl<T> BootCell<T> {
    pub const fn new() -> Self {
        Self {
            data: UnsafeCell::new(MaybeUninit::uninit()),
            state: AtomicU8::new(UNSET),
        }
    }

    /// Place `v` in the cell.
    ///
    /// # Safety
    /// Single writer, before `smp: done`. Must not race `get` / `try_get`.
    pub unsafe fn set(&self, v: T) {
        debug_assert_eq!(
            self.state.load(Ordering::Acquire),
            UNSET,
            "BootCell::set twice"
        );
        unsafe { (*self.data.get()).write(v) };
        self.state.store(SET, Ordering::Release);
    }

    #[allow(clippy::expect_used)]
    pub fn get(&self) -> &T {
        self.try_get().expect("BootCell unset")
    }

    pub fn try_get(&self) -> Option<&T> {
        if self.state.load(Ordering::Acquire) == SET {
            Some(unsafe { (*self.data.get()).assume_init_ref() })
        } else {
            None
        }
    }

    /// Payload address after [`set`]. GDT/TSS must init at this address
    /// so descriptor bases are not a stack temporary.
    #[inline]
    pub fn as_ptr(&self) -> *mut T {
        unsafe { (*self.data.get()).as_mut_ptr() }
    }
}

/// Exclusive `&mut T` with IRQs off. Same-CPU re-entry panics; other CPUs spin.
///
/// Data shared across CPUs as a lock should use `SpinMutex`.
pub struct IrqCell<T> {
    data: UnsafeCell<T>,
    /// 0 free, else `cpu_id + 1` (1 when `per_cpu` is not live).
    owner: AtomicU32,
}

unsafe impl<T> Sync for IrqCell<T> {}
unsafe impl<T> Send for IrqCell<T> {}

impl<T> IrqCell<T> {
    pub const fn new(v: T) -> Self {
        Self {
            data: UnsafeCell::new(v),
            owner: AtomicU32::new(0),
        }
    }

    #[inline(always)]
    #[allow(clippy::panic)]
    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let _irq = InterruptGuard::enter();
        let me = owner_token();
        loop {
            match self
                .owner
                .compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(owner) if owner == me => panic!("IrqCell re-entry"),
                Err(_) => core::hint::spin_loop(),
            }
        }
        struct Unlock<'a>(&'a AtomicU32);
        impl Drop for Unlock<'_> {
            fn drop(&mut self) {
                self.0.store(0, Ordering::Release);
            }
        }
        let _u = Unlock(&self.owner);
        f(unsafe { &mut *self.data.get() })
    }

    /// Address of the payload. `lidt`, and AP `ap_entry` before `GS_BASE`
    /// (InterruptGuard would `gs:[0]`).
    #[inline]
    pub fn as_ptr(&self) -> *mut T {
        self.data.get()
    }

    /// Clear the owner so the next `with` takes the cell. For the panic
    /// dump and the in-guest catch of a re-entry panic.
    ///
    /// # Safety
    /// The recorded holder never touches the payload again: its CPU is
    /// stopped on the panic path (DESIGN §2.5), or an `arch::catch`
    /// longjmp skipped its `Unlock` and its closure will not resume.
    pub unsafe fn force_unlock(&self) {
        self.owner.store(0, Ordering::Release);
    }
}

fn owner_token() -> u32 {
    #[cfg(target_os = "none")]
    {
        crate::per_cpu_init::try_current()
            .map(|c| c.cpu_id + 1)
            .unwrap_or(1)
    }
    #[cfg(not(target_os = "none"))]
    {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    #[test]
    fn bootcell_set_get() {
        let c = BootCell::new();
        assert!(c.try_get().is_none());
        unsafe { c.set(9u32) };
        assert_eq!(*c.get(), 9);
        assert_eq!(c.try_get().copied(), Some(9));
    }

    #[test]
    #[should_panic(expected = "BootCell unset")]
    fn bootcell_get_unset_panics() {
        let c: BootCell<u32> = BootCell::new();
        let _ = c.get();
    }

    #[test]
    fn irqcell_with_mutates() {
        let c = IrqCell::new(1u32);
        c.with(|v| *v = 4);
        c.with(|v| assert_eq!(*v, 4));
    }

    #[test]
    fn irqcell_reentry_panics() {
        let c = IrqCell::new(0u32);
        let hit = catch_unwind(AssertUnwindSafe(|| {
            c.with(|_| {
                c.with(|_| {});
            });
        }));
        assert!(hit.is_err());
        // SAFETY: the unwind ended both `with` closures, so the recorded
        // holder never touches the payload again; established here.
        unsafe { c.force_unlock() };
        c.with(|v| *v = 1);
        assert_eq!(c.with(|v| *v), 1);
    }
}
