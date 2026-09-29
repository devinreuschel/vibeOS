//! IRQ-aware spinlock core. DESIGN §2.3, ROADMAP §3.5.
//!
//! Portable CAS + owner tracking. The kernel wraps this in
//! [`InterruptGuard`] so every acquire runs with IF off. Host tests
//! drive it with a fake owner token.
//!
//! Also DESIGN §2.11 rule 3's operation gate, [`OpGate`], whose `kill`
//! sleeps through the hooks the kernel installs ([`set_gate_wait`]).

pub mod lock;

/// The loom variant switch (C-LOOM, ROADMAP §10.8).
///
/// A loom model's failing variant runs the kernel's own code with one site
/// weakened, and loom must find the race that opens. Each weakenable site
/// is a [`Site`](variant::Site); the code there takes
/// [`pick`](variant::pick)'s answer, which outside `cfg(loom)` is always
/// the kernel's choice, so no build but a model's can take the other. A
/// model weakens one site for its run with `weaken`. A later slice adds
/// one `Site` per variant.
pub mod variant {
    /// A site a loom variant weakens.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[repr(u8)]
    pub enum Site {
        /// `kalloc`'s `TryArc` put decrements with `Relaxed`, not `Release`.
        TryArcDecrement = 1,
        /// `sync::OpGate::enter` reads the dead mark before it counts
        /// itself in.
        OpGateCountFirst = 2,
    }

    /// `kernel` at `site`, or `weak` while a loom model has weakened it.
    #[inline(always)]
    pub fn pick<T>(site: Site, kernel: T, weak: T) -> T {
        #[cfg(loom)]
        if WEAKENED.load(crate::atomic::statics::Ordering::Relaxed) == site as u8 {
            return weak;
        }
        #[cfg(not(loom))]
        let _ = (site, weak);
        kernel
    }

    /// The weakened site, or 0. `core`'s atomic: the switch is set outside
    /// the model's threads, and loom must not schedule it.
    #[cfg(loom)]
    static WEAKENED: crate::atomic::statics::AtomicU8 = crate::atomic::statics::AtomicU8::new(0);

    /// Weaken `site` until the returned guard drops, which also happens as
    /// a failing model unwinds.
    #[cfg(loom)]
    pub fn weaken(site: Site) -> Weakened {
        WEAKENED.store(site as u8, crate::atomic::statics::Ordering::Relaxed);
        Weakened(())
    }

    /// Restores the kernel's choice at every site when dropped.
    #[cfg(loom)]
    pub struct Weakened(());

    #[cfg(loom)]
    impl Drop for Weakened {
        fn drop(&mut self) {
            WEAKENED.store(0, crate::atomic::statics::Ordering::Relaxed);
        }
    }
}

use core::marker::PhantomData;
use core::ptr::NonNull;

use crate::atomic::{AtomicBool, AtomicUsize, Ordering, spin_loop, statics};

/// 0 means unlocked. Owners are never 0.
pub const UNLOCKED: usize = 0;

pub struct SpinLock {
    locked: AtomicBool,
    owner: AtomicUsize,
}

impl SpinLock {
    /// `const`, so a kernel `static` `SpinMutex` can hold one. Loom's
    /// atomics have no `const fn new` (C-ATOMICS), so a model builds it at
    /// run time.
    #[cfg(not(loom))]
    pub const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            owner: AtomicUsize::new(UNLOCKED),
        }
    }

    #[cfg(loom)]
    pub fn new() -> Self {
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
        loop {
            if self.try_acquire(owner) {
                return;
            }
            spin_loop();
        }
    }

    /// One CAS. `false` if held by someone else. Panics on recurse.
    pub fn try_acquire(&self, owner: usize) -> bool {
        assert!(owner != UNLOCKED, "spin: owner 0 is reserved");
        match self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        {
            Ok(_) => {
                self.owner.store(owner, Ordering::Relaxed);
                true
            }
            Err(_) => {
                let held = self.owner.load(Ordering::Relaxed);
                assert!(held != owner, "spin: recursive lock");
                false
            }
        }
    }

    /// Release. Panics if free or `owner` does not hold it.
    pub fn release(&self, owner: usize) {
        assert!(
            self.locked.load(Ordering::Relaxed),
            "spin: unlock of free lock"
        );
        let held = self.owner.load(Ordering::Relaxed);
        assert!(held == owner, "spin: unlock by non-owner");
        self.owner.store(UNLOCKED, Ordering::Relaxed);
        self.locked.store(false, Ordering::Release);
    }
}

