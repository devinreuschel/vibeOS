//! Timekeeping arithmetic, seqlock, and PIT/TSC constants. DESIGN §6.
//!
//! Portable half: the clocksource arithmetic and ranking, the latched
//! seqlock that publishes the clock, deadline math, wall-clock offset.
//! Port I/O, HPET MMIO, and the IRQ0 handler live in the binary crate.

use crate::atomic::{AtomicU64, Ordering, fence, statics};
use crate::sync::variant::{self, Site};

/// PIT input frequency in Hz. DESIGN §6.1.
pub const PIT_HZ: u64 = 1_193_182;
/// Target bootstrap tick rate. Divisor is 1193.
pub const PIT_TICK_HZ: u64 = 1_000;
pub const PIT_DIVISOR: u16 = 1193;
/// Channel 2 count for a ~10 ms calibration window.
pub const PIT_CALIB_COUNT: u16 = 11_932;
pub const PIT_CALIB_MS: u64 = 10;

pub const PIT_CH0: u16 = 0x40;
pub const PIT_CH2: u16 = 0x42;
pub const PIT_CMD: u16 = 0x43;
/// Speaker / channel 2 gate. Bit 0 gates ch2, bit 5 is the OUT pin.
pub const PIT_GATE: u16 = 0x61;
pub const IO_WAIT_PORT: u16 = 0x80;

/// Channel 0, lobyte/hibyte, mode 2 (rate generator), binary.
pub const PIT_CMD_CH0_MODE2: u8 = 0x34;
/// Channel 2, lobyte/hibyte, mode 0 (one-shot), binary.
pub const PIT_CMD_CH2_ONESHOT: u8 = 0xB0;

/// Channel 0 program: command, lo, io_wait, hi. ROADMAP §2.5.
pub const PIT_CH0_WRITES: &[(u16, u8)] = &[
    (PIT_CMD, PIT_CMD_CH0_MODE2),
    (PIT_CH0, (PIT_DIVISOR as u8)),
    (IO_WAIT_PORT, 0),
    (PIT_CH0, (PIT_DIVISOR >> 8) as u8),
];

/// Femtoseconds in one millisecond.
pub const FS_PER_MS: u128 = 1_000_000_000_000;
/// HPET period must be in (1 ns, 100 ns] per the spec's 100 ns cap.
pub const HPET_PERIOD_FS_MIN: u32 = 1_000_000;
pub const HPET_PERIOD_FS_MAX: u32 = 100_000_000;

/// Refuse a calibration that would poison every delay. 50 MHz .. 10 GHz.
pub const TSC_PER_MS_MIN: u64 = 50_000;
pub const TSC_PER_MS_MAX: u64 = 10_000_000;

/// Bootstrap tick period. `next_deadline` uses this so tickless can
/// replace it later without rewriting callers. DESIGN §6.6.
pub const TICK_NS: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalibSource {
    Hpet,
    Pit,
}

impl CalibSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            CalibSource::Hpet => "hpet",
            CalibSource::Pit => "pit",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant {
    pub ns: u64,
}

/// Next deadline after `now`. Phase 2 is a 1 ms periodic tick; the
/// interface is "next deadline" so tickless does not rewrite the scheduler.
pub fn next_deadline(now: Instant) -> Instant {
    Instant {
        ns: now.ns.saturating_add(TICK_NS),
    }
}

pub const fn hpet_period_ok(period_fs: u32) -> bool {
    period_fs >= HPET_PERIOD_FS_MIN && period_fs <= HPET_PERIOD_FS_MAX
}

pub const fn tsc_per_ms_sane(v: u64) -> bool {
    v >= TSC_PER_MS_MIN && v <= TSC_PER_MS_MAX
}

/// `tsc_delta` over `hpet_delta` ticks of `period_fs`. None if poison.
pub fn tsc_per_ms_from_hpet(tsc_delta: u64, hpet_delta: u64, period_fs: u32) -> Option<u64> {
    if tsc_delta == 0 || hpet_delta == 0 || !hpet_period_ok(period_fs) {
        return None;
    }
    let elapsed_fs = hpet_delta as u128 * period_fs as u128;
    if elapsed_fs == 0 {
        return None;
    }
    let v = (tsc_delta as u128).saturating_mul(FS_PER_MS) / elapsed_fs;
    let v = u64::try_from(v).ok()?;
    tsc_per_ms_sane(v).then_some(v)
}

/// `tsc_delta` over a PIT channel 2 one-shot of `count` ticks.
pub fn tsc_per_ms_from_pit(tsc_delta: u64, count: u16) -> Option<u64> {
    if tsc_delta == 0 || count == 0 {
        return None;
    }
    let v = (tsc_delta as u128 * PIT_HZ as u128) / (count as u128 * 1000);
    let v = u64::try_from(v).ok()?;
    tsc_per_ms_sane(v).then_some(v)
}

/// Invariant TSC: PIT vs HPET must land in 75–125%.
pub const CALIB_BAND_INVARIANT: (u64, u64) = (75, 125);

/// True if `sample` is in `[lo_pct, hi_pct]` percent of `reference`.
pub const fn calib_in_band(reference: u64, sample: u64, lo_pct: u64, hi_pct: u64) -> bool {
    let lo = reference.saturating_mul(lo_pct) / 100;
    let hi = reference.saturating_mul(hi_pct) / 100;
    sample >= lo && sample <= hi
}

