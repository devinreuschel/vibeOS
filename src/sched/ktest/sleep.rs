//! Lock and sleep assertion tests (ROADMAP §10.3, F108, F110): each trips
//! one assertion under `arch::catch::catch_panic` and reads which one fired
//! from `sync_init::testing::take_trip`.

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use vibeos::lock::{Held, RANK_BUDDY};
use vibeos::time::Instant;

use crate::apic_init;
use crate::arch;
use crate::irq::hardirq;
use crate::irq_init;
use crate::ktest::{Outcome, sleep_until, spin_until_ns};
use crate::per_cpu_init;
use crate::sync::blocking_init::{BlockingMutex, RwLock, Semaphore};
use crate::sync_init::{self, SpinMutex};
use crate::thread_init;
use crate::time_init;
use crate::x86;

static SWITCH_HELPER_RAN: AtomicBool = AtomicBool::new(false);

fn switch_helper_entry() {
    SWITCH_HELPER_RAN.store(true, Ordering::Release);
}

/// This CPU's `irq_nest`, IF and held-lock word, which a caught assertion
/// leaves changed: `arch::catch::catch_panic`'s longjmp skips every guard.
struct Saved {
    nest: u32,
    if_on: bool,
    held: Held,
}

impl Saved {
    fn now() -> Self {
        Self {
            nest: per_cpu_init::irq_nest(),
            if_on: x86::interrupts_enabled(),
            held: sync_init::testing::held(),
        }
    }

    /// Put back the held word and set `irq_nest` to `nest`, leaving IF
    /// off. Call with IF off, on the CPU `now` ran on.
    fn restore_locks(&self, nest: u32) {
        per_cpu_init::current()
            .irq_nest
            .store(nest, Ordering::Relaxed);
        // SAFETY: `self.held` is what this CPU held before the caught
        // call, and every lock that call counted is a test-local `SpinMutex`
        // that is never unlocked or used again or one whose guard released
        // it before the assertion; established by
        // `sched::ktest::sleep::lock_across_switch_asserts` and the other callers.
        unsafe { sync_init::testing::restore_held(self.held) };
    }

    fn check(&self) -> Result<(), Outcome> {
        if per_cpu_init::irq_nest() != self.nest || x86::interrupts_enabled() != self.if_on {
            return Err(crate::fail_fmt!(
                "irq_nest {} IF {}, want {} {}",
                per_cpu_init::irq_nest(),
                u8::from(x86::interrupts_enabled()),
                self.nest,
                u8::from(self.if_on)
            ));
        }
        if sync_init::testing::held() != self.held {
            return Err(Outcome::Fail("held word changed"));
        }
        Ok(())
    }
}

/// ROADMAP §10.3 (F108): a yield while holding a ranked `SpinMutex` trips
/// `sync_init::assert_switch_clean` in `thread_init::switch_now`.
pub(crate) fn lock_across_switch_asserts() -> Outcome {
    use sync_init::testing::SleepTrip;
    SWITCH_HELPER_RAN.store(false, Ordering::Release);
    let saved = Saved::now();
    // IF off from the spawn to the yield, so no tick runs the helper first:
    // a lone thread's yield re-picks it and never reaches `switch_now`.
    let g = x86::InterruptGuard::enter();
    if thread_init::spawn_here("switch_helper", switch_helper_entry).is_err() {
        return Outcome::Fail("spawn");
    }
    let nest = per_cpu_init::irq_nest();
    let _ = sync_init::testing::take_trip();
    let _ = thread_init::testing::take_refused_switch();
    // Ranked below SCHED, so the rank order check stays quiet.
    let m = SpinMutex::with_rank(0u32, RANK_BUDDY);
    let hit = arch::catch::catch_panic(|| {
        let _g = m.lock();
        thread_init::yield_now();
    });
    let trip = sync_init::testing::take_trip();
    let refused = thread_init::testing::take_refused_switch();
    saved.restore_locks(nest);
    // `schedule_inner` set the refused thread Running and took it off the
    // run queue before `switch_now`: switch to it to undo that.
    if let Some(id) = refused {
        thread_init::switch_to(id);
    }
    drop(g);
    let ran = sleep_until(|| SWITCH_HELPER_RAN.load(Ordering::Acquire), 1_000);
    if !hit {
        return Outcome::Fail("no panic");
    }
    if trip != Some(SleepTrip::SwitchHeld) {
        return crate::fail_fmt!("trip {:?}, want SwitchHeld", trip);
    }
    if refused.is_none() {
        return Outcome::Fail("no refused switch recorded");
    }
    if !ran {
        return Outcome::Fail("helper did not run");
    }
    if let Err(o) = saved.check() {
        return o;
    }
    Outcome::Ok
}

