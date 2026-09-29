//! In-guest tests for time (kernel_tests only). Rows: [`TESTS`].

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::time::{
    CalibSource, Instant, TICK_NS, calib_band, calib_in_band, interpolate_ns, next_deadline,
    unix_from_civil,
};

use crate::acpi_init;
use crate::ktest::{Outcome, Test, test};
use crate::per_cpu_init;
use crate::thread_init;
use crate::time_init::{self, STATE};

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

// ---------------------------------------------------------------------------
// The clock tests (ROADMAP §10.2, F100). Readers on every CPU read the
// seqlock clock without `LAST_NS`'s clamp and match each read against the
// tick records CPU 0 publishes here before each `TickClock::write`, a
// channel the seqlock does not guard (DESIGN §6.4).

/// Slots in the tick record ring. [`check`] reports [`Check::Stale`] after
/// [`STALE_MS`], within which at most one tick per ms cannot wrap it.
pub(crate) const PUB_RING: usize = 1024;
/// A sample older than this is [`Check::Stale`], never a mismatch.
const STALE_MS: u64 = 500;
/// Generation of the newest tick record.
static PUB_GEN: AtomicU64 = AtomicU64::new(0);
/// Each generation's tick TSC, in slot `gen % PUB_RING`.
static PUB_TSC: [AtomicU64; PUB_RING] = [const { AtomicU64::new(0) }; PUB_RING];
/// The planted tear: a stalled read takes its tick and its TSC stamp from
/// two `TickClock::read` calls, as a skipped seqlock retry would.
static TEAR: AtomicBool = AtomicBool::new(false);

/// Record generation `generation`'s tick TSC. Called by `time_init::init`
/// (generation 0) and by CPU 0's tick before its `TickClock::write`, the
/// one writer.
pub(crate) fn publish_tick(generation: u64, tsc: u64) {
    if let Some(slot) = PUB_TSC.get((generation % PUB_RING as u64) as usize) {
        // Relaxed: published by the Release store to `PUB_GEN` below.
        slot.store(tsc, Ordering::Relaxed);
    }
    // Release: pairs with the Acquire load in `published_gen`.
    PUB_GEN.store(generation, Ordering::Release);
}

/// The newest published tick generation.
pub(crate) fn published_gen() -> u64 {
    // Acquire: pairs with the Release store in `publish_tick`, so the slots
    // of every generation up to the one returned are visible.
    PUB_GEN.load(Ordering::Acquire)
}

pub(crate) fn set_tear(on: bool) {
    // Relaxed: the readers the next `run_readers` spawns see it through the
    // spawn itself; no data hangs off it.
    TEAR.store(on, Ordering::Relaxed);
}

/// One unclamped clock read and what [`check`] needs to match it.
#[derive(Clone, Copy)]
pub(crate) struct Sample {
    /// The reading, in ns since boot.
    pub ns: u64,
    /// The TSC the reading interpolated from.
    pub cycles: u64,
    /// [`published_gen`] before and after the read.
    pub gen_before: u64,
    pub gen_after: u64,
    /// The TSC before the read, which [`check`] ages the sample from.
    pub tsc_start: u64,
    /// The seqlock retried the read.
    pub retried: bool,
}

/// Spin until two more ticks are published after `from`, or 5 ms of TSC.
fn stall(from: u64, tsc_per_ms: u64) {
    let t0 = time_init::read_tsc();
    let cap = tsc_per_ms.saturating_mul(5);
    while published_gen() < from.saturating_add(2) && time_init::read_tsc().wrapping_sub(t0) < cap {
        core::hint::spin_loop();
    }
}

/// Read the clock through `TickClock::now_ns_with`, never
/// `time_init::now_ns`, so `LAST_NS` hides nothing. With `stall`, the
/// first TSC sample waits inside the seqlock window for two ticks, so the
/// read retries; with the planted tear set, a stalled read pairs one
/// generation's tick with a later one's TSC stamp instead.
pub(crate) fn now_ns_unclamped(stall_read: bool) -> Option<Sample> {
    let clock = time_init::tick_clock()?;
    let k = time_init::tsc_per_ms();
    let tsc_start = time_init::read_tsc();
    let gen_before = published_gen();
    let (ns, cycles, retried) = if stall_read && TEAR.load(Ordering::Relaxed) {
        let (tick, _) = clock.read();
        stall(published_gen(), k);
        let (_, tsc) = clock.read();
        let t = time_init::read_tsc();
        (interpolate_ns(tick, tsc, t, k), t, false)
    } else {
        let mut calls = 0u32;
        let mut cycles = 0u64;
        let ns = clock.now_ns_with(
            || {
                calls = calls.saturating_add(1);
                if stall_read && calls == 1 {
                    stall(published_gen(), k);
                }
                cycles = time_init::read_tsc();
                cycles
            },
            k,
        );
        (ns, cycles, calls > 1)
    };
    Some(Sample {
        ns,
        cycles,
        gen_before,
        gen_after: published_gen(),
        tsc_start,
        retried,
    })
}

