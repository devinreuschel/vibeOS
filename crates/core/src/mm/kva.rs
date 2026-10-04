//! Kernel virtual address range allocator, portable half. DESIGN §4.5.
//!
//! First-fit over the 64 GiB window at `KVA_START`. `kva_init` returns
//! a range only after its shootdown (DESIGN §2.4, §4.5); freed ranges
//! go to the tail of the free list so a stale pointer keeps faulting as
//! long as possible.
//!
//! This type only hands out VA. Mapping, guard pages, vmap, and the
//! deferred-free list live in the binary crate — they need the buddy
//! allocator and page tables.

/// DESIGN §4.1.
pub const KVA_START: u64 = 0xFFFF_D000_0000_0000;
pub const KVA_SIZE: u64 = 64 * 1024 * 1024 * 1024;
pub const KVA_END: u64 = KVA_START + KVA_SIZE;

pub const PAGE_SIZE: u64 = 4096;
/// Default kernel stack: 4 pages (16 KiB) plus the unmapped guard.
pub const DEFAULT_STACK_PAGES: usize = 4;

use crate::limits::MAX_KVA_RANGES as MAX_RANGES;

/// Why a KVA request failed.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KvaError {
    /// The node pool refused: `Kva::init` found no node. `Kva::alloc` at
    /// its live-range cap returns `None`, which `kva_init` reports as `NoVa`.
    Exhausted,
    /// A page count of 0 or above the cap.
    Size,
    /// No free VA range of the size asked for.
    NoVa,
    /// The buddy had no frame to map.
    NoFrames,
    /// The page tables refused a mapping.
    Map,
}

/// Kernel virtual space or frames running out is no memory; a bad size, a bad argument.
impl From<KvaError> for crate::kerror::KError {
    fn from(e: KvaError) -> Self {
        match e {
            KvaError::Exhausted | KvaError::NoVa | KvaError::NoFrames | KvaError::Map => {
                Self::NoMem
            }
            KvaError::Size => Self::Inval,
        }
    }
}

impl KvaError {
    pub fn as_str(self) -> &'static str {
        match self {
            KvaError::Exhausted => "exhausted",
            KvaError::Size => "size",
            KvaError::NoVa => "no va",
            KvaError::NoFrames => "no frames",
            KvaError::Map => "map",
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Range {
    start: u64,
    len: u64,
}

/// Order `v` by `start`: an in-place heapsort, with no recursion and a
/// frame of a few words. `core`'s unstable sort recurses, with a small-sort
/// scratch buffer in its frames, and took about 5 KiB of a 16 KiB kernel
/// stack sorting a full node pool, past a worker's DESIGN §4.5 budget once
/// an interrupt landed on top.
fn sort_by_start(v: &mut [Range]) {
    let n = v.len();
    let mut i = n / 2;
    while i > 0 {
        i -= 1;
        sift_down(v, i, n);
    }
    let mut end = n;
    while end > 1 {
        end -= 1;
        v.swap(0, end);
        sift_down(v, 0, end);
    }
}

/// Restore the max-heap order of `v[..end]` below `root`.
fn sift_down(v: &mut [Range], mut root: usize, end: usize) {
    loop {
        // `end <= v.len() <= MAX_RANGES`, far from overflow.
        let mut child = 2 * root + 1;
        if child >= end {
            return;
        }
        if child + 1 < end && v[child + 1].start > v[child].start {
            child += 1;
        }
        if v[root].start >= v[child].start {
            return;
        }
        v.swap(root, child);
        root = child;
    }
}

/// Snapshot for `meminfo`. `used` is bytes currently reserved (including
/// guard pages), not necessarily mapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KvaStats {
    pub used: u64,
    pub capacity: u64,
    pub free_ranges: usize,
}

/// Smallest `s >= range_start` with `s ≡ offset (mod align)`.
fn first_aligned(range_start: u64, align: u64, offset: u64) -> Option<u64> {
    if align == 0 {
        return None;
    }
    let rem = range_start % align;
    let delta = if rem <= offset {
        offset - rem
    } else {
        align - rem + offset
    };
    range_start.checked_add(delta)
}

// Node links are `u16`, and `alloc`'s cap of `MAX_RANGES - 2` live ranges
// leaves room for at least one.
const _: () = assert!(MAX_RANGES <= u16::MAX as usize);
const _: () = assert!(MAX_RANGES >= 3);

/// Live ranges `alloc` hands out at most. After a coalesce the free list
/// holds at most `live + 1` ranges, since a live range separates any two,
/// so with `live` below `MAX_RANGES - 1` a free always finds a node.
const LIVE_CAP: usize = MAX_RANGES - 2;

pub struct Kva {
    nodes: [Range; MAX_RANGES],
    next: [Option<u16>; MAX_RANGES],
    head: Option<u16>,
    tail: Option<u16>,
    /// Unused node indices, stack-allocated.
    slots: [u16; MAX_RANGES],
    nslots: usize,
    /// Ranges `alloc` handed out and `free` has not taken back.
    live: usize,
    used: u64,
    capacity: u64,
}

impl Kva {
    pub const fn empty() -> Self {
        Self {
            nodes: [Range { start: 0, len: 0 }; MAX_RANGES],
            next: [None; MAX_RANGES],
            head: None,
            tail: None,
            slots: [0; MAX_RANGES],
            nslots: 0,
            live: 0,
            used: 0,
            capacity: 0,
        }
    }