/// Clears this CPU's `IN_ISR` when dropped, so a test that sets it cannot
/// leave it set on an early return.
struct IsrFlag;

impl IsrFlag {
    /// Set this CPU's `IN_ISR`. IF must be off until the flag drops.
    fn set() -> Self {
        hardirq::testing::set_in_isr(true);
        IsrFlag
    }
}

impl Drop for IsrFlag {
    fn drop(&mut self) {
        hardirq::testing::set_in_isr(false);
    }
}

/// ROADMAP §10.3 (F110): with `IN_ISR` set, `park` and a voluntary
/// `yield_now` fail at the call.
pub(crate) fn block_in_hard_irq_asserts() -> Outcome {
    use sync_init::testing::SleepTrip;
    let saved = Saved::now();
    let g = x86::InterruptGuard::enter();
    let nest = per_cpu_init::irq_nest();
    let _ = sync_init::testing::take_trip();
    let flag = IsrFlag::set();
    let deadline = Instant {
        ns: time_init::now_ns().saturating_add(1_000_000),
    };
    let park_hit = arch::catch::catch_panic(|| thread_init::park(Some(deadline)));
    let park_trip = sync_init::testing::take_trip();
    saved.restore_locks(nest);
    let yield_hit = arch::catch::catch_panic(thread_init::yield_now);
    let yield_trip = sync_init::testing::take_trip();
    // `schedule_inner`'s guard never dropped.
    saved.restore_locks(nest);
    drop(flag);
    let still = irq_init::in_hard_irq();
    drop(g);
    for (what, hit, trip) in [
        ("park", park_hit, park_trip),
        ("yield_now", yield_hit, yield_trip),
    ] {
        if !hit {
            return crate::fail_fmt!("{what}: no panic");
        }
        if trip != Some(SleepTrip::HardIrq) {
            return crate::fail_fmt!("{what}: trip {:?}, want HardIrq", trip);
        }
    }
    if still {
        return Outcome::Fail("IN_ISR still set");
    }
    if let Err(o) = saved.check() {
        return o;
    }
    Outcome::Ok
}

/// What `irq_init::in_hard_irq` said in the top half (`TOP_HARD`) and the
/// bottom half (`BOTTOM_HARD`): 0 not run, 1 false, 2 true.
static TOP_HARD: AtomicU8 = AtomicU8::new(0);
static BOTTOM_HARD: AtomicU8 = AtomicU8::new(0);

fn hard_state() -> u8 {
    if irq_init::in_hard_irq() { 2 } else { 1 }
}

fn top_half() {
    TOP_HARD.store(hard_state(), Ordering::Release);
}

fn bottom_half() {
    BOTTOM_HARD.store(hard_state(), Ordering::Release);
}

