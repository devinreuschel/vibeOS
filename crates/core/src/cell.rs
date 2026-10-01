//! One write-once cell and one IRQ-off exclusive cell. DESIGN §2.3.
//!
//! Three cells in the kernel:
//! - `sync_init::SpinMutex` (kernel): shared across CPUs
//! - [`IrqCell`]: IRQ-off exclusive `&mut T` for CPU-local and boot-only state, and the log
//!   ring's unranked lock (DESIGN §2.3); same-CPU re-entry panics
//! - [`BootCell`]: write once before `smp: done`, then shared `&T`
//!
//! Both carry std's bounds, as `OnceLock` and `Mutex` do: `BootCell<T>` is `Sync` only when
//! `T: Send + Sync`, `IrqCell<T, A>` only when `T: Send`, and each is `Send` when `T: Send`. The
//! assertions below, built by the kernel and by `make test-unit`, fail the build otherwise.
//!
//! `IrqCell` takes its port `A` through the §10.3 seam: it masks interrupts with
//! [`InterruptMask`], takes its owner token from [`PerCpuBase::cpu_id`], and runs the port's
//! [`CellHooks::acquire_check`] before it takes the cell. The kernel names both cells over its
//! port in `src/cell.rs`; host tests run them on the stub port, whose CPU id is per host
//! thread, so a cell two threads contend is taken in turn.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::MaybeUninit;

use crate::arch::{InterruptMask, PerCpuBase};
use crate::atomic::{AtomicU8, AtomicU32, Ordering};
use crate::sync::variant::{self, Site};

const UNSET: u8 = 0;
const SET: u8 = 1;

/// Write-once before SMP, then shared. `get` panics if still unset.
pub struct BootCell<T> {
    data: UnsafeCell<MaybeUninit<T>>,
    state: AtomicU8,
}

// SAFETY: invariant I22, established at `cell::BootCell::set`: the one
// write happens before the Release store of `state` that every reader
// Acquires, and afterwards only `&T` is handed out, so sharing the cell
// shares `&T` (needs `T: Sync`) and lets any holder's thread see the value
// the setter's thread moved in (needs `T: Send`). `Send` is the auto trait:
// `UnsafeCell<MaybeUninit<T>>` is `Send` exactly when `T: Send`.
unsafe impl<T: Send + Sync> Sync for BootCell<T> {}

/// `BootCell`'s initial value, one body for both constructors.
macro_rules! boot_cell_new {
    () => {
        BootCell {
            data: UnsafeCell::new(MaybeUninit::uninit()),
            state: AtomicU8::new(UNSET),
        }
    };
}

// Write-once; Default would look like a normal cell.
#[allow(clippy::new_without_default)]
impl<T> BootCell<T> {
    /// An unset cell. `const` outside `cfg(loom)`, whose atomics have no
    /// `const fn new` (C-ATOMICS).
    #[cfg(not(loom))]
    pub const fn new() -> Self {
        boot_cell_new!()
    }

    /// An unset cell (loom's atomics have no `const fn new`).
    #[cfg(loom)]
    pub fn new() -> Self {
        boot_cell_new!()
    }

    /// Place `v` in the cell.
    ///
    /// # Safety
    /// Single writer, before `smp: done`. Must not race `get` / `try_get`.
    pub unsafe fn set(&self, v: T) {
        assert_eq!(
            self.state.load(Ordering::Acquire),
            UNSET,
            "BootCell::set twice"
        );
        // SAFETY: invariant I22: `state` is still UNSET, so no reader has
        // been handed `&T`, and this is the one writer, before `smp: done`;
        // established by `cell::BootCell::set`'s `# Safety` contract.
        unsafe { (*self.data.get()).write(v) };
        self.state.store(SET, Ordering::Release);
    }