    /// Reset to one free range `[start, start + size)`. In place: the pool
    /// is larger than a kernel stack, so no temporary `Kva` is built.
    pub fn init(&mut self, start: u64, size: u64) -> Result<(), KvaError> {
        assert!(size.is_multiple_of(PAGE_SIZE));
        assert!(start.is_multiple_of(PAGE_SIZE));
        self.nodes.fill(Range { start: 0, len: 0 });
        self.next.fill(None);
        self.head = None;
        self.tail = None;
        for (i, slot) in self.slots.iter_mut().enumerate() {
            *slot = i as u16;
        }
        self.nslots = MAX_RANGES;
        self.live = 0;
        self.used = 0;
        self.capacity = size;
        let i = self.alloc_slot().ok_or(KvaError::Exhausted)?;
        self.nodes[i as usize] = Range { start, len: size };
        self.next[i as usize] = None;
        self.head = Some(i);
        self.tail = Some(i);
        Ok(())
    }

    pub fn stats(&self) -> KvaStats {
        let mut n = 0usize;
        let mut cur = self.head;
        while let Some(i) = cur {
            n += 1;
            cur = self.next[i as usize];
        }
        KvaStats {
            used: self.used,
            capacity: self.capacity,
            free_ranges: n,
        }
    }

    /// First-fit. `len` page-aligned. Returns the start VA. `None` when no
    /// free range fits, or when `MAX_RANGES - 2` ranges are live already.
    pub fn alloc(&mut self, len: u64) -> Option<u64> {
        self.alloc_placed(len, PAGE_SIZE, 0)
    }