/// What [`check`] found for a [`Sample`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Check {
    Match,
    /// No generation the read could have used gives its value;
    /// `nearest_ns` is the closest one's.
    Mismatch {
        nearest_ns: u64,
    },
    /// Older than [`STALE_MS`]: the ring may have wrapped under it.
    Stale,
}

/// The reading generation `generation` gives at TSC `t`: the clock's
/// interpolation, recomputed here rather than by calling it, so that a
/// regression in it fails the match too.
fn expected_ns(generation: u64, tsc_gen: u64, t: u64, tsc_per_ms: u64) -> u64 {
    let base = u128::from(generation).saturating_mul(1_000_000);
    let extra = u128::from(t.saturating_sub(tsc_gen)).saturating_mul(1_000_000)
        / u128::from(tsc_per_ms.max(1));
    u64::try_from(base.saturating_add(extra)).unwrap_or(u64::MAX)
}

/// Match `s` within `tol_ns` against each generation it could have read:
/// the clock may still hold `gen_before - 1`, since a record is published
/// before its seqlock write, and holds at most `gen_after`.
pub(crate) fn check(s: &Sample, tol_ns: u64) -> Check {
    let k = time_init::tsc_per_ms();
    let lo = s.gen_before.saturating_sub(1);
    if s.gen_after.saturating_sub(lo) >= (PUB_RING / 2) as u64 {
        return Check::Stale;
    }
    let mut nearest: Option<(u64, u64)> = None;
    let mut generation = lo;
    while generation <= s.gen_after {
        if let Some(slot) = PUB_TSC.get((generation % PUB_RING as u64) as usize) {
            // Relaxed: `gen_after` came from `published_gen`'s Acquire load,
            // which makes every slot up to it visible.
            let want = expected_ns(generation, slot.load(Ordering::Relaxed), s.cycles, k);
            let diff = want.abs_diff(s.ns);
            if diff <= tol_ns {
                return Check::Match;
            }
            if nearest.is_none_or(|(d, _)| diff < d) {
                nearest = Some((diff, want));
            }
        }
        generation = generation.saturating_add(1);
    }
    // Aged after the slot loads: a slot read within STALE_MS of
    // `tsc_start` has not been overwritten.
    if time_init::read_tsc().wrapping_sub(s.tsc_start) > STALE_MS.saturating_mul(k) {
        return Check::Stale;
    }
    Check::Mismatch {
        nearest_ns: nearest.map_or(0, |(_, w)| w),
    }
}

/// Reads per reader thread.
const READS: u32 = 10_000;
/// Every this many reads, one stalls in the seqlock window.
const STALL_EVERY: u32 = 500;
/// A read matches a published generation within this. The checker and the
/// clock compute the same integer formula from the same generation, so a
/// good read matches exactly; 1 µs leaves room for a rounding step in the
/// interpolation, and a torn pair moves the reading by the TSC time between
/// two ticks, about 1 ms.
const CLOCK_TOL_NS: u64 = 1_000;
/// Ticks that must be published during a run.
const MIN_TICKS: u64 = 10;
/// How long [`run_readers`] waits for its readers, and again after `STOP`.
const WAIT_MS: u64 = 5_000;

/// How many readers per online CPU, and how often each yields.
pub(crate) struct ReaderCfg {
    pub per_cpu: u32,
    pub yield_every: u32,
}

pub(crate) const MONOTONIC: ReaderCfg = ReaderCfg {
    per_cpu: 1,
    yield_every: 1_000,
};
pub(crate) const YIELDS: ReaderCfg = ReaderCfg {
    per_cpu: 2,
    yield_every: 10,
};

