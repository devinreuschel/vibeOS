//! Kernel locks. ROADMAP §3.5, DESIGN §2.3 / §9.4.
//!
//! `SpinMutex` is IRQ-aware (the only spinlock), with the lock-rank
//! tracker. The blocking primitives, which call the scheduler, are in
//! `sync::blocking_init`.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::panic::Location;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
#[cfg(feature = "kernel_tests")]
use core::sync::atomic::{AtomicU16, AtomicUsize};

#[cfg(feature = "kernel_tests")]
use vibeos::atomic::statics::AtomicU8;

use vibeos::lock::{Held, RANK_SCHED, RankError};
use vibeos::sync::SpinLock;
use vibeos::thread::Tcb;

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

/// Whether the spin-poll hook is set (`sync::ktest`).
#[cfg(feature = "kernel_tests")]
pub(super) fn spin_poll_installed() -> bool {
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

// SAFETY: invariant I232: `data` is reached only through a guard, and one
// guard exists at a time, so sharing the mutex hands one holder at a time
// `&mut T`, which `T: Send` covers (AGENTS.md rule 6); established by
// `sync_init::SpinMutex::lock`.
unsafe impl<T: Send> Sync for SpinMutex<T> {}
// SAFETY: moving the mutex moves the `T` it owns, which `T: Send` allows;
// the `SpinLock` holds only atomics. Established here.
unsafe impl<T: Send> Send for SpinMutex<T> {}

pub struct SpinMutexGuard<'a, T> {
    mutex: &'a SpinMutex<T>,
    owner: usize,
    /// The rank `lock_enter` counted (0 when it counted nothing).
    rank: u8,
    _irq: InterruptGuard,
}

impl<T> SpinMutex<T> {
    pub const fn with_rank(v: T, rank: u8) -> Self {
        Self {
            lock: SpinLock::new(),
            data: UnsafeCell::new(v),
            rank,
        }
    }

    /// Take the lock. The rank checker refuses it while this CPU holds a
    /// lock of its rank or a higher one (DESIGN §2.1, §2.3), or runs code
    /// that takes no lock (§2.2's last row).
    #[track_caller]
    pub fn lock(&self) -> SpinMutexGuard<'_, T> {
        self.lock_counted(false)
    }

    /// Take a second lock of a rank this CPU already holds. `subclass`
    /// (1 or more) is this lock's place in its pair, which a
    /// `// pair order:` comment above the call names (DESIGN §2.3).
    #[track_caller]
    pub fn lock_nested(&self, subclass: u8) -> SpinMutexGuard<'_, T> {
        debug_assert!(subclass >= 1, "lock_nested: subclass 0");
        self.lock_counted(true)
    }

    #[track_caller]
    fn lock_counted(&self, nested: bool) -> SpinMutexGuard<'_, T> {
        let irq = InterruptGuard::enter();
        let owner = owner_token();
        let rank = lock_enter(self.rank, nested);
        while !self.lock.try_acquire(owner) {
            #[cfg(feature = "kernel_tests")]
            record_spin(self.rank);
            spin_poll();
            core::hint::spin_loop();
        }
        SpinMutexGuard {
            mutex: self,
            owner,
            rank,
            _irq: irq,
        }
    }

    /// One shot. `None` if another CPU holds it. The rank checker refuses
    /// it as it does `lock`, before anything else, so a ranked lock this CPU
    /// already holds fails the check; a rank-0 lock this CPU holds returns
    /// `None` without a second CAS (which would panic the TAS).
    #[track_caller]
    pub fn try_lock(&self) -> Option<SpinMutexGuard<'_, T>> {
        let irq = InterruptGuard::enter();
        let owner = owner_token();
        let rank = lock_enter(self.rank, false);
        if self.lock.is_locked() && self.lock.owner() == owner {
            lock_leave(rank);
            return None;
        }
        if self.lock.try_acquire(owner) {
            Some(SpinMutexGuard {
                mutex: self,
                owner,
                rank,
                _irq: irq,
            })
        } else {
            lock_leave(rank);
            None
        }
    }

    /// Whether any CPU, this one included, holds the lock right now. A
    /// snapshot, for tests: another CPU may take or drop it at once.
    #[cfg(feature = "kernel_tests")]
    pub fn is_locked(&self) -> bool {
        self.lock.is_locked()
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
        // SAFETY: invariant I232: this guard holds the lock, so no other
        // reference to `data` exists; established by
        // `sync_init::SpinMutex::lock`.
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T> DerefMut for SpinMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: invariant I232: this guard holds the lock and `&mut self`
        // borrows it uniquely, so this is the only reference to `data`;
        // established by `sync_init::SpinMutex::lock`.
        unsafe { &mut *self.mutex.data.get() }
    }
}