impl Default for SpinLock {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Operation gate (DESIGN §2.11 rule 3)

/// The dead mark in [`OpGate`]'s state word; the bits below it count the
/// operations inside.
const DEAD: usize = 1 << (usize::BITS - 1);

/// [`OpGate::enter`] failed: the gate has been killed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dead;

/// DESIGN §2.11 rule 3's operation gate: the count of operations in
/// progress on an object that can be removed while references to it
/// remain, and a dead mark. Every operation through a reference
/// [`enter`](OpGate::enter)s and leaves; once [`kill`](OpGate::kill) has
/// marked the gate dead every new `enter` fails, and `kill` returns only
/// after the last operation inside has left. One atomic word holds both.
pub struct OpGate {
    state: AtomicUsize,
}

impl OpGate {
    /// An open gate with no operation inside. `const`, so a kernel `static`
    /// can hold one; loom's atomics have no `const fn new` (C-ATOMICS).
    #[cfg(not(loom))]
    pub const fn new() -> Self {
        Self {
            state: AtomicUsize::new(0),
        }
    }

    #[cfg(loom)]
    pub fn new() -> Self {
        Self {
            state: AtomicUsize::new(0),
        }
    }

    /// Count one operation in, or fail with [`Dead`] once the gate is
    /// killed. The guard leaves on [`OpGuard::exit`] or drop.
    pub fn enter(&self) -> Result<OpGuard<'_>, Dead> {
        if variant::pick(variant::Site::OpGateCountFirst, true, false) {
            self.enter_as::<true>()
        } else {
            self.enter_as::<false>()
        }
    }

    /// `enter`, counting in before it reads the dead mark (`COUNT_FIRST`),
    /// which the kernel always does. The other order, reading the mark and
    /// then counting in, lets a `kill` between the two see no operation
    /// inside and return while this one proceeds; only a loom variant
    /// takes it (`variant::Site::OpGateCountFirst`).
    fn enter_as<const COUNT_FIRST: bool>(&self) -> Result<OpGuard<'_>, Dead> {
        if !COUNT_FIRST {
            if self.state.load(Ordering::Acquire) & DEAD != 0 {
                return Err(Dead);
            }
            self.state.fetch_add(1, Ordering::Acquire);
            return Ok(OpGuard::new(self));
        }
        // One read-modify-write both counts this operation in and reads the
        // mark, so it and `kill`'s `fetch_or` are ordered: either `kill`
        // sees this operation inside and waits for it, or this sees the
        // mark. Acquire, as a lock's acquire: the operation's accesses to
        // the object stay after its count-in.
        let old = self.state.fetch_add(1, Ordering::Acquire);
        let g = OpGuard::new(self);
        if old & DEAD != 0 {
            g.exit();
            return Err(Dead);
        }
        Ok(g)
    }

    /// Mark the gate dead and wait until no operation is inside.
    pub fn kill(&self) {
        self.kill_and_wake(|| {});
    }

    /// Mark the gate dead, run `wake`, which wakes every thread sleeping on
    /// the object so each one rechecks the gate and fails (DESIGN §2.11
    /// rule 3), then sleep until no operation is inside, through the sleep
    /// [`set_gate_wait`] installs. It may sleep, so only where DESIGN §2.9
    /// allows a sleep.
    pub fn kill_and_wake(&self, wake: impl FnOnce()) {
        // AcqRel: Release, so an operation that finds the mark sees what
        // came before the kill; Acquire, so this reads every count-in
        // ordered before it.
        self.state.fetch_or(DEAD, Ordering::AcqRel);
        wake();
        while self.inside() != 0 {
            gate_sleep(self);
        }
    }

    /// Whether [`kill`](OpGate::kill) has marked the gate dead.
    pub fn is_dead(&self) -> bool {
        self.state.load(Ordering::Acquire) & DEAD != 0
    }

    /// Operations inside now. Acquire pairs with each leaver's Release
    /// decrement, so when this reads 0 every operation's accesses happen
    /// before the reader's next step.
    pub fn inside(&self) -> usize {
        self.state.load(Ordering::Acquire) & !DEAD
    }
}

#[cfg(not(loom))]
impl Default for OpGate {
    fn default() -> Self {
        Self::new()
    }
}

/// One operation inside an [`OpGate`]. It leaves on [`exit`](Self::exit)
/// or drop. It keeps the gate as a pointer, so no reference to the gate is
/// live across its final decrement, after which the owner may free it
/// (AGENTS.md rule 5).
#[must_use = "an OpGuard leaves its gate when dropped; hold it for the operation"]
pub struct OpGuard<'a> {
    gate: NonNull<OpGate>,
    _gate: PhantomData<&'a OpGate>,
}