/// Reader ids, handed out at entry.
static NEXT: AtomicU32 = AtomicU32::new(0);
/// Readers that have finished.
static DONE: AtomicU32 = AtomicU32::new(0);
/// Tells readers to stop early.
static STOP: AtomicBool = AtomicBool::new(false);
/// Stalled reads the seqlock retried.
static RETRIED: AtomicU64 = AtomicU64::new(0);
/// Samples [`check`] called stale.
static STALE: AtomicU64 = AtomicU64::new(0);
/// The current run's `ReaderCfg::yield_every`.
static YIELD_EVERY: AtomicU32 = AtomicU32::new(1);

const FAIL_NONE: u32 = 0;
const FAIL_MISMATCH: u32 = 1;
const FAIL_BACKWARDS: u32 = 2;
const FAIL_NO_CLOCK: u32 = 3;
/// The first failure's kind, claimed by `compare_exchange`; the fields
/// below are its record.
static FAIL_KIND: AtomicU32 = AtomicU32::new(FAIL_NONE);
static FAIL_READER: AtomicU32 = AtomicU32::new(0);
static FAIL_CPU: AtomicU32 = AtomicU32::new(0);
static FAIL_READ: AtomicU32 = AtomicU32::new(0);
static FAIL_NS: AtomicU64 = AtomicU64::new(0);
static FAIL_NEAREST: AtomicU64 = AtomicU64::new(0);

/// The first failing read: which reader, on which CPU, which read, and the
/// reading with the nearest published value (for `Backwards`, the new and
/// the previous `now_us`).
#[derive(Clone, Copy)]
pub(crate) struct FailRec {
    pub reader: u32,
    pub cpu: u32,
    pub read: u32,
    pub ns: u64,
    pub nearest: u64,
}

/// Why a [`run_readers`] run failed.
#[derive(Clone, Copy)]
pub(crate) enum ReaderFail {
    Mismatch(FailRec),
    Backwards(FailRec),
    /// More than `READS / 10` stale samples.
    Starved(u64),
    /// Fewer than [`MIN_TICKS`] ticks published during the run.
    NoTick(u64),
    /// No stalled read retried.
    NoOverlap,
    Spawn(&'static str),
    /// Readers finished, of readers spawned, when the wait ran out.
    Stuck(u32, u32),
}

impl ReaderFail {
    fn name(self) -> &'static str {
        match self {
            Self::Mismatch(_) => "mismatch",
            Self::Backwards(_) => "backwards",
            Self::Starved(_) => "starved",
            Self::NoTick(_) => "no tick",
            Self::NoOverlap => "no overlap",
            Self::Spawn(_) => "spawn",
            Self::Stuck(..) => "stuck",
        }
    }

    fn outcome(self) -> Outcome {
        match self {
            Self::Mismatch(r) => crate::fail_fmt!(
                "reader {} cpu{} read {}: {} ns, nearest published {} ns",
                r.reader,
                r.cpu,
                r.read,
                r.ns,
                r.nearest
            ),
            Self::Backwards(r) => crate::fail_fmt!(
                "now_us {} -> {} at read {} (reader {} cpu{})",
                r.nearest,
                r.ns,
                r.read,
                r.reader,
                r.cpu
            ),
            Self::Starved(n) => crate::fail_fmt!("{n} stale samples"),
            Self::NoTick(n) => {
                crate::fail_fmt!("{n} ticks published during the run, want {MIN_TICKS}")
            }
            Self::NoOverlap => Outcome::Fail("no stalled read retried"),
            Self::Spawn(e) => crate::fail_fmt!("reader spawn: {e}"),
            Self::Stuck(done, n) => crate::fail_fmt!("{done} of {n} readers finished"),
        }
    }
}