    /// First-fit of `len` bytes at `start ≡ offset (mod align)`.
    pub fn alloc_placed(&mut self, len: u64, align: u64, offset: u64) -> Option<u64> {
        if len == 0
            || !len.is_multiple_of(PAGE_SIZE)
            || align == 0
            || !align.is_multiple_of(PAGE_SIZE)
            || offset >= align
            || !offset.is_multiple_of(PAGE_SIZE)
            || self.live >= LIVE_CAP
        {
            return None;
        }
        let mut prev: Option<u16> = None;
        let mut cur = self.head;
        while let Some(i) = cur {
            let r = self.nodes[i as usize];
            if let Some(start) = first_aligned(r.start, align, offset)
                && start
                    .checked_add(len)
                    .is_some_and(|end| end <= r.start + r.len)
                && start >= r.start
            {
                let prefix = start - r.start;
                let suffix_start = start + len;
                let suffix = (r.start + r.len) - suffix_start;
                if prefix > 0 && suffix > 0 {
                    if self.nslots == 0 {
                        return None;
                    }
                    self.nodes[i as usize] = Range {
                        start: r.start,
                        len: prefix,
                    };
                    let j = self.alloc_slot()?;
                    self.nodes[j as usize] = Range {
                        start: suffix_start,
                        len: suffix,
                    };
                    self.next[j as usize] = self.next[i as usize];
                    self.next[i as usize] = Some(j);
                    if self.tail == Some(i) {
                        self.tail = Some(j);
                    }
                } else if prefix > 0 {
                    self.nodes[i as usize] = Range {
                        start: r.start,
                        len: prefix,
                    };
                } else if suffix > 0 {
                    self.nodes[i as usize] = Range {
                        start: suffix_start,
                        len: suffix,
                    };
                } else {
                    self.unlink(prev, i);
                    self.free_slot(i);
                }
                self.used += len;
                self.live += 1;
                return Some(start);
            }
            prev = cur;
            cur = self.next[i as usize];
        }
        None
    }

    /// Reserve `2S` VA for a power-of-two stack of `pages` pages (`S`).
    /// Returns the guard start: `S` unmapped bytes, then the stack at a
    /// `2S`-aligned address (DESIGN §4.5).
    pub fn alloc_guarded(&mut self, pages: usize) -> Option<u64> {
        if pages == 0 || !pages.is_power_of_two() {
            return None;
        }
        let s = (pages as u64).checked_mul(PAGE_SIZE)?;
        let total = s.checked_mul(2)?;
        self.alloc_placed(total, total, s)
    }

    /// VA bytes a guarded stack of `pages` mapped pages occupies.
    pub const fn guarded_va_len(pages: usize) -> u64 {
        (pages as u64).saturating_mul(PAGE_SIZE).saturating_mul(2)
    }

    /// Append `[start, start+len)`, a range `alloc` handed out, to the
    /// tail. No coalescing on this path: DESIGN wants recently-freed VA at
    /// the tail so it is not the next first-fit hit. With no node left it
    /// coalesces the list first, which always leaves one ([`LIVE_CAP`]).
    pub fn free(&mut self, start: u64, len: u64) {
        assert!(len.is_multiple_of(PAGE_SIZE) && start.is_multiple_of(PAGE_SIZE));
        assert!(len > 0);
        assert!(
            self.used >= len && self.live > 0,
            "kva: free {len} when used is {} over {} ranges",
            self.used,
            self.live
        );
        if self.nslots == 0 {
            self.coalesce_all();
        }
        // Invariant: a coalesced list holds at most `live + 1` ranges and
        // `live <= LIVE_CAP`, so a node is left (`Kva::alloc`'s cap).
        assert!(self.nslots > 0, "kva: no node after coalesce");
        self.nslots -= 1;
        let i = self.slots[self.nslots];
        self.nodes[i as usize] = Range { start, len };
        self.next[i as usize] = None;
        if let Some(t) = self.tail {
            self.next[t as usize] = Some(i);
        } else {
            self.head = Some(i);
        }
        self.tail = Some(i);
        self.used -= len;
        self.live -= 1;
    }

    fn unlink(&mut self, prev: Option<u16>, i: u16) {
        let nxt = self.next[i as usize];
        if let Some(p) = prev {
            self.next[p as usize] = nxt;
        } else {
            self.head = nxt;
        }
        if self.tail == Some(i) {
            self.tail = prev;
        }
        self.next[i as usize] = None;
    }

    fn alloc_slot(&mut self) -> Option<u16> {
        if self.nslots == 0 {
            return None;
        }
        self.nslots -= 1;
        Some(self.slots[self.nslots])
    }