impl<'a> OpGuard<'a> {
    fn new(gate: &'a OpGate) -> Self {
        Self {
            gate: NonNull::from(gate),
            _gate: PhantomData,
        }
    }

    /// Leave the gate.
    pub fn exit(self) {
        drop(self);
    }
}

impl Drop for OpGuard<'_> {
    fn drop(&mut self) {
        let g = self.gate.as_ptr();
        // SAFETY: the guard borrows the gate for `'a`, so it is live here;
        // established by `sync::OpGuard::new`. Only the atomic is reached,
        // through a shared reference that ends with this call.
        let old = unsafe { (*g).state.fetch_sub(1, Ordering::Release) };
        // The gate is not touched again: once the count reaches zero its
        // killer may return and its owner free it. The wake names no gate.
        if old == DEAD | 1 {
            gate_wake();
        }
    }
}

/// The kernel's sleep for [`OpGate::kill`]: `fn(&OpGate)`, or null for a
/// spin.
static GATE_SLEEP: statics::AtomicPtr<()> = statics::AtomicPtr::new(core::ptr::null_mut());
/// The kernel's wake for the last operation out of a dead gate: `fn()`, or
/// null for none.
static GATE_WAKE: statics::AtomicPtr<()> = statics::AtomicPtr::new(core::ptr::null_mut());

// A hook slot holds a `fn` pointer as a data pointer.
const _: () = assert!(core::mem::size_of::<fn(&OpGate)>() == core::mem::size_of::<*mut ()>());
const _: () = assert!(core::mem::size_of::<fn()>() == core::mem::size_of::<*mut ()>());

/// Install the kernel's sleep and wake for [`OpGate::kill`] (C-COUNTED):
/// `sleep` returns once the gate may have emptied, or after a bounded wait,
/// and `wake` wakes the sleepers. Until they are installed `kill` spins.
pub fn set_gate_wait(sleep: fn(&OpGate), wake: fn()) {
    // Release: pairs with the Acquire loads in `gate_sleep` and `gate_wake`.
    GATE_WAKE.store(wake as *mut (), statics::Ordering::Release);
    GATE_SLEEP.store(sleep as *mut (), statics::Ordering::Release);
}

fn gate_sleep(g: &OpGate) {
    // Acquire: pairs with the Release store in `set_gate_wait`.
    let p = GATE_SLEEP.load(statics::Ordering::Acquire);
    if p.is_null() {
        spin_loop();
        return;
    }
    // SAFETY: invariant: a non-null `GATE_SLEEP` holds a `fn(&OpGate)`,
    // and a `fn` pointer is pointer-sized (the const assertion above);
    // established by `sync::set_gate_wait`, its only store.
    let f = unsafe { core::mem::transmute::<*mut (), fn(&OpGate)>(p) };
    f(g);
}