/// `fetch_max` then return the larger of previous and `n`. `last` is a
/// `static` (`time_init::LAST_NS`), so it takes the seam's `core` flavour.
pub fn monotonic_max(last: &statics::AtomicU64, n: u64) -> u64 {
    last.fetch_max(n, statics::Ordering::Relaxed).max(n)
}

fn clamp_u64(v: u128) -> u64 {
    if v > u64::MAX as u128 {
        u64::MAX
    } else {
        v as u64
    }
}

/// The ACPI PM timer's rate in Hz (ACPI 6.5 §4.8.3.3).
pub const PM_TIMER_HZ: u64 = 3_579_545;

/// A free-running counter the clock can read: the clocksource candidates
/// DESIGN §6.4 ranks, with Linux's clocksource names.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClocksourceId {
    Tsc = 1,
    Hpet = 2,
    AcpiPm = 3,
}

impl ClocksourceId {
    /// The name the `time: clocksource <name>` line prints.
    pub const fn as_str(self) -> &'static str {
        match self {
            ClocksourceId::Tsc => "tsc",
            ClocksourceId::Hpet => "hpet",
            ClocksourceId::AcpiPm => "acpi_pm",
        }
    }

    /// The id whose discriminant is `v`, as a seqlock payload word holds it.
    pub const fn from_u64(v: u64) -> Option<Self> {
        match v {
            1 => Some(ClocksourceId::Tsc),
            2 => Some(ClocksourceId::Hpet),
            3 => Some(ClocksourceId::AcpiPm),
            _ => None,
        }
    }
}

/// Cycles to nanoseconds as `cycles * mult >> shift`, evaluated in `u128`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scale {
    pub mult: u64,
    pub shift: u32,
}

impl Scale {
    /// The shift every [`Scale::from_hz`] uses: `mult` keeps 32 fractional
    /// bits of ns per cycle, under 1 ns of error per 2^32 cycles.
    pub const SHIFT: u32 = 32;

    /// The scale of a counter running at `hz`. None for 0 Hz, or a rate so
    /// high that `mult` would be 0.
    pub fn from_hz(hz: u64) -> Option<Scale> {
        if hz == 0 {
            return None;
        }
        let mult = (1_000_000_000u128 << Self::SHIFT) / u128::from(hz);
        let mult = u64::try_from(mult).ok()?;
        (mult != 0).then_some(Scale {
            mult,
            shift: Self::SHIFT,
        })
    }

    /// `cycles` in nanoseconds, saturating at `u64::MAX`.
    pub fn to_ns(self, cycles: u64) -> u64 {
        let v = u128::from(cycles).saturating_mul(u128::from(self.mult));
        clamp_u64(v.checked_shr(self.shift).unwrap_or(0))
    }
}

/// One clocksource: its id, its rate as a [`Scale`], and how many low bits
/// of a raw read count (24 or 32 for the PM timer, 32 or 64 for the HPET,
/// 64 for the TSC).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Counter {
    pub id: ClocksourceId,
    pub scale: Scale,
    pub width: u32,
}

impl Counter {
    /// None for a width outside 1..=64 or a rate [`Scale::from_hz`] refuses.
    pub fn new(id: ClocksourceId, hz: u64, width: u32) -> Option<Counter> {
        if !(1..=64).contains(&width) {
            return None;
        }
        Some(Counter {
            id,
            scale: Scale::from_hz(hz)?,
            width,
        })
    }

    /// The bits of a raw read that count.
    pub const fn mask(self) -> u64 {
        if self.width >= 64 {
            u64::MAX
        } else {
            (1u64 << self.width).wrapping_sub(1)
        }
    }

    /// Cycles from `base` to `now`, modulo 2^width: correct across at most
    /// one wrap, which is why a narrow counter is read every half wrap.
    pub const fn delta(self, now: u64, base: u64) -> u64 {
        now.wrapping_sub(base) & self.mask()
    }
}

/// What the clock publishes: the counter `id` read `cycles` at `ns` since
/// boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub id: ClocksourceId,
    pub cycles: u64,
    pub ns: u64,
}

/// Nanoseconds since boot at raw read `raw_now` of `c`, the counter `s`
/// names: `s.ns + ((raw_now - s.cycles) mod 2^width) * mult >> shift` in
/// `u128`, saturating.
pub fn ns_at(s: Snapshot, c: Counter, raw_now: u64) -> u64 {
    s.ns.saturating_add(c.scale.to_ns(c.delta(raw_now, s.cycles)))
}

/// The one writer's state (CPU 0's tick). Each [`Snapshot`] it returns is
/// `ns0 + to_ns(ext)`, where `ext` is the whole cycle count since `ns0`,
/// so rounding in one base never carries into the next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockWriter {
    counter: Counter,
    last_raw: u64,
    ext: u64,
    ns0: u64,
}

impl ClockWriter {
    /// A writer whose counter `c` read `raw` at `ns`.
    pub const fn new(c: Counter, raw: u64, ns: u64) -> Self {
        Self {
            counter: c,
            last_raw: raw & c.mask(),
            ext: 0,
            ns0: ns,
        }
    }

    pub const fn counter(&self) -> Counter {
        self.counter
    }