    fn free_slot(&mut self, i: u16) {
        self.slots[self.nslots] = i;
        self.nslots += 1;
        self.nodes[i as usize] = Range { start: 0, len: 0 };
    }

    /// Overflow valve: sort + merge adjacent, rebuild as a single
    /// address-ordered list, in place. Normal frees never take this path.
    fn coalesce_all(&mut self) {
        // A node on the free list has `len > 0`; a spare node has 0
        // (`free_slot`, `init`). Move the listed ones to the front.
        let mut n = 0usize;
        for r in 0..MAX_RANGES {
            if self.nodes[r].len > 0 {
                self.nodes[n] = self.nodes[r];
                n += 1;
            }
        }
        sort_by_start(&mut self.nodes[..n]);
        let mut w = 0usize;
        for r in 0..n {
            if w > 0 {
                let prev = self.nodes[w - 1];
                if prev.start + prev.len == self.nodes[r].start {
                    self.nodes[w - 1].len += self.nodes[r].len;
                    continue;
                }
            }
            self.nodes[w] = self.nodes[r];
            w += 1;
        }
        self.nodes[w..].fill(Range { start: 0, len: 0 });
        self.next.fill(None);
        for i in 1..w {
            self.next[i - 1] = Some(i as u16);
        }
        self.head = if w > 0 { Some(0) } else { None };
        self.tail = w.checked_sub(1).map(|t| t as u16);
        self.nslots = 0;
        for i in w..MAX_RANGES {
            self.slots[self.nslots] = i as u16;
            self.nslots += 1;
        }
    }

    /// Test helper: walk the free list into a vec of (start, len).
    #[cfg(test)]
    fn free_list(&self) -> std::vec::Vec<(u64, u64)> {
        let mut out = std::vec::Vec::new();
        let mut cur = self.head;
        while let Some(i) = cur {
            let r = self.nodes[i as usize];
            out.push((r.start, r.len));
            cur = self.next[i as usize];
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> std::boxed::Box<Kva> {
        window(64)
    }

    #[test]
    fn first_fit_from_front() {
        let mut k = fresh();
        let a = k.alloc(2 * PAGE_SIZE).unwrap();
        assert_eq!(a, KVA_START);
        let b = k.alloc(PAGE_SIZE).unwrap();
        assert_eq!(b, KVA_START + 2 * PAGE_SIZE);
        assert_eq!(k.stats().used, 3 * PAGE_SIZE);
    }

    #[test]
    fn freed_ranges_go_to_tail() {
        let mut k = fresh();
        let a = k.alloc(2 * PAGE_SIZE).unwrap();
        let _b = k.alloc(2 * PAGE_SIZE).unwrap();
        k.free(a, 2 * PAGE_SIZE);
        // Free list head is the leftover after the two allocs (starting
        // at +4 pages). The just-freed `a` must sit at the tail, so the
        // next first-fit of 2 pages takes leftover, not `a`.
        let c = k.alloc(2 * PAGE_SIZE).unwrap();
        assert_ne!(c, a, "freshly freed range must not be the next first-fit");
        assert_eq!(c, KVA_START + 4 * PAGE_SIZE);
        let list = k.free_list();
        assert_eq!(list.last().unwrap().0, a);
    }

    #[test]
    fn alloc_free_roundtrip_restores_used() {
        let mut k = fresh();
        let s0 = k.stats();
        let a = k.alloc(8 * PAGE_SIZE).unwrap();
        assert_eq!(k.stats().used, 8 * PAGE_SIZE);
        k.free(a, 8 * PAGE_SIZE);
        assert_eq!(k.stats().used, s0.used);
        assert_eq!(k.stats().capacity, s0.capacity);
        // Freed ranges go to the tail without coalescing, so the range
        // count may grow. used is the invariant that matters.
    }

    #[test]
    fn guarded_reserves_2s_aligned() {
        let mut k = fresh();
        let guard = k.alloc_guarded(4).unwrap();
        let s = 4 * PAGE_SIZE;
        let stack = guard + s;
        assert_eq!(stack % (2 * s), 0);
        assert_eq!(guard + s, stack);
        assert_eq!(k.stats().used, 8 * PAGE_SIZE);
        let bit = s.trailing_zeros();
        assert_eq!(stack & (1 << bit), 0);
        assert_ne!(guard & (1 << bit), 0);
        k.free(guard, Kva::guarded_va_len(4));
        assert_eq!(k.stats().used, 0);
        assert!(k.alloc_guarded(3).is_none());
        assert!(k.alloc_guarded(0).is_none());
    }

    #[test]
    fn guarded_each_power_of_two_size() {
        for pages in [1usize, 2, 4, 8] {
            let mut k = window(64);
            let guard = k.alloc_guarded(pages).unwrap();
            let s = pages as u64 * PAGE_SIZE;
            let stack = guard + s;
            assert_eq!(stack % (2 * s), 0, "pages={pages}");
            let bit = s.trailing_zeros();
            assert_eq!(stack & (1 << bit), 0);
            assert_ne!(guard & (1 << bit), 0);
            k.free(guard, Kva::guarded_va_len(pages));
        }
    }

    #[test]
    fn exhaustion_returns_none() {
        let mut k = fresh();
        assert!(k.alloc(64 * PAGE_SIZE).is_some());
        assert!(k.alloc(PAGE_SIZE).is_none());
        assert!(k.alloc_guarded(1).is_none());
    }

    #[test]
    fn reject_unaligned_and_zero() {
        let mut k = fresh();
        assert!(k.alloc(0).is_none());
        assert!(k.alloc(PAGE_SIZE - 1).is_none());
        assert!(k.alloc_guarded(0).is_none());
    }

    #[test]
    fn split_leaves_remainder_in_place() {
        let mut k = fresh();
        let a = k.alloc(PAGE_SIZE).unwrap();
        assert_eq!(a, KVA_START);
        let list = k.free_list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0], (KVA_START + PAGE_SIZE, 63 * PAGE_SIZE));
    }

