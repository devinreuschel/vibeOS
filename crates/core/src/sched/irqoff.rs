//! IF-off stretch bookkeeping for the `irqoff` tracer (ROADMAP §10.3,
//! INVARIANTS.md §2.9 rule 2). Portable and host-tested; the kernel half,
//! `sched::irqoff`, stamps the cycle counter where IF changes and records
//! each closed stretch here.
//!
//! Everything is atomics (C-ATOMICS) and nothing allocates, so the kernel's
//! hooks can run inside `InterruptGuard` and the lock paths without
//! recursing.

use core::fmt;
use core::panic::Location;

use crate::atomic::statics::{AtomicU32, AtomicU64, Ordering};

/// The longest IF=0 stretch rule 2 allows, in ns of cycle-counter time
/// (INVARIANTS.md §2.9 rule 2). Under `-icount shift=0` one instruction is
/// 1 ns of guest time, so this is exactly the rule's 100,000 instructions.
pub const BOUND_NS: u64 = 100_000;

/// `cycles` of a counter running at `freq_hz`, in ns: computed in `u128`,
/// saturating at `u64::MAX`. A zero frequency measures nothing (0).
pub fn cycles_to_ns(cycles: u64, freq_hz: u64) -> u64 {
    if freq_hz == 0 {
        return 0;
    }
    let ns = u128::from(cycles) * 1_000_000_000 / u128::from(freq_hz);
    u64::try_from(ns).unwrap_or(u64::MAX)
}

/// Tag bit of a [`Site`] that holds a `&'static Location` address.
const LOC: u64 = 1 << 63;
/// Kind of a [`Site`] that names an IDT vector: `0x100 | vector`.
const VEC: u64 = 0x100;
/// Kind of a [`Site`] that names a syscall stub: `0x200 | kind`.
const SYSCALL: u64 = 0x200;

/// Where an IF=0 stretch began: the Rust code that turned IF off (its
/// `Location`), an entry stub's vector, or a syscall stub. Encoded in one
/// `u64` so a per-CPU atomic holds it; 0 is no site.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Site(u64);

impl Site {
    /// No site.
    pub const NONE: Site = Site(0);
    /// The syscall entry stub, before its `sti`.
    pub const SYSCALL_ENTRY: Site = Site(SYSCALL);
    /// The syscall exit, from its `cli` to the return.
    pub const SYSCALL_EXIT: Site = Site(SYSCALL | 1);

    /// The code at `loc`. A canonical address keeps its sign in bit 62,
    /// which [`Site::location`] copies back into the tag bit.
    pub fn caller(loc: &'static Location<'static>) -> Site {
        Site(core::ptr::from_ref(loc) as u64 | LOC)
    }

    /// The entry stub of `vector`.
    pub const fn vector(vector: u8) -> Site {
        Site(VEC | vector as u64)
    }

    /// A site the asm stubs pass as a plain number (`vector` or a syscall
    /// kind): never a `Location`, whatever the bits.
    pub const fn from_stub(bits: u64) -> Site {
        Site(bits & !LOC)
    }

    /// The encoding, for a per-CPU atomic. [`Site::from_bits`] reverses it.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// The site `bits` encodes.
    ///
    /// # Safety
    /// `bits` came from [`Site::bits`] (or is 0): a tagged value is then a
    /// `&'static Location`'s address.
    pub const unsafe fn from_bits(bits: u64) -> Site {
        Site(bits)
    }

    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// The `Location` this site names, if it names one.
    pub fn location(self) -> Option<&'static Location<'static>> {
        if self.0 & LOC == 0 {
            return None;
        }
        let low = self.0 & !LOC;
        let addr = if low & (1 << 62) != 0 { low | LOC } else { low };
        // SAFETY: invariant: a tagged `Site` holds a `&'static Location`'s
        // address with bit 63 forced on, which the sign in bit 62 restores
        // for a canonical address; established by `sched::irqoff::Site::caller`,
        // the only constructor that sets the tag (`from_stub` clears it and
        // `from_bits` is unsafe).
        Some(unsafe { &*(addr as *const Location<'static>) })
    }
}

impl fmt::Display for Site {
    /// `file:line`, `vec0xNN`, `syscall:entry` or `syscall:exit`, never with
    /// a space or an exception mnemonic, which the harness would read as a
    /// panic signature.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(loc) = self.location() {
            for c in loc.file().chars() {
                let c = if c.is_whitespace() || c == '#' {
                    '_'
                } else {
                    c
                };
                f.write_fmt(format_args!("{c}"))?;
            }
            return write!(f, ":{}", loc.line());
        }
        match self.0 {
            0 => f.write_str("none"),
            v if v & !0xFF == VEC => write!(f, "vec{:#04x}", v & 0xFF),
            v if v == Site::SYSCALL_ENTRY.0 => f.write_str("syscall:entry"),
            v if v == Site::SYSCALL_EXIT.0 => f.write_str("syscall:exit"),
            v => write!(f, "site{v:#x}"),
        }
    }
}

/// First octave the histogram resolves: `2^LOW_SHIFT` ns.
const LOW_SHIFT: u32 = 6;
/// Octaves from `2^6` to `2^38` ns.
const OCTAVES: usize = 32;
/// Sub-buckets per octave.
const SUBS: usize = 8;
const BUCKETS: usize = OCTAVES * SUBS;

/// A log-linear histogram of ns: 8 sub-buckets per power of two over
/// `2^6..2^38` ns, plus an underflow and an overflow bucket. A percentile
/// is a bucket's upper edge, at most one sub-bucket (1/8 of its octave's
/// base) above the exact value.
pub struct Histogram {
    buckets: [AtomicU32; BUCKETS],
    under: AtomicU32,
    over: AtomicU32,
    max: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    pub const fn new() -> Self {
        Self {
            buckets: [const { AtomicU32::new(0) }; BUCKETS],
            under: AtomicU32::new(0),
            over: AtomicU32::new(0),
            max: AtomicU64::new(0),
        }
    }

