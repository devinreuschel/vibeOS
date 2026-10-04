//! [`ReloadSpans`]: a down-counting timer's reload period from reads of its
//! count. DESIGN §6.3.

/// Reload-to-reload spans [`ReloadSpans`] takes the median of.
pub const RELOAD_SPANS: usize = 20;

/// A down-counting timer's reload period in TSC cycles, from reads of its
/// count, each bracketed by the TSC (`kernel_tests`' `pit_tick_rate` and
/// `lapic_timer_rearm`). The count is the timer's own state, which the
/// device computes from its clock when it is read, so the period it shows
/// does not depend on when the timer's interrupts are delivered: under TCG
/// a host that wakes QEMU's main loop every 5 to 10 ms delivers them at
/// that granularity, while the count keeps the programmed rate.
///
/// A count above the one before marks a reload between the two reads. A
/// span is the TSC between two such marks with every read between them,
/// the marks' own included, no more than `max_gap` cycles after the read
/// before it: then no reload fell unseen between two reads as long as the
/// period exceeds `max_gap`, and each mark is placed within `max_gap` of
/// its reload. A longer gap (an interrupt, a preemption, a host stall)
/// drops the span it falls in. The median of [`RELOAD_SPANS`] spans, the
/// lower of the middle two, is the period; one of `max_gap` or less can
/// alias, so the caller picks `max_gap` below the shortest period it must
/// tell apart.
#[derive(Clone, Copy, Debug)]
pub struct ReloadSpans {
    max_gap: u64,
    /// The last read: the TSC before it and its count.
    prev: Option<(u64, u64)>,
    /// The TSC of the last reload mark with no long gap since.
    mark: Option<u64>,
    spans: [u64; RELOAD_SPANS],
    n: usize,
}

impl ReloadSpans {
    pub const fn new(max_gap: u64) -> Self {
        Self {
            max_gap,
            prev: None,
            mark: None,
            spans: [0; RELOAD_SPANS],
            n: 0,
        }
    }

    /// One read of the count `count`, which the TSC read `tsc_lo` before
    /// and `tsc_hi` after. The read is a fresh start when its bracket,
    /// with the read before it, spans more than `max_gap` or runs
    /// backwards.
    pub fn push(&mut self, tsc_lo: u64, count: u64, tsc_hi: u64) {
        let prev = self.prev.replace((tsc_lo, count));
        let Some((prev_lo, prev_count)) = prev else {
            return;
        };
        let gap = tsc_hi.checked_sub(prev_lo);
        if tsc_hi < tsc_lo || gap.is_none_or(|g| g > self.max_gap) {
            self.mark = None;
            return;
        }
        if count <= prev_count {
            return;
        }
        if let Some(mark) = self.mark
            && let Some(slot) = self.spans.get_mut(self.n)
        {
            *slot = tsc_hi.saturating_sub(mark);
            self.n = self.n.saturating_add(1);
        }
        self.mark = Some(tsc_hi);
    }

    /// Spans taken so far, at most [`RELOAD_SPANS`].
    pub fn len(&self) -> usize {
        self.n
    }

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// The period, the median span, once [`RELOAD_SPANS`] spans are in.
    pub fn period(&self) -> Option<u64> {
        if self.n < RELOAD_SPANS {
            return None;
        }
        let mut spans = self.spans;
        spans.sort_unstable();
        spans.get((RELOAD_SPANS - 1) / 2).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads of a down-counter whose period is `period` TSC cycles, at the
    /// TSC values `at` lists, each bracketed 10 cycles either side.
    fn reload_period(max_gap: u64, period: u64, at: impl Iterator<Item = u64>) -> ReloadSpans {
        let mut r = ReloadSpans::new(max_gap);
        for t in at {
            let count = period - 1 - (t + 10) % period;
            r.push(t, count, t + 20);
        }
        r
    }

    #[test]
    fn reload_spans_find_the_period_however_late_the_reads() {
        let k = 2_400_000u64;
        // A read every 5 us: the period within the reads' spacing.
        let steady = reload_period(k / 4, k, (0..25 * 480).map(|i| i * k / 200));
        let p = steady.period().unwrap();
        assert!(p.abs_diff(k) <= k / 200 + 20, "{p}");
        // Every few reads, a gap of 5 to 10 ms the way a host that sleeps
        // in 5 ms steps holds a vCPU: the spans it cuts are dropped.
        let mut t = 0u64;
        let gappy = (0..40_000u64).map(|i| {
            t += if i % 300 == 299 {
                k * (5 + i % 6)
            } else {
                k / 200
            };
            t
        });
        let p = reload_period(k / 4, k, gappy).period().unwrap();
        assert!(p.abs_diff(k) <= k / 200 + 20, "{p}");
        // A period of 3 and of a third of the tick reads as such.
        let slow = reload_period(k / 4, 3 * k, (0..90 * 200).map(|i| i * k / 200));
        assert!(slow.period().unwrap().abs_diff(3 * k) <= k / 200 + 20);
        let fast = reload_period(k / 4, k / 3, (0..25 * 200).map(|i| i * k / 200));
        assert!(fast.period().unwrap().abs_diff(k / 3) <= k / 200 + 20);
    }

    #[test]
    fn reload_spans_need_short_gaps_and_enough_spans() {
        let k = 2_400_000u64;
        // Fewer than RELOAD_SPANS spans: no period yet.
        let few = reload_period(k / 4, k, (0..10 * 200).map(|i| i * k / 200));
        assert!(few.len() < RELOAD_SPANS && few.period().is_none());
        // Every gap past max_gap: no reload is marked, nothing is in.
        let sparse = reload_period(k / 4, k, (0..400).map(|i| i * (k / 4 + 1)));
        assert!(sparse.is_empty() && sparse.period().is_none());
        // A bracket that runs backwards drops the reload marked before it.
        let marks = |third_hi| {
            let mut r = ReloadSpans::new(k / 4);
            r.push(0, 5, 10);
            r.push(20, 9, 30);
            r.push(40, 3, third_hi);
            r.push(50, 8, 60);
            r.len()
        };
        assert_eq!(marks(45), 1);
        assert_eq!(marks(35), 0);
    }
}