fn owner_token() -> usize {
    match per_cpu_init::try_current() {
        Some(c) => c.cpu_id as usize + 1,
        None => 1,
    }
}

/// Ranks `SPINS` counts (index by rank; 0 unused).
#[cfg(feature = "kernel_tests")]
pub(super) const SPIN_RANKS: usize = 7;

/// Spin iterations per lock rank, which `sync::ktest::spin_counts` reads.
#[cfg(feature = "kernel_tests")]
pub(super) static SPINS: [AtomicU64; SPIN_RANKS] = [const { AtomicU64::new(0) }; SPIN_RANKS];

#[cfg(feature = "kernel_tests")]
fn record_spin(rank: u8) {
    let i = rank as usize;
    if i < SPIN_RANKS {
        SPINS[i].fetch_add(1, Ordering::Relaxed);
    }
}

fn lock_cpu() -> usize {
    match per_cpu_init::try_current() {
        Some(c) => c.cpu_id as usize,
        None => 0,
    }
}

/// Each CPU's held ranks and lockless depth (`vibeos::lock::Held`), in
/// every build. Skipped until GS is live. Changed with `fetch_add` and
/// `fetch_sub`, since an NMI may run between a load and a store.
static HELD: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];

/// This CPU's `HELD` slot, once per-CPU data is live.
fn held_slot() -> Option<&'static AtomicU64> {
    if !per_cpu_init::is_live() {
        return None;
    }
    HELD.get(lock_cpu())
}

/// This CPU's held locks. `Held::EMPTY` before per-CPU data is live.
pub fn held() -> Held {
    match held_slot() {
        // Relaxed: only this CPU writes its slot.
        Some(h) => Held::from_raw(h.load(Ordering::Relaxed)),
        None => Held::EMPTY,
    }
}

/// Bit `rank - 1` set for each rank this CPU holds; 0 before per-CPU
/// data is live. Call with IF off, so the thread stays on the CPU whose
/// slot it reads.
pub fn held_mask() -> u8 {
    held().mask()
}

/// A blocking call from a device's hard-IRQ top half fails at the call
/// (invariant I2): `Sched::begin_wait` and a voluntary `schedule` call it
/// with IF off, where the per-CPU read is stable. In every build.
#[track_caller]
pub fn assert_not_hard_irq() {
    let hard = crate::irq::hardirq::in_hard_irq();
    #[cfg(feature = "kernel_tests")]
    if hard {
        testing::trip(testing::SleepTrip::HardIrq);
    }
    assert!(
        !hard,
        "blocking call in hard-IRQ context at {}",
        Location::caller()
    );
}

/// Whether the checks only debug and `kernel_tests` builds make are on:
/// `HELD` empty and IF on at a sleep (DESIGN §2.9 rule 4).
const SLEEP_CHECKS: bool = cfg!(any(debug_assertions, feature = "kernel_tests"));

/// The context a sleep starts from: hard-IRQ flag, held rank mask, and IF.
/// `with_sched` records one before it takes SCHED, for `Sched::begin_wait`,
/// which runs under SCHED with IF off and cannot read them itself.
#[derive(Clone, Copy)]
pub struct SleepCtx {
    pub hard_irq: bool,
    /// Bit `rank - 1` for each rank held ([`held_mask`]).
    pub held: u8,
    pub if_on: bool,
}

impl SleepCtx {
    /// A context every check passes: what a build without the debug checks
    /// records, and `Sched`'s value before its first `with_sched`.
    pub const UNCHECKED: SleepCtx = SleepCtx {
        hard_irq: false,
        held: 0,
        if_on: true,
    };

