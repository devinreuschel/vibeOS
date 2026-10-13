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
/// model weakens one site for its run with `weaken`, or runs through
/// [`check`](variant::check), which also fixes the model's bound. A later
/// slice adds one `Site` per variant.
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
        /// `time::TickClock`'s sequence bump is one `fetch_add(1, AcqRel)`
        /// with neither fence.
        SeqlockBumpAcqRel = 3,
        /// `time::TickClock`'s sequence bump drops its leading
        /// `fence(Release)`.
        SeqlockLeadingFence = 4,
        /// `irq::ipi::WakeInbox::push` sets the word's summary bit before
        /// the slot's bit.
        InboxSummaryFirst = 5,
        /// `cell::IrqCell::with` releases the owner word with `Relaxed`.
        IrqCellUnlockRelaxed = 6,
        /// `block::DoneWord::publish` stores the status with `Relaxed`.
        IoDoneRelaxed = 7,
        /// `thread::OnCpu::clear` stores with `Relaxed`.
        OnCpuClearRelaxed = 8,
        /// `kalloc::UsersArc`'s put decrements `users` with `Relaxed`, not
        /// `Release`.
        UsersDecrement = 9,
        /// `kalloc::CoreArc::pin` raises `users` with a blind `fetch_add`,
        /// not get-unless-zero.
        UsersBlindPin = 10,
        /// ASID fast path stores `active_asid` instead of compare-exchange.
        AsidFastStore = 11,
        /// ASID rollover skips reserving the ASIDs it exchanged out.
        AsidSkipReserve = 12,
        /// Call-function publishes the even round with a Relaxed store.
        CallPublishRelaxed = 13,
        /// Call-function acks with a Relaxed `fetch_or`.
        CallAckRelaxed = 14,
        /// Call-function publishes by resetting `acked`, the order a reused
        /// slot gets wrong.
        CallPublishOnAcked = 15,
    }

    /// `kernel` at `site`, or `weak` while a loom model has weakened it.
    #[inline(always)]
    pub fn pick<T>(site: Site, kernel: T, weak: T) -> T {
        #[cfg(loom)]
        if weakened(site) {
            return weak;
        }
        #[cfg(not(loom))]
        let _ = (site, weak);
        kernel
    }

    // The weakened site, or 0. A `std` thread-local, not the seam's (loom's
    // under `cfg(loom)`): loom runs a model's threads on the test's own OS
    // thread, so every model thread sees the run's site, loom schedules
    // nothing on it, and libtest's concurrent tests keep their own.
    #[cfg(loom)]
    std::thread_local! {
        static WEAKENED: core::cell::Cell<u8> = const { core::cell::Cell::new(0) };
    }

    /// True while the running model has weakened `site`.
    #[cfg(loom)]
    pub fn weakened(site: Site) -> bool {
        WEAKENED.with(|w| w.get() == site as u8)
    }

    /// Weaken `site` until the returned guard drops, which also happens as
    /// a failing model unwinds.
    #[cfg(loom)]
    pub fn weaken(site: Site) -> Weakened {
        WEAKENED.with(|w| w.set(site as u8));
        Weakened(())
    }

    /// Restores the kernel's choice at every site when dropped.
    #[cfg(loom)]
    pub struct Weakened(());

    #[cfg(loom)]
    impl Drop for Weakened {
        fn drop(&mut self) {
            WEAKENED.with(|w| w.set(0));
        }
    }

    /// A model's stated bound (ROADMAP §10.8's preamble): the threads it
    /// runs, main included, and the preemptions loom explores per
    /// execution.
    #[cfg(loom)]
    #[derive(Clone, Copy, Debug)]
    pub struct Bound {
        pub threads: usize,
        pub preemptions: usize,
    }

    /// Run `f` as a loom model with `variant`'s site weakened (none for the
    /// base model), exploring every interleaving within `bound`. The bound
    /// is set here, so `LOOM_MAX_PREEMPTIONS`, `LOOM_MAX_PERMUTATIONS` and
    /// `LOOM_MAX_DURATION` cannot change what is checked. The site is
    /// cleared on return and as a failing model unwinds.
    #[cfg(loom)]
    pub fn check(variant: Option<Site>, bound: Bound, f: impl Fn() + Sync + Send + 'static) {
        let _w = variant.map(weaken);
        let mut b = loom::model::Builder::new();
        b.max_threads = bound.threads;
        b.preemption_bound = Some(bound.preemptions);
        b.max_permutations = None;
        b.max_duration = None;
        b.check(f);
    }

    #[cfg(all(test, loom))]
    mod loom_models {
        extern crate std;

        use super::*;
        use crate::atomic::statics::{AtomicUsize, Ordering};
        use loom::thread;

        /// Loom executions that saw `weakened` answer as expected; `core`'s
        /// atomic, outside the model.
        static SEEN: AtomicUsize = AtomicUsize::new(0);

        /// Each execution spawns one loom thread, which reports whether it
        /// sees `site` weakened; every execution must agree with `want`.
        fn reach(variant: Option<Site>, want: bool) {
            SEEN.store(0, Ordering::Relaxed);
            let bound = Bound {
                threads: 2,
                preemptions: 1,
            };
            check(variant, bound, move || {
                let t = thread::spawn(|| weakened(Site::SeqlockBumpAcqRel));
                assert_eq!(t.join().unwrap(), want);
                assert_eq!(weakened(Site::SeqlockBumpAcqRel), want);
                SEEN.fetch_add(1, Ordering::Relaxed);
            });
            assert!(SEEN.load(Ordering::Relaxed) >= 1);
            assert!(!weakened(Site::SeqlockBumpAcqRel), "check clears the site");
        }

        #[test]
        fn loom_variant_reaches_model_threads() {
            reach(Some(Site::SeqlockBumpAcqRel), true);
            reach(None, false);
            reach(Some(Site::OnCpuClearRelaxed), false);
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
        // Relaxed: a snapshot; pairs with nothing.
        self.locked.load(Ordering::Relaxed)
    }

    pub fn owner(&self) -> usize {
        // Relaxed: a snapshot for diagnostics; pairs with nothing.
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
        // Acquire: pairs with the Release store in `release`.
        // Relaxed on failure: pairs with nothing.
        match self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        {
            Ok(_) => {
                // Relaxed: written under the lock; pairs with nothing.
                self.owner.store(owner, Ordering::Relaxed);
                true
            }
            Err(_) => {
                // Relaxed: a match is only this owner's own store; pairs with nothing.
                let held = self.owner.load(Ordering::Relaxed);
                assert!(held != owner, "spin: recursive lock");
                false
            }
        }
    }

    /// Release. Panics if free or `owner` does not hold it.
    pub fn release(&self, owner: usize) {
        // Relaxed: the holder reads its own store; pairs with nothing.
        assert!(
            self.locked.load(Ordering::Relaxed),
            "spin: unlock of free lock"
        );
        // Relaxed: as `locked` above; pairs with nothing.
        let held = self.owner.load(Ordering::Relaxed);
        assert!(held == owner, "spin: unlock by non-owner");
        // Relaxed: the Release store below publishes it; pairs with nothing.
        self.owner.store(UNLOCKED, Ordering::Relaxed);
        // Release: pairs with the Acquire compare-exchange in `try_acquire`.
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

/// An object removed under its operation gate is no longer a device.
impl From<Dead> for crate::kerror::KError {
    fn from(_: Dead) -> Self {
        Self::NoDev
    }
}

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
            // Acquire: pairs with the AcqRel `fetch_or` in `kill_and_wake`.
            if self.state.load(Ordering::Acquire) & DEAD != 0 {
                return Err(Dead);
            }
            // Acquire: pairs with each Release `fetch_sub` in `OpGuard::drop`.
            self.state.fetch_add(1, Ordering::Acquire);
            return Ok(OpGuard::new(self));
        }
        // One read-modify-write both counts this operation in and reads the
        // mark, so it and `kill`'s `fetch_or` are ordered: either `kill`
        // sees this operation inside and waits for it, or this sees the
        // mark. Acquire, as a lock's acquire: the operation's accesses to
        // the object stay after its count-in.
        // Acquire: pairs with `kill_and_wake`'s `fetch_or` and each Release `fetch_sub`.
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
        // AcqRel: pairs with the count-in in `enter_as`. Release, so an
        // operation that finds the mark sees what came before the kill;
        // Acquire, so this reads every count-in ordered before it.
        self.state.fetch_or(DEAD, Ordering::AcqRel);
        wake();
        while self.inside() != 0 {
            gate_sleep(self);
        }
    }

    /// Whether [`kill`](OpGate::kill) has marked the gate dead.
    pub fn is_dead(&self) -> bool {
        // Acquire: pairs with the AcqRel `fetch_or` in `kill_and_wake`.
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
        // Release: pairs with the Acquire load in `OpGate::inside`.
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
    // Release: pairs with the Acquire load in `gate_sleep`.
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
