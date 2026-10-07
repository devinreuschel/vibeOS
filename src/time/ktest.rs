//! In-guest tests for time (kernel_tests only). Rows: [`TESTS`].

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::kva::DEFAULT_STACK_PAGES;
#[cfg(not(target_arch = "aarch64"))]
use vibeos::time::CalibSource;
#[cfg(target_arch = "x86_64")]
use vibeos::time::{CALIB_BAND_INVARIANT, calib_in_band};
use vibeos::time::{ClocksourceId, Counter, Snapshot, ns_at};
#[cfg(target_arch = "x86_64")]
use vibeos::time::{Instant, ReloadSpans, next_deadline};
#[cfg(target_arch = "x86_64")]
use vibeos::time::{RELOAD_SPANS, TICK_NS, unix_from_civil};

#[cfg(target_arch = "x86_64")]
use vibeos::apic::TimerMode;

#[cfg(target_arch = "x86_64")]
use crate::apic_init;
use crate::ktest::{Outcome, Test, test};
#[cfg(not(target_arch = "aarch64"))]
use crate::machine_init;
use crate::per_cpu_init;
use crate::thread_init;
use crate::time_init;
#[cfg(not(target_arch = "aarch64"))]
use crate::time_init::STATE;

/// A fresh PIT channel 2 calibration (`tsc_per_ms`).
#[cfg(target_arch = "x86_64")]
pub(crate) fn measure_pit_ch2() -> Option<u64> {
    let use_rdtscp = STATE.try_get().is_some_and(|s| s.use_rdtscp);
    time_init::calibrate_pit(use_rdtscp)
}

/// Fresh HPET window. ktest compares this to PIT under the same SMP load;
/// boot `tsc_per_ms` was sampled before APs came up.
#[cfg(target_arch = "x86_64")]
pub(crate) fn measure_hpet() -> Option<u64> {
    let hpet = machine_init::info()?.hpet_info()?;
    time_init::calibrate_hpet(&hpet, STATE.try_get().is_some_and(|s| s.use_rdtscp))
}