    /// This CPU's context now. IF is read first; the flag and `HELD` are
    /// read inside a short `InterruptGuard`, so both name the CPU the
    /// guard pinned, and it drops before anything asserts on them.
    pub fn now() -> SleepCtx {
        let if_on = crate::x86::interrupts_enabled();
        let _irq = InterruptGuard::enter();
        SleepCtx {
            hard_irq: crate::irq::hardirq::in_hard_irq(),
            held: held_mask(),
            if_on,
        }
    }

    /// Assert that a thread in this context may sleep: not in a device's
    /// hard-IRQ top half (invariant I2, every build), and, in debug and
    /// `kernel_tests` builds, holding no ranked lock and with IF on
    /// (invariant I40). The rank and IF checks are off once `HALTING` is
    /// set, as the rank checker is.
    #[track_caller]
    pub fn check(self) {
        #[cfg(feature = "kernel_tests")]
        if self.hard_irq {
            testing::trip(testing::SleepTrip::HardIrq);
        }
        assert!(
            !self.hard_irq,
            "sleeping call in hard-IRQ context at {}",
            Location::caller()
        );
        if !SLEEP_CHECKS || halting() {
            return;
        }
        #[cfg(feature = "kernel_tests")]
        if self.held != 0 {
            testing::trip(testing::SleepTrip::Held);
        }
        assert!(
            self.held == 0,
            "sleeping call holding ranks {:#x} at {}",
            self.held,
            Location::caller()
        );
        #[cfg(feature = "kernel_tests")]
        if !self.if_on {
            testing::trip(testing::SleepTrip::IfOff);
        }
        assert!(
            self.if_on,
            "sleeping call with IF off at {}",
            Location::caller()
        );
    }
}

/// What `with_sched` records for `Sched::begin_wait`: [`SleepCtx::now`] in
/// debug and `kernel_tests` builds, else [`SleepCtx::UNCHECKED`], since
/// `begin_wait` checks the hard-IRQ flag itself in every build.
pub fn sleep_ctx() -> SleepCtx {
    if SLEEP_CHECKS {
        SleepCtx::now()
    } else {
        SleepCtx::UNCHECKED
    }
}

/// Every call that may sleep calls this first, before it takes any lock
/// (DESIGN §2.9 rule 4): not in a device's hard-IRQ top half, and, in
/// debug and `kernel_tests` builds, no ranked lock held and IF on
/// ([`SleepCtx::check`]). A caught panic leaks nothing: the reads' guard
/// has dropped before the assertion.
#[track_caller]
pub fn might_sleep() {
    SleepCtx::now().check();
}

/// A context switch leaves this CPU holding no ranked lock. `HELD` is per
/// CPU, so a lock held across `switch_context` would be charged to the
/// thread that runs next (invariant I1). Called first in
/// `thread_init::switch_now`, with IF off. Off before per-CPU data is live
/// and once `HALTING` is set, as the rank checker is.
#[track_caller]
pub fn assert_switch_clean() {
    if held_slot().is_none() || halting() {
        return;
    }
    let mask = held_mask();
    #[cfg(feature = "kernel_tests")]
    if mask != 0 {
        testing::trip(testing::SleepTrip::SwitchHeld);
    }
    assert!(
        mask == 0,
        "context switch holding ranks {mask:#x} at {}",
        Location::caller()
    );
}

/// Whether this CPU may take `SCHED` now: it is not in a lockless section
/// (DESIGN §2.2's last row: NMI, `#MC`, `#DB`, `service_incoming` work),
/// holds neither `SCHED` nor a lock ranked after it, and the panic path has
/// not set `HALTING`. False before per-CPU data is live, when `HELD` cannot
/// say.
pub(crate) fn may_take_sched() -> bool {
    if halting() {
        return false;
    }
    let Some(slot) = held_slot() else {
        return false;
    };
    // Relaxed: only this CPU writes its slot.
    let held = Held::from_raw(slot.load(Ordering::Relaxed));
    held.acquire(RANK_SCHED).is_ok()
}

