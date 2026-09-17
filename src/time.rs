//! Timekeeping arithmetic, seqlock, and PIT/TSC constants. DESIGN §6.
//!
//! Portable half: interpolation, seqlock, deadline math, wall-clock offset.
//! Port I/O, HPET MMIO, and the IRQ0 handler live in the binary crate.

use core::sync::atomic::{fence, AtomicU64, Ordering};

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

/// Tick milliseconds plus TSC interpolation since that tick.
/// Wrapping TSC delta; saturates at `u64::MAX`.
pub fn interpolate_us(tick_ms: u64, tsc_at_tick: u64, tsc_now: u64, tsc_per_ms: u64) -> u64 {
    interpolate(tick_ms, tsc_at_tick, tsc_now, tsc_per_ms, 1_000)
}

pub fn interpolate_ns(tick_ms: u64, tsc_at_tick: u64, tsc_now: u64, tsc_per_ms: u64) -> u64 {
    interpolate(tick_ms, tsc_at_tick, tsc_now, tsc_per_ms, 1_000_000)
}

fn interpolate(
    tick_ms: u64,
    tsc_at_tick: u64,
    tsc_now: u64,
    tsc_per_ms: u64,
    per_ms: u128,
) -> u64 {
    let base = (tick_ms as u128).saturating_mul(per_ms);
    if tsc_per_ms == 0 {
        return clamp_u64(base);
    }
    let delta = tsc_now.wrapping_sub(tsc_at_tick) as u128;
    let extra = delta.saturating_mul(per_ms) / tsc_per_ms as u128;
    clamp_u64(base.saturating_add(extra))
}

fn clamp_u64(v: u128) -> u64 {
    if v > u64::MAX as u128 {
        u64::MAX
    } else {
        v as u64
    }
}

/// Seqlock over (tick, tsc snapshot). Writer: bump, write both, bump,
/// release. Reader: acquire, retry until a stable even sequence. DESIGN §6.4.
pub struct TickClock {
    seq: AtomicU64,
    tick: AtomicU64,
    tsc: AtomicU64,
}

