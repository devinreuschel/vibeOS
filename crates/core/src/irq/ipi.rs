//! IPI protocol helpers. DESIGN §7.6–§7.9, ROADMAP §4.8–§4.10.
//!
//! Portable: the wake inbox, RR placement, shootdown waiter/ack math.
//! MMIO and IDT live in the binary crate.

use crate::atomic::{AtomicU64, Ordering};
use crate::thread::{CpuAffinity, MAX_THREADS};

/// Matches the 64-bit online mask. PerCpu is heap-sized from MADT.
pub const MAX_IPI_CPUS: usize = 64;

/// One CPU's cross-CPU wake inbox (DESIGN §7.6): a bitmap of `WORDS`
/// `AtomicU64` words, one bit per thread-table slot, and a summary word
/// with one bit per word. A remote CPU pushes a slot and sends `0xFD`; the
/// owner drains it into its run queue. Nothing allocates, and a slot pushed
/// twice before a drain is queued once.
pub struct WakeInbox<const WORDS: usize> {
    summary: AtomicU64,
    words: [AtomicU64; WORDS],
}

impl<const WORDS: usize> WakeInbox<WORDS> {
    /// The summary word has a bit per word.
    const FITS: () = assert!(WORDS >= 1 && WORDS <= 64, "WakeInbox: 1..=64 words");

    /// An empty inbox. `const` outside `cfg(loom)`, whose atomics have no
    /// `const fn new` (C-ATOMICS).
    #[cfg(not(loom))]
    pub const fn new() -> Self {
        #[allow(clippy::let_unit_value, reason = "forces the FITS assertion")]
        let () = Self::FITS;
        Self {
            summary: AtomicU64::new(0),
            words: [const { AtomicU64::new(0) }; WORDS],
        }
    }