fn gate_wake() {
    // Acquire: pairs with the Release store in `set_gate_wait`.
    let p = GATE_WAKE.load(statics::Ordering::Acquire);
    if p.is_null() {
        return;
    }
    // SAFETY: invariant: a non-null `GATE_WAKE` holds a `fn()`, and a `fn`
    // pointer is pointer-sized (the const assertion above); established by
    // `sync::set_gate_wait`, its only store.
    let f = unsafe { core::mem::transmute::<*mut (), fn()>(p) };
    f();
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
    fn try_acquire_fails_when_held() {
        let l = SpinLock::new();
        assert!(l.try_acquire(1));
        assert!(!l.try_acquire(2));
        l.release(1);
        assert!(l.try_acquire(2));
        l.release(2);
    }

    #[test]
    fn reacquire_changes_owner() {
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

#[cfg(all(test, not(loom)))]
mod opgate_tests {
    extern crate std;

    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool as StdAtomicBool, Ordering as StdOrdering};
    use std::thread;
    use std::time::{Duration, Instant};

    /// Spin until `f` holds, for at most 5 s.
    fn wait_for(f: impl Fn() -> bool) -> bool {
        let t0 = Instant::now();
        while !f() {
            if t0.elapsed() > Duration::from_secs(5) {
                return false;
            }
            thread::yield_now();
        }
        true
    }

    #[test]
    fn opgate_enter_after_kill_fails() {
        let gate = OpGate::new();
        assert!(!gate.is_dead());
        let g = gate.enter().expect("an open gate refused");
        assert_eq!(gate.inside(), 1);
        g.exit();
        assert_eq!(gate.inside(), 0);
        gate.kill();
        assert!(gate.is_dead());
        assert!(matches!(gate.enter(), Err(Dead)));
        assert_eq!(gate.inside(), 0, "a failed enter stayed inside");
        assert!(matches!(gate.enter(), Err(Dead)));
    }

    #[test]
    fn opgate_kill_returns_after_last_exit() {
        let gate = Arc::new(OpGate::new());
        let release = Arc::new(StdAtomicBool::new(false));
        let exited = Arc::new(StdAtomicBool::new(false));
        let returned = Arc::new(StdAtomicBool::new(false));
        let holder = {
            let (gate, release, exited) = (gate.clone(), release.clone(), exited.clone());
            thread::spawn(move || {
                let g = gate.enter().expect("an open gate refused");
                assert!(wait_for(|| release.load(StdOrdering::SeqCst)));
                exited.store(true, StdOrdering::SeqCst);
                g.exit();
            })
        };
        assert!(wait_for(|| gate.inside() == 1));
        let killer = {
            let (gate, exited, returned) = (gate.clone(), exited.clone(), returned.clone());
            thread::spawn(move || {
                gate.kill();
                returned.store(true, StdOrdering::SeqCst);
                exited.load(StdOrdering::SeqCst)
            })
        };
        assert!(wait_for(|| gate.is_dead()));
        assert!(matches!(gate.enter(), Err(Dead)));
        thread::sleep(Duration::from_millis(50));
        assert!(
            !returned.load(StdOrdering::SeqCst),
            "kill returned with an operation inside"
        );
        release.store(true, StdOrdering::SeqCst);
        holder.join().unwrap();
        assert!(killer.join().unwrap(), "kill returned before the last exit");
        assert_eq!(gate.inside(), 0);
    }

    #[test]
    fn opgate_kill_wakes_before_it_waits() {
        // The holder leaves only once the killer's wake has run, so a kill
        // that waited before it woke would never return.
        let gate = Arc::new(OpGate::new());
        let woken = Arc::new(StdAtomicBool::new(false));
        let holder = {
            let (gate, woken) = (gate.clone(), woken.clone());
            thread::spawn(move || {
                let g = gate.enter().expect("an open gate refused");
                let ok = wait_for(|| woken.load(StdOrdering::SeqCst));
                g.exit();
                ok
            })
        };
        assert!(wait_for(|| gate.inside() == 1));
        gate.kill_and_wake(|| {
            assert!(gate.is_dead(), "woken before the gate was marked dead");
            woken.store(true, StdOrdering::SeqCst);
        });
        assert!(holder.join().unwrap(), "kill waited before it woke");
        assert_eq!(gate.inside(), 0);
    }
}

#[cfg(all(test, loom))]
mod loom_models {
    extern crate std;

    use super::*;
    use loom::cell::UnsafeCell;
    use loom::sync::Arc;
    use loom::thread;

    /// A gate and the object state it guards.
    struct Shared {
        gate: OpGate,
        cell: UnsafeCell<u32>,
    }

    // SAFETY: `cell` is read only inside the gate and written only after
    // `kill` returns; the models check, through loom, that the gate orders
    // the two. Established here.
    unsafe impl Sync for Shared {}

    /// A reads the state inside the gate; B kills the gate, then writes the
    /// state as a teardown would.
    fn opgate_model() {
        loom::model(|| {
            let s = Arc::new(Shared {
                gate: OpGate::new(),
                cell: UnsafeCell::new(0),
            });
            let a = {
                let s = s.clone();
                thread::spawn(move || {
                    if let Ok(g) = s.gate.enter() {
                        // SAFETY: inside the gate, `kill` has not returned,
                        // so no teardown writes `cell`; established by
                        // `sync::OpGate::kill_and_wake`.
                        let _v = s.cell.with(|p| unsafe { *p });
                        g.exit();
                    }
                })
            };
            s.gate.kill();
            // SAFETY: `kill` returned, so no operation is inside and none
            // can enter; established by `sync::OpGate::kill_and_wake`.
            s.cell.with_mut(|p| unsafe { *p = 1 });
            a.join().unwrap();
        });
    }

    #[test]
    fn loom_opgate() {
        opgate_model();
    }

    #[test]
    #[should_panic(expected = "Causality violation")]
    fn loom_opgate_check_first_fails() {
        let _w = variant::weaken(variant::Site::OpGateCountFirst);
        opgate_model();
    }
}