/// ROADMAP §10.3 (F110): `in_hard_irq()` is true in a device top half and
/// false in its threaded bottom half. A self-IPI on an allocated vector
/// enters through the same stub and `irq_init::dispatch` as a device's MSI.
pub(crate) fn in_hard_irq_top_bottom() -> Outcome {
    TOP_HARD.store(0, Ordering::Release);
    BOTTOM_HARD.store(0, Ordering::Release);
    let g = x86::InterruptGuard::enter();
    let me = thread_init::current_cpu();
    let v = match irq_init::allocate_vector(me) {
        Ok(v) => v,
        Err(e) => return Outcome::Fail(e.as_str()),
    };
    if irq_init::set_threaded(v, Some(top_half), bottom_half).is_err() {
        let _ = irq_init::free_vector(v);
        return Outcome::Fail("set_threaded");
    }
    if apic_init::send_ipi_cpu(me, v).is_err() {
        let _ = irq_init::free_vector(v);
        return Outcome::Fail("send_ipi_cpu");
    }
    drop(g);
    let done = spin_until_ns(
        || TOP_HARD.load(Ordering::Acquire) != 0 && BOTTOM_HARD.load(Ordering::Acquire) != 0,
        500_000_000,
    );
    if irq_init::free_vector(v).is_err() {
        return Outcome::Fail("free_vector");
    }
    if !done {
        return crate::fail_fmt!(
            "top {} bottom {}: a half did not run",
            TOP_HARD.load(Ordering::Acquire),
            BOTTOM_HARD.load(Ordering::Acquire)
        );
    }
    match (
        TOP_HARD.load(Ordering::Acquire),
        BOTTOM_HARD.load(Ordering::Acquire),
    ) {
        (2, 1) => Outcome::Ok,
        (t, b) => crate::fail_fmt!("in_hard_irq: top {t} bottom {b}, want 2 1"),
    }
}

static SLEEP_MUTEX: BlockingMutex<u32> = BlockingMutex::new(0);
static SLEEP_RW: RwLock<u32> = RwLock::new(0);
static SLEEP_SEM: Semaphore = Semaphore::new(1);

fn sleep_mutex_lock() {
    drop(SLEEP_MUTEX.lock());
}

fn sleep_rw_read() {
    drop(SLEEP_RW.read());
}

fn sleep_rw_write() {
    drop(SLEEP_RW.write());
}

fn sleep_sem_acquire() {
    SLEEP_SEM.acquire();
    SLEEP_SEM.release();
}

fn sleep_park() {
    thread_init::park(Some(Instant {
        ns: time_init::now_ns().saturating_add(1_000_000),
    }));
}

/// ROADMAP §10.3 (F108, F075): every call that may sleep fails its check
/// under a ranked `SpinMutex`, and `BlockingMutex::lock` fails it with IF
/// off. Each check fires before the call takes anything.
pub(crate) fn sleep_under_spinlock_asserts() -> Outcome {
    use sync_init::testing::SleepTrip;
    let calls: [(&str, fn()); 5] = [
        ("BlockingMutex::lock", sleep_mutex_lock),
        ("RwLock::read", sleep_rw_read),
        ("RwLock::write", sleep_rw_write),
        ("Semaphore::acquire", sleep_sem_acquire),
        ("park", sleep_park),
    ];
    let saved = Saved::now();
    let _ = sync_init::testing::take_trip();
    for (what, call) in calls {
        // A fresh lock each time: the longjmp leaves it locked.
        let s = SpinMutex::with_rank((), RANK_BUDDY);
        let nest = per_cpu_init::irq_nest();
        let hit = arch::catch::catch_panic(|| {
            let _s = s.lock();
            call();
        });
        let trip = sync_init::testing::take_trip();
        saved.restore_locks(nest);
        if saved.if_on {
            x86::sti();
        }
        if !hit {
            return crate::fail_fmt!("{what}: no panic under a spinlock");
        }
        if trip != Some(SleepTrip::Held) {
            return crate::fail_fmt!("{what}: trip {:?}, want Held", trip);
        }
    }
    let g = x86::InterruptGuard::enter();
    let nest = per_cpu_init::irq_nest();
    let hit = arch::catch::catch_panic(sleep_mutex_lock);
    let trip = sync_init::testing::take_trip();
    saved.restore_locks(nest);
    drop(g);
    if !hit {
        return Outcome::Fail("BlockingMutex::lock: no panic with IF off");
    }
    if trip != Some(SleepTrip::IfOff) {
        return crate::fail_fmt!("BlockingMutex::lock: trip {:?}, want IfOff", trip);
    }
    // The assertions fired before anything was acquired.
    match SLEEP_MUTEX.try_lock() {
        Some(g) => drop(g),
        None => return Outcome::Fail("BlockingMutex left locked"),
    }
    if let Err(o) = saved.check() {
        return o;
    }
    Outcome::Ok
}