/// Claim the failure record for the first failing read and stop the rest.
fn record_fail(kind: u32, rec: FailRec) {
    // Relaxed: the record is published by this reader's Release add to
    // `DONE`, which `run_readers` reads with Acquire before the record.
    if FAIL_KIND
        .compare_exchange(FAIL_NONE, kind, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
    {
        FAIL_READER.store(rec.reader, Ordering::Relaxed);
        FAIL_CPU.store(rec.cpu, Ordering::Relaxed);
        FAIL_READ.store(rec.read, Ordering::Relaxed);
        FAIL_NS.store(rec.ns, Ordering::Relaxed);
        FAIL_NEAREST.store(rec.nearest, Ordering::Relaxed);
    }
    // Relaxed: a hint to stop early; nothing is read through it.
    STOP.store(true, Ordering::Relaxed);
}

/// One reader: [`READS`] unclamped reads, each matched by [`check`], a
/// stall every [`STALL_EVERY`], a `yield_now` every `YIELD_EVERY`, and the
/// clamped `now_us` checked for order.
fn reader_entry() {
    // Relaxed: ids only need to differ.
    let reader = NEXT.fetch_add(1, Ordering::Relaxed);
    let cpu = thread_init::current_cpu();
    // Relaxed: set before the spawn that started this thread.
    let yield_every = YIELD_EVERY.load(Ordering::Relaxed).max(1);
    let mut last_us = time_init::now_us();
    let mut read = 0u32;
    // Relaxed: a hint, as in `record_fail`.
    while read < READS && !STOP.load(Ordering::Relaxed) {
        let n = read.saturating_add(1);
        let rec = |ns, nearest| FailRec {
            reader,
            cpu,
            read,
            ns,
            nearest,
        };
        let Some(s) = now_ns_unclamped(n.is_multiple_of(STALL_EVERY)) else {
            record_fail(FAIL_NO_CLOCK, rec(0, 0));
            break;
        };
        match check(&s, CLOCK_TOL_NS) {
            Check::Match => {}
            Check::Stale => {
                // Relaxed: a counter read after `DONE`'s Acquire.
                STALE.fetch_add(1, Ordering::Relaxed);
            }
            Check::Mismatch { nearest_ns } => {
                record_fail(FAIL_MISMATCH, rec(s.ns, nearest_ns));
                break;
            }
        }
        if s.retried && n.is_multiple_of(STALL_EVERY) {
            // Relaxed: a counter read after `DONE`'s Acquire.
            RETRIED.fetch_add(1, Ordering::Relaxed);
        }
        let us = time_init::now_us();
        if us < last_us {
            record_fail(FAIL_BACKWARDS, rec(us, last_us));
            break;
        }
        last_us = us;
        if n.is_multiple_of(yield_every) {
            thread_init::yield_now();
        }
        read = n;
    }
    // Release: pairs with the Acquire load in `wait_done`, publishing this
    // reader's counters and failure record.
    DONE.fetch_add(1, Ordering::Release);
}

/// Sleep in 1 ms steps until `n` readers are done or `ms` pass. Sleeping
/// never spins on a CPU a reader shares.
fn wait_done(n: u32, ms: u64) -> bool {
    let start = time_init::now_ns();
    let limit = ms.saturating_mul(1_000_000);
    // Acquire: pairs with the Release add at the end of `reader_entry`.
    while DONE.load(Ordering::Acquire) < n {
        if time_init::now_ns().saturating_sub(start) > limit {
            return false;
        }
        thread_init::sleep_ms(1);
    }
    true
}

/// Stop the readers already spawned and wait for them.
fn stop_readers(spawned: u32) -> bool {
    // Relaxed: a hint, as in `record_fail`.
    STOP.store(true, Ordering::Relaxed);
    wait_done(spawned, WAIT_MS)
}

/// Run `cfg.per_cpu` readers on each online CPU while the timer fires, and
/// report the first failure.
pub(crate) fn run_readers(cfg: &ReaderCfg) -> Result<(), ReaderFail> {
    // Relaxed throughout the reset: the spawns below publish it.
    NEXT.store(0, Ordering::Relaxed);
    DONE.store(0, Ordering::Relaxed);
    STOP.store(false, Ordering::Relaxed);
    RETRIED.store(0, Ordering::Relaxed);
    STALE.store(0, Ordering::Relaxed);
    FAIL_KIND.store(FAIL_NONE, Ordering::Relaxed);
    YIELD_EVERY.store(cfg.yield_every, Ordering::Relaxed);
    let g0 = published_gen();
    let mask = per_cpu_init::online_mask();
    let mut spawned = 0u32;
    for cpu in 0..64u32 {
        if mask & (1u64 << cpu) == 0 {
            continue;
        }
        for _ in 0..cfg.per_cpu {
            if let Err(e) = thread_init::spawn_on("clock-rd", reader_entry, cpu) {
                if !stop_readers(spawned) {
                    // Relaxed: after `DONE`'s Acquire in `wait_done`.
                    return Err(ReaderFail::Stuck(DONE.load(Ordering::Relaxed), spawned));
                }
                return Err(ReaderFail::Spawn(e.as_str()));
            }
            spawned = spawned.saturating_add(1);
        }
    }
    if !wait_done(spawned, WAIT_MS) {
        let _stopped = stop_readers(spawned);
        return Err(ReaderFail::Stuck(DONE.load(Ordering::Acquire), spawned));
    }
    let ticks = published_gen().saturating_sub(g0);
    // Relaxed: every reader's Release add to `DONE` was read with Acquire in
    // `wait_done`, which makes these visible.
    let rec = FailRec {
        reader: FAIL_READER.load(Ordering::Relaxed),
        cpu: FAIL_CPU.load(Ordering::Relaxed),
        read: FAIL_READ.load(Ordering::Relaxed),
        ns: FAIL_NS.load(Ordering::Relaxed),
        nearest: FAIL_NEAREST.load(Ordering::Relaxed),
    };
    match FAIL_KIND.load(Ordering::Relaxed) {
        FAIL_MISMATCH => return Err(ReaderFail::Mismatch(rec)),
        FAIL_BACKWARDS => return Err(ReaderFail::Backwards(rec)),
        FAIL_NO_CLOCK => return Err(ReaderFail::NoTick(0)),
        _ => {}
    }
    let stale = STALE.load(Ordering::Relaxed);
    if stale > u64::from(READS / 10) {
        return Err(ReaderFail::Starved(stale));
    }
    if ticks < MIN_TICKS {
        return Err(ReaderFail::NoTick(ticks));
    }
    if RETRIED.load(Ordering::Relaxed) == 0 {
        return Err(ReaderFail::NoOverlap);
    }
    Ok(())
}

fn readers_outcome(r: Result<(), ReaderFail>) -> Outcome {
    match r {
        Ok(()) => Outcome::Ok,
        Err(f) => f.outcome(),
    }
}

pub(crate) fn test_now_us_monotonic() -> Outcome {
    readers_outcome(run_readers(&MONOTONIC))
}

pub(crate) fn test_now_us_under_yields() -> Outcome {
    readers_outcome(run_readers(&YIELDS))
}

/// Both clock tests must fail on a planted tear.
pub(crate) fn test_now_us_planted_tear() -> Outcome {
    set_tear(true);
    let a = run_readers(&MONOTONIC);
    let b = if matches!(a, Err(ReaderFail::Stuck(..))) {
        a
    } else {
        run_readers(&YIELDS)
    };
    set_tear(false);
    let name = |r: Result<(), ReaderFail>| r.err().map_or("ok", ReaderFail::name);
    if matches!(a, Err(ReaderFail::Mismatch(_))) && matches!(b, Err(ReaderFail::Mismatch(_))) {
        Outcome::Ok
    } else {
        crate::fail_fmt!("tear not caught: monotonic {}, yields {}", name(a), name(b))
    }
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

/// Seconds in a mean Gregorian year, to name the year a bad RTC reads.
const MEAN_YEAR_S: u64 = 31_556_952;

pub(crate) fn test_rtc_offset() -> Outcome {
    let Some(a) = time_init::unix_time_s() else {
        return Outcome::Skip("rtc unread");
    };
    let lo = unix_from_civil(2024, 1, 1, 0, 0, 0);
    let hi = unix_from_civil(2100, 1, 1, 0, 0, 0);
    let (Some(lo), Some(hi)) = (lo, hi) else {
        return Outcome::Fail("unix_from_civil");
    };
    if !(lo..hi).contains(&a) {
        let year = 1970 + a / MEAN_YEAR_S;
        return crate::fail_fmt!("rtc reads year {year} (unix {a}), not 2024 to 2099");
    }
    time_init::busy_wait_ms(20);
    let Some(b) = time_init::unix_time_s() else {
        return Outcome::Fail("rtc lost");
    };
    if b < a {
        return Outcome::Fail("wall clock went backwards");
    }
    let now = time_init::now_ns();
    let d = deadline_after(Instant { ns: now });
    if d.ns <= now || d.ns - now > TICK_NS {
        return crate::fail_fmt!(
            "deadline_after({now}) = {}, want (now, now + TICK_NS]",
            d.ns
        );
    }
    Outcome::Ok
}

/// This subsystem's in-guest tests, in run order; `crate::ktest::GROUPS`
/// runs them (DESIGN §8.2).
pub(crate) const TESTS: &[Test] = &[
    test("pit_tick_rate", test_pit_tick_rate),
    test("now_us_monotonic", test_now_us_monotonic),
    test("now_us_under_yields", test_now_us_under_yields),
    test("now_us_planted_tear", test_now_us_planted_tear),
    test("tsc_calib_source", test_tsc_calib_source),
    test("uptime_sides", test_uptime_sides),
    test("rtc_offset", test_rtc_offset),
];