    /// The snapshot as of the last read.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            id: self.counter.id,
            cycles: self.last_raw,
            ns: self.ns0.saturating_add(self.counter.scale.to_ns(self.ext)),
        }
    }

    /// Take a new read `raw` of the counter. At least one read per half
    /// wrap keeps every wrap counted.
    pub fn advance(&mut self, raw: u64) -> Snapshot {
        let d = self.counter.delta(raw, self.last_raw);
        self.ext = self.ext.saturating_add(d);
        self.last_raw = raw & self.counter.mask();
        self.snapshot()
    }

    /// Move to counter `to` with no step: the old counter read `raw_old`
    /// and `to` read `raw_new` at the same instant, and the new base is the
    /// time a reader of the old counter gets at `raw_old`.
    pub fn switch(&mut self, to: Counter, raw_old: u64, raw_new: u64) -> Snapshot {
        self.ns0 = ns_at(self.snapshot(), self.counter, raw_old);
        self.counter = to;
        self.ext = 0;
        self.last_raw = raw_new & to.mask();
        self.snapshot()
    }
}

/// What a port found at boot, for [`rank`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Candidates {
    pub tsc: Option<Counter>,
    pub tsc_invariant: bool,
    pub tsc_warp_ok: bool,
    pub hpet: Option<Counter>,
    pub pm: Option<Counter>,
}

/// The clocksource (DESIGN §6.4): the TSC when CPUID reports it invariant
/// and no warp test saw it step backward, then the HPET main counter, then
/// the ACPI PM timer. None: no candidate.
pub fn rank(c: &Candidates) -> Option<Counter> {
    let tsc = c.tsc.filter(|_| c.tsc_invariant && c.tsc_warp_ok);
    tsc.or(c.hpet).or(c.pm)
}

/// The HPET main counter's rate for a `period_fs` [`hpet_period_ok`]
/// accepts.
pub fn hpet_hz(period_fs: u32) -> Option<u64> {
    if !hpet_period_ok(period_fs) {
        return None;
    }
    let hz = 1_000_000_000_000_000u64.checked_div(u64::from(period_fs))?;
    (hz != 0).then_some(hz)
}

/// The HPET main counter's width from `GCAP_ID`: 64 bits when
/// `COUNT_SIZE_CAP` (bit 13) is set, else 32.
pub const fn hpet_counter_width(gcap: u64) -> u32 {
    if gcap & (1 << 13) != 0 { 64 } else { 32 }
}

/// One copy of the latch's payload: a [`Snapshot`], its id as a word.
struct Payload {
    id: AtomicU64,
    cycles: AtomicU64,
    ns: AtomicU64,
}

impl Payload {
    #[cfg(not(loom))]
    const fn new() -> Self {
        Self {
            id: AtomicU64::new(0),
            cycles: AtomicU64::new(0),
            ns: AtomicU64::new(0),
        }
    }

    #[cfg(loom)]
    fn new() -> Self {
        Self {
            id: AtomicU64::new(0),
            cycles: AtomicU64::new(0),
            ns: AtomicU64::new(0),
        }
    }

    fn store(&self, s: Snapshot) {
        // Relaxed: ordered by the fences around the sequence bumps.
        self.id.store(s.id as u64, Ordering::Relaxed);
        self.cycles.store(s.cycles, Ordering::Relaxed);
        self.ns.store(s.ns, Ordering::Relaxed);
    }

    /// The raw words: id, cycles, ns. The id is 0 before the first write.
    fn load(&self) -> (u64, u64, u64) {
        // Relaxed: ordered by the reader's `fence(Acquire)`.
        (
            self.id.load(Ordering::Relaxed),
            self.cycles.load(Ordering::Relaxed),
            self.ns.load(Ordering::Relaxed),
        )
    }
}

/// The snapshot a payload's words hold; None before the first write.
fn decode((id, cycles, ns): (u64, u64, u64)) -> Option<Snapshot> {
    Some(Snapshot {
        id: ClocksourceId::from_u64(id)?,
        cycles,
        ns,
    })
}

/// The clock: a latched seqlock over the clocksource's [`Snapshot`],
/// DESIGN §6.4. The one writer (CPU 0's tick) bumps `seq` to odd and
/// stores copy 0, then bumps it to even and stores copy 1. A reader reads
/// the copy the low bit of `seq` names, which is never the one being
/// stored, so it never waits for the writer and retries only when `seq`
/// moved under it. The id travels with its base, so a clocksource switch
/// and its base publish as one value.
pub struct TickClock {
    seq: AtomicU64,
    copy0: Payload,
    copy1: Payload,
}