    #[allow(
        clippy::expect_used,
        reason = "invariant I22: every `BootCell` the kernel reads is set during boot, before its first reader (`cell::BootCell::set`)"
    )]
    pub fn get(&self) -> &T {
        self.try_get().expect("BootCell unset")
    }

    pub fn try_get(&self) -> Option<&T> {
        if self.state.load(Ordering::Acquire) == SET {
            // SAFETY: invariant I22: SET is stored with Release after the one
            // write, and this Acquire load saw it, so the value is
            // initialized and never written again; established by
            // `cell::BootCell::set`.
            Some(unsafe { (*self.data.get()).assume_init_ref() })
        } else {
            None
        }
    }

    /// Payload address after [`set`]. GDT/TSS must init at this address
    /// so descriptor bases are not a stack temporary.
    #[inline]
    pub fn as_ptr(&self) -> *mut T {
        // `MaybeUninit<T>` is `repr(transparent)`, so the cast keeps the
        // `UnsafeCell`'s provenance and builds no reference.
        self.data.get().cast::<T>()
    }
}

/// A port's policy check before an [`IrqCell`] acquire. The default does
/// nothing; the kernel's refuses a cell inside a lockless section (DESIGN
/// §2.2's last row), as `SpinMutex::lock` does.
pub trait CellHooks {
    /// Runs with interrupts masked, before the owner word is taken.
    #[inline(always)]
    #[track_caller]
    fn acquire_check() {}
}

#[cfg(any(test, feature = "std"))]
impl CellHooks for crate::arch::stub::Arch {}

/// Exclusive `&mut T` with IRQs off. Same-CPU re-entry panics; other CPUs spin.
///
/// Data shared across CPUs as a lock should use `SpinMutex`. `A` is the
/// port: its [`InterruptMask`] masks, and its [`PerCpuBase::cpu_id`] names
/// the owner.
#[repr(C)]
pub struct IrqCell<T, A> {
    data: UnsafeCell<T>,
    /// 0 free, else the owner's `cpu_id + 1`.
    owner: AtomicU32,
    _port: PhantomData<fn() -> A>,
}

// SAFETY: `IrqCell::with` gives `&mut T` to one holder at a time (the
// `owner` compare-exchange, Acquire, and its Release unlock); established
// here. Handing `&mut T` to whichever CPU holds the cell moves `T` between
// threads, so it needs `T: Send`, as `Mutex` does. `A` is only named, never
// held (`PhantomData<fn() -> A>` is `Send` and `Sync` for every `A`); its
// `Send` bound is check_cells' rule that every parameter is bounded.
// `Send` is the auto trait: `UnsafeCell<T>` with that `PhantomData` is
// `Send` exactly when `T: Send`.
unsafe impl<T: Send, A: Send> Sync for IrqCell<T, A> {}

// The layout the core tool reads in the log ring's cell (docs/VMCOREINFO.md):
// `data` at 0, then the owner word right after it, as `#[repr(C)]` places
// them for any payload. `log`'s block asserts `KernelLog`'s size, which with
// these fixes its `owner` at the logger's size. Beside the type because its
// fields are private.
#[cfg(not(loom))]
const _: () = {
    use core::mem::{offset_of, size_of};
    type K = IrqCell<[u64; 3], ()>;
    assert!(offset_of!(K, data) == 0);
    assert!(offset_of!(K, owner) == 24);
    assert!(size_of::<K>() == 32);
};

/// `IrqCell`'s initial value, one body for both constructors.
macro_rules! irq_cell_new {
    ($v:expr) => {
        IrqCell {
            data: UnsafeCell::new($v),
            owner: AtomicU32::new(0),
            _port: PhantomData,
        }
    };
}

impl<T, A> IrqCell<T, A> {
    /// A free cell holding `v`. `const` outside `cfg(loom)`, whose atomics
    /// have no `const fn new` (C-ATOMICS).
    #[cfg(not(loom))]
    pub const fn new(v: T) -> Self {
        irq_cell_new!(v)
    }

    /// A free cell holding `v` (loom's atomics have no `const fn new`).
    #[cfg(loom)]
    pub fn new(v: T) -> Self {
        irq_cell_new!(v)
    }

    /// Address of the payload. `lidt`, and AP `ap_entry` before `GS_BASE`
    /// (the interrupt mask would read per-CPU state).
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
        // Release: pairs with the next holder's Acquire compare-exchange in
        // `with`, as the unlock does.
        self.owner.store(0, Ordering::Release);
    }
}