    /// An empty inbox (loom's atomics have no `const fn new`).
    #[cfg(loom)]
    pub fn new() -> Self {
        #[allow(clippy::let_unit_value, reason = "forces the FITS assertion")]
        let () = Self::FITS;
        Self {
            summary: AtomicU64::new(0),
            words: core::array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// Queue `slot`: a Release `fetch_or` of its bit, then a Release
    /// `fetch_or` of its word's summary bit, so a drain that sees the
    /// summary bit sees the slot's bit. False only for a slot at or past
    /// `WORDS * 64`, which queues nothing.
    pub fn push(&self, slot: usize) -> bool {
        let w = slot / 64;
        let Some(word) = self.words.get(w) else {
            return false;
        };
        word.fetch_or(1u64 << (slot % 64), Ordering::Release);
        self.summary.fetch_or(1u64 << w, Ordering::Release);
        true
    }

    /// Take every queued slot: an Acquire `swap(0)` of the summary, then an
    /// Acquire `swap(0)` of each word it flags, calling `f(slot)` for each
    /// set bit in ascending order. True if `f` ran. A push that races the
    /// drain sets its summary bit after its slot bit, so a slot this drain
    /// misses is flagged for the next one.
    pub fn drain(&self, mut f: impl FnMut(usize)) -> bool {
        let mut summary = self.summary.swap(0, Ordering::Acquire);
        let mut any = false;
        while summary != 0 {
            let w = summary.trailing_zeros() as usize;
            summary &= summary - 1;
            let Some(word) = self.words.get(w) else {
                continue;
            };
            let mut bits = word.swap(0, Ordering::Acquire);
            while bits != 0 {
                let b = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                f(w * 64 + b);
                any = true;
            }
        }
        any
    }

    /// True when no slot is queued: a snapshot. Acquire, pairing with the
    /// drain's swaps as with a push's `fetch_or`s, so a caller that sees it
    /// empty sees what the drain did before it.
    pub fn is_empty(&self) -> bool {
        self.summary.load(Ordering::Acquire) == 0
            && self.words.iter().all(|w| w.load(Ordering::Acquire) == 0)
    }
}

#[cfg(not(loom))]
impl<const WORDS: usize> Default for WakeInbox<WORDS> {
    fn default() -> Self {
        Self::new()
    }
}

/// Words in the kernel's inbox: one bit per thread-table slot.
pub const INBOX_WORDS: usize = MAX_THREADS.div_ceil(64);

// The summary is one u64, so the thread table has at most 64 * 64 slots.
const _: () = assert!(INBOX_WORDS <= 64, "the inbox summary is one u64");

/// The kernel's per-CPU wake inbox, sized from `limits::MAX_THREADS`.
pub type ThreadInbox = WakeInbox<INBOX_WORDS>;

/// CPUs that must ack, excluding the initiator.
pub const fn waiter_mask(online: u64, self_cpu: u32) -> u64 {
    if self_cpu >= 64 {
        online
    } else {
        online & !(1u64 << self_cpu)
    }
}

pub const fn all_acked(waiters: u64, acked: u64) -> bool {
    waiters & acked == waiters
}

/// Most ranges one TLB shootdown round carries (DESIGN §7.9). A caller
/// with more sends one round per this many.
pub const SHOOT_RANGES: usize = 16;

/// Most pages one [`ShootRange`] names: a receiver runs one `invlpg` per
/// page with IF=0, so a round stays at most `SHOOT_RANGES * 32` of them.
pub const SHOOT_RANGE_PAGES: u64 = 32;

const PAGE_MASK: u64 = 0xFFF;

const _: () = assert!(
    SHOOT_RANGE_PAGES <= PAGE_MASK,
    "a range's page count packs into its start's low 12 bits"
);

/// One kernel VA range a shootdown round invalidates: a page-aligned start
/// and 1 to [`SHOOT_RANGE_PAGES`] pages, packed in one word (the count in
/// the start's low 12 bits) so a round's slot publishes it in one atomic
/// store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShootRange(u64);

impl ShootRange {
    /// The range of `pages` pages from `start`, or `None` when `start` is
    /// not page-aligned or `pages` is 0 or above [`SHOOT_RANGE_PAGES`].
    pub const fn new(start: u64, pages: u64) -> Option<Self> {
        if start & PAGE_MASK != 0 || pages == 0 || pages > SHOOT_RANGE_PAGES {
            return None;
        }
        Some(Self(start | pages))
    }

    /// The page that holds `va`.
    pub const fn page(va: u64) -> Self {
        Self((va & !PAGE_MASK) | 1)
    }

    /// The packed word, for a round's slot.
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// A word [`raw`](Self::raw) packed. Any word decodes to a range of at
    /// most [`PAGE_MASK`] pages; a slot holds only packed ones.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn start(self) -> u64 {
        self.0 & !PAGE_MASK
    }

    pub const fn pages(self) -> u64 {
        self.0 & PAGE_MASK
    }
}

/// Round-robin among set bits of `online`. `rr` is the cursor; updated.
pub fn pick_cpu(affinity: CpuAffinity, online: u64, rr: &mut u32) -> u32 {
    let n = online.count_ones();
    let fallback = if online & 1 != 0 {
        0
    } else {
        online.trailing_zeros()
    };
    match affinity {
        CpuAffinity::Pinned(c) => {
            if c < 64 && online & (1u64 << c) != 0 {
                c
            } else {
                fallback
            }
        }
        CpuAffinity::Any => {
            if n <= 1 {
                return fallback;
            }
            let start = *rr;
            *rr = rr.wrapping_add(1);
            let mut i = 0u32;
            while i < 64 {
                let c = start.wrapping_add(i) % 64;
                if online & (1u64 << c) != 0 {
                    return c;
                }
                i += 1;
            }
            fallback
        }
    }
}

/// Stay on last CPU for `Any` (no migration). Pinned if that CPU is online.
pub fn home_cpu(affinity: CpuAffinity, last: u32, online: u64) -> u32 {
    match affinity {
        CpuAffinity::Pinned(c) => {
            if c < 64 && online & (1u64 << c) != 0 {
                c
            } else if last < 64 && online & (1u64 << last) != 0 {
                last
            } else {
                0
            }
        }
        CpuAffinity::Any => {
            if last < 64 && online & (1u64 << last) != 0 {
                last
            } else {
                0
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drained<const W: usize>(inbox: &WakeInbox<W>) -> std::vec::Vec<usize> {
        let mut out = std::vec::Vec::new();
        inbox.drain(|s| out.push(s));
        out
    }

    #[test]
    fn wake_inbox_push_drain_past_64() {
        let inbox = WakeInbox::<16>::new();
        assert!(inbox.is_empty());
        for slot in [1023, 64, 0, 130, 63] {
            assert!(inbox.push(slot));
        }
        assert!(!inbox.is_empty());
        assert_eq!(drained(&inbox), std::vec![0, 63, 64, 130, 1023]);
        assert!(inbox.is_empty());
    }

    #[test]
    fn wake_inbox_double_push_queues_once() {
        let inbox = WakeInbox::<16>::new();
        assert!(inbox.push(70));
        assert!(inbox.push(70));
        assert_eq!(drained(&inbox), std::vec![70]);
        assert!(!inbox.drain(|_| {}), "nothing left");
    }

    #[test]
    fn wake_inbox_drain_clears_summary() {
        let inbox = WakeInbox::<16>::new();
        assert!(inbox.push(5));
        assert!(inbox.push(900));
        assert!(inbox.drain(|_| {}));
        assert_eq!(inbox.summary.load(Ordering::Relaxed), 0);
        assert!(inbox.words.iter().all(|w| w.load(Ordering::Relaxed) == 0));
    }

    #[test]
    fn wake_inbox_push_out_of_range_refused() {
        let inbox = WakeInbox::<16>::new();
        assert!(!inbox.push(1024));
        assert!(!inbox.push(usize::MAX));
        assert!(inbox.is_empty());
        assert!(ThreadInbox::new().push(MAX_THREADS - 1));
    }

    #[test]
    fn shootdown_waiters_exclude_self() {
        assert_eq!(waiter_mask(0b1111, 0), 0b1110);
        assert_eq!(waiter_mask(0b1111, 3), 0b0111);
        assert_eq!(waiter_mask(0b1, 0), 0);
        assert!(all_acked(0b1110, 0b1110));
        assert!(!all_acked(0b1110, 0b0100));
        assert!(all_acked(0, 0));
    }

    #[test]
    fn shoot_range_packs_start_and_pages() {
        let r = ShootRange::new(0xFFFF_9000_0000_3000, 4).unwrap();
        assert_eq!(r.start(), 0xFFFF_9000_0000_3000);
        assert_eq!(r.pages(), 4);
        assert_eq!(ShootRange::from_raw(r.raw()), r);
        assert_eq!(
            ShootRange::new(0x5000, SHOOT_RANGE_PAGES).map(ShootRange::pages),
            Some(SHOOT_RANGE_PAGES)
        );
        assert_eq!(ShootRange::new(0x5008, 1), None);
        assert_eq!(ShootRange::new(0x5000, 0), None);
        assert_eq!(ShootRange::new(0x5000, SHOOT_RANGE_PAGES + 1), None);
        let p = ShootRange::page(0xFFFF_9000_0000_3ABC);
        assert_eq!((p.start(), p.pages()), (0xFFFF_9000_0000_3000, 1));
    }

    #[test]
    fn rr_any_walks_online_cpus() {
        let mut rr = 0u32;
        let a = pick_cpu(CpuAffinity::Any, 0b1011, &mut rr);
        let b = pick_cpu(CpuAffinity::Any, 0b1011, &mut rr);
        let c = pick_cpu(CpuAffinity::Any, 0b1011, &mut rr);
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        assert_eq!(c, 3);
        assert_eq!(pick_cpu(CpuAffinity::Pinned(1), 0b1011, &mut rr), 1);
        assert_eq!(pick_cpu(CpuAffinity::Pinned(2), 0b1011, &mut rr), 0);
        assert_eq!(pick_cpu(CpuAffinity::Any, 0b1, &mut rr), 0);
    }

    #[test]
    fn home_does_not_migrate_any() {
        assert_eq!(home_cpu(CpuAffinity::Any, 3, 0b1111), 3);
        assert_eq!(home_cpu(CpuAffinity::Pinned(1), 3, 0b1111), 1);
        assert_eq!(home_cpu(CpuAffinity::Pinned(5), 3, 0b1111), 3);
        assert_eq!(home_cpu(CpuAffinity::Any, 7, 0b1), 0);
    }
}