/// The current thread's `Tcb`, or `None` before per-CPU data is live and
/// before the bootstrap thread is installed.
fn current_tcb() -> Option<NonNull<Tcb>> {
    NonNull::new(per_cpu_init::try_current()?.current)
}

/// Whether a counted object may be released in place here (DESIGN §2.11
/// rule 6, §4.4 rule 1): IF=1, no ranked lock held on this CPU, and the
/// current thread is not a no-reclaim thread. Before per-CPU data is live
/// IF alone decides. `kalloc::set_release_context` installs it.
pub(crate) fn may_release_here() -> bool {
    if !crate::x86::interrupts_enabled() {
        return false;
    }
    if held_mask() != 0 {
        return false;
    }
    match current_tcb() {
        // SAFETY: invariant I9: a `Tcb` is never freed, so the current
        // thread's pointer stays valid, and `no_reclaim` is atomic because
        // `Sched::get_mut` may build `&mut Tcb` for it on another CPU;
        // established by `thread_init::spawn_inner`.
        Some(t) => unsafe { t.as_ref() }.no_reclaim.load(Ordering::Relaxed) == 0,
        None => true,
    }
}

/// While alive, the thread that made it is a no-reclaim thread: a counted
/// object whose count reaches zero here defers its release (DESIGN §2.11
/// rule 6). Built by [`no_reclaim`]; it names the thread's `Tcb`, so it is
/// `!Send`.
pub(crate) struct NoReclaim {
    tcb: Option<NonNull<Tcb>>,
}

/// Mark the current thread no-reclaim until the returned guard drops.
/// Nesting counts. Nothing before per-CPU data is live.
pub(crate) fn no_reclaim() -> NoReclaim {
    let tcb = current_tcb();
    if let Some(t) = tcb {
        // SAFETY: invariant I9: a `Tcb` is never freed, so the current
        // thread's pointer stays valid, and `no_reclaim` is atomic because
        // `Sched::get_mut` may build `&mut Tcb` for it on another CPU;
        // established by `thread_init::spawn_inner`. Relaxed: only this
        // thread reads its own count.
        unsafe { t.as_ref() }
            .no_reclaim
            .fetch_add(1, Ordering::Relaxed);
    }
    NoReclaim { tcb }
}

impl Drop for NoReclaim {
    fn drop(&mut self) {
        if let Some(t) = self.tcb {
            // SAFETY: invariant I9: a `Tcb` is never freed, so the pointer
            // `no_reclaim` took stays valid; established by
            // `thread_init::spawn_inner`. The guard is `!Send`, so this is
            // the thread whose count `no_reclaim` raised.
            unsafe { t.as_ref() }
                .no_reclaim
                .fetch_sub(1, Ordering::Relaxed);
        }
    }
}

#[cfg(feature = "kernel_tests")]
static RANK_FAILURES: AtomicU64 = AtomicU64::new(0);

#[track_caller]
#[allow(
    clippy::panic,
    reason = "a lock taken out of rank order is a kernel bug, never input: DESIGN §2.3's nesting rule, checked by `vibeos::lock::Held::acquire`"
)]
fn rank_refused(e: RankError, held: Held) -> ! {
    #[cfg(feature = "kernel_tests")]
    RANK_FAILURES.fetch_add(1, Ordering::Relaxed);
    panic!(
        "lock order: {e} (held {:#x}) at {}",
        held.raw(),
        Location::caller()
    );
}

/// Whether the rank checker is off: the panic path, §2.2's stated
/// exception, has set `HALTING`.
fn halting() -> bool {
    crate::serial::raw::HALTING.load(Ordering::Acquire)
}

/// Count one lock of `rank` on this CPU, or panic if the rank checker
/// refuses it. `nested` allows a rank already held (`lock_nested`). A
/// rank-0 lock is not counted but is still refused inside a lockless
/// section. Returns the rank counted, which the guard releases: 0 when
/// nothing was.
#[track_caller]
fn lock_enter(rank: u8, nested: bool) -> u8 {
    let Some(slot) = held_slot() else {
        return 0;
    };
    if halting() {
        return 0;
    }
    // Relaxed: only this CPU writes its slot, with IF off; an NMI that
    // raises the depth lowers it again before it returns.
    let held = Held::from_raw(slot.load(Ordering::Relaxed));
    let r = if nested {
        held.acquire_nested(rank)
    } else {
        held.acquire(rank)
    };
    if let Err(e) = r {
        rank_refused(e, held);
    }
    if rank == 0 {
        return 0;
    }
    let now = slot.fetch_add(Held::count_unit(rank), Ordering::Relaxed);
    #[cfg(feature = "kernel_tests")]
    trace_record(rank, Held::from_raw(now).count(rank) + 1);
    #[cfg(not(feature = "kernel_tests"))]
    let _ = now;
    rank
}