    #[test]
    fn init_ok_on_fresh_window() {
        let mut k = std::boxed::Box::new(Kva::empty());
        assert_eq!(k.init(KVA_START, PAGE_SIZE), Ok(()));
        assert_eq!(k.stats().capacity, PAGE_SIZE);
        assert_eq!(k.stats().used, 0);
        assert_eq!(k.stats().free_ranges, 1);
    }

    /// A window of `pages` pages at `KVA_START`.
    fn window(pages: u64) -> std::boxed::Box<Kva> {
        let mut k = std::boxed::Box::new(Kva::empty());
        k.init(KVA_START, pages * PAGE_SIZE).unwrap();
        k
    }

    #[test]
    fn kva_frees_past_128_ranges_out_of_order() {
        let mut k = window(1024);
        let guards: std::vec::Vec<u64> = (0..300).map(|_| k.alloc_guarded(1).unwrap()).collect();
        assert_eq!(k.stats().used, 600 * PAGE_SIZE);
        // Odd ones last first, then the even ones: none is adjacent to a
        // range freed before it until the evens fill the gaps.
        for g in guards.iter().skip(1).step_by(2).rev() {
            k.free(*g, 2 * PAGE_SIZE);
        }
        assert!(k.stats().free_ranges > 128);
        for g in guards.iter().step_by(2) {
            k.free(*g, 2 * PAGE_SIZE);
        }
        assert_eq!(k.stats().used, 0);
        assert_eq!(k.live, 0);
        // Every page is still on the list, once.
        let total: u64 = k.free_list().iter().map(|r| r.1).sum();
        assert_eq!(total, 1024 * PAGE_SIZE);
    }