/// Which source calibrated the TSC at boot.
#[cfg(not(target_arch = "aarch64"))]
pub(crate) fn source() -> CalibSource {
    STATE
        .try_get()
        .map(|s| s.source)
        .unwrap_or(CalibSource::Pit)
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn deadline_after(now: Instant) -> Instant {
    next_deadline(now)
}

/// PIT interrupts [`test_pit_tick_rate`] waits for after it has read the
/// period: the tick still arrives.
#[cfg(target_arch = "x86_64")]
const PIT_FIRES_MIN: u64 = 20;

/// The reload period, in TSC cycles, of the down-counting timer `count`
/// reads on this CPU: the median span [`ReloadSpans`] finds between its
/// reloads. Each read is taken with IF off between two TSC reads, and one
/// more than a quarter tick after the read before it starts afresh, so no
/// reload goes unseen for a period above a quarter tick and each span is
/// within a quarter tick of the period, inside the half-tick to two-tick
/// band the callers test. Under TCG the count is QEMU's clock at the read, whenever
/// the host delivers the timer's interrupts. `Err` with the spans taken
/// when the run's deadline comes first.
#[cfg(target_arch = "x86_64")]
pub(crate) fn reload_period(count: impl Fn() -> u64, tsc_per_ms: u64) -> Result<u64, usize> {
    let mut spans = ReloadSpans::new(tsc_per_ms / 4);
    loop {
        let (lo, c, hi) = {
            let _irq = crate::arch::current::InterruptGuard::enter();
            let lo = time_init::read_tsc();
            let c = count();
            (lo, c, time_init::read_tsc())
        };
        spans.push(lo, c, hi);
        if let Some(p) = spans.period() {
            return Ok(p);
        }
        if crate::ktest::deadline_near() == Some(true) {
            return Err(spans.len());
        }
    }
}

/// The PIT's tick rate, in a boot where the PIT drives the tick (`make
/// test-kernel`'s hpet=off boot): channel 0's reload period, read from its
/// latched count against the TSC ([`reload_period`]), lies between 0.5 and
/// 2 ms, and the PIT's interrupts keep arriving. The interrupts' own
/// arrival times are the host's: QEMU's main loop raises them, and a
/// macOS host wakes it no more often than every 5 to 10 ms (ROADMAP §10.2).
#[cfg(target_arch = "x86_64")]
pub(crate) fn test_pit_tick_rate() -> Outcome {
    if apic_init::timer_mode() != TimerMode::Pit {
        return Outcome::Fail("pit does not drive the tick");
    }
    if !crate::arch::current::interrupts_enabled() {
        return Outcome::Fail("IF off");
    }
    let k = time_init::tsc_per_ms();
    if k == 0 {
        return Outcome::Fail("no tsc_per_ms");
    }
    let f0 = time_init::pit_fires();
    let period = match reload_period(|| u64::from(time_init::pit_ch0_count()), k) {
        Ok(p) => p,
        Err(n) => {
            return crate::fail_fmt!(
                "{n} of {RELOAD_SPANS} pit reload spans by the run's deadline"
            );
        }
    };
    if !(k / 2..=k.saturating_mul(2)).contains(&period) {
        let us = period.saturating_mul(1000) / k;
        return crate::fail_fmt!("pit reload period {us} us, want 500 to 2000");
    }
    let fired = || time_init::pit_fires().wrapping_sub(f0);
    if crate::ktest::wait_for(|| fired() >= PIT_FIRES_MIN) {
        return Outcome::Ok;
    }
    crate::fail_fmt!(
        "{} of {PIT_FIRES_MIN} pit interrupts by the run's deadline",
        fired()
    )
}

// ---------------------------------------------------------------------------
// The clock tests (ROADMAP §10.2, F100). Readers on every CPU read the
// seqlock clock without `LAST_NS`'s clamp and match each read against the
// snapshots CPU 0 publishes here before each `TickClock::write`, a channel
// the seqlock does not guard (DESIGN §6.4).

/// Slots in the snapshot ring. [`check`] reports [`Check::Stale`] after
/// [`STALE_MS`], within which at most one tick per ms cannot wrap it.
pub(crate) const PUB_RING: usize = 1024;
/// A sample older than this is [`Check::Stale`], never a mismatch.
const STALE_MS: u64 = 500;
/// The generation the next [`publish_tick`] takes.
static PUB_NEXT: AtomicU64 = AtomicU64::new(0);
/// Generation of the newest snapshot record.
static PUB_GEN: AtomicU64 = AtomicU64::new(0);
/// Each generation's snapshot, in slot `gen % PUB_RING`: its clocksource
/// id, cycles and ns.
static PUB_ID: [AtomicU64; PUB_RING] = [const { AtomicU64::new(0) }; PUB_RING];
static PUB_CYCLES: [AtomicU64; PUB_RING] = [const { AtomicU64::new(0) }; PUB_RING];
static PUB_NS: [AtomicU64; PUB_RING] = [const { AtomicU64::new(0) }; PUB_RING];
/// The planted tear: a stalled read takes its base ns and its base cycles
/// from two `TickClock::read` calls, as a skipped seqlock retry would.
static TEAR: AtomicBool = AtomicBool::new(false);

/// Record the next generation's snapshot. Called by `time_init::publish`
/// before each `TickClock::write`, which only CPU 0 makes (the init write,
/// each tick, and a clocksource switch), so generations never race.
pub(crate) fn publish_tick(snap: Snapshot) {
    // Relaxed: the one writer's own counter.
    let generation = PUB_NEXT.fetch_add(1, Ordering::Relaxed);
    let slot = (generation % PUB_RING as u64) as usize;
    if let (Some(id), Some(cycles), Some(ns)) =
        (PUB_ID.get(slot), PUB_CYCLES.get(slot), PUB_NS.get(slot))
    {
        // Relaxed: published by the Release store to `PUB_GEN` below.
        id.store(snap.id as u64, Ordering::Relaxed);
        cycles.store(snap.cycles, Ordering::Relaxed);
        ns.store(snap.ns, Ordering::Relaxed);
    }
    // Release: pairs with the Acquire load in `published_gen`.
    PUB_GEN.store(generation, Ordering::Release);
}

/// The newest published generation.
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
    /// The clocksource read the reading converted, and its counter.
    pub raw: u64,
    pub id: ClocksourceId,
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
/// clocksource read waits inside the seqlock window for two ticks, so the
/// read retries; with the planted tear set, a stalled read pairs one
/// generation's base ns with a later one's base cycles instead.
pub(crate) fn now_ns_unclamped(stall_read: bool) -> Option<Sample> {
    let clock = time_init::tick_clock()?;
    let k = time_init::tsc_per_ms();
    let tsc_start = time_init::read_tsc();
    let gen_before = published_gen();
    let (ns, raw, id, retried) = if stall_read && TEAR.load(Ordering::Relaxed) {
        let a = clock.read()?;
        stall(published_gen(), k);
        let b = clock.read()?;
        let c = time_init::counter(b.id)?;
        let raw = time_init::read_counter(b.id)?;
        let torn = Snapshot {
            cycles: b.cycles,
            ..a
        };
        (ns_at(torn, c, raw), raw, b.id, false)
    } else {
        let mut calls = 0u32;
        let mut raw = 0u64;
        let mut id = ClocksourceId::Tsc;
        let ns = clock.now_ns_with(
            |which| {
                calls = calls.saturating_add(1);
                if stall_read && calls == 1 {
                    stall(published_gen(), k);
                }
                raw = time_init::read_counter(which).unwrap_or(0);
                id = which;
                raw
            },
            time_init::counter,
        );
        (ns, raw, id, calls > 1)
    };
    Some(Sample {
        ns,
        raw,
        id,
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

/// The reading a snapshot of `id` with base `cycles` and `ns` gives at
/// raw read `raw`: the clocksource formula, recomputed here rather than by
/// calling `ns_at`, so that a regression in it fails the match too.
fn expected_ns(id: ClocksourceId, cycles: u64, ns: u64, raw: u64) -> Option<u64> {
    let c = time_init::counter(id)?;
    let delta = raw.wrapping_sub(cycles) & c.mask();
    let extra = u128::from(delta).saturating_mul(u128::from(c.scale.mult)) >> c.scale.shift;
    let v = u128::from(ns).saturating_add(extra);
    Some(u64::try_from(v).unwrap_or(u64::MAX))
}

/// Match `s` within `tol_ns` against each generation it could have read:
/// the clock may still hold `gen_before - 1`, since a record is published
/// before its seqlock write, and holds at most `gen_after`. A generation of
/// another clocksource than the read's cannot match.
pub(crate) fn check(s: &Sample, tol_ns: u64) -> Check {
    let k = time_init::tsc_per_ms();
    let lo = s.gen_before.saturating_sub(1);
    if s.gen_after.saturating_sub(lo) >= (PUB_RING / 2) as u64 {
        return Check::Stale;
    }
    let mut nearest: Option<(u64, u64)> = None;
    let mut generation = lo;
    while generation <= s.gen_after {
        let slot = (generation % PUB_RING as u64) as usize;
        if let (Some(id), Some(cycles), Some(ns)) =
            (PUB_ID.get(slot), PUB_CYCLES.get(slot), PUB_NS.get(slot))
        {
            // Relaxed: `gen_after` came from `published_gen`'s Acquire load,
            // which makes every slot up to it visible.
            let id = ClocksourceId::from_u64(id.load(Ordering::Relaxed));
            let want = id.filter(|&i| i == s.id).and_then(|i| {
                expected_ns(
                    i,
                    cycles.load(Ordering::Relaxed),
                    ns.load(Ordering::Relaxed),
                    s.raw,
                )
            });
            if let Some(want) = want {
                let diff = want.abs_diff(s.ns);
                if diff <= tol_ns {
                    return Check::Match;
                }
                if nearest.is_none_or(|(d, _)| diff < d) {
                    nearest = Some((diff, want));
                }
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
/// good read matches exactly; a torn pair moves the reading by the time
/// between two ticks, about 1 ms, far outside 1 µs.
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

/// The boot calibration is consistent with the tables, and on an invariant
/// TSC one PIT channel 2 measurement lands within 75–125% of one fresh HPET
/// sample. Without an invariant TSC (TCG) the band means nothing, so it
/// skips; the nightly KVM leg runs it.
pub(crate) fn test_tsc_calib_source() -> Outcome {
    // CNTFRQ is the counter and `invariant_tsc` stays clear. The TCG row
    // already expects this reason; the aarch64 row covers HVF.
    #[cfg(target_arch = "aarch64")]
    {
        Outcome::Skip("no invariant tsc")
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        test_tsc_calib_source_x86()
    }
}

#[cfg(not(target_arch = "aarch64"))]
fn test_tsc_calib_source_x86() -> Outcome {
    let present = machine_init::info().is_some_and(|d| d.hpet_info().is_some());
    let k = time_init::tsc_per_ms();
    match source() {
        CalibSource::Hpet => {
            if !present {
                return Outcome::Fail("hpet source without table");
            }
            if !(50_000..=10_000_000).contains(&k) {
                return Outcome::Fail("tsc_per_ms out of range");
            }
            if !time_init::tsc_invariant() {
                return Outcome::Skip("no invariant tsc");
            }
            // Boot HPET ran before APs. Remeasure both under this SMP load.
            #[cfg(not(target_arch = "x86_64"))]
            {
                Outcome::Skip("x86 calib")
            }
            #[cfg(target_arch = "x86_64")]
            let Some(hpet) = measure_hpet() else {
                return Outcome::Fail("hpet calib failed");
            };
            #[cfg(target_arch = "x86_64")]
            let Some(pit) = measure_pit_ch2() else {
                return Outcome::Fail("pit ch2 calib failed");
            };
            #[cfg(target_arch = "x86_64")]
            {
                let (lo, hi) = CALIB_BAND_INVARIANT;
                if calib_in_band(hpet, pit, lo, hi) {
                    Outcome::Ok
                } else {
                    crate::fail_fmt!("pit ch2 {pit}/ms outside {lo}-{hi}% of hpet {hpet}/ms")
                }
            }
        }
        CalibSource::Pit => {
            if present {
                return Outcome::Fail("pit source despite hpet table");
            }
            if !(50_000..=10_000_000).contains(&k) {
                return Outcome::Fail("tsc_per_ms out of range");
            }
            Outcome::Ok
        }
    }
}

/// Seconds in a mean Gregorian year, to name the year a bad RTC reads.
#[cfg(target_arch = "x86_64")]
const MEAN_YEAR_S: u64 = 31_556_952;

#[cfg(target_arch = "x86_64")]
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

// ---------------------------------------------------------------------------
// clocksource_if_off_50ms (ROADMAP §10.3, F027)

/// How long CPU 0 holds IF off, in ns of the reference counter.
const IF_OFF_NS: u64 = 50_000_000;

/// 0 while the body runs; then 1, with [`IF_OFF_VALS`] set, or `2 + i` for
/// failure `i` of [`IF_OFF_ERRS`].
static IF_OFF_STATE: AtomicU32 = AtomicU32::new(0);
static IF_OFF_ERRS: [&str; 6] = [
    "no clocksource",
    "no reference counter",
    "no clock read",
    "not on cpu0",
    "two ticks did not follow the window",
    "no clock read bracketed within 20 us",
];
/// A clock read counts when the reference reads on either side of it are
/// this close: 0.04% of the window, so a vCPU stall between a read and its
/// reference cannot move the comparison by the 1% it checks.
const BRACKET_NS: u64 = 20_000;
/// How many (reference, clock, reference) triples [`bracketed`] tries.
const BRACKET_TRIES: u32 = 1_000;

/// A clock read and the reference reads on either side of it, taken again
/// until those are within [`BRACKET_NS`]: (reference before, clock,
/// reference after). Err: an index into [`IF_OFF_ERRS`].
fn bracketed(
    read: &impl Fn() -> Result<u64, usize>,
    since: &impl Fn(u64, u64) -> u64,
) -> Result<(u64, u64, u64), usize> {
    for _ in 0..BRACKET_TRIES {
        let lo = read()?;
        let now = now_raw().ok_or(2usize)?;
        let hi = read()?;
        if since(lo, hi) <= BRACKET_NS {
            return Ok((lo, now, hi));
        }
    }
    Err(5)
}
/// Δ`now_ns` and Δreference across the IF-off window, then across the
/// window and the two ticks after it; the reference's id.
static IF_OFF_VALS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static IF_OFF_REF: AtomicU64 = AtomicU64::new(0);

/// A counter to measure the clocksource against: the TSC under the HPET or
/// the PM timer; under the TSC, the HPET, else the PM timer. CNTVCT is the
/// only counter on aarch64, so the check is that `now_ns` tracks it.
fn reference(cs: ClocksourceId) -> Option<Counter> {
    let order: &[ClocksourceId] = match cs {
        ClocksourceId::Tsc => &[ClocksourceId::Hpet, ClocksourceId::AcpiPm],
        ClocksourceId::Hpet | ClocksourceId::AcpiPm => &[ClocksourceId::Tsc],
        ClocksourceId::Cntvct => &[ClocksourceId::Cntvct],
    };
    order.iter().find_map(|&id| time_init::counter(id))
}

/// One unclamped `now_ns`, through the hook that skips `LAST_NS`.
fn now_raw() -> Option<u64> {
    now_ns_unclamped(false).map(|s| s.ns)
}

/// The IF-off measurement, on CPU 0. Ok: (Δnow, Δref) over the window,
/// (Δnow, Δref) over the window and the two ticks after it, and the
/// reference's id. Err: an index into [`IF_OFF_ERRS`].
fn if_off_measure() -> Result<([u64; 4], ClocksourceId), usize> {
    let cs = time_init::clocksource().ok_or(0usize)?;
    let rc = reference(cs).ok_or(1usize)?;
    let read = || time_init::read_counter(rc.id).ok_or(2usize);
    let since = |from: u64, to: u64| rc.scale.to_ns(rc.delta(to, from));
    // The reference time between two bracketed reads: from the middle of
    // the first bracket to the middle of the second.
    let between = |a: (u64, u64, u64), b: (u64, u64, u64)| {
        since(a.0, b.2)
            .saturating_sub(since(a.0, a.2) / 2)
            .saturating_sub(since(b.0, b.2) / 2)
    };
    let (start, end) = {
        let _off = crate::sched::irqoff::deliberate("clocksource 50 ms IF-off window");
        if thread_init::current_cpu() != 0 {
            return Err(3);
        }
        let start = bracketed(&read, &since)?;
        loop {
            if since(start.2, read()?) >= IF_OFF_NS {
                break;
            }
            core::hint::spin_loop();
        }
        (start, bracketed(&read, &since)?)
    };
    // IF is back on: the tick resumes. A clock that counted ticks lost the
    // window's here. The run's deadline bounds the wait: a loaded host can
    // hold a tick back for tens of milliseconds (ROADMAP §10.2).
    let t0 = time_init::ticks();
    if !crate::ktest::wait_for(|| time_init::ticks() >= t0.saturating_add(2)) {
        return Err(4);
    }
    let after = bracketed(&read, &since)?;
    let vals = [
        end.1.saturating_sub(start.1),
        between(start, end),
        after.1.saturating_sub(start.1),
        between(start, after),
    ];
    Ok((vals, rc.id))
}

fn if_off_body() {
    let state = match if_off_measure() {
        Ok((vals, id)) => {
            for (slot, v) in IF_OFF_VALS.iter().zip(vals) {
                // Relaxed: published by the Release store to the state.
                slot.store(v, Ordering::Relaxed);
            }
            IF_OFF_REF.store(id as u64, Ordering::Relaxed);
            1
        }
        Err(i) => 2u32.saturating_add(i as u32),
    };
    // Release: pairs with the Acquire load in the test.
    IF_OFF_STATE.store(state, Ordering::Release);
}

/// `now` within 1% of `reference`.
fn within_1pct(now: u64, reference: u64) -> bool {
    now.abs_diff(reference) <= reference / 100
}

/// CPU 0 holds IF off for 50 ms of a counter that is not the clocksource,
/// and `now_ns` advances by the same within 1%, across the window and again
/// once two ticks have followed it (F027).
pub(crate) fn test_clocksource_if_off_50ms() -> Outcome {
    // Relaxed: the spawn below publishes it to the body.
    IF_OFF_STATE.store(0, Ordering::Relaxed);
    let opts = thread_init::SpawnOpts {
        stack_pages: DEFAULT_STACK_PAGES,
        cpu: Some(0),
    };
    if thread_init::spawn_opts("clock-ifoff", if_off_body, opts).is_err() {
        return Outcome::Fail("spawn");
    }
    // Acquire: pairs with the Release store in `if_off_body`. The registry
    // thread sleeps between checks, since the body runs on CPU 0 beside it.
    if !crate::ktest::sleep_for(|| IF_OFF_STATE.load(Ordering::Acquire) != 0) {
        return Outcome::Fail("body did not finish");
    }
    let state = IF_OFF_STATE.load(Ordering::Acquire);
    if state != 1 {
        let i = state.saturating_sub(2) as usize;
        return Outcome::Fail(IF_OFF_ERRS.get(i).copied().unwrap_or("bad state"));
    }
    // Relaxed: after the Acquire load of the state.
    let v = |i: usize| IF_OFF_VALS.get(i).map_or(0, |a| a.load(Ordering::Relaxed));
    let rid =
        ClocksourceId::from_u64(IF_OFF_REF.load(Ordering::Relaxed)).map_or("?", |c| c.as_str());
    let (dn, dr, dn2, dr2) = (v(0), v(1), v(2), v(3));
    if !within_1pct(dn, dr) {
        return crate::fail_fmt!("if off: now_ns +{dn} ns, {rid} +{dr} ns");
    }
    if !within_1pct(dn2, dr2) {
        return crate::fail_fmt!("after ticks: now_ns +{dn2} ns, {rid} +{dr2} ns");
    }
    Outcome::Ok
}

#[cfg(target_arch = "aarch64")]
pub(crate) fn test_pit_tick_rate() -> Outcome {
    Outcome::Skip("x86 PIT")
}

/// This subsystem's in-guest tests, in run order; `crate::ktest::GROUPS`
/// runs them (DESIGN §8.2).
pub(crate) const TESTS: &[Test] = &[
    test("pit_tick_rate", test_pit_tick_rate).opt_in(),
    test("now_us_monotonic", test_now_us_monotonic),
    test("now_us_under_yields", test_now_us_under_yields),
    test("now_us_planted_tear", test_now_us_planted_tear),
    test("tsc_calib_source", test_tsc_calib_source),
    #[cfg(target_arch = "x86_64")]
    test("rtc_offset", test_rtc_offset),
    test("clocksource_if_off_50ms", test_clocksource_if_off_50ms),
];