    /// The bucket of `ns`: `None` below `2^6` ns, `Some(BUCKETS)` at or
    /// above `2^38` ns.
    fn index(ns: u64) -> Option<usize> {
        if ns < 1 << LOW_SHIFT {
            return None;
        }
        let k = 63 - ns.leading_zeros();
        let octave = (k - LOW_SHIFT) as usize;
        if octave >= OCTAVES {
            return Some(BUCKETS);
        }
        let sub = ((ns >> (k - 3)) & 7) as usize;
        Some(octave * SUBS + sub)
    }

    /// The largest ns bucket `i` holds.
    fn upper(i: usize) -> u64 {
        let k = (i / SUBS) as u32 + LOW_SHIFT;
        let sub = (i % SUBS) as u64;
        (1u64 << k) + ((sub + 1) << (k - 3)) - 1
    }

    pub fn record(&self, ns: u64) {
        // Relaxed: counters; readers want totals, not an order.
        match Self::index(ns) {
            None => self.under.fetch_add(1, Ordering::Relaxed),
            Some(BUCKETS) => self.over.fetch_add(1, Ordering::Relaxed),
            Some(i) => self.buckets[i].fetch_add(1, Ordering::Relaxed),
        };
        self.max.fetch_max(ns, Ordering::Relaxed);
    }

    /// Samples recorded.
    pub fn count(&self) -> u64 {
        let mut n = u64::from(self.under.load(Ordering::Relaxed))
            + u64::from(self.over.load(Ordering::Relaxed));
        for b in &self.buckets {
            n += u64::from(b.load(Ordering::Relaxed));
        }
        n
    }

    /// The largest sample.
    pub fn max(&self) -> u64 {
        self.max.load(Ordering::Relaxed)
    }

    /// The `permille`-th percentile (990 for p99): the upper edge of the
    /// bucket holding that rank, never above [`Histogram::max`]. 0 when
    /// empty.
    pub fn percentile(&self, permille: u32) -> u64 {
        let n = self.count();
        if n == 0 {
            return 0;
        }
        let rank = (n * u64::from(permille.min(1000))).div_ceil(1000).max(1);
        let max = self.max();
        let mut seen = u64::from(self.under.load(Ordering::Relaxed));
        if seen >= rank {
            return max.min((1 << LOW_SHIFT) - 1);
        }
        for (i, b) in self.buckets.iter().enumerate() {
            seen += u64::from(b.load(Ordering::Relaxed));
            if seen >= rank {
                return max.min(Self::upper(i));
            }
        }
        max
    }
}

/// One site's totals. `key` is its [`Site`] bits, 0 while the slot is free.
/// Deliberate stretches (C-IRQOFF-GUARD) count apart and stay out of the
/// histogram, the max and `over`.
pub struct SiteStats {
    key: AtomicU64,
    pub count: AtomicU32,
    pub over: AtomicU32,
    pub deliberate: AtomicU32,
    pub deliberate_max_ns: AtomicU64,
    /// The `reason` [`SiteStats::set_reason`] kept, as a `&'static str`'s
    /// address and length (0 for none).
    reason_ptr: AtomicU64,
    reason_len: AtomicU64,
    pub unmatched: AtomicU32,
    pub max_ns: AtomicU64,
    pub hist: Histogram,
    /// What the last report printed, so the next prints deltas. Only the
    /// reporter writes these.
    pub reported_count: AtomicU32,
    pub reported_over: AtomicU32,
    pub reported_deliberate: AtomicU32,
    pub reported_unmatched: AtomicU32,
}

impl Default for SiteStats {
    fn default() -> Self {
        Self::new()
    }
}

impl SiteStats {
    pub const fn new() -> Self {
        Self {
            key: AtomicU64::new(0),
            count: AtomicU32::new(0),
            over: AtomicU32::new(0),
            deliberate: AtomicU32::new(0),
            deliberate_max_ns: AtomicU64::new(0),
            reason_ptr: AtomicU64::new(0),
            reason_len: AtomicU64::new(0),
            unmatched: AtomicU32::new(0),
            max_ns: AtomicU64::new(0),
            hist: Histogram::new(),
            reported_count: AtomicU32::new(0),
            reported_over: AtomicU32::new(0),
            reported_deliberate: AtomicU32::new(0),
            reported_unmatched: AtomicU32::new(0),
        }
    }