    #[test]
    fn kva_alloc_refuses_at_live_cap() {
        let mut k = window(MAX_RANGES as u64 + 16);
        let mut got = std::vec::Vec::new();
        for _ in 0..LIVE_CAP {
            got.push(k.alloc(PAGE_SIZE).unwrap());
        }
        assert_eq!(k.alloc(PAGE_SIZE), None, "the live-range cap refuses");
        assert_eq!(k.alloc_guarded(1), None);
        k.free(got.pop().unwrap(), PAGE_SIZE);
        assert!(k.alloc(PAGE_SIZE).is_some(), "a free makes room again");
    }

    #[test]
    fn kva_free_never_fails_at_live_cap() {
        let mut k = window(4 * MAX_RANGES as u64);
        let mut live: std::collections::VecDeque<u64> =
            (0..LIVE_CAP).map(|_| k.alloc(PAGE_SIZE).unwrap()).collect();
        // Free the oldest and take a new page: each round adds a range to
        // the list and keeps `LIVE_CAP` ranges live, so the pool runs dry
        // and the coalesce runs at the cap.
        let mut ran_dry = false;
        for _ in 0..2 * MAX_RANGES {
            ran_dry |= k.nslots == 0;
            let va = live.pop_front().unwrap();
            k.free(va, PAGE_SIZE);
            live.push_back(k.alloc(PAGE_SIZE).unwrap());
            assert_eq!(k.live, LIVE_CAP);
        }
        assert!(ran_dry, "the node pool never ran dry");
        for va in live {
            k.free(va, PAGE_SIZE);
        }
        assert_eq!(k.stats().used, 0);
        assert_eq!(k.live, 0);
    }

    #[test]
    fn kva_coalesce_in_place_merges_neighbours() {
        let mut k = window(64);
        let a = k.alloc(PAGE_SIZE).unwrap();
        let b = k.alloc(PAGE_SIZE).unwrap();
        let c = k.alloc(PAGE_SIZE).unwrap();
        let d = k.alloc(PAGE_SIZE).unwrap();
        k.free(c, PAGE_SIZE);
        k.free(a, PAGE_SIZE);
        k.free(b, PAGE_SIZE);
        assert_eq!(k.stats().free_ranges, 4);
        k.coalesce_all();
        // a, b, c merge; d is live; the remainder after d stays apart.
        assert_eq!(
            k.free_list(),
            std::vec![(a, 3 * PAGE_SIZE), (d + PAGE_SIZE, 60 * PAGE_SIZE)]
        );
        assert_eq!(k.nslots, MAX_RANGES - 2);
        k.free(d, PAGE_SIZE);
        k.coalesce_all();
        assert_eq!(k.free_list(), std::vec![(KVA_START, 64 * PAGE_SIZE)]);
        assert_eq!(k.stats().used, 0);
    }

    #[test]
    fn sort_by_start_orders_like_std() {
        for n in [0usize, 1, 2, 3, 17, 64, MAX_RANGES] {
            // A fixed scramble of distinct starts, with some runs and repeats.
            let mut v: std::vec::Vec<Range> = (0..n as u64)
                .map(|i| Range {
                    start: (i.wrapping_mul(0x9E37_79B9) % 1021) * PAGE_SIZE,
                    len: i + 1,
                })
                .collect();
            let mut want = v.clone();
            want.sort_by_key(|r| r.start);
            sort_by_start(&mut v);
            let got: std::vec::Vec<u64> = v.iter().map(|r| r.start).collect();
            let want: std::vec::Vec<u64> = want.iter().map(|r| r.start).collect();
            assert_eq!(got, want, "n = {n}");
        }
    }

    #[test]
    fn fixed_tables_match_limits() {
        use crate::limits::MAX_KVA_RANGES;
        let k = std::boxed::Box::new(Kva::empty());
        assert_eq!(k.nodes.len(), MAX_KVA_RANGES);
        assert_eq!(k.next.len(), MAX_KVA_RANGES);
        assert_eq!(k.slots.len(), MAX_KVA_RANGES);
    }
}