impl TickClock {
    /// `const` outside loom, whose atomics have no `const fn new`
    /// (C-ATOMICS); the kernel's `static` clock needs it.
    #[cfg(not(loom))]
    pub const fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            copy0: Payload::new(),
            copy1: Payload::new(),
        }
    }

    #[cfg(loom)]
    pub fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            copy0: Payload::new(),
            copy1: Payload::new(),
        }
    }

    /// One sequence bump. The CPU 0 tick is the only writer, so a Relaxed
    /// `fetch_add` suffices for the count itself.
    fn bump(&self) {
        // The loom models' variants (ROADMAP §10.8): F098's lone AcqRel
        // `fetch_add`, and the bump without its leading fence.
        if variant::pick(Site::SeqlockBumpAcqRel, false, true) {
            self.seq.fetch_add(1, Ordering::AcqRel);
            return;
        }
        if variant::pick(Site::SeqlockLeadingFence, true, false) {
            // Release: orders the stores of the copy written before this
            // bump ahead of it, for a reader whose Acquire load of `seq`
            // sees the bump and then reads that copy.
            fence(Ordering::Release);
        }
        self.seq.fetch_add(1, Ordering::Relaxed);
        // Release: pairs with the reader's `fence(Acquire)` after its
        // payload loads. A reader that loaded any store made after this
        // fence sees this bump in its re-check of `seq` and retries.
        fence(Ordering::Release);
    }

    /// First half of [`TickClock::write`]: `seq` goes odd, so readers read
    /// copy 1 while copy 0 is stored.
    fn write_copy0(&self, s: Snapshot) {
        self.bump();
        self.copy0.store(s);
    }

    /// Second half: `seq` goes even, so readers read copy 0 while copy 1
    /// is stored.
    fn write_copy1(&self, s: Snapshot) {
        self.bump();
        self.copy1.store(s);
    }

    /// ISR path. No alloc, no logging. One writer at a time.
    pub fn write(&self, s: Snapshot) {
        self.write_copy0(s);
        self.write_copy1(s);
    }

    /// The copy `seq` value `s` names: copy 1 while the writer stores
    /// copy 0 (odd), else copy 0.
    fn copy(&self, s: u64) -> &Payload {
        if s & 1 == 0 { &self.copy0 } else { &self.copy1 }
    }

    /// The published snapshot; None before the first write. Retries only
    /// when `seq` changed.
    pub fn read(&self) -> Option<Snapshot> {
        loop {
            let s1 = self.seq.load(Ordering::Acquire);
            let v = self.copy(s1).load();
            // Payload loads must not move past the seq re-check.
            fence(Ordering::Acquire);
            let s2 = self.seq.load(Ordering::Relaxed);
            if s1 == s2 {
                return decode(v);
            }
        }
    }

    /// Nanoseconds since boot: the published snapshot and a read of the
    /// counter it names, `read_raw(id)`, which `counter(id)` describes
    /// ([`ns_at`]). The read comes after the payload loads and before the
    /// sequence re-check, so a write in between forces a retry. 0 before
    /// the first write; the snapshot's own `ns` if `counter` knows no such
    /// id.
    pub fn now_ns_with(
        &self,
        mut read_raw: impl FnMut(ClocksourceId) -> u64,
        counter: impl Fn(ClocksourceId) -> Option<Counter>,
    ) -> u64 {
        loop {
            let s1 = self.seq.load(Ordering::Acquire);
            let v = self.copy(s1).load();
            fence(Ordering::Acquire);
            let snap = decode(v);
            let raw = snap.map(|s| read_raw(s.id));
            let s2 = self.seq.load(Ordering::Relaxed);
            if s1 != s2 {
                continue;
            }
            return match (snap, raw) {
                (Some(s), Some(raw)) => counter(s.id).map_or(s.ns, |c| ns_at(s, c, raw)),
                _ => 0,
            };
        }
    }
}

impl Default for TickClock {
    fn default() -> Self {
        Self::new()
    }
}

/// Wall clock = RTC unix seconds at boot plus monotonic elapsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WallOrigin {
    pub unix_s: u64,
    pub mono_ns: u64,
}

pub fn wall_unix_s(origin: WallOrigin, now_ns: u64) -> u64 {
    let elapsed = now_ns.saturating_sub(origin.mono_ns) / 1_000_000_000;
    origin.unix_s.saturating_add(elapsed)
}

/// Days from civil date, then unix seconds. Howard Hinnant's algorithm.
pub fn unix_from_civil(year: i32, month: u8, day: u8, hour: u8, min: u8, sec: u8) -> Option<u64> {
    if !(1..=12).contains(&month) || day == 0 || day > 31 || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let days = days_from_civil(year, month as i32, day as i32);
    let secs = days
        .checked_mul(86400)?
        .checked_add(hour as i64 * 3600)?
        .checked_add(min as i64 * 60)?
        .checked_add(sec as i64)?;
    u64::try_from(secs).ok()
}

fn days_from_civil(mut y: i32, m: i32, d: i32) -> i64 {
    y -= i32::from(m <= 2);
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as i64;
    let mp = m + if m > 2 { -3 } else { 9 };
    let doy = (153 * mp as i64 + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era as i64 * 146097 + doe - 719468
}

/// A civil (proleptic Gregorian, UTC) date and time of day.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Civil {
    pub year: i32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub min: u8,
    pub sec: u8,
}

/// The civil date and time of `secs` unix seconds: the inverse of
/// [`unix_from_civil`]. Howard Hinnant's `civil_from_days`.
pub fn civil_from_unix(secs: u64) -> Civil {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (year, month, day) = civil_from_days(days);
    Civil {
        year,
        month,
        day,
        hour: (rem / 3600) as u8,
        min: (rem / 60 % 60) as u8,
        sec: (rem % 60) as u8,
    }
}

/// `(year, month, day)` of day `z` since 1970-01-01, for `0 <= z`.
fn civil_from_days(z: i64) -> (i32, u8, u8) {
    let z = z + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y as i32, m, d)
}

