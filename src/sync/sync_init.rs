//! Kernel locks. ROADMAP §3.5, DESIGN §2.3 / §9.4.
//!
//! `SpinMutex` is IRQ-aware (the only spinlock), with the lock-rank
//! tracker. The blocking primitives, which call the scheduler, are in
//! `sync::blocking_init`.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::panic::Location;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
#[cfg(feature = "kernel_tests")]
use core::sync::atomic::{AtomicU16, AtomicUsize};

use vibeos::lock::{Held, RankError};
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
#[cfg(feature = "kernel_tests")]
pub fn held() -> Held {
    match held_slot() {
        // Relaxed: only this CPU writes its slot.
        Some(h) => Held::from_raw(h.load(Ordering::Relaxed)),
        None => Held::EMPTY,
    }
}

/// Bit `rank - 1` set for each rank this CPU holds.
#[cfg(feature = "kernel_tests")]
pub fn held_mask() -> u8 {
    held().mask()
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
