//! In-guest test of the timer's rearm and rate (kernel_tests only).
//! Rows: the parent `ktest.rs`'s `TESTS`.

use vibeos::apic::TimerMode;

use crate::apic_init;
use crate::ktest::Outcome;
use crate::time::ktest::reload_period;
use crate::time_init;

/// PIT interrupts [`test_lapic_timer_rearm`] waits for when the PIT
/// drives the tick.
const REARM_FIRES: u64 = 20;

/// The timer keeps firing across many ticks, at the tick the kernel
/// programs. Each fire comes only if the tick before it left the timer
/// armed (`rearm_deadline` in TSC-deadline mode, the periodic reload
/// otherwise), and the bounds come from what the kernel controls and the
/// timer's own count, not from when the host delivers its interrupts
/// (ROADMAP §10.2):
/// - periodic mode: this CPU's LAPIC holds the LVT, initial count and
///   divide the kernel programs for one tick, and the reload period its
///   current-count register shows against the TSC ([`reload_period`]) lies
///   between half a tick and two;
/// - TSC-deadline mode: every fire CPU 0 takes in the window rearmed the
///   next one tick, `tsc_per_ms`, ahead, and the median of the 20
///   intervals between the 21 fires, in TSC cycles, lies between half a
///   tick and two. Only an accelerator offers TSC-deadline (TCG's CPU
///   models have none), where the guest's deadline fires the interrupt
///   itself, so these intervals do not wait on QEMU's main loop as the
///   periodic mode's do under TCG;
/// - both: CPU 0 takes 21 fires by the run's deadline.
///
/// When the PIT drives the tick (no HPET, `make test-kernel`'s hpet=off
/// boot), the test waits for 20 PIT interrupts, and their rate is
/// `pit_tick_rate`'s claim, in that boot.
pub(crate) fn test_lapic_timer_rearm() -> Outcome {
    use apic_init::testing::{self as apic_testing, FIRE_STAMPS};
    let mode = apic_init::timer_mode();
    if mode == TimerMode::Pit {
        let t0 = time_init::pit_fires();
        if crate::ktest::wait_for(|| time_init::pit_fires().wrapping_sub(t0) >= REARM_FIRES) {
            return Outcome::Ok;
        }
        let n = time_init::pit_fires().wrapping_sub(t0);
        return crate::fail_fmt!("rearm stalled: {n} of {REARM_FIRES} pit fires");
    }
    let k = time_init::tsc_per_ms();
    if k == 0 {
        return Outcome::Fail("no tsc_per_ms");
    }
    let us = |c: u64| c.saturating_mul(1000) / k;
    if mode == TimerMode::Periodic {
        if let Some((got, want)) = apic_testing::periodic_mismatch() {
            return crate::fail_fmt!(
                "periodic timer lvt {:#x} icr {:#x} dcr {:#x}, the kernel programs {:#x} {:#x} {:#x}",
                got[0],
                got[1],
                got[2],
                want[0],
                want[1],
                want[2]
            );
        }
        // The registry runs on CPU 0 alone, so every read is one LAPIC's.
        let period = match reload_period(|| u64::from(apic_testing::timer_count()), k) {
            Ok(p) => p,
            Err(n) => {
                return crate::fail_fmt!(
                    "{n} of {} lapic reload spans by the run's deadline",
                    vibeos::time::RELOAD_SPANS
                );
            }
        };
        if !(k / 2..=k.saturating_mul(2)).contains(&period) {
            return crate::fail_fmt!("lapic reload period {} us, want 500 to 2000", us(period));
        }
    }
    let (r0, off0, _) = apic_testing::rearms();
    apic_testing::arm_fire_stamps();
    let full = crate::ktest::wait_for(|| apic_testing::fire_stamps() >= FIRE_STAMPS);
    let mut stamps = [0u64; FIRE_STAMPS];
    let n = apic_testing::take_fire_stamps(&mut stamps);
    let (r1, off1, last) = apic_testing::rearms();
    if !full {
        return crate::fail_fmt!("rearm stalled: {n} of {FIRE_STAMPS} lapic fires");
    }
    if mode != TimerMode::TscDeadline {
        return Outcome::Ok;
    }
    let need = (FIRE_STAMPS - 1) as u64;
    if r1.wrapping_sub(r0) < need {
        return crate::fail_fmt!("{} rearms over {need} fires", r1.wrapping_sub(r0));
    }
    if off1 != off0 {
        return crate::fail_fmt!(
            "{} rearms not one tick ahead: the last {last} TSC cycles, want {k}",
            off1.wrapping_sub(off0)
        );
    }
    let mut gaps = [0u64; FIRE_STAMPS - 1];
    for (g, w) in gaps.iter_mut().zip(stamps.windows(2)) {
        *g = w[1].wrapping_sub(w[0]);
    }
    gaps.sort_unstable();
    let median = gaps[(gaps.len() - 1) / 2];
    if (k / 2..=k.saturating_mul(2)).contains(&median) {
        return Outcome::Ok;
    }
    // Where the spacing went: each fire's handler time, from its stamp to
    // the TSC its rearm read, and how long after the deadline that rearm
    // armed the next fire was stamped (delivery, the guest's IF-off time
    // included). A missing rearm reads as zero.
    let mut at = [0u64; FIRE_STAMPS];
    let mut deadline = [0u64; FIRE_STAMPS];
    apic_testing::fire_rearms(&mut at, &mut deadline);
    let mut handler = [0u64; FIRE_STAMPS - 1];
    let mut late = [0u64; FIRE_STAMPS - 1];
    for (((h, l), w), (a, d)) in handler
        .iter_mut()
        .zip(late.iter_mut())
        .zip(stamps.windows(2))
        .zip(at.iter().zip(deadline.iter()))
    {
        *h = a.saturating_sub(w[0]);
        *l = w[1].saturating_sub(*d);
    }
    handler.sort_unstable();
    late.sort_unstable();
    crate::fail_fmt!(
        "lapic fire interval median {} us (min {}, max {}), want 500 to 2000; handler median {} us (max {}), past deadline median {} us (max {})",
        us(median),
        us(gaps[0]),
        us(gaps[gaps.len() - 1]),
        us(handler[(handler.len() - 1) / 2]),
        us(handler[handler.len() - 1]),
        us(late[(late.len() - 1) / 2]),
        us(late[late.len() - 1])
    )
}
