//! Lock and sleep assertion tests (ROADMAP §10.3, F108, F110): each trips
//! one assertion under `arch::catch::catch_panic` and reads which one fired
//! from `sync_init::testing::take_trip`.

use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::lock::{Held, RANK_BUDDY};

use crate::arch;
use crate::ktest::{Outcome, sleep_until};
use crate::per_cpu_init;
use crate::sync_init::{self, SpinMutex};
use crate::thread_init;
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