impl<T, A: InterruptMask + PerCpuBase + CellHooks> IrqCell<T, A> {
    /// Run `f` on the payload with interrupts masked and the cell owned.
    ///
    /// Masks through `A::save_disable`, runs `A::acquire_check`, then takes
    /// the owner word from 0 to this CPU's token with an Acquire
    /// compare-exchange: a same-CPU owner is re-entry and panics, another
    /// CPU's is waited out. The owner word is released (Release) before
    /// interrupts are restored, so an interrupt taken on this CPU after the
    /// restore finds the cell free.
    #[inline(always)]
    #[track_caller]
    #[allow(
        clippy::panic,
        reason = "IrqCell is never re-entered on its owner CPU (`cell::IrqCell::with`): IRQs are off while it is held, so only a kernel bug re-enters"
    )]
    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        /// Restores the mask `save_disable` saved, on every exit.
        struct Unmask<A: InterruptMask>(Option<A::Saved>);
        impl<A: InterruptMask> Drop for Unmask<A> {
            fn drop(&mut self) {
                if let Some(s) = self.0.take() {
                    A::restore(s);
                }
            }
        }
        let _irq = Unmask::<A>(Some(A::save_disable()));
        A::acquire_check();
        let me = owner_token::<A>();
        loop {
            // Acquire: pairs with the Release store of 0 in `Unlock::drop`
            // (or `force_unlock`), so this holder sees the last one's writes.
            match self
                .owner
                .compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(owner) if owner == me => panic!("IrqCell re-entry"),
                Err(_) => crate::atomic::spin_loop(),
            }
        }
        struct Unlock<'a>(&'a AtomicU32);
        impl Drop for Unlock<'_> {
            fn drop(&mut self) {
                // Release: pairs with the next holder's Acquire
                // compare-exchange in `with`. Relaxed only in the log ring
                // loom model's variant (ROADMAP §10.8).
                self.0.store(
                    0,
                    variant::pick(
                        Site::IrqCellUnlockRelaxed,
                        Ordering::Release,
                        Ordering::Relaxed,
                    ),
                );
            }
        }
        // Declared after `_irq`, so it drops first: the owner word is free
        // before interrupts come back on.
        let _u = Unlock(&self.owner);
        // SAFETY: the Acquire compare-exchange above made this CPU the one
        // owner until `_u` drops, so this is the only reference to `data`;
        // established here.
        f(unsafe { &mut *self.data.get() })
    }
}

/// This CPU's owner token: `cpu_id + 1`, never 0. A port's `cpu_id` is 0
/// until its per-CPU base is live, so early boot owns as CPU 0.
fn owner_token<A: PerCpuBase>() -> u32 {
    A::cpu_id().saturating_add(1)
}

/// Fail the build unless `$ty` implements `$tr`.
#[macro_export]
macro_rules! assert_impl {
    ($ty:ty: $tr:path) => {
        const _: () = {
            const fn implements<T: ?Sized + $tr>() {}
            implements::<$ty>();
        };
    };
}

/// Fail the build if `$ty` implements `$tr`.
///
/// `Probe<M>` has a blanket impl for every type at `M = ()` and a second
/// impl at `M = Implements` for the types that implement `$tr`. Naming
/// `<$ty as Probe<_>>` leaves `M` to inference, which succeeds only when
/// exactly one impl applies, so the build fails exactly when `$ty: $tr`.
#[macro_export]
macro_rules! assert_not_impl {
    ($ty:ty: $tr:path) => {
        const _: fn() = || {
            trait Probe<M> {
                fn probe() {}
            }
            impl<T: ?Sized> Probe<()> for T {}
            struct Implements;
            impl<T: ?Sized + $tr> Probe<Implements> for T {}
            <$ty as Probe<_>>::probe();
        };
    };
}