    /// The site this slot counts, or [`Site::NONE`] while it is free.
    pub fn site(&self) -> Site {
        // Acquire: pairs with the claiming CAS in `SiteTable::get_or_insert`.
        let bits = self.key.load(Ordering::Acquire);
        // SAFETY: invariant: a slot's key is 0 or a `Site::bits` value;
        // established by `sched::irqoff::SiteTable::get_or_insert`, its only
        // store.
        unsafe { Site::from_bits(bits) }
    }

    /// Count one closed stretch; a `deliberate` one apart from the rest.
    pub fn record(&self, c: Closed, deliberate: bool) {
        // Relaxed: counters; the reporter reads totals, not an order.
        if deliberate {
            self.deliberate.fetch_add(1, Ordering::Relaxed);
            self.deliberate_max_ns.fetch_max(c.ns, Ordering::Relaxed);
            return;
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        self.max_ns.fetch_max(c.ns, Ordering::Relaxed);
        self.hist.record(c.ns);
        if c.over {
            self.over.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Keep `reason` for this site's `deliberate` line. The first reason
    /// stays: one site is one `deliberate` call, whose reason is a literal.
    pub fn set_reason(&self, reason: &'static str) {
        if reason.is_empty() {
            return;
        }
        // AcqRel on success: only the one CAS that claims the pointer
        // stores the length, and its Release store below publishes the
        // pair to `reason`.
        if self
            .reason_ptr
            .compare_exchange(
                0,
                reason.as_ptr() as u64,
                Ordering::AcqRel,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            self.reason_len
                .store(reason.len() as u64, Ordering::Release);
        }
    }

    /// The reason [`SiteStats::set_reason`] kept, or `""`.
    pub fn reason(&self) -> &'static str {
        // Acquire: pairs with the Release store in `set_reason`, after
        // which the pointer is the claimed one.
        let n = self.reason_len.load(Ordering::Acquire);
        if n == 0 {
            return "";
        }
        let p = self.reason_ptr.load(Ordering::Relaxed);
        // SAFETY: invariant: a nonzero `reason_len` was stored once, by the
        // CAS winner that stored `reason_ptr`, both from one `&'static str`;
        // established by `sched::irqoff::SiteStats::set_reason`, their only
        // stores.
        let bytes = unsafe { core::slice::from_raw_parts(p as *const u8, n as usize) };
        core::str::from_utf8(bytes).unwrap_or("")
    }
}

/// A fixed table of [`SiteStats`], keyed by [`Site`]: lock-free insert by a
/// CAS on an empty key and linear probing, and a `dropped` count of the
/// sites that found it full.
pub struct SiteTable<const N: usize> {
    slots: [SiteStats; N],
    dropped: AtomicU32,
}

impl<const N: usize> Default for SiteTable<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> SiteTable<N> {
    pub const fn new() -> Self {
        Self {
            slots: [const { SiteStats::new() }; N],
            dropped: AtomicU32::new(0),
        }
    }

    /// The slot for `site`, claimed on first use. `None` when the table is
    /// full, which counts one `dropped`, or for [`Site::NONE`].
    pub fn get_or_insert(&self, site: Site) -> Option<&SiteStats> {
        let key = site.bits();
        if key == 0 || N == 0 {
            return None;
        }
        // Fibonacci hashing spreads the Location addresses, which share
        // their low bits' alignment.
        let start = (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as usize % N;
        for i in 0..N {
            let slot = &self.slots[(start + i) % N];
            // Acquire: pairs with the claiming CAS below on another CPU.
            let k = slot.key.load(Ordering::Acquire);
            if k == key {
                return Some(slot);
            }
            if k == 0 {
                // AcqRel: publishes the claim; a failed CAS reads the key
                // another CPU claimed the slot with.
                match slot
                    .key
                    .compare_exchange(0, key, Ordering::AcqRel, Ordering::Acquire)
                {
                    Ok(_) => return Some(slot),
                    Err(k) if k == key => return Some(slot),
                    Err(_) => {}
                }
            }
        }
        // Relaxed: a counter.
        self.dropped.fetch_add(1, Ordering::Relaxed);
        None
    }

    /// The claimed slots.
    pub fn iter(&self) -> impl Iterator<Item = &SiteStats> {
        self.slots.iter().filter(|s| !s.site().is_none())
    }

    /// Sites that found the table full.
    pub fn dropped(&self) -> u32 {
        // Relaxed: a counter.
        self.dropped.load(Ordering::Relaxed)
    }
}

/// An open IF=0 stretch: the counter when IF went to 0, the site that did
/// it, the cycles an exempt wait spent inside it (rule 2's exemptions),
/// and whether a `deliberate` guard marked it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stretch {
    pub start: u64,
    pub site: Site,
    pub exempt: u64,
    pub deliberate: bool,
}

/// A closed stretch: its length in ns without the exempt cycles, and
/// whether that is past [`BOUND_NS`] (never for a deliberate one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Closed {
    pub ns: u64,
    pub over: bool,
}

/// Close `s` at counter value `now`. The counter may wrap between the two
/// stamps.
pub fn close(s: &Stretch, now: u64, freq_hz: u64) -> Closed {
    let held = now.wrapping_sub(s.start).saturating_sub(s.exempt);
    let ns = cycles_to_ns(held, freq_hz);
    Closed {
        ns,
        over: !s.deliberate && ns > BOUND_NS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GHZ: u64 = 1_000_000_000;

    fn stretch(start: u64, exempt: u64, deliberate: bool) -> Stretch {
        Stretch {
            start,
            site: Site::vector(0x20),
            exempt,
            deliberate,
        }
    }

    #[test]
    fn bound_is_rule_2() {
        assert_eq!(BOUND_NS, 100_000);
        assert_eq!(cycles_to_ns(100_000, 1_000_000_000), 100_000);
        assert_eq!(cycles_to_ns(3_000, 3_000_000_000), 1_000);
        assert_eq!(cycles_to_ns(u64::MAX, 1), u64::MAX);
        assert_eq!(cycles_to_ns(5, 0), 0);
    }

    #[test]
    fn close_subtracts_exempt() {
        let c = close(&stretch(1_000, 150_000, false), 201_000, GHZ);
        assert_eq!(c.ns, 50_000);
        assert!(!c.over);
        // More exempt than elapsed (nested exemptions) is a zero stretch.
        let c = close(&stretch(1_000, 999_999, false), 2_000, GHZ);
        assert_eq!(c.ns, 0);
    }

    #[test]
    fn close_wraps_counter() {
        let c = close(&stretch(u64::MAX - 99, 0, false), 200_000, GHZ);
        assert_eq!(c.ns, 200_100);
        assert!(c.over);
    }

    #[test]
    fn over_only_past_bound() {
        assert!(!close(&stretch(0, 0, false), BOUND_NS, GHZ).over);
        assert!(close(&stretch(0, 0, false), BOUND_NS + 1, GHZ).over);
        // 2 GHz: 200,001 cycles is just past 100,000 ns.
        assert!(!close(&stretch(0, 0, false), 200_000, 2 * GHZ).over);
        assert!(close(&stretch(0, 0, false), 200_002, 2 * GHZ).over);
    }

    #[test]
    fn deliberate_never_over() {
        let c = close(&stretch(0, 0, true), 3_000_000_000, GHZ);
        assert_eq!(c.ns, 3_000_000_000);
        assert!(!c.over);
    }

    #[test]
    fn p99_within_bucket() {
        // A fixed LCG, so the samples are the same every run.
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = || {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            x >> 33
        };
        for round in 0..20 {
            let h = Histogram::new();
            let n = 500 + round * 97;
            let mut v: Vec<u64> = (0..n)
                .map(|_| {
                    // Spread over 2^4..2^30 ns, underflow included.
                    let bits = 4 + next() % 27;
                    (1u64 << bits) + next() % (1u64 << bits)
                })
                .collect();
            for &ns in &v {
                h.record(ns);
            }
            v.sort_unstable();
            for permille in [500u32, 900, 990, 999, 1000] {
                let rank = (v.len() as u64 * u64::from(permille)).div_ceil(1000).max(1);
                let exact = v[rank as usize - 1];
                let got = h.percentile(permille);
                // The bucket's upper edge: at or above the exact value, and
                // above it by at most one sub-bucket, 1/8 of the octave's
                // base, so within 12.5% of the exact value.
                assert!(got >= exact, "p{permille}: {got} < exact {exact}");
                assert!(
                    got - exact <= exact / 8,
                    "p{permille}: {got} is over 12.5% above exact {exact}"
                );
            }
            assert_eq!(h.max(), *v.last().unwrap());
            assert_eq!(h.count(), n);
        }
        assert_eq!(Histogram::new().percentile(990), 0);
    }

    #[test]
    fn histogram_edges() {
        let h = Histogram::new();
        h.record(10);
        h.record(1 << 40);
        assert_eq!(h.count(), 2);
        assert_eq!(h.percentile(500), 63);
        assert_eq!(h.percentile(1000), 1 << 40);
        for i in 0..BUCKETS {
            let hi = Histogram::upper(i);
            assert_eq!(Histogram::index(hi), Some(i));
            assert_eq!(Histogram::index(hi + 1), Some(i + 1));
        }
    }

    #[test]
    fn table_full_counts_dropped() {
        let t: SiteTable<4> = SiteTable::new();
        for v in 0..4u8 {
            assert!(t.get_or_insert(Site::vector(v)).is_some());
        }
        // A known site still finds its slot; a fifth one is dropped.
        assert!(t.get_or_insert(Site::vector(2)).is_some());
        assert!(t.get_or_insert(Site::vector(9)).is_none());
        assert!(t.get_or_insert(Site::vector(10)).is_none());
        assert_eq!(t.dropped(), 2);
        assert_eq!(t.iter().count(), 4);
        assert!(t.get_or_insert(Site::NONE).is_none());
        let s = t.get_or_insert(Site::vector(3)).unwrap();
        assert_eq!(s.site(), Site::vector(3));
    }

    #[test]
    fn site_stats_split_deliberate() {
        let s = SiteStats::new();
        s.record(
            Closed {
                ns: 150_000,
                over: true,
            },
            false,
        );
        s.record(
            Closed {
                ns: 90,
                over: false,
            },
            false,
        );
        s.record(
            Closed {
                ns: 3_000_000,
                over: false,
            },
            true,
        );
        assert_eq!(s.reason(), "");
        s.set_reason("test hold");
        s.set_reason("second");
        assert_eq!(s.reason(), "test hold");
        assert_eq!(s.count.load(Ordering::Relaxed), 2);
        assert_eq!(s.over.load(Ordering::Relaxed), 1);
        assert_eq!(s.max_ns.load(Ordering::Relaxed), 150_000);
        assert_eq!(s.deliberate.load(Ordering::Relaxed), 1);
        assert_eq!(s.deliberate_max_ns.load(Ordering::Relaxed), 3_000_000);
    }

    #[test]
    fn site_fmt_has_no_space_or_mnemonic() {
        let loc = Location::caller();
        let here = Site::caller(loc);
        let text = format!("{here}");
        assert_eq!(text, format!("{}:{}", loc.file(), loc.line()));
        assert!(text.contains("irqoff.rs"), "{text}");
        assert_eq!(here.location().map(|l| l.file()), Some(file!()));
        let all = [
            text,
            format!("{}", Site::vector(0x0E)),
            format!("{}", Site::vector(0x0D)),
            format!("{}", Site::vector(0xF0)),
            format!("{}", Site::SYSCALL_ENTRY),
            format!("{}", Site::SYSCALL_EXIT),
            format!("{}", Site::from_stub(u64::MAX)),
        ];
        assert_eq!(all[1], "vec0x0e");
        assert_eq!(all[3], "vec0xf0");
        assert_eq!(all[4], "syscall:entry");
        assert_eq!(all[5], "syscall:exit");
        for s in &all {
            assert!(!s.contains(' ') && !s.contains('#'), "{s}");
            for m in ["PF", "GP", "DF", "UD", "MC", "NMI"] {
                assert!(!s.contains(m), "{s} names {m}");
            }
        }
        assert!(Site::from_stub(u64::MAX).location().is_none());
    }
}