pub const fn bcd_to_bin(v: u8) -> u8 {
    (v & 0x0F) + ((v >> 4) & 0x0F) * 10
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn pit_divisor_is_1khz() {
        assert_eq!(PIT_HZ / PIT_DIVISOR as u64, PIT_TICK_HZ);
        assert_eq!(PIT_CALIB_COUNT, 11_932);
        assert_eq!(PIT_CH0_WRITES[0], (PIT_CMD, PIT_CMD_CH0_MODE2));
        assert_eq!(PIT_CH0_WRITES[1], (PIT_CH0, 1193u16 as u8));
        assert_eq!(PIT_CH0_WRITES[2], (IO_WAIT_PORT, 0));
        assert_eq!(PIT_CH0_WRITES[3], (PIT_CH0, (1193u16 >> 8) as u8));
    }

    /// A late base can put one read above the next base; `monotonic_max`
    /// holds the high water.
    #[test]
    fn monotonic_max_holds_high_water() {
        let last = AtomicU64::new(0);
        assert_eq!(monotonic_max(&last, 7_500), 7_500);
        assert_eq!(monotonic_max(&last, 6_000), 7_500);
        assert_eq!(monotonic_max(&last, 8_000), 8_000);
        assert_eq!(monotonic_max(&last, u64::MAX), u64::MAX);
        assert_eq!(monotonic_max(&last, 0), u64::MAX);
    }

    #[test]
    fn hpet_and_pit_calib_math() {
        // 10 ms of 10 ns HPET ticks = 1_000_000 counts, 2.5 GHz TSC.
        let tsc = 2_500_000 * 10;
        assert_eq!(
            tsc_per_ms_from_hpet(tsc, 1_000_000, 10_000_000),
            Some(2_500_000)
        );
        assert_eq!(tsc_per_ms_from_hpet(tsc, 0, 10_000_000), None);
        assert_eq!(tsc_per_ms_from_hpet(tsc, 1_000_000, 0), None);
        assert_eq!(tsc_per_ms_from_hpet(100, 1_000_000, 10_000_000), None); // too small
        let v = tsc_per_ms_from_pit(2_500_000 * 10, PIT_CALIB_COUNT).unwrap();
        assert!((2_490_000..=2_510_000).contains(&v), "{v}");
        assert!(!tsc_per_ms_sane(0));
        assert!(!tsc_per_ms_sane(1));
        assert!(!tsc_per_ms_sane(u64::MAX));
        assert!(tsc_per_ms_sane(1_000_000));
    }

    #[test]
    fn calib_band_invariant() {
        let (lo, hi) = CALIB_BAND_INVARIANT;
        assert_eq!((lo, hi), (75, 125));
        assert!(calib_in_band(1_000_000, 750_000, lo, hi));
        assert!(calib_in_band(1_000_000, 1_250_000, lo, hi));
        assert!(!calib_in_band(1_000_000, 749_999, lo, hi));
        assert!(!calib_in_band(1_000_000, 1_250_001, lo, hi));
    }

    #[test]
    fn next_deadline_is_1ms_ahead() {
        let a = Instant { ns: 0 };
        assert_eq!(next_deadline(a).ns, TICK_NS);
        let b = Instant { ns: u64::MAX - 10 };
        assert_eq!(next_deadline(b).ns, u64::MAX);
    }

    #[test]
    fn unix_civil_epoch_and_known_day() {
        assert_eq!(unix_from_civil(1970, 1, 1, 0, 0, 0), Some(0));
        assert_eq!(unix_from_civil(2000, 1, 1, 0, 0, 0), Some(946_684_800));
        assert_eq!(unix_from_civil(2026, 9, 17, 5, 6, 0), Some(1_789_621_560));
        assert_eq!(unix_from_civil(1970, 13, 1, 0, 0, 0), None);
        assert_eq!(bcd_to_bin(0x59), 59);
        assert_eq!(bcd_to_bin(0x00), 0);
    }

    /// Every day from 1970 through 2200, at a varying time of day, maps
    /// back to the seconds it came from.
    #[test]
    fn civil_from_unix_inverts_unix_from_civil() {
        let last = unix_from_civil(2200, 12, 31, 0, 0, 0).unwrap() / 86400;
        let mut prev: Option<Civil> = None;
        for day in 0..=last {
            let t = day * 86400 + (day * 7919) % 86400;
            let c = civil_from_unix(t);
            assert_eq!(
                unix_from_civil(c.year, c.month, c.day, c.hour, c.min, c.sec),
                Some(t),
                "{c:?}"
            );
            if let Some(p) = prev {
                assert!(
                    (p.year, p.month, p.day) < (c.year, c.month, c.day),
                    "{p:?} {c:?}"
                );
            }
            prev = Some(c);
        }
        assert_eq!(
            civil_from_unix(0),
            Civil {
                year: 1970,
                month: 1,
                day: 1,
                hour: 0,
                min: 0,
                sec: 0
            }
        );
        let end = civil_from_unix(last * 86400 + 86399);
        assert_eq!((end.year, end.month, end.day), (2200, 12, 31));
        assert_eq!((end.hour, end.min, end.sec), (23, 59, 59));
    }

    #[test]
    fn wall_tracks_monotonic_offset() {
        let origin = WallOrigin {
            unix_s: 1_000,
            mono_ns: 5_000_000_000,
        };
        assert_eq!(wall_unix_s(origin, 5_000_000_000), 1_000);
        assert_eq!(wall_unix_s(origin, 8_000_000_000), 1_003);
    }

    /// A 1 GHz counter, so a cycle is a nanosecond.
    fn ghz() -> Counter {
        Counter::new(ClocksourceId::Tsc, 1_000_000_000, 64).unwrap()
    }

    fn snap(cycles: u64, ns: u64) -> Snapshot {
        Snapshot {
            id: ClocksourceId::Tsc,
            cycles,
            ns,
        }
    }

    /// DESIGN §8.1: the writer publishes an independent timestamp. A torn
    /// (base, cycles) pair must not match that value. Seqlock retry must.
    #[test]
    fn now_us_seqlock_retry_under_simulated_writer() {
        let clock = TickClock::new();
        // Independent published snapshots. The writer's second one is the
        // timestamp we compare against, not a value derived from the
        // possibly-torn first read.
        let first = snap(100, 1_000_000);
        let second = snap(200, 2_000_000);
        clock.write(first);

        let mut samples = 0u32;
        let got = clock.now_ns_with(
            |_| {
                samples += 1;
                if samples == 1 {
                    clock.write(second);
                    return 10_000;
                }
                250
            },
            |_| Some(ghz()),
        );
        assert_eq!(samples, 2, "reader must retry after the simulated ISR");
        let expected = ns_at(second, ghz(), 250);
        assert_eq!(got, expected);
        let torn = ns_at(first, ghz(), 10_000);
        assert_ne!(
            torn, expected,
            "torn mix must not accidentally equal the independent timestamp"
        );
    }

    /// F098: a reader that interrupts the writer between its two copies
    /// (`seq` odd) returns the older value instead of waiting.
    #[test]
    fn latch_read_mid_write_returns_older() {
        let clock = TickClock::new();
        assert_eq!(clock.read(), None);
        assert_eq!(clock.now_ns_with(|_| 5, |_| Some(ghz())), 0);
        let a = snap(100, 1_000);
        let b = snap(200, 2_000);
        clock.write(a);
        clock.write_copy0(b);
        assert_eq!(clock.seq.load(Ordering::Relaxed) & 1, 1);
        assert_eq!(clock.read(), Some(a));
        assert_eq!(clock.now_ns_with(|_| 150, |_| Some(ghz())), 1_050);
        clock.write_copy1(b);
        assert_eq!(clock.read(), Some(b));
        assert_eq!(clock.now_ns_with(|_| 250, |_| Some(ghz())), 2_050);
    }

    #[test]
    fn seqlock_read_stable_pair() {
        let clock = TickClock::new();
        clock.write(snap(42, 99));
        assert_eq!(clock.read(), Some(snap(42, 99)));
        let pm = Snapshot {
            id: ClocksourceId::AcpiPm,
            cycles: 43,
            ns: 100,
        };
        clock.write(pm);
        assert_eq!(clock.read(), Some(pm));
    }

    #[test]
    fn seqlock_threaded_writer_never_tears() {
        let clock = Arc::new(TickClock::new());
        let stop = Arc::new(AtomicBool::new(false));
        let bad = Arc::new(AtomicU64::new(0));
        let reads = Arc::new(AtomicU64::new(0));
        const K: u64 = 1_000;
        const MAGIC: u64 = 7;
        // Each snapshot's ns is a fixed function of its cycles. Seed so a
        // reader that wins the first timeslice is not counted as a tear.
        clock.write(snap(1, 1u64.wrapping_mul(K).wrapping_add(MAGIC)));

        let w = {
            let clock = clock.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                let mut i = 2u64;
                while !stop.load(Ordering::Relaxed) {
                    clock.write(snap(i, i.wrapping_mul(K).wrapping_add(MAGIC)));
                    i = i.wrapping_add(1);
                    if i == 0 {
                        i = 1;
                    }
                    thread::yield_now();
                }
            })
        };
        let r = {
            let clock = clock.clone();
            let stop = stop.clone();
            let bad = bad.clone();
            let reads = reads.clone();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let s = clock.read();
                    reads.fetch_add(1, Ordering::Relaxed);
                    if s.is_none_or(|s| s.ns != s.cycles.wrapping_mul(K).wrapping_add(MAGIC)) {
                        bad.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        };
        // Miri's virtual clock gives the reader about 26 reads in 50 ms.
        let (run_ms, min_reads) = if cfg!(miri) { (500, 100) } else { (50, 1_000) };
        thread::sleep(Duration::from_millis(run_ms));
        stop.store(true, Ordering::Relaxed);
        let _ = w.join();
        let _ = r.join();
        assert_eq!(bad.load(Ordering::Relaxed), 0);
        assert!(reads.load(Ordering::Relaxed) > min_reads);
    }

    /// An independent evaluation of the clocksource formula.
    fn formula(s: Snapshot, c: Counter, raw: u64) -> u64 {
        let mask: u128 = if c.width >= 64 {
            u128::from(u64::MAX)
        } else {
            (1u128 << c.width) - 1
        };
        let d = (u128::from(raw) + (1u128 << 64) - u128::from(s.cycles)) & mask;
        let v = u128::from(s.ns) + ((d * u128::from(c.scale.mult)) >> c.scale.shift);
        v.min(u128::from(u64::MAX)) as u64
    }

    #[test]
    fn scale_from_hz_one_second() {
        for hz in [PM_TIMER_HZ, 100_000_000, 2_500_000_000] {
            let ns = Scale::from_hz(hz).unwrap().to_ns(hz);
            assert!(ns.abs_diff(1_000_000_000) <= 2, "{hz} Hz: {ns}");
        }
        assert_eq!(Scale::from_hz(0), None);
        assert_eq!(Counter::new(ClocksourceId::Tsc, 1_000, 0), None);
        assert_eq!(Counter::new(ClocksourceId::Tsc, 1_000, 65), None);
    }

    #[test]
    fn ns_at_matches_u128_formula() {
        for width in [24u32, 32, 64] {
            let c = Counter::new(ClocksourceId::AcpiPm, PM_TIMER_HZ, width).unwrap();
            let m = c.mask();
            let cases = [
                (0u64, 0u64),
                (5, 1_000),
                (m - 10, 20),
                (m, 0),
                (100, 99),
                (m / 2, m / 2 + 12_345),
            ];
            for (cycles, raw) in cases {
                let s = Snapshot {
                    id: c.id,
                    cycles,
                    ns: 7_000_000_000,
                };
                assert_eq!(
                    ns_at(s, c, raw),
                    formula(s, c, raw),
                    "w{width} {cycles} {raw}"
                );
            }
            // Wrapped: raw < cycles is one wrap on, not a backward step.
            let s = Snapshot {
                id: c.id,
                cycles: m - 9,
                ns: 0,
            };
            assert_eq!(ns_at(s, c, 0), c.scale.to_ns(10));
        }
    }

    #[test]
    fn cycles_to_ns_near_u64_max() {
        let c = Counter::new(ClocksourceId::Tsc, 1_000, 64).unwrap();
        // One cycle is 1 ms, so u64::MAX cycles overflow u64 ns.
        assert_eq!(c.scale.to_ns(u64::MAX), u64::MAX);
        let s = Snapshot {
            id: c.id,
            cycles: 0,
            ns: u64::MAX - 5,
        };
        assert_eq!(ns_at(s, c, 1), u64::MAX);
        assert_eq!(ns_at(s, c, 0), u64::MAX - 5);
        let mut w = ClockWriter::new(c, 0, u64::MAX - 1);
        assert_eq!(w.advance(u64::MAX).ns, u64::MAX);
        assert_eq!(w.advance(u64::MAX - 1).ns, u64::MAX);
    }

    #[test]
    fn wrap24_ten_wraps_read_every_half_wrap_loses_none() {
        let c = Counter::new(ClocksourceId::AcpiPm, PM_TIMER_HZ, 24).unwrap();
        let half = 1u64 << 23;
        let start = 0x00AB_CDEFu64;
        let mut w = ClockWriter::new(c, start, 0);
        let mut total = 0u64;
        let mut last = w.snapshot();
        for _ in 0..20 {
            total += half;
            last = w.advance((start + total) & c.mask());
        }
        assert_eq!(total >> 24, 10, "ten wraps");
        assert_eq!(last.ns, c.scale.to_ns(total));
        // A reader between two reads: within 1 ns of the whole count.
        let mid = total + half / 2;
        let got = ns_at(last, c, (start + mid) & c.mask());
        assert!(got.abs_diff(c.scale.to_ns(mid)) <= 1, "{got}");

        // Control: one read per full wrap and a bit loses a wrap each time.
        let mut lossy = ClockWriter::new(c, start, 0);
        let step = (1u64 << 24) + 5;
        let snap = lossy.advance((start + step) & c.mask());
        assert_eq!(snap.ns, c.scale.to_ns(5));
        assert!(snap.ns < c.scale.to_ns(step));
    }

    #[test]
    fn writer_switch_is_continuous() {
        let hpet = Counter::new(ClocksourceId::Hpet, 100_000_000, 64).unwrap();
        let tsc = Counter::new(ClocksourceId::Tsc, 2_500_000_000, 64).unwrap();
        let mut w = ClockWriter::new(hpet, 1_000, 0);
        let s = w.advance(1_000 + 100_000_000); // 1 s
        assert_eq!(s.ns, 1_000_000_000);
        let before = ns_at(s, hpet, 1_000 + 150_000_000); // 1.5 s
        let t = w.switch(tsc, 1_000 + 150_000_000, 77);
        assert_eq!(t.id, ClocksourceId::Tsc);
        assert_eq!(t.cycles, 77);
        assert_eq!(t.ns, before);
        assert_eq!(w.counter(), tsc);
        // 2.5e9 TSC cycles later is one more second.
        let u = w.advance(77 + 2_500_000_000);
        assert!(u.ns.abs_diff(before + 1_000_000_000) <= 1, "{}", u.ns);
    }

    fn counters() -> (Counter, Counter, Counter) {
        (
            Counter::new(ClocksourceId::Tsc, 2_000_000_000, 64).unwrap(),
            Counter::new(ClocksourceId::Hpet, 100_000_000, 64).unwrap(),
            Counter::new(ClocksourceId::AcpiPm, PM_TIMER_HZ, 24).unwrap(),
        )
    }

    #[test]
    fn rank_prefers_tsc_then_hpet_then_pm() {
        let (tsc, hpet, pm) = counters();
        let mut c = Candidates {
            tsc: Some(tsc),
            tsc_invariant: true,
            tsc_warp_ok: true,
            hpet: Some(hpet),
            pm: Some(pm),
        };
        assert_eq!(rank(&c), Some(tsc));
        c.tsc = None;
        assert_eq!(rank(&c), Some(hpet));
        c.hpet = None;
        assert_eq!(rank(&c), Some(pm));
    }

    #[test]
    fn rank_needs_invariant_warp_clean_tsc() {
        let (tsc, hpet, pm) = counters();
        let base = Candidates {
            tsc: Some(tsc),
            tsc_invariant: true,
            tsc_warp_ok: true,
            hpet: Some(hpet),
            pm: Some(pm),
        };
        let not_inv = Candidates {
            tsc_invariant: false,
            ..base
        };
        assert_eq!(rank(&not_inv), Some(hpet));
        let warped = Candidates {
            tsc_warp_ok: false,
            ..base
        };
        assert_eq!(rank(&warped), Some(hpet));
        let only_tsc = Candidates {
            tsc_warp_ok: false,
            hpet: None,
            ..base
        };
        assert_eq!(rank(&only_tsc), Some(pm));
    }

    #[test]
    fn rank_none_without_candidates() {
        let (tsc, _, _) = counters();
        let none = Candidates {
            tsc: None,
            tsc_invariant: true,
            tsc_warp_ok: true,
            hpet: None,
            pm: None,
        };
        assert_eq!(rank(&none), None);
        // A TSC that is not invariant is no candidate.
        let bad_tsc = Candidates {
            tsc: Some(tsc),
            tsc_invariant: false,
            ..none
        };
        assert_eq!(rank(&bad_tsc), None);
    }

    #[test]
    fn hpet_counter_width_from_gcap() {
        assert_eq!(hpet_counter_width(0), 32);
        assert_eq!(hpet_counter_width(1 << 13), 64);
        // QEMU's GCAP_ID: 10 ns period, 64-bit, legacy capable, vendor 0x8086.
        assert_eq!(hpet_counter_width(0x0098_9680_8086_A201), 64);
        assert_eq!(hpet_hz(10_000_000), Some(100_000_000));
        assert_eq!(hpet_hz(69_841_279), Some(14_318_179));
        assert_eq!(hpet_hz(0), None);
        assert_eq!(hpet_hz(HPET_PERIOD_FS_MAX + 1), None);
    }

    #[test]
    fn clocksource_names() {
        assert_eq!(ClocksourceId::Tsc.as_str(), "tsc");
        assert_eq!(ClocksourceId::Hpet.as_str(), "hpet");
        assert_eq!(ClocksourceId::AcpiPm.as_str(), "acpi_pm");
        for id in [
            ClocksourceId::Tsc,
            ClocksourceId::Hpet,
            ClocksourceId::AcpiPm,
        ] {
            assert_eq!(ClocksourceId::from_u64(id as u64), Some(id));
        }
        assert_eq!(ClocksourceId::from_u64(0), None);
        assert_eq!(ClocksourceId::from_u64(4), None);
    }

    #[test]
    fn calib_source_names() {
        assert_eq!(CalibSource::Hpet.as_str(), "hpet");
        assert_eq!(CalibSource::Pit.as_str(), "pit");
    }
}

