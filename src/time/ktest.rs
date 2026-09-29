//! In-guest tests for time (kernel_tests only). Rows: the list in crate::ktest.

use vibeos::time::{CalibSource, Instant, calib_band, calib_in_band, next_deadline};

use crate::acpi_init;
use crate::ktest::Outcome;
use crate::time_init::{self, STATE};
use crate::x86;

/// A fresh PIT channel 2 calibration (`tsc_per_ms`).
pub(crate) fn measure_pit_ch2() -> Option<u64> {
    let use_rdtscp = STATE.try_get().is_some_and(|s| s.use_rdtscp);
    time_init::calibrate_pit(use_rdtscp)
}

/// Fresh HPET window. ktest compares this to PIT under the same SMP load;
/// boot `tsc_per_ms` was sampled before APs came up.
pub(crate) fn measure_hpet() -> Option<u64> {
    let hpet = acpi_init::info()?.hpet?;
    time_init::calibrate_hpet(&hpet, STATE.try_get().is_some_and(|s| s.use_rdtscp))
}

/// Which source calibrated the TSC at boot.
pub(crate) fn source() -> CalibSource {
    STATE
        .try_get()
        .map(|s| s.source)
        .unwrap_or(CalibSource::Pit)
}

pub(crate) fn deadline_after(now: Instant) -> Instant {
    next_deadline(now)
}

pub(crate) fn test_pit_tick_rate() -> Outcome {
    let t0 = time_init::uptime_ms();
    time_init::busy_wait_ms(80);
    let t1 = time_init::uptime_ms();
    let dt = t1.saturating_sub(t0);
    if (40..=160).contains(&dt) {
        Outcome::Ok
    } else {
        crate::marker!("vibeOS: ktest:   ticks {t0} -> {t1} dt={dt}");
        Outcome::Fail("pit not ~1 kHz")
    }
}

pub(crate) fn test_now_us_monotonic() -> Outcome {
    let mut last = time_init::now_us();
    let mut i = 0u32;
    while i < 10_000 {
        let n = time_init::now_us();
        if n < last {
            crate::marker!("vibeOS: ktest:   now_us {last} -> {n} at {i}");
            return Outcome::Fail("now_us went backwards");
        }
        last = n;
        i += 1;
    }
    Outcome::Ok
}

pub(crate) fn test_now_us_under_yields() -> Outcome {
    let mut last = time_init::now_us();
    let mut i = 0u32;
    while i < 10_000 {
        let n = time_init::now_us();
        if n < last {
            crate::marker!("vibeOS: ktest:   yield now_us {last} -> {n} at {i}");
            return Outcome::Fail("now_us went backwards under yield");
        }
        last = n;
        if i.is_multiple_of(200) {
            x86::hlt_once();
        }
        i += 1;
    }
    Outcome::Ok
}

pub(crate) fn test_tsc_calib_source() -> Outcome {
    let present = acpi_init::info().is_some_and(|i| i.hpet_present());
    match source() {
        CalibSource::Hpet => {
            if !present {
                return Outcome::Fail("hpet source without table");
            }
            let k = time_init::tsc_per_ms();
            if !(50_000..=10_000_000).contains(&k) {
                return Outcome::Fail("tsc_per_ms out of range");
            }
            // Boot HPET ran before APs. Remeasure both under this SMP load.
            let ref_k = measure_hpet().unwrap_or(k);
            let (lo_pct, hi_pct) = calib_band(time_init::tsc_invariant());
            let mut last_pit = 0u64;
            let mut i = 0u32;
            while i < 3 {
                if let Some(pit) = measure_pit_ch2() {
                    last_pit = pit;
                    if calib_in_band(ref_k, pit, lo_pct, hi_pct) {
                        return Outcome::Ok;
                    }
                }
                i += 1;
            }
            crate::marker!("vibeOS: ktest:   hpet {k}/ms ref {ref_k}/ms pit {last_pit}/ms");
            if last_pit == 0 {
                Outcome::Fail("pit ch2 calib failed")
            } else {
                Outcome::Fail("pit ch2 disagreed with hpet")
            }
        }
        CalibSource::Pit => {
            if present {
                return Outcome::Fail("pit source despite hpet table");
            }
            let k = time_init::tsc_per_ms();
            if !(50_000..=10_000_000).contains(&k) {
                return Outcome::Fail("tsc_per_ms out of range");
            }
            Outcome::Ok
        }
    }
}

pub(crate) fn test_uptime_sides() -> Outcome {
    time_init::busy_wait_ms(30);
    let tick = time_init::uptime_ms();
    let us = time_init::now_us();
    if tick == 0 {
        return Outcome::Fail("tick still 0");
    }
    let tick_us = tick.saturating_mul(1000);
    let lo = tick_us.saturating_mul(50) / 100;
    let hi = tick_us.saturating_mul(150) / 100 + 2000;
    if us >= lo && us <= hi {
        Outcome::Ok
    } else {
        crate::marker!("vibeOS: ktest:   tick {tick} ms tsc {us} us");
        Outcome::Fail("tick and tsc sides diverged")
    }
}

pub(crate) fn test_rtc_offset() -> Outcome {
    let Some(a) = time_init::unix_time_s() else {
        return Outcome::Skip("rtc unread");
    };
    time_init::busy_wait_ms(20);
    let Some(b) = time_init::unix_time_s() else {
        return Outcome::Fail("rtc lost");
    };
    if b < a {
        return Outcome::Fail("wall clock went backwards");
    }
    let _ = deadline_after(Instant {
        ns: time_init::now_ns(),
    });
    Outcome::Ok
}
