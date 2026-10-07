//! IPI protocol helpers. DESIGN §7.6–§7.9, ROADMAP §4.8–§4.10.
//!
//! Portable: the wake inbox, RR placement, shootdown waiter/ack math.
//! MMIO and IDT live in the binary crate.

use crate::arch::InterruptMask;
use crate::atomic::{AtomicPtr, AtomicU64, Ordering, fence};
use crate::sync::variant::{self, Site};
use crate::thread::{CpuAffinity, MAX_THREADS};

/// Matches the 64-bit online mask. PerCpu is heap-sized from MADT.
pub const MAX_IPI_CPUS: usize = 64;

/// One CPU's cross-CPU wake inbox (DESIGN §7.6): a bitmap of `WORDS`
/// `AtomicU64` words, one bit per thread-table slot, and a summary word
/// with one bit per word. A remote CPU pushes a slot and sends `0xFD`; the
/// owner drains it into its run queue. Nothing allocates, and a slot pushed
/// twice before a drain is queued once. The words are private: `push` and
/// `drain` carry the only orders (ROADMAP §10.8's loom model runs them).
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
        // The loom model's variant sets the summary bit first (ROADMAP
        // §10.8): a drain between the two finds the word empty.
        let early = variant::pick(Site::InboxSummaryFirst, false, true);
        if early {
            // Release: pairs with the drain's Acquire swap of the summary.
            self.summary.fetch_or(1u64 << w, Ordering::Release);
        }
        // Release: pairs with the drain's Acquire swap of this word.
        word.fetch_or(1u64 << (slot % 64), Ordering::Release);
        if !early {
            // Release, after the word bit: pairs with the drain's Acquire
            // swap of the summary, so a drain that sees this bit sees the
            // slot's.
            self.summary.fetch_or(1u64 << w, Ordering::Release);
        }
        true
    }

    /// Take every queued slot: an Acquire `swap(0)` of the summary, then an
    /// Acquire `swap(0)` of each word it flags, calling `f(slot)` for each
    /// set bit in ascending order. True if `f` ran. A push that races the
    /// drain sets its summary bit after its slot bit, so a slot this drain
    /// misses is flagged for the next one.
    ///
    /// Runs on the owner CPU with interrupts masked through the port `A`
    /// (DESIGN §7.6), so a reschedule IPI's drain cannot interleave with
    /// this one; checked in debug builds.
    pub fn drain<A: InterruptMask>(&self, mut f: impl FnMut(usize)) -> bool {
        debug_assert!(!A::enabled(), "WakeInbox::drain with interrupts on");
        // Acquire: pairs with the Release summary `fetch_or` in `push`, so
        // every word bit set before a summary bit this swap sees is visible.
        let mut summary = self.summary.swap(0, Ordering::Acquire);
        let mut any = false;
        while summary != 0 {
            let w = summary.trailing_zeros() as usize;
            summary &= summary - 1;
            let Some(word) = self.words.get(w) else {
                continue;
            };
            // Acquire: pairs with the Release word `fetch_or` in `push`.
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
        // Acquire: pairs with the Release `fetch_or`s in `push`.
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

/// One call-function round (ROADMAP §11.4, F109).
///
/// The release word is [`round`](Self::round), an even counter. A repeated
/// waiter mask is not a new observation, and `acked.store(0)` does not
/// publish the payload to a CPU whose bit is already clear. The sender
/// stores the odd count and a release fence, then `acked`, `func`, `arg`,
/// and the mask, then the even count with Release. A responder
/// Acquire-loads the count, reads the payload, and checks the count again
/// after an acquire fence.
///
/// Litmus: `tests/litmus/call_function.litmus` and
/// `tests/litmus/call_function_relaxed.litmus`.
pub struct CallSlot {
    func: AtomicPtr<()>,
    arg: AtomicPtr<()>,
    waiters: AtomicU64,
    acked: AtomicU64,
    /// Even: published round. Odd: the payload is being overwritten. 0 is
    /// no round yet.
    round: AtomicU64,
}

impl CallSlot {
    /// An empty slot. `const` outside `cfg(loom)`.
    #[cfg(not(loom))]
    pub const fn empty() -> Self {
        Self {
            func: AtomicPtr::new(core::ptr::null_mut()),
            arg: AtomicPtr::new(core::ptr::null_mut()),
            waiters: AtomicU64::new(0),
            acked: AtomicU64::new(u64::MAX),
            round: AtomicU64::new(0),
        }
    }

    /// An empty slot (loom's atomics have no `const fn new`).
    #[cfg(loom)]
    pub fn empty() -> Self {
        Self {
            func: AtomicPtr::new(core::ptr::null_mut()),
            arg: AtomicPtr::new(core::ptr::null_mut()),
            waiters: AtomicU64::new(0),
            acked: AtomicU64::new(u64::MAX),
            round: AtomicU64::new(0),
        }
    }

    /// Publish one round. `Site::CallPublishRelaxed` stores the even count
    /// Relaxed. `Site::CallPublishOnAcked` is the old order: `acked` is the
    /// release word and `round` does not move.
    pub fn publish(&self, func: *mut (), arg: *mut (), waiters: u64) {
        if variant::pick(Site::CallPublishOnAcked, false, true) {
            // Relaxed: the Release store of `acked` below publishes it; pairs with nothing.
            self.func.store(func, Ordering::Relaxed);
            // Relaxed: as `func`; pairs with nothing.
            self.arg.store(arg, Ordering::Relaxed);
            // Relaxed: as `func`; pairs with nothing.
            self.waiters.store(waiters, Ordering::Relaxed);
            // Release: pairs with the Acquire load of `acked` in `poll`.
            self.acked.store(0, Ordering::Release);
            return;
        }
        let even = self.begin_round();
        // Relaxed: the Release store of `round` below publishes it; pairs with nothing.
        self.acked.store(0, Ordering::Relaxed);
        // Relaxed: as `acked`; pairs with nothing.
        self.func.store(func, Ordering::Relaxed);
        // Relaxed: as `acked`; pairs with nothing.
        self.arg.store(arg, Ordering::Relaxed);
        // Relaxed: as `acked`; pairs with nothing.
        self.waiters.store(waiters, Ordering::Relaxed);
        // Release: pairs with the Acquire load of `round` in `poll`.
        // Relaxed: the loom variant; pairs with nothing.
        let ord = variant::pick(
            Site::CallPublishRelaxed,
            Ordering::Release,
            Ordering::Relaxed,
        );
        self.round.store(even, ord);
    }

    /// `(round, func, arg)` when `me` should run this round. `seen` is the
    /// even round this CPU already took, or 0.
    pub fn poll(&self, me: u64, seen: u64) -> Option<(u64, *mut (), *mut ())> {
        if me == 0 {
            return None;
        }
        if variant::pick(Site::CallPublishOnAcked, false, true) {
            // Acquire: pairs with the Release store of `acked` in `publish`.
            let a = self.acked.load(Ordering::Acquire);
            if a & me != 0 {
                return None;
            }
            // Relaxed: the Acquire load of `acked` orders it; pairs with nothing.
            if self.waiters.load(Ordering::Relaxed) & me == 0 {
                return None;
            }
            // Relaxed: as `waiters`; pairs with nothing.
            let func = self.func.load(Ordering::Relaxed);
            // Relaxed: as `waiters`; pairs with nothing.
            let arg = self.arg.load(Ordering::Relaxed);
            return Some((seen.wrapping_add(1).max(1), func, arg));
        }
        // Acquire: pairs with the Release store of `round` in `publish`.
        let r1 = self.round.load(Ordering::Acquire);
        if r1 == 0 || r1 & 1 != 0 || r1 == seen {
            return None;
        }
        // Relaxed: the re-check below pairs these with the round `r1` published; pairs with nothing.
        let w = self.waiters.load(Ordering::Relaxed);
        // Relaxed: as `waiters`; pairs with nothing.
        let a = self.acked.load(Ordering::Relaxed);
        // Relaxed: as `waiters`; pairs with nothing.
        let func = self.func.load(Ordering::Relaxed);
        // Relaxed: as `waiters`; pairs with nothing.
        let arg = self.arg.load(Ordering::Relaxed);
        // Acquire: pairs with the Release fence in `begin_round`.
        fence(Ordering::Acquire);
        // Relaxed: the Acquire fence orders it; pairs with nothing.
        let r2 = self.round.load(Ordering::Relaxed);
        if r1 != r2 || w & me == 0 || a & me != 0 {
            return None;
        }
        Some((r1, func, arg))
    }

    /// Ack after the closure. Last access to `arg` (DESIGN §2.8).
    /// `Site::CallAckRelaxed` uses Relaxed.
    pub fn ack(&self, me: u64) {
        // Release: pairs with the Acquire load in the initiator's wait.
        // Relaxed: the loom variant; pairs with nothing.
        let ord = variant::pick(Site::CallAckRelaxed, Ordering::Release, Ordering::Relaxed);
        self.acked.fetch_or(me, ord);
    }

    /// The initiator's wait: Acquire-load `acked` until every waiter bit.
    pub fn acked(&self) -> u64 {
        // Acquire: pairs with each responder's Release `fetch_or`.
        self.acked.load(Ordering::Acquire)
    }

    /// The ack word, for the kernel's `wait_acks` poll.
    pub fn acked_ref(&self) -> &AtomicU64 {
        &self.acked
    }

    /// End the round so a late IPI does not re-run the closure. The odd
    /// count lands before the mask drops, and the even count waits for
    /// the next [`publish`](Self::publish). Stays before the initiator
    /// releases `CALL_BUSY`.
    pub fn clear_waiters(&self) {
        let _even = self.begin_round();
        // Relaxed: the fence in `begin_round` orders it; pairs with nothing.
        self.waiters.store(0, Ordering::Relaxed);
        // Relaxed: as `waiters`; pairs with nothing.
        self.func.store(core::ptr::null_mut(), Ordering::Relaxed);
    }

    /// Store the odd count, if the current one is even, then a release
    /// fence. Returns the even count the caller publishes, or ignores
    /// when it is only invalidating.
    fn begin_round(&self) -> u64 {
        // Relaxed: this initiator is the only writer of `round`; pairs with nothing.
        let cur = self.round.load(Ordering::Relaxed);
        let odd = if cur & 1 == 0 {
            cur.wrapping_add(1)
        } else {
            cur
        };
        if odd != cur {
            // Relaxed: the Release fence below orders it ahead of the payload; pairs with nothing.
            self.round.store(odd, Ordering::Relaxed);
        }
        // Release: pairs with the Acquire fence in `poll`. A reader that
        // sees a later payload store also sees this odd count and retries.
        fence(Ordering::Release);
        odd.wrapping_add(1)
    }
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

    use crate::arch::stub::Arch;

    /// Drain as the owner CPU does, masked.
    fn drain<const W: usize>(inbox: &WakeInbox<W>, f: impl FnMut(usize)) -> bool {
        let _masked = Arch::save_disable();
        inbox.drain::<Arch>(f)
    }

    fn drained<const W: usize>(inbox: &WakeInbox<W>) -> std::vec::Vec<usize> {
        let mut out = std::vec::Vec::new();
        drain(inbox, |s| out.push(s));
        out
    }

    #[test]
    fn wake_inbox_push_drain() {
        // Across a word boundary, and the largest id of a two-word inbox.
        let two = WakeInbox::<2>::new();
        for slot in [127, 64, 63, 0] {
            assert!(two.push(slot));
        }
        // A double push drains once.
        assert!(two.push(64));
        assert_eq!(drained(&two), std::vec![0, 63, 64, 127]);
        assert!(two.is_empty());
        assert!(!drain(&two, |_| {}), "each id drains once");
        // The kernel's inbox, at its largest id.
        let inbox = ThreadInbox::new();
        let last = MAX_THREADS - 1;
        assert!(inbox.push(last));
        assert!(inbox.push(last));
        assert_eq!(drained(&inbox), std::vec![last]);
        // Out of range, as P10-S56 made it: refused, nothing queued.
        assert!(!two.push(128));
        assert!(!inbox.push(INBOX_WORDS * 64));
        assert!(!inbox.push(usize::MAX));
        assert!(two.is_empty() && inbox.is_empty());
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "WakeInbox::drain with interrupts on")]
    fn wake_inbox_drain_needs_mask() {
        crate::arch::stub::reset();
        let inbox = WakeInbox::<1>::new();
        assert!(inbox.push(3));
        inbox.drain::<Arch>(|_| {});
    }

    #[test]
    fn wake_inbox_three_pushers() {
        use std::sync::Arc;
        const SLOTS: usize = 16 * 64;
        let inbox = Arc::new(WakeInbox::<16>::new());
        let pushers: std::vec::Vec<_> = (0..3)
            .map(|k| {
                let inbox = Arc::clone(&inbox);
                std::thread::spawn(move || {
                    for slot in (k..SLOTS).step_by(3) {
                        assert!(inbox.push(slot));
                    }
                })
            })
            .collect();
        // The owner drains, masked, until every slot has come out.
        let mut seen = std::vec![0u32; SLOTS];
        let mut total = 0;
        while total < SLOTS {
            drain(&inbox, |s| {
                seen[s] += 1;
                total += 1;
            });
            crate::atomic::spin_loop();
        }
        for p in pushers {
            p.join().unwrap();
        }
        assert!(!drain(&inbox, |_| {}));
        assert!(seen.iter().all(|&n| n == 1), "every slot drained once");
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
        assert!(!drain(&inbox, |_| {}), "nothing left");
    }

    #[test]
    fn wake_inbox_drain_clears_summary() {
        let inbox = WakeInbox::<16>::new();
        assert!(inbox.push(5));
        assert!(inbox.push(900));
        assert!(drain(&inbox, |_| {}));
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

#[cfg(all(test, loom))]
mod loom_models {
    extern crate std;

    use super::*;
    use crate::arch::stub::Arch;
    use crate::sync::variant::{Bound, check};
    use loom::sync::Arc;
    use loom::thread;
    use std::vec::Vec;

    /// The pushed slots: two in word 0 and one in word 1.
    const IDS: [usize; 3] = [1, 2, 65];

    /// Take every queued slot as the owner CPU does, masked.
    fn drain(inbox: &WakeInbox<2>, got: &mut Vec<usize>) {
        let _masked = <Arch as InterruptMask>::save_disable();
        inbox.drain::<Arch>(|slot| got.push(slot));
    }

    /// Three CPUs each push one slot while main, the owner CPU, drains;
    /// main drains again after the joins. Over both drains each slot is
    /// delivered exactly once. Bound: 4 threads (3 pushers of one push
    /// each, main 2 drains), 2 preemptions.
    fn inbox_model(v: Option<Site>) {
        let bound = Bound {
            threads: 4,
            preemptions: 2,
        };
        check(v, bound, || {
            let inbox = Arc::new(WakeInbox::<2>::new());
            let pushers: Vec<_> = IDS
                .iter()
                .map(|&id| {
                    let inbox = inbox.clone();
                    thread::spawn(move || assert!(inbox.push(id)))
                })
                .collect();
            let mut got = Vec::new();
            drain(&inbox, &mut got);
            for p in pushers {
                p.join().unwrap();
            }
            drain(&inbox, &mut got);
            for id in IDS {
                match got.iter().filter(|&&g| g == id).count() {
                    0 => panic!("inbox: ThreadId lost"),
                    1 => {}
                    _ => panic!("inbox: ThreadId delivered twice"),
                }
            }
            assert_eq!(got.len(), IDS.len());
        });
    }

    #[test]
    fn loom_wake_inbox_three_pushers() {
        inbox_model(None);
    }

    #[test]
    #[should_panic(expected = "inbox: ThreadId lost")]
    fn loom_wake_inbox_summary_first_loses_id_fails() {
        inbox_model(Some(Site::InboxSummaryFirst));
    }

    /// Initiator publishes a payload word; one responder yields until
    /// `poll`, reads it, writes the witness, then acks. Weakened
    /// publish lets the responder see the even round without the
    /// payload; weakened ack lets the wait reuse the witness before
    /// the responder's last write. Bound: 2 threads (responder 1 take
    /// and 1 ack, main 1 publish and the wait), 3 preemptions.
    fn call_model(v: Option<Site>) {
        let bound = Bound {
            threads: 2,
            preemptions: 3,
        };
        check(v, bound, || {
            struct Frame {
                slot: CallSlot,
                witness: loom::cell::UnsafeCell<u32>,
            }
            // SAFETY: the responder writes `witness` before it acks, and
            // main writes it only after the wait sees the ack; the models
            // check, through loom, that the ack orders the two. Established
            // here.
            unsafe impl Sync for Frame {}
            let f = Arc::new(Frame {
                slot: CallSlot::empty(),
                witness: loom::cell::UnsafeCell::new(0),
            });
            let payload = Arc::new(loom::sync::atomic::AtomicU64::new(0));
            let bit = 1u64;
            let responder = {
                let f = f.clone();
                let payload = payload.clone();
                thread::spawn(move || {
                    let (fnp, arg) = loop {
                        if let Some((_round, fnp, arg)) = f.slot.poll(bit, 0) {
                            break (fnp, arg);
                        }
                        thread::yield_now();
                    };
                    if fnp as usize != 0xC0FFEE {
                        panic!("call: stale func");
                    }
                    // SAFETY: `arg` is the `payload` atomic this model owns.
                    // established here.
                    let seen = unsafe {
                        (*(arg as *const loom::sync::atomic::AtomicU64)).load(Ordering::Relaxed)
                    };
                    if seen != 0xC0FFEE {
                        panic!("call: stale arg");
                    }
                    let _ = payload;
                    // SAFETY: the wait has not seen the ack. established here.
                    f.witness.with_mut(|p| unsafe { *p = 1 });
                    f.slot.ack(bit);
                })
            };
            payload.store(0xC0FFEE, Ordering::Relaxed);
            f.slot.publish(
                core::ptr::without_provenance_mut(0xC0FFEE),
                core::ptr::from_ref(payload.as_ref()) as *mut (),
                bit,
            );
            while !all_acked(bit, f.slot.acked()) {
                thread::yield_now();
            }
            // SAFETY: the ack is visible, so the responder has made its last
            // access. established here.
            f.witness.with_mut(|p| unsafe { *p = 2 });
            responder.join().unwrap();
        });
    }

    #[test]
    fn loom_call_function() {
        call_model(None);
    }

    #[test]
    #[should_panic(expected = "call:")]
    fn loom_call_publish_relaxed_fails() {
        call_model(Some(Site::CallPublishRelaxed));
    }

    #[test]
    #[should_panic(expected = "Causality violation")]
    fn loom_call_ack_relaxed_fails() {
        call_model(Some(Site::CallAckRelaxed));
    }

    /// Executions that observed round 2's pair. `core`'s atomic, so loom
    /// does not schedule it.
    static OBSERVED: crate::atomic::statics::AtomicUsize =
        crate::atomic::statics::AtomicUsize::new(0);

    /// Round 1 runs to completion on this thread: mask `1`, acked, then
    /// cleared, so bit 2 is already clear in the leftover ack word. A and
    /// B then race with round 2 (mask `3`). A was in round 1; B was not.
    /// Each polls twice, the second after its ack, so a wiped ack that
    /// invites the CPU again fails the model. Missing a round is allowed:
    /// loom still interleaves the polls with the publish. A bad pair is
    /// not. Bound: 3 threads (A, B, main), 3 preemptions.
    fn call_reuse_model(v: Option<Site>) {
        let bound = Bound {
            threads: 3,
            preemptions: 3,
        };
        OBSERVED.store(0, crate::atomic::statics::Ordering::Relaxed);
        check(v, bound, || {
            let slot = Arc::new(CallSlot::empty());
            slot.publish(
                core::ptr::without_provenance_mut(0x11),
                core::ptr::without_provenance_mut(0x12),
                1,
            );
            let Some((seen_a, func, arg)) = slot.poll(1, 0) else {
                panic!("call: setup missed");
            };
            if (func as usize, arg as usize) != (0x11, 0x12) {
                panic!("call: setup torn");
            }
            slot.ack(1);
            slot.clear_waiters();
            let take = |bit: u64, seen: u64| {
                let slot = slot.clone();
                thread::spawn(move || {
                    let mut seen = seen;
                    let mut n = 0;
                    for _ in 0..2 {
                        let Some((round, func, arg)) = slot.poll(bit, seen) else {
                            continue;
                        };
                        let got = (func as usize, arg as usize);
                        if n != 0 || got != (0x21, 0x22) {
                            panic!("call: bad payload {got:?}");
                        }
                        n = 1;
                        seen = round;
                        slot.ack(bit);
                        // `core`'s atomic: a count across executions, not part of the model.
                        OBSERVED.fetch_add(1, crate::atomic::statics::Ordering::Relaxed);
                    }
                })
            };
            let a = take(1, seen_a);
            let b = take(2, 0);
            slot.publish(
                core::ptr::without_provenance_mut(0x21),
                core::ptr::without_provenance_mut(0x22),
                0b11,
            );
            a.join().unwrap();
            b.join().unwrap();
        });
    }

    #[test]
    fn loom_call_function_reuse() {
        call_reuse_model(None);
        assert!(
            OBSERVED.load(crate::atomic::statics::Ordering::Relaxed) > 0,
            "round 2 was never observed"
        );
    }

    #[test]
    #[should_panic(expected = "call: bad payload")]
    fn loom_call_function_reuse_acked_fails() {
        call_reuse_model(Some(Site::CallPublishOnAcked));
    }
}