impl TickClock {
    pub const fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            tick: AtomicU64::new(0),
            tsc: AtomicU64::new(0),
        }
    }

    /// ISR path. No alloc, no logging.
    ///
    /// Odd bump is `fetch_add(AcqRel)`, not Relaxed load/store. Relaxed
    /// lets the compiler publish (tick, tsc) while seq still looks even.
    /// Release on that RMW is the wrong side of the increment (it orders
    /// writes *before* it). Acquire keeps the payload stores after seq is
    /// odd. Even bump is Release.
    pub fn write(&self, tick: u64, tsc: u64) {
        self.seq.fetch_add(1, Ordering::AcqRel);
        self.tick.store(tick, Ordering::Relaxed);
        self.tsc.store(tsc, Ordering::Relaxed);
        self.seq.fetch_add(1, Ordering::Release);
    }

    /// Stable (tick, tsc_at_tick). Retries on odd or changed sequence.
    pub fn read(&self) -> (u64, u64) {
        loop {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 != 0 {
                continue;
            }
            let tick = self.tick.load(Ordering::Relaxed);
            let tsc = self.tsc.load(Ordering::Relaxed);
            // Payload loads must not move past the seq re-check.
            fence(Ordering::Acquire);
            let s2 = self.seq.load(Ordering::Relaxed);
            if s1 == s2 {
                return (tick, tsc);
            }
        }
    }

    /// `now_us` with `read_tsc` sampled *after* the snapshot and before
    /// the sequence re-check, so a writer mid-read forces a retry.
    pub fn now_us_with<F: FnMut() -> u64>(&self, mut read_tsc: F, tsc_per_ms: u64) -> u64 {
        self.now_with(&mut read_tsc, tsc_per_ms, interpolate_us)
    }

    pub fn now_ns_with<F: FnMut() -> u64>(&self, mut read_tsc: F, tsc_per_ms: u64) -> u64 {
        self.now_with(&mut read_tsc, tsc_per_ms, interpolate_ns)
    }

    fn now_with<F: FnMut() -> u64>(
        &self,
        read_tsc: &mut F,
        tsc_per_ms: u64,
        interp: fn(u64, u64, u64, u64) -> u64,
    ) -> u64 {
        loop {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 != 0 {
                continue;
            }
            let tick = self.tick.load(Ordering::Relaxed);
            let tsc = self.tsc.load(Ordering::Relaxed);
            let tsc_now = read_tsc();
            fence(Ordering::Acquire);
            let s2 = self.seq.load(Ordering::Relaxed);
            if s1 == s2 {
                return interp(tick, tsc, tsc_now, tsc_per_ms);
            }
        }
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

pub const fn bcd_to_bin(v: u8) -> u8 {
    (v & 0x0F) + ((v >> 4) & 0x0F) * 10
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::Arc;
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

    #[test]
    fn interpolate_zero_and_exact_ms() {
        let k = 2_500_000;
        assert_eq!(interpolate_us(0, 100, 100, k), 0);
        assert_eq!(interpolate_us(5, 0, 0, k), 5_000);
        assert_eq!(interpolate_us(5, 0, k, k), 6_000);
        assert_eq!(interpolate_ns(5, 0, k, k), 6_000_000);
        assert_eq!(interpolate_us(1, 0, 0, 0), 1_000);
    }

    #[test]
    fn interpolate_near_u64_max() {
        let k = 1_000_000;
        let tick = u64::MAX / 1000;
        let us = interpolate_us(tick, 0, 0, k);
        assert_eq!(us, tick.saturating_mul(1000));
        assert_eq!(interpolate_us(u64::MAX, 0, 0, k), u64::MAX);
        assert_eq!(interpolate_ns(u64::MAX, 0, 0, k), u64::MAX);
        // wrapping TSC across u64::MAX: one tick of delta
        assert_eq!(interpolate_us(0, u64::MAX, 0, 1000), 1);
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

    #[test]
    fn wall_tracks_monotonic_offset() {
        let origin = WallOrigin {
            unix_s: 1_000,
            mono_ns: 5_000_000_000,
        };
        assert_eq!(wall_unix_s(origin, 5_000_000_000), 1_000);
        assert_eq!(wall_unix_s(origin, 8_000_000_000), 1_003);
    }

    /// DESIGN §8.1: the writer publishes an independent timestamp. A torn
    /// (tick, tsc) pair must not match that value. Seqlock retry must.
    #[test]
    fn now_us_seqlock_retry_under_simulated_writer() {
        let clock = TickClock::new();
        const K: u64 = 1_000_000;
        // Independent published pairs. Writer's second pair is the
        // timestamp we compare against — not a value derived from the
        // possibly-torn first read.
        const TICK0: u64 = 1;
        const TSC0: u64 = 100;
        const TICK1: u64 = 2;
        const TSC1: u64 = 200;
        clock.write(TICK0, TSC0);

        let mut samples = 0u32;
        let got = clock.now_us_with(
            || {
                samples += 1;
                if samples == 1 {
                    clock.write(TICK1, TSC1);
                    return 10_000;
                }
                250
            },
            K,
        );
        assert_eq!(samples, 2, "reader must retry after the simulated ISR");
        let expected = interpolate_us(TICK1, TSC1, 250, K);
        assert_eq!(got, expected);
        let torn = interpolate_us(TICK0, TSC0, 10_000, K);
        assert_ne!(
            torn, expected,
            "torn mix must not accidentally equal the independent timestamp"
        );
    }

    #[test]
    fn seqlock_read_stable_pair() {
        let clock = TickClock::new();
        clock.write(42, 99);
        assert_eq!(clock.read(), (42, 99));
        clock.write(43, 100);
        assert_eq!(clock.read(), (43, 100));
    }

    #[test]
    fn seqlock_threaded_writer_never_tears() {
        let clock = Arc::new(TickClock::new());
        let stop = Arc::new(AtomicBool::new(false));
        let bad = Arc::new(AtomicU64::new(0));
        let reads = Arc::new(AtomicU64::new(0));
        const K: u64 = 1_000;
        const MAGIC: u64 = 7;
        // Default (0, 0) is even-seq but fails MAGIC. Seed so a reader
        // that wins the first timeslice is not counted as a tear.
        clock.write(1, 1u64.wrapping_mul(K).wrapping_add(MAGIC));

        let w = {
            let clock = clock.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                let mut i = 2u64;
                while !stop.load(Ordering::Relaxed) {
                    clock.write(i, i.wrapping_mul(K).wrapping_add(MAGIC));
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
                    let (tick, tsc) = clock.read();
                    reads.fetch_add(1, Ordering::Relaxed);
                    if tsc != tick.wrapping_mul(K).wrapping_add(MAGIC) {
                        bad.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        };
        thread::sleep(Duration::from_millis(50));
        stop.store(true, Ordering::Relaxed);
        let _ = w.join();
        let _ = r.join();
        assert_eq!(bad.load(Ordering::Relaxed), 0);
        assert!(reads.load(Ordering::Relaxed) > 1_000);
    }

    #[test]
    fn calib_source_names() {
        assert_eq!(CalibSource::Hpet.as_str(), "hpet");
        assert_eq!(CalibSource::Pit.as_str(), "pit");
    }
}