fn lock_leave(rank: u8) {
    if rank == 0 {
        return;
    }
    let Some(slot) = held_slot() else {
        return;
    };
    if Held::from_raw(slot.load(Ordering::Relaxed)).count(rank) != 0 {
        slot.fetch_sub(Held::count_unit(rank), Ordering::Relaxed);
    }
}

/// Refuse an `IrqCell` inside a lockless section: shootdown or
/// call-function work, or an NMI, `#MC`, or CPL-0 `#DB` body (DESIGN §2.2's
/// last row). Off once `HALTING` is set.
#[track_caller]
pub fn check_cell_context() {
    let Some(slot) = held_slot() else {
        return;
    };
    if halting() {
        return;
    }
    let held = Held::from_raw(slot.load(Ordering::Relaxed));
    if let Err(e) = held.check_cell() {
        rank_refused(e, held);
    }
}

/// While alive, this CPU takes no lock: `lock_enter` and `IrqCell::with`
/// refuse (DESIGN §2.2's last row). Built by [`lockless_section`].
pub struct LocklessSection {
    raised: bool,
    /// Per-CPU state: the section ends on the CPU it began on.
    _not_send: core::marker::PhantomData<*const ()>,
}

/// Raise this CPU's lockless depth until the returned guard drops.
/// Nothing before per-CPU data is live.
pub fn lockless_section() -> LocklessSection {
    let raised = held_slot().is_some_and(|slot| {
        // Relaxed: only this CPU changes its slot, and an NMI that raises
        // the depth between this load and the add lowers it again first.
        if Held::from_raw(slot.load(Ordering::Relaxed)).lockless_depth() == u8::MAX {
            return false;
        }
        slot.fetch_add(Held::depth_unit(), Ordering::Relaxed);
        true
    });
    LocklessSection {
        raised,
        _not_send: core::marker::PhantomData,
    }
}

impl Drop for LocklessSection {
    fn drop(&mut self) {
        if !self.raised {
            return;
        }
        if let Some(slot) = held_slot() {
            slot.fetch_sub(Held::depth_unit(), Ordering::Relaxed);
        }
    }
}

/// The CPU whose acquisitions are traced; `usize::MAX` when disarmed.
#[cfg(feature = "kernel_tests")]
static TRACE_CPU: AtomicUsize = AtomicUsize::new(usize::MAX);
#[cfg(feature = "kernel_tests")]
static TRACE_LEN: AtomicUsize = AtomicUsize::new(0);
/// `rank << 8 | count` of each entry.
#[cfg(feature = "kernel_tests")]
static TRACE_RC: [AtomicU16; testing::TRACE_CAP] =
    [const { AtomicU16::new(0) }; testing::TRACE_CAP];