/// The bounds above, checked. The port parameter is `()`, so these build in
/// the kernel with no stub: a raw pointer and `Rc` are neither `Send` nor
/// `Sync`, `Cell` is `Send` but not `Sync`.
mod bounds {
    #![allow(
        clippy::disallowed_types,
        reason = "permanent: `Rc` is only named in compile-time bound checks; nothing is allocated"
    )]

    extern crate alloc;

    use alloc::rc::Rc;
    use core::cell::Cell;

    use super::{BootCell, IrqCell};

    crate::assert_not_impl!(IrqCell<Rc<()>, ()>: Sync);
    crate::assert_not_impl!(IrqCell<*const (), ()>: Sync);
    crate::assert_not_impl!(BootCell<Cell<u8>>: Sync);
    crate::assert_not_impl!(BootCell<Rc<()>>: Send);
    crate::assert_not_impl!(BootCell<*const ()>: Send);
    crate::assert_impl!(IrqCell<Cell<u8>, ()>: Sync);
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::arch::stub::{self, Arch};
    use std::panic::{AssertUnwindSafe, catch_unwind};

    type Cell<T> = IrqCell<T, Arch>;

    #[test]
    fn bootcell_set_get() {
        let c = BootCell::new();
        assert!(c.try_get().is_none());
        // SAFETY: `c` is this test's local, set once, with no other
        // thread; established here.
        unsafe { c.set(9u32) };
        assert_eq!(*c.get(), 9);
        assert_eq!(c.try_get().copied(), Some(9));
    }

    /// Holds in release builds too (DESIGN §9.4): `make check` runs it with
    /// debug assertions off.
    #[test]
    #[should_panic(expected = "BootCell::set twice")]
    fn release_assert_bootcell_set_twice() {
        let c = BootCell::new();
        // SAFETY: `c` is this test's local with no other thread; the second
        // `set` panics on its assert before it writes; established here.
        unsafe {
            c.set(1u32);
            c.set(2u32);
        }
    }

    #[test]
    #[should_panic(expected = "BootCell unset")]
    fn bootcell_get_unset_panics() {
        let c: BootCell<u32> = BootCell::new();
        let _ = c.get();
    }

    #[test]
    fn irqcell_with_mutates() {
        let c = Cell::new(1u32);
        c.with(|v| *v = 4);
        c.with(|v| assert_eq!(*v, 4));
    }

    #[test]
    fn irqcell_reentry_panics() {
        stub::reset();
        let c = Cell::new(0u32);
        let hit = catch_unwind(AssertUnwindSafe(|| {
            c.with(|_| {
                c.with(|_| {});
            });
        }));
        assert!(hit.is_err());
        // SAFETY: the unwind ended both `with` closures, so the recorded
        // holder never touches the payload again; established here.
        unsafe { c.force_unlock() };
        assert!(Arch::enabled(), "the unwind restored the mask");
        c.with(|v| *v = 1);
        assert_eq!(c.with(|v| *v), 1);
    }

    #[test]
    fn irqcell_contended_is_not_reentry() {
        use std::sync::Arc;
        use std::sync::mpsc::channel;
        let c = Arc::new(Cell::new(0u32));
        let (held_tx, held) = channel();
        let (entering_tx, entering) = channel();
        let holder = {
            let c = Arc::clone(&c);
            std::thread::spawn(move || {
                c.with(|v| {
                    held_tx.send(()).unwrap();
                    // Hold the cell until the other thread is about to enter
                    // `with`, then a little longer so it spins on the owner.
                    entering.recv().unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    *v += 1;
                });
            })
        };
        held.recv().unwrap();
        let waiter = {
            let c = Arc::clone(&c);
            std::thread::spawn(move || {
                entering_tx.send(()).unwrap();
                c.with(|v| *v += 1);
            })
        };
        holder.join().unwrap();
        waiter
            .join()
            .expect("a contended cell is taken in turn, not re-entry");
        assert_eq!(c.with(|v| *v), 2);
    }

    #[test]
    fn irqcell_masks_through_port() {
        stub::reset();
        let c = Cell::new(());
        assert!(Arch::enabled());
        c.with(|()| assert!(!Arch::enabled(), "masked inside with"));
        assert!(Arch::enabled(), "restored after with");
        let outer = Arch::save_disable();
        c.with(|()| assert!(!Arch::enabled()));
        assert!(!Arch::enabled(), "a caller's own mask stays");
        Arch::restore(outer);
        assert!(Arch::enabled());
        let ev = stub::take_events();
        assert_eq!(
            ev.as_slice(),
            &[
                stub::Event::IrqsOff,
                stub::Event::IrqsOn,
                stub::Event::IrqsOff,
                stub::Event::IrqsOn,
            ][..]
        );
    }

    /// The four portable hand-off primitives, driven from real host threads
    /// on the stub port, each thread its own CPU (ROADMAP §10.8).
    #[test]
    fn portable_prims_threads() {
        use crate::atomic::AtomicU64;
        use crate::block::DoneWord;
        use crate::irq::ipi::WakeInbox;
        use crate::sched::thread::OnCpu;
        use std::sync::Arc;
        use std::sync::mpsc::channel;
        use std::vec::Vec;

        // IrqCell: four threads, 1,000 increments each, on distinct CPUs.
        let cell = Arc::new(IrqCell::<u64, Arch>::new(0));
        let adders: Vec<_> = (0..4)
            .map(|_| {
                let cell = Arc::clone(&cell);
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        cell.with(|v| *v += 1);
                    }
                    Arch::cpu_id()
                })
            })
            .collect();
        let mut cpus: Vec<u32> = adders.into_iter().map(|a| a.join().unwrap()).collect();
        assert_eq!(cell.with(|v| *v), 4000);
        cpus.sort_unstable();
        cpus.dedup();
        assert_eq!(cpus.len(), 4, "each host thread is its own CPU");

        // WakeInbox: three threads push disjoint thirds of the ids while a
        // fourth, masked, drains until it has every id exactly once.
        const SLOTS: usize = 4 * 64;
        let inbox = Arc::new(WakeInbox::<4>::new());
        let owner = {
            let inbox = Arc::clone(&inbox);
            std::thread::spawn(move || {
                let _masked = Arch::save_disable();
                let mut seen = std::vec![0u32; SLOTS];
                let mut total = 0;
                while total < SLOTS {
                    inbox.drain::<Arch>(|s| {
                        seen[s] += 1;
                        total += 1;
                    });
                    crate::atomic::spin_loop();
                }
                seen
            })
        };
        let pushers: Vec<_> = (0..3)
            .map(|k| {
                let inbox = Arc::clone(&inbox);
                std::thread::spawn(move || {
                    for id in (k..SLOTS).step_by(3) {
                        assert!(inbox.push(id));
                    }
                })
            })
            .collect();
        for p in pushers {
            p.join().unwrap();
        }
        let seen = owner.join().unwrap();
        assert!(seen.iter().all(|&n| n == 1), "every id drained once");
        // A push's summary bit can land after the drain that took its word
        // bit: the next drain clears it and finds nothing.
        {
            let _masked = Arch::save_disable();
            assert!(!inbox.drain::<Arch>(|_| {}));
        }
        assert!(inbox.is_empty());

        // DoneWord: one thread publishes a status while another polls; the
        // poller sees what the publisher wrote before it.
        let done = Arc::new(DoneWord::new());
        let payload = Arc::new(AtomicU64::new(0));
        let poller = {
            let (done, payload) = (Arc::clone(&done), Arc::clone(&payload));
            std::thread::spawn(move || {
                let status = loop {
                    if let Some(s) = done.poll() {
                        break s;
                    }
                    crate::atomic::spin_loop();
                };
                (status, payload.load(Ordering::Relaxed))
            })
        };
        payload.store(42, Ordering::Relaxed);
        done.publish(7);
        assert_eq!(poller.join().unwrap(), (7, 42));

        // OnCpu: one thread sets, saves, then clears the flag while another
        // waits for `is_clear` and sees the save.
        let on_cpu = Arc::new(OnCpu::new());
        let saved = Arc::new(AtomicU64::new(0));
        let (set_tx, set_rx) = channel();
        let switcher = {
            let (on_cpu, saved) = (Arc::clone(&on_cpu), Arc::clone(&saved));
            std::thread::spawn(move || {
                on_cpu.set();
                set_tx.send(()).unwrap();
                saved.store(0x5A7E, Ordering::Relaxed);
                on_cpu.clear();
            })
        };
        let reaper = {
            let (on_cpu, saved) = (Arc::clone(&on_cpu), Arc::clone(&saved));
            std::thread::spawn(move || {
                set_rx.recv().unwrap();
                while !on_cpu.is_clear() {
                    crate::atomic::spin_loop();
                }
                saved.load(Ordering::Relaxed)
            })
        };
        switcher.join().unwrap();
        assert_eq!(reaper.join().unwrap(), 0x5A7E);
    }
}