#[cfg(all(test, loom))]
mod loom_models {
    extern crate std;

    use super::*;
    use crate::sync::variant::{Bound, check};
    use loom::sync::Arc;
    use loom::thread;

    /// Write `t`'s pair: every word derives from `t`, so the id, `cycles`
    /// and `ns` (an independent timestamp, `t * 1_000 + 7`) of one write
    /// match only each other.
    fn pair(t: u64) -> Snapshot {
        Snapshot {
            id: ClocksourceId::from_u64(t).unwrap(),
            cycles: t,
            ns: t * 1_000 + 7,
        }
    }

    /// A read's pair number; a read that mixes two writes fails.
    fn published(r: Option<Snapshot>) -> u64 {
        match r {
            Some(s) if (1..=2).contains(&s.cycles) && s == pair(s.cycles) => s.cycles,
            _ => panic!("seqlock: torn read"),
        }
    }

    /// The `TickClock` latch against its one writer. Main publishes pair(1)
    /// before the spawn, so both copies hold a published pair (`new`'s
    /// zero words are none); the writer thread then writes pair(2), which
    /// bumps `seq` twice and overwrites each copy once, while main reads
    /// twice. Every read is a published pair, and after the join the read
    /// is pair(2). Bound: 2 threads (the writer 1 write, main 2 reads),
    /// 3 preemptions. A second write multiplies the run time about 70-fold
    /// at this bound; both variants tear within one.
    fn seqlock_model(v: Option<Site>) {
        let bound = Bound {
            threads: 2,
            preemptions: 3,
        };
        check(v, bound, || {
            let c = Arc::new(TickClock::new());
            c.write(pair(1));
            let w = {
                let c = c.clone();
                thread::spawn(move || {
                    c.write(pair(2));
                })
            };
            for _ in 0..2 {
                published(c.read());
            }
            w.join().unwrap();
            assert_eq!(published(c.read()), 2);
        });
    }

    #[test]
    fn loom_seqlock_latch() {
        seqlock_model(None);
    }

    #[test]
    #[should_panic(expected = "seqlock: torn read")]
    fn loom_seqlock_acqrel_bump_tears_fails() {
        seqlock_model(Some(Site::SeqlockBumpAcqRel));
    }

    #[test]
    #[should_panic(expected = "seqlock: torn read")]
    fn loom_seqlock_no_leading_fence_tears_fails() {
        seqlock_model(Some(Site::SeqlockLeadingFence));
    }
}