#[cfg(feature = "kernel_tests")]
static TRACE_AT: [AtomicPtr<Location<'static>>; testing::TRACE_CAP] =
    [const { AtomicPtr::new(ptr::null_mut()) }; testing::TRACE_CAP];

/// Record one acquisition if this CPU is armed. Relaxed throughout:
/// only the armed CPU writes, and it reads the result itself.
#[cfg(feature = "kernel_tests")]
#[track_caller]
fn trace_record(rank: u8, count: u8) {
    if TRACE_CPU.load(Ordering::Relaxed) != lock_cpu() {
        return;
    }
    let i = TRACE_LEN.load(Ordering::Relaxed);
    if i >= testing::TRACE_CAP {
        return;
    }
    TRACE_RC[i].store(u16::from(rank) << 8 | u16::from(count), Ordering::Relaxed);
    TRACE_AT[i].store(
        core::ptr::from_ref(Location::caller()).cast_mut(),
        Ordering::Relaxed,
    );
    TRACE_LEN.store(i + 1, Ordering::Relaxed);
}

/// Test access to the rank checker (kernel_tests only).
#[cfg(feature = "kernel_tests")]
pub mod testing {
    use super::*;

    /// This CPU's held locks.
    pub fn held() -> Held {
        super::held()
    }

    /// Put back this CPU's held word after an `arch::catch` longjmp skipped
    /// the guards that would have released it.
    ///
    /// # Safety
    /// `h` is what this CPU held before the skipped acquisitions, and every
    /// lock those acquisitions counted is released or was never taken.
    pub unsafe fn restore_held(h: Held) {
        if let Some(slot) = held_slot() {
            slot.store(h.raw(), Ordering::Relaxed);
        }
    }

    /// Rank-checker refusals since boot.
    pub fn rank_failures() -> u64 {
        RANK_FAILURES.load(Ordering::Relaxed)
    }

    /// Which lock or sleep assertion fired last: the panic message is lost
    /// to `arch::catch::catch_panic`, so a test reads this instead.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum SleepTrip {
        /// `sync_init::might_sleep` or `assert_not_hard_irq`: a sleeping or
        /// blocking call from a device's hard-IRQ top half.
        HardIrq = 2,
        /// `sync_init::SleepCtx::check`: a sleeping call holding a ranked
        /// lock.
        Held = 3,
        /// `sync_init::SleepCtx::check`: a sleeping call with IF off.
        IfOff = 4,
        /// `sync_init::assert_switch_clean`: a ranked lock held across a
        /// context switch.
        SwitchHeld = 1,
    }

    /// The last [`SleepTrip`], 0 when none is recorded.
    static TRIP: AtomicU8 = AtomicU8::new(0);

    /// Record `t` just before its assertion fires.
    pub(super) fn trip(t: SleepTrip) {
        TRIP.store(t as u8, Ordering::Relaxed);
    }

    /// Take the last recorded [`SleepTrip`] and clear it.
    pub fn take_trip() -> Option<SleepTrip> {
        match TRIP.swap(0, Ordering::Relaxed) {
            1 => Some(SleepTrip::SwitchHeld),
            2 => Some(SleepTrip::HardIrq),
            3 => Some(SleepTrip::Held),
            4 => Some(SleepTrip::IfOff),
            _ => None,
        }
    }

    /// Entries one armed trace keeps; later acquisitions are dropped.
    pub const TRACE_CAP: usize = 16;

    /// One counted acquisition: its rank, this CPU's count of that rank
    /// once it was taken, and the call site of `lock`, `try_lock` or
    /// `lock_nested`.
    #[derive(Clone, Copy)]
    pub struct TraceEntry {
        pub rank: u8,
        pub count: u8,
        pub at: &'static Location<'static>,
    }

    /// Trace this CPU's counted acquisitions until [`trace_take`]. Call with
    /// IF off, so the thread stays on this CPU.
    pub fn trace_arm() {
        TRACE_LEN.store(0, Ordering::Relaxed);
        TRACE_CPU.store(lock_cpu(), Ordering::Relaxed);
    }

    /// Disarm the trace and return what it recorded, in order.
    pub fn trace_take() -> [Option<TraceEntry>; TRACE_CAP] {
        TRACE_CPU.store(usize::MAX, Ordering::Relaxed);
        let n = TRACE_LEN.load(Ordering::Relaxed).min(TRACE_CAP);
        core::array::from_fn(|i| {
            if i >= n {
                return None;
            }
            let rc = TRACE_RC[i].load(Ordering::Relaxed);
            let at = TRACE_AT[i].load(Ordering::Relaxed);
            // SAFETY: invariant: a non-null `TRACE_AT` slot holds a
            // `&'static Location` cast to a pointer, and nothing writes
            // through it; established by `sync_init::trace_record`, its only
            // store.
            let at: &'static Location<'static> = unsafe { at.as_ref() }?;
            Some(TraceEntry {
                rank: (rc >> 8) as u8,
                count: rc as u8,
                at,
            })
        })
    }
}
