//! Physical memory manager: buddy allocator, portable half.
//!
//! DESIGN §4.2. Free blocks of order *k* cover `2^k` contiguous 4 KiB
//! frames. Split on allocation, merge with the buddy on free. Free-list
//! nodes live inside the free pages themselves (intrusive), so the
//! allocator needs no auxiliary bitmap and no allocation to bootstrap.
//! The whole `MAX_ORDER` blocks of each region `insert_region` takes are
//! kept as a run, a `[start, end)` pair in the `Buddy` itself, rather than
//! linked, and a block leaves its run only when it is taken: boot writes
//! no node into memory it has not used (ROADMAP §10.2).
//!
//! The one sharp edge is the same one called out in the design doc: a stray
//! write into a freed page corrupts these lists, and the crash surfaces
//! somewhere unrelated hours later. Guard pages on kernel stacks are the
//! rule that keeps this survivable (DESIGN §9.2).
//!
//! Host-testability. `Buddy::new(hhdm)` fixes, for the allocator's whole
//! life, the offset that maps a physical address to the (kernel-virtual,
//! or host-owned) address at which the free-list node lives:
//!
//!   node_ptr(phys) = phys + hhdm_offset
//!
//! In the kernel this is `BootInfo.hhdm_offset`. In host tests it is
//! `real_backing_ptr - phys_base` ([`testing::Pool`]), so the same code
//! drives a `Vec`-backed pool without any hardware.
//!
//! What is deliberately deferred to Slice B / phase 4:
//! - Per-frame metadata array (refcounts, page cache linkage, etc).
//!   `Buddy` carries no per-frame state; adding a parallel `[Frame; N]`
//!   later does not require rewriting anything here.
//! - O(1) coalescing. Right now finding the buddy in its free list is a
//!   scan. The doubly-linked list means removal is O(1) once we know the
//!   buddy is there, but the "is it there" check is O(list length). Fine
//!   for phase 1; a per-order bitmap or an XOR-tag scheme replaces it in
//!   the SMP-hardened rewrite.

#![allow(clippy::identity_op)] // order-0 size is `1 << 0` on purpose

use crate::atomic::statics::{AtomicUsize, Ordering};
use core::num::NonZeroU64;
use core::ops::Range;

pub const PAGE_BITS: u32 = 12;
pub const PAGE_SIZE: u64 = 1 << PAGE_BITS;

/// Maximum block order. Order *k* covers `2^k` frames, so `MAX_ORDER = 10`
/// covers 4 MiB blocks — enough for the phase-1 exit gate and slightly
/// bigger than a 2 MiB page-table mapping, so the paging code can pull
/// whole 2 MiB regions from a single allocation.
pub const MAX_ORDER: usize = 10;

pub type PhysAddr = u64;

/// Frames whose [`Frames`] token was dropped instead of freed. A
/// statistic that orders nothing, so every access is Relaxed.
static LEAKED_FRAMES: AtomicUsize = AtomicUsize::new(0);

/// Frames leaked by dropped [`Frames`] tokens since boot. `meminfo`
/// prints it.
pub fn leaked_frames() -> usize {
    // Relaxed: a statistic; pairs with nothing.
    LEAKED_FRAMES.load(Ordering::Relaxed)
}

/// Ownership of one buddy block: `2^order` frames at `base`. Only
/// [`Buddy::alloc`] and [`Buddy::alloc_constrained`] hand one out, and
/// [`Buddy::free`] takes it back; [`PhysAddr`] stays a `Copy` address
/// that owns nothing (DESIGN §4.2). A page-table entry holds a frame by
/// [`Frames::into_entry`], and the page-table code that clears the entry
/// takes it back with [`Frames::from_entry`].
///
/// Dropping a `Frames` leaks the block: the drop counts it in
/// [`leaked_frames`] and never frees, since a free on drop would take
/// BUDDY under whatever lock the holder was dropped under. In debug builds
/// it then panics naming the `alloc` call that made it.
///
/// A token cannot be copied:
///
/// ```compile_fail,E0382
/// fn twice(f: vibeos::pmm::Frames) -> (vibeos::pmm::Frames, vibeos::pmm::Frames) {
///     (f, f)
/// }
/// ```
///
/// or cloned:
///
/// ```compile_fail,E0277
/// fn need<T: Clone>() {}
/// need::<vibeos::pmm::Frames>();
/// ```
///
/// or made from an address:
///
/// ```compile_fail,E0277
/// let f: vibeos::pmm::Frames = 0x1000u64.into();
/// # core::mem::forget(f);
/// ```
///
/// and only `unsafe` code rebuilds one from a cleared entry:
///
/// ```compile_fail,E0133
/// let f = vibeos::pmm::Frames::from_entry(0x1000, 0);
/// # core::mem::forget(f);
/// ```
#[must_use = "a dropped Frames leaks its block; pass it to Buddy::free"]
pub struct Frames {
    /// `base | order`: the base is page aligned and never frame 0, so the
    /// order fits in the low bits and the value is never zero.
    raw: NonZeroU64,
    #[cfg(debug_assertions)]
    site: &'static core::panic::Location<'static>,
}

impl Frames {
    const ORDER_MASK: u64 = PAGE_SIZE - 1;

    /// # Safety
    /// `base` is nonzero and aligned to `order`, and `order <= MAX_ORDER`.
    #[track_caller]
    unsafe fn new(base: PhysAddr, order: u8) -> Self {
        // SAFETY: `NonZeroU64::new_unchecked` needs a nonzero value;
        // `base` is nonzero by this fn's `# Safety` contract, established
        // here, so `base | order` is too.
        let raw = unsafe { NonZeroU64::new_unchecked(base | order as u64) };
        Self {
            raw,
            #[cfg(debug_assertions)]
            site: core::panic::Location::caller(),
        }
    }

    /// Physical base of the block.
    pub fn base(&self) -> PhysAddr {
        self.raw.get() & !Self::ORDER_MASK
    }

    /// The block is `2^order` frames.
    pub fn order(&self) -> u8 {
        (self.raw.get() & Self::ORDER_MASK) as u8
    }

    /// Frames in the block.
    pub fn count(&self) -> usize {
        1usize << self.order()
    }

    /// Move ownership into a page-table entry or another owner record,
    /// which [`Frames::from_entry`] later turns back into a token.
    pub fn into_entry(self) -> PhysAddr {
        let base = self.base();
        core::mem::forget(self);
        base
    }

    /// Take a frame back out of the record that holds it.
    ///
    /// # Safety
    /// `pa` came from [`Frames::into_entry`] of a `Frames` of this
    /// `order`, and the caller just cleared the entry that held it, so no
    /// other token or entry names the block.
    #[track_caller]
    pub unsafe fn from_entry(pa: PhysAddr, order: u8) -> Frames {
        // SAFETY: `Frames::new`'s contract; `pa` is the base of a live
        // token, nonzero and aligned to `order <= MAX_ORDER`, by this fn's
        // `# Safety` contract, established here.
        unsafe { Frames::new(pa, order) }
    }
}

impl Drop for Frames {
    #[allow(
        clippy::panic,
        reason = "ROADMAP §10.3: a dropped Frames panics in debug builds, DESIGN §4.2"
    )]
    fn drop(&mut self) {
        // Relaxed: a statistic; pairs with nothing.
        LEAKED_FRAMES.fetch_add(self.count(), Ordering::Relaxed);
        #[cfg(debug_assertions)]
        {
            // A host test that fails while it holds a token unwinds
            // through here; a second panic would abort the test binary.
            #[cfg(any(test, feature = "std"))]
            if std::thread::panicking() {
                return;
            }
            panic!(
                "pmm: dropped Frames {:#x} order {} allocated at {}:{}:{}",
                self.base(),
                self.order(),
                self.site.file(),
                self.site.line(),
                self.site.column()
            );
        }
    }
}

/// Snapshot of allocator state. Computed cheaply; the running counter is
/// O(1) and `largest_free_order` is a linear scan over the (small) order
/// array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PmmStats {
    pub total_frames: usize,
    pub free_frames: usize,
    pub largest_free_order: Option<u8>,
}

/// Intrusive doubly-linked free-list node, written into the first bytes of
/// each free block. 16 bytes; every 4 KiB block has enough room.
#[repr(C)]
struct FreeNode {
    next: u64,
    prev: u64,
}

/// Sentinel meaning "no more nodes". Frame 0 is always excluded from the
/// allocator (DESIGN §2.4), so 0 is unambiguous.
const NULL: u64 = 0;

/// Runs of whole free `MAX_ORDER` blocks a [`Buddy`] keeps unlinked. A
/// memory map with more top-order runs than this has the rest linked.
pub const TOP_RUNS: usize = 16;

pub struct Buddy {
    heads: [u64; MAX_ORDER + 1],
    counts: [usize; MAX_ORDER + 1],
    total_frames: usize,
    free_frames: usize,
    hhdm_offset: u64,
    /// `[span_lo, span_hi)` covers every range `insert_region` took, so
    /// `free` refuses a block that was never this buddy's.
    span_lo: u64,
    span_hi: u64,
    /// Runs `[start, end)` of whole free top-order blocks that
    /// `insert_region` recorded instead of linking, lowest first; the first
    /// `nruns` are live. Their blocks count in `counts[MAX_ORDER]` and
    /// `free_frames`, and a block leaves its run only when it is taken, so
    /// boot writes no free-list link into memory it has not used: one link
    /// in every free 4 MiB block was one first touch of every 4 MiB of RAM
    /// (ROADMAP §10.2).
    runs: [(u64, u64); TOP_RUNS],
    nruns: usize,
}

impl Buddy {
    /// New empty buddy whose free-list nodes live at `phys + hhdm`. The
    /// offset is fixed for the allocator's life: every range later handed
    /// to `insert_region` must be writable at that offset.
    pub const fn new(hhdm: u64) -> Self {
        Self {
            heads: [NULL; MAX_ORDER + 1],
            counts: [0; MAX_ORDER + 1],
            total_frames: 0,
            free_frames: 0,
            hhdm_offset: hhdm,
            span_lo: u64::MAX,
            span_hi: 0,
            runs: [(0, 0); TOP_RUNS],
            nruns: 0,
        }
    }

    pub fn hhdm_offset(&self) -> u64 {
        self.hhdm_offset
    }

    /// Write the HHDM offset on an empty buddy. The kernel static starts
    /// at 0; `pmm_init::init` stores Limine's offset before any insert.
    pub fn set_hhdm(&mut self, hhdm: u64) {
        assert!(
            self.total_frames == 0 && self.nruns == 0,
            "Buddy::set_hhdm after insert"
        );
        self.hhdm_offset = hhdm;
    }

    pub fn stats(&self) -> PmmStats {
        let mut largest: Option<u8> = None;
        // Walk high to low so we stop at the first non-empty order.
        for k in (0..=MAX_ORDER).rev() {
            if self.counts[k] > 0 {
                largest = Some(k as u8);
                break;
            }
        }
        PmmStats {
            total_frames: self.total_frames,
            free_frames: self.free_frames,
            largest_free_order: largest,
        }
    }

    /// Free blocks, every order counted: how many `alloc` calls, highest
    /// order first, take every free frame, since such a drain splits no
    /// block.
    pub fn free_blocks(&self) -> usize {
        self.counts.iter().fold(0usize, |n, &c| n.saturating_add(c))
    }

    /// Contribute a [start, end) physical range to the free lists.
    /// Splits into maximally-aligned, maximally-sized buddy blocks; the
    /// whole `MAX_ORDER` blocks among them become one run, which writes
    /// nothing into them, while a run slot is free ([`TOP_RUNS`]).
    /// Any bytes below the first page, or beyond the last page, are
    /// ignored. Frame 0 is skipped unconditionally.
    ///
    /// # Safety
    /// Caller vouches that the entire range is real, writable memory
    /// reachable through `hhdm_offset`, that no piece of it is currently
    /// in the free lists, and that nothing else holds a pointer into it:
    /// from here on only the buddy writes it (invariant I46).
    pub unsafe fn insert_region(&mut self, start: PhysAddr, end: PhysAddr) {
        let start = start.max(PAGE_SIZE);
        let mut a = align_up(start, PAGE_SIZE);
        let b = align_down(end, PAGE_SIZE);
        if a < b {
            self.span_lo = self.span_lo.min(a);
            self.span_hi = self.span_hi.max(b);
        }
        while a < b {
            let remaining_pages = (b - a) / PAGE_SIZE;
            // Largest order whose block fits: bounded by alignment of `a`
            // and by the pages left in the range.
            let max_by_align = (a.trailing_zeros() - PAGE_BITS) as usize;
            let max_by_len = remaining_pages.ilog2() as usize;
            let k = max_by_align.min(max_by_len).min(MAX_ORDER);
            if k == MAX_ORDER {
                // `a` is top-order aligned and a whole top block fits: every
                // whole top block from here on is one run.
                let top = PAGE_SIZE << MAX_ORDER;
                let n = (b - a) / top;
                let run_end = a + n * top;
                if self.add_run(a, run_end) {
                    let frames = (n as usize) << MAX_ORDER;
                    self.total_frames += frames;
                    self.counts[MAX_ORDER] += n as usize;
                    self.free_frames += frames;
                    a = run_end;
                    continue;
                }
            }
            self.total_frames += 1 << k;
            // SAFETY: `push_free`'s contract; `[a, a + 2^k frames)` lies in
            // the range this fn's `# Safety` contract hands over, unused and
            // not yet on a list (invariant I46, established here).
            unsafe { self.push_free(a, k as u8) };
            a += PAGE_SIZE << k;
        }
    }

    /// Allocate a block of `order`, owned by the returned token. Splits
    /// down from a larger order if none is available at `order` directly.
    /// Returns `None` on exhaustion or for `order > MAX_ORDER`.
    #[track_caller]
    #[must_use]
    pub fn alloc(&mut self, order: u8) -> Option<Frames> {
        let base = self.take_block(order)?;
        // SAFETY: `Frames::new`'s contract; `take_block` returned a block of
        // `order <= MAX_ORDER` aligned to its size, and frame 0 never enters
        // the buddy (invariant I15, established at
        // `mm::pmm::Buddy::insert_region`).
        Some(unsafe { Frames::new(base, order) })
    }

    /// Allocate a block of `order` whose end, `base + 2^order` frames, is
    /// at or below `max_phys`. Walks the free lists at `order` and above
    /// for the first block that fits, unlinks it and splits it down.
    #[track_caller]
    #[must_use]
    pub fn alloc_constrained(&mut self, order: u8, max_phys: u64) -> Option<Frames> {
        let target = order as usize;
        if target > MAX_ORDER {
            return None;
        }
        let size = PAGE_SIZE << order;
        let fits = |base: u64| base.checked_add(size).is_some_and(|end| end <= max_phys);
        let mut k = target;
        let mut found = NULL;
        while k <= MAX_ORDER && found == NULL {
            let mut cur = self.heads[k];
            while cur != NULL {
                if fits(cur) {
                    found = cur;
                    break;
                }
                // SAFETY: `cur` is a node on the order-`k` free list, which
                // lives in a free page only the buddy writes (invariant
                // I46, established at `mm::pmm::Buddy::insert_region`).
                cur = unsafe { (*self.node_ptr(cur)).next };
            }
            if found == NULL {
                k += 1;
            }
        }
        if found != NULL {
            // SAFETY: `unlink`'s contract; the walk above found `found` on
            // the order-`k` free list, established here.
            unsafe { self.unlink(found, k as u8) };
        } else {
            // A run's lowest block is the one of it likeliest to end at or
            // below `max_phys`.
            k = MAX_ORDER;
            found = self.take_run_block(|start, _| fits(start).then_some(start))?;
        }
        // Keep the low piece, push the high half at each order below `k`.
        while k > target {
            k -= 1;
            // SAFETY: `push_free`'s contract; the high half of the block
            // unlinked above is unused and the buddy's (invariant I46,
            // established here).
            unsafe { self.push_free(found + (PAGE_SIZE << k), k as u8) };
        }
        // SAFETY: `Frames::new`'s contract; `found` is aligned to its
        // order-`k` block and so to `order`, and is not frame 0 (invariant
        // I15, established at `mm::pmm::Buddy::insert_region`).
        Some(unsafe { Frames::new(found, order) })
    }

    /// Give a block back. Safe: only a token this buddy handed out, or one
    /// rebuilt from the entry that held it, names a block.
    ///
    /// Panics if the block lies outside every range `insert_region` took,
    /// or is already free.
    pub fn free(&mut self, f: Frames) {
        let (base, order) = (f.base(), f.order());
        core::mem::forget(f);
        let end = base.checked_add(PAGE_SIZE << order);
        assert!(
            base >= self.span_lo && end.is_some_and(|e| e <= self.span_hi),
            "pmm: free {base:#x} order {order} outside the buddy's span"
        );
        // SAFETY: `deallocate`'s contract; the token owned the block, so the
        // buddy handed it out at `order` and has not taken it back
        // (`mm::pmm::Frames`, a non-`Copy` token); `deallocate` also checks
        // both.
        unsafe { self.deallocate(base, order) };
    }

    /// Take a block of `order` off the free lists, splitting down from a
    /// larger order if none is free at `order` directly. `None` on
    /// exhaustion. [`Buddy::alloc`] wraps the block in its token.
    fn take_block(&mut self, order: u8) -> Option<PhysAddr> {
        let target = order as usize;
        if target > MAX_ORDER {
            return None;
        }
        let mut k = target;
        while k < MAX_ORDER && self.heads[k] == NULL {
            k += 1;
        }
        let addr = if self.heads[k] != NULL {
            // SAFETY: `pop_head`'s contract; the loop above stopped at an
            // order `k` whose head is not NULL, established here.
            unsafe { self.pop_head(k as u8) }
        } else {
            // Only `MAX_ORDER`'s list is left: its runs, highest block
            // first, as the list it replaces handed out its last-linked,
            // highest block first.
            self.take_run_block(|_, end| Some(end - (PAGE_SIZE << MAX_ORDER)))?
        };
        // Split down, pushing the right half at each intermediate order.
        while k > target {
            k -= 1;
            let buddy = addr + (PAGE_SIZE << k);
            // SAFETY: `push_free`'s contract; the right half of the block
            // popped above is unused and the buddy's (invariant I46,
            // established here).
            unsafe { self.push_free(buddy, k as u8) };
        }
        Some(addr)
    }

    /// Free a previously-allocated block at `order`. Coalesces with its
    /// buddy up to `MAX_ORDER`. [`Buddy::free`] is the only caller.
    ///
    /// # Safety
    /// `phys` is a block of `order` this buddy handed out and has not yet
    /// taken back. Panics on double-free at the same order or on
    /// misalignment.
    unsafe fn deallocate(&mut self, mut phys: PhysAddr, mut order: u8) {
        assert!(
            (order as usize) <= MAX_ORDER,
            "pmm: order {order} > MAX_ORDER"
        );
        let block_size = PAGE_SIZE << order;
        assert!(
            phys & (block_size - 1) == 0,
            "pmm: dealloc phys {phys:#x} not aligned to order {order}"
        );
        // Double-free detection: bail if `phys` is currently part of any
        // free block at `order` or above. A per-frame bitmap would answer
        // this in O(1); we settle for O(MAX_ORDER * list_len) since it
        // only runs on `deallocate` and MAX_ORDER is small. Phase 4
        // rewrites this alongside per-frame metadata.
        assert!(
            // SAFETY: `covered_by_free_block`'s contract; only the buddy
            // writes its free lists, so their nodes are intact (invariant
            // I46, established at `mm::pmm::Buddy::insert_region`).
            !unsafe { self.covered_by_free_block(phys, order) },
            "pmm: double free at {phys:#x} order {order}"
        );

        while (order as usize) < MAX_ORDER {
            let buddy = phys ^ (PAGE_SIZE << order);
            // SAFETY: `in_free_list`'s contract; the free lists are intact
            // (invariant I46, established at
            // `mm::pmm::Buddy::insert_region`).
            if !unsafe { self.in_free_list(buddy, order) } {
                break;
            }
            // SAFETY: `unlink`'s contract; `in_free_list` just found `buddy`
            // on the order-`order` list, established here.
            unsafe { self.unlink(buddy, order) };
            phys = phys.min(buddy);
            order += 1;
        }
        // SAFETY: `push_free`'s contract; `phys` is the block this fn's
        // `# Safety` contract hands back, merged with free buddies the loop
        // unlinked, so it is unused and the buddy's (invariant I46,
        // established here).
        unsafe { self.push_free(phys, order) };
    }

    /// Order whose block covers `bytes` and is aligned to `align`. `None`
    /// for an empty size, a size above the largest block (refused before
    /// rounding, so `next_power_of_two` cannot overflow; F103), an
    /// alignment that is not a power of two, or a block above `MAX_ORDER`.
    pub fn order_for(bytes: u64, align: u64) -> Option<u8> {
        if bytes == 0 || bytes > PAGE_SIZE << MAX_ORDER {
            return None;
        }
        let align = if align == 0 { PAGE_SIZE } else { align };
        if !align.is_power_of_two() {
            return None;
        }
        let need_size = bytes.max(PAGE_SIZE).next_power_of_two();
        let need_align = align.max(PAGE_SIZE);
        let need = need_size.max(need_align);
        let pages = need / PAGE_SIZE;
        if pages == 0 || (pages & (pages - 1)) != 0 {
            return None;
        }
        let order = pages.trailing_zeros() as u8;
        if (order as usize) > MAX_ORDER {
            None
        } else {
            Some(order)
        }
    }

    // ------------------ private helpers ------------------

    #[inline]
    fn node_ptr(&self, phys: u64) -> *mut FreeNode {
        // Wrapping so a hhdm_offset chosen so that vec_ptr = phys_base +
        // hhdm_offset works for host tests without caring about the sign
        // of the difference.
        (phys.wrapping_add(self.hhdm_offset) as usize) as *mut FreeNode
    }

    /// # Safety
    /// `phys` is an unused block of `order` that this buddy owns, not on
    /// any free list, and writable at `phys + hhdm_offset` (invariant I46).
    unsafe fn push_free(&mut self, phys: u64, order: u8) {
        let k = order as usize;
        let head = self.heads[k];
        let node = self.node_ptr(phys);
        // SAFETY: `node` is the first bytes of the unused block `phys` this
        // fn's `# Safety` contract hands over, and `head`, when not NULL, is
        // a node on the order-`k` list; only the buddy writes either
        // (invariant I46, established here).
        unsafe {
            node.write(FreeNode {
                next: head,
                prev: NULL,
            });
            if head != NULL {
                (*self.node_ptr(head)).prev = phys;
            }
        }
        self.heads[k] = phys;
        self.counts[k] += 1;
        self.free_frames += 1 << k;
    }

    /// # Safety
    /// Order `order` has a free head; list nodes live in free pages.
    unsafe fn pop_head(&mut self, order: u8) -> u64 {
        let k = order as usize;
        let head = self.heads[k];
        assert!(head != NULL, "pmm: pop_head on empty order {k}");
        let n = self.node_ptr(head);
        // SAFETY: the assert above makes `head` a node on the order-`k`
        // list, in a free page only the buddy writes (invariant I46,
        // established at `mm::pmm::Buddy::insert_region`).
        let next = unsafe { (*n).next };
        self.heads[k] = next;
        if next != NULL {
            // SAFETY: `next` is the following node on the same list
            // (invariant I46, established at
            // `mm::pmm::Buddy::insert_region`).
            unsafe {
                (*self.node_ptr(next)).prev = NULL;
            }
        }
        self.counts[k] -= 1;
        self.free_frames -= 1 << k;
        head
    }

    /// # Safety
    /// `phys` is currently a free-list node of `order`.
    unsafe fn unlink(&mut self, phys: u64, order: u8) {
        let k = order as usize;
        let node = self.node_ptr(phys);
        // SAFETY: `phys` is a node on the order-`k` list by this fn's
        // `# Safety` contract, established here, and lives in a free page
        // only the buddy writes (invariant I46).
        let (prev, next) = unsafe { ((*node).prev, (*node).next) };
        if prev == NULL {
            self.heads[k] = next;
        } else {
            // SAFETY: `prev` is `phys`'s neighbour on the same list
            // (invariant I46, established at
            // `mm::pmm::Buddy::insert_region`).
            unsafe {
                (*self.node_ptr(prev)).next = next;
            }
        }
        if next != NULL {
            // SAFETY: `next` is `phys`'s neighbour on the same list
            // (invariant I46, established at
            // `mm::pmm::Buddy::insert_region`).
            unsafe {
                (*self.node_ptr(next)).prev = prev;
            }
        }
        self.counts[k] -= 1;
        self.free_frames -= 1 << k;
    }

    /// # Safety
    /// Free-list nodes of `order` are intact.
    unsafe fn in_free_list(&self, phys: u64, order: u8) -> bool {
        let mut cur = self.heads[order as usize];
        while cur != NULL {
            if cur == phys {
                return true;
            }
            // SAFETY: `cur` is a node on the order-`order` list, intact by
            // this fn's `# Safety` contract (invariant I46, established
            // here).
            cur = unsafe { (*self.node_ptr(cur)).next };
        }
        false
    }

    /// Record `[start, end)`, whole top-order blocks on no list, as a run:
    /// it extends the run that ends at `start`, or takes a free slot. False
    /// when every slot is taken, and the caller links the blocks instead.
    fn add_run(&mut self, start: u64, end: u64) -> bool {
        if start >= end {
            return true;
        }
        if let Some(last) = self.nruns.checked_sub(1).and_then(|i| self.runs.get_mut(i))
            && last.1 == start
        {
            last.1 = end;
            return true;
        }
        match self.runs.get_mut(self.nruns) {
            Some(slot) => {
                *slot = (start, end);
                self.nruns += 1;
                true
            }
            None => false,
        }
    }

    /// Take one top-order block out of a run: from the last run back, the
    /// first for which `pick(start, end)` names a block, which must be the
    /// run's first or last block. The run shrinks, or goes when it empties.
    fn take_run_block(&mut self, pick: impl Fn(u64, u64) -> Option<u64>) -> Option<u64> {
        let top = PAGE_SIZE << MAX_ORDER;
        let mut i = self.nruns;
        while i > 0 {
            i -= 1;
            let Some(&(start, end)) = self.runs.get(i) else {
                continue;
            };
            let Some(blk) = pick(start, end) else {
                continue;
            };
            assert!(
                blk == start || blk == end - top,
                "pmm: run block {blk:#x} not at an end of its run"
            );
            let rest = if blk == start {
                (start + top, end)
            } else {
                (start, end - top)
            };
            if rest.0 < rest.1 {
                self.runs[i] = rest;
            } else {
                self.runs.copy_within(i + 1..self.nruns, i);
                self.nruns -= 1;
            }
            self.counts[MAX_ORDER] -= 1;
            self.free_frames -= 1 << MAX_ORDER;
            return Some(blk);
        }
        None
    }

    /// Whether `phys` lies in one of the runs. An index loop, which the
    /// Kani proofs unwind cheaply.
    fn in_run(&self, phys: u64) -> bool {
        let mut i = 0usize;
        while i < self.nruns {
            if let Some(&(start, end)) = self.runs.get(i)
                && start <= phys
                && phys < end
            {
                return true;
            }
            i += 1;
        }
        false
    }

    /// True iff `phys` is currently inside some free block at `at_least_order`
    /// or larger. Used to detect double-free even when the freed piece has
    /// already been coalesced into a bigger block.
    ///
    /// # Safety
    /// Free-list nodes are intact.
    unsafe fn covered_by_free_block(&self, phys: u64, at_least_order: u8) -> bool {
        if self.in_run(phys) {
            return true;
        }
        for k in (at_least_order as usize)..=MAX_ORDER {
            let block_start = phys & !((PAGE_SIZE << k) - 1);
            // SAFETY: `in_free_list`'s contract; the lists are intact by
            // this fn's `# Safety` contract (invariant I46, established
            // here).
            if unsafe { self.in_free_list(block_start, k as u8) } {
                return true;
            }
        }
        false
    }
}

#[inline]
const fn align_up(x: u64, align: u64) -> u64 {
    (x + align - 1) & !(align - 1)
}

#[inline]
const fn align_down(x: u64, align: u64) -> u64 {
    x & !(align - 1)
}

/// True if `[phys, phys+size)` straddles a power-of-two `boundary`.
///
/// ```
/// use vibeos::pmm::crosses_boundary;
/// assert!(crosses_boundary(0xFFFF_F800, 0x1000, 1 << 32));
/// assert!(!crosses_boundary(0x1000, 0x1000, 1 << 32));
/// ```
pub const fn crosses_boundary(phys: u64, size: u64, boundary: u64) -> bool {
    if boundary == 0 || size == 0 {
        return false;
    }
    let mask = boundary - 1;
    (phys & mask) + size > boundary
}

/// One past the last byte a SIPI can start an AP at: real mode reaches
/// only the first 1 MiB (DESIGN §7.3).
pub const TRAMPOLINE_LIMIT: u64 = 0x10_0000;

/// The AP trampoline page (DESIGN §7.3): the lowest 4 KiB page `p` with
/// `0x1000 <= p` and `p + 0x1000 <= 1 MiB` that lies wholly inside one of
/// `usable`, or `None`. Each range is rounded inward to pages; a range
/// that wraps or is empty holds no page. The firmware map is untrusted
/// input (DESIGN §2.10), so nothing here panics or overflows.
pub fn choose_trampoline_page<I: IntoIterator<Item = Range<u64>>>(usable: I) -> Option<u64> {
    let mut best: Option<u64> = None;
    for r in usable {
        let Some(lo) = r.start.checked_add(PAGE_SIZE - 1) else {
            continue;
        };
        let lo = align_down(lo, PAGE_SIZE).max(PAGE_SIZE);
        let hi = align_down(r.end, PAGE_SIZE).min(TRAMPOLINE_LIMIT);
        let Some(page_end) = lo.checked_add(PAGE_SIZE) else {
            continue;
        };
        if page_end <= hi && best.is_none_or(|b| lo < b) {
            best = Some(lo);
        }
    }
    best
}

/// Emit, in ascending order, each part of `r` that lies outside every
/// range `excl()` yields, each exclusion rounded outward to pages. The
/// exclusions may overlap and come in any order; `excl` is called again for
/// each step, so nothing is collected and there is no cap on how many there
/// are (DESIGN §2.4). An empty or wrapped exclusion excludes nothing. It
/// allocates nothing and panics on nothing.
pub fn clip_usable<I: Iterator<Item = Range<u64>>>(
    r: Range<u64>,
    excl: impl Fn() -> I,
    mut emit: impl FnMut(Range<u64>),
) {
    let mut cur = r.start;
    while cur < r.end {
        // The end of the furthest exclusion covering `cur`, and the start
        // of the nearest one ahead of it.
        let mut skip_to: Option<u64> = None;
        let mut next = r.end;
        for e in excl() {
            if e.start >= e.end {
                continue;
            }
            let lo = align_down(e.start, PAGE_SIZE);
            let hi = match e.end.checked_add(PAGE_SIZE - 1) {
                Some(x) => align_down(x, PAGE_SIZE),
                None => u64::MAX,
            };
            if lo <= cur && cur < hi {
                skip_to = Some(skip_to.map_or(hi, |s| s.max(hi)));
            } else if lo > cur && lo < next {
                next = lo;
            }
        }
        match skip_to {
            Some(h) => cur = h,
            None => {
                emit(cur..next);
                cur = next;
            }
        }
    }
}

/// Host-test backing store shared by the `pmm`, `dma`, `paging` and
/// `addr_space` tests.
#[cfg(test)]
pub(crate) mod testing {
    use super::{Buddy, PAGE_SIZE, PhysAddr};
    use std::vec;
    use std::vec::Vec;

    /// 4 MiB aligned so a 1024-frame pool registers as one order-10 block.
    pub(crate) const TEST_PHYS_BASE: u64 = 0x0040_0000;

    /// Vec-backed "physical memory". Owns the backing buffer and builds a
    /// `Buddy` whose offset maps `TEST_PHYS_BASE` onto it.
    pub(crate) struct Pool {
        _mem: Vec<u64>,
        pub(crate) phys_base: u64,
        pub(crate) phys_end: u64,
        pub(crate) buddy: Buddy,
    }

    impl Pool {
        pub(crate) fn new(frames: usize) -> Self {
            // Poison bytes so a bug reading uninitialized memory shows up.
            let words = (frames * PAGE_SIZE as usize) / 8;
            let mem: Vec<u64> = vec![0xDEAD_BEEF_DEAD_BEEFu64; words];
            let hhdm = (mem.as_ptr() as u64).wrapping_sub(TEST_PHYS_BASE);
            let phys_end = TEST_PHYS_BASE + (frames as u64) * PAGE_SIZE;
            let mut buddy = Buddy::new(hhdm);
            // SAFETY: `insert_region`'s contract; `mem` is `frames` pages
            // of host memory this pool owns, reached at `hhdm`, and on no
            // list yet (invariant I46, established here).
            unsafe { buddy.insert_region(TEST_PHYS_BASE, phys_end) };
            Self {
                _mem: mem,
                phys_base: TEST_PHYS_BASE,
                phys_end,
                buddy,
            }
        }

        /// The offset the pool's `Buddy` was built with.
        pub(crate) fn hhdm(&self) -> u64 {
            self.buddy.hhdm_offset()
        }

        pub(crate) fn contains(&self, phys: PhysAddr) -> bool {
            phys >= self.phys_base && phys < self.phys_end
        }
    }
}

// ------------------ host tests ------------------

#[cfg(test)]
mod tests {
    use super::testing::{Pool, TEST_PHYS_BASE};
    use super::*;

    use std::panic;
    use std::vec;
    use std::vec::Vec;

    /// Holds in release builds too (DESIGN §9.4): `make check` runs it with
    /// debug assertions off. The buddy has no regions, and its node for
    /// `NULL` is a zeroed local, so the unchecked code reads zeroes rather
    /// than writing through a wild pointer.
    #[test]
    #[should_panic(expected = "pmm: pop_head on empty order 0")]
    fn release_assert_pop_head_empty_order() {
        let zero = [0u64; 4];
        let mut b = Buddy::new((zero.as_ptr() as u64).wrapping_sub(NULL));
        // SAFETY: breaks `pop_head`'s contract on purpose; its `assert!`
        // fires before any read, and `NULL` maps to the zeroed local `zero`
        // anyway, established here.
        unsafe { b.pop_head(0) };
    }

    #[test]
    fn set_hhdm_only_while_empty() {
        let mut b = Buddy::new(0);
        b.set_hhdm(0xFFFF_8000_0000_0000);
        assert_eq!(b.hhdm_offset(), 0xFFFF_8000_0000_0000);
    }

    #[test]
    fn exhaustion_returns_none() {
        let mut p = Pool::new(16);
        // 16 frames, order 0. Drain them one at a time.
        let mut taken = Vec::new();
        while let Some(f) = p.buddy.alloc(0) {
            taken.push(f);
        }
        assert_eq!(taken.len(), 16);
        assert!(p.buddy.alloc(0).is_none());
        assert!(p.buddy.alloc(3).is_none());
        // Now free everything and confirm we're back at 16 free.
        for f in taken {
            p.buddy.free(f);
        }
        assert_eq!(p.buddy.stats().free_frames, 16);
    }

    #[test]
    fn per_order_alignment() {
        let mut p = Pool::new(1024);
        for order in 0u8..=(MAX_ORDER as u8) {
            let block = p.buddy.alloc(order).expect("order fits in 1024 frames");
            let block_size = PAGE_SIZE << order;
            assert!(
                block.base() & (block_size - 1) == 0,
                "order {order} allocation {:#x} not aligned to {block_size:#x}",
                block.base()
            );
            assert!(p.contains(block.base()));
            p.buddy.free(block);
        }
        assert!(p.buddy.alloc(MAX_ORDER as u8 + 1).is_none());
    }

    #[test]
    fn stats_track_totals_and_largest() {
        let mut p = Pool::new(1024);
        let s0 = p.buddy.stats();
        assert_eq!(s0.total_frames, 1024);
        assert_eq!(s0.free_frames, 1024);
        // 1024 frames = one order-10 block, so that's the largest.
        assert_eq!(s0.largest_free_order, Some(10));

        // Take an order-3 block; largest order stays.
        let a = p.buddy.alloc(3).unwrap();
        let s1 = p.buddy.stats();
        assert_eq!(s1.free_frames, 1024 - 8);

        p.buddy.free(a);
        let s2 = p.buddy.stats();
        assert_eq!(s2, s0, "state after alloc+free must equal initial");
    }

    #[test]
    fn free_blocks_bounds_a_highest_order_first_drain() {
        // 1000 frames: blocks of order 9, 8, 7, 6, 5 and 3 (512 + 256 +
        // 128 + 64 + 32 + 8).
        let mut p = Pool::new(1000);
        assert_eq!(p.buddy.free_blocks(), 6);
        let a = p.buddy.alloc(0).unwrap();
        // The order-3 block split into order 2, 1 and 0 blocks.
        assert_eq!(p.buddy.free_blocks(), 8);
        let mut held = Vec::new();
        let want = p.buddy.free_blocks();
        for order in (0..=MAX_ORDER as u8).rev() {
            while let Some(f) = p.buddy.alloc(order) {
                held.push(f);
            }
        }
        assert_eq!(held.len(), want);
        assert_eq!(p.buddy.free_blocks(), 0);
        held.push(a);
        for f in held {
            p.buddy.free(f);
        }
        assert_eq!(p.buddy.free_blocks(), 6);
    }

    #[test]
    fn coalescing_after_freeing_alternate_blocks() {
        // Start with one order-3 block worth of frames: 8 frames.
        let mut p = Pool::new(8);
        assert_eq!(p.buddy.stats().largest_free_order, Some(3));

        // Split all the way down: allocate all 8 as order-0.
        let mut frames = Vec::new();
        for _ in 0..8 {
            frames.push(p.buddy.alloc(0).unwrap());
        }
        assert_eq!(p.buddy.stats().free_frames, 0);
        assert_eq!(p.buddy.stats().largest_free_order, None);

        // Free every other one. Because buddies are paired by XOR of
        // (PAGE_SIZE << order), freeing frames [0,2,4,6] leaves each with
        // an allocated buddy, so nothing merges.
        frames.sort_by_key(|f| f.base());
        let mut odd = Vec::new();
        for (i, f) in frames.into_iter().enumerate() {
            if i % 2 == 0 {
                p.buddy.free(f);
            } else {
                odd.push(f);
            }
        }
        let mid = p.buddy.stats();
        assert_eq!(mid.free_frames, 4);
        assert_eq!(mid.largest_free_order, Some(0));

        // Now free the odd ones; each free should cascade all the way
        // back up to the single order-3 block we started with.
        for f in odd {
            p.buddy.free(f);
        }
        let end = p.buddy.stats();
        assert_eq!(end.free_frames, 8);
        assert_eq!(
            end.largest_free_order,
            Some(3),
            "coalesce did not reach the original order-3 block"
        );
    }

    #[test]
    fn split_and_merge_across_orders() {
        let mut p = Pool::new(16); // one order-4 block
        assert_eq!(p.buddy.stats().largest_free_order, Some(4));
        let a = p.buddy.alloc(2).unwrap(); // splits 4 -> 3 -> 2
        let s = p.buddy.stats();
        assert_eq!(s.free_frames, 12);
        // We should now have one free block at each of orders 2 and 3.
        assert_eq!(s.largest_free_order, Some(3));
        p.buddy.free(a);
        assert_eq!(p.buddy.stats().largest_free_order, Some(4));
    }

    #[test]
    fn random_alloc_free_returns_to_initial() {
        // Tiny deterministic LCG so this test is reproducible without a
        // dev-dependency on rand.
        let mut rng: u64 = 0x1234_5678_9abc_def0;
        let mut next = || {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            rng
        };

        let mut p = Pool::new(256);
        let baseline = p.buddy.stats();

        let mut live: Vec<Frames> = Vec::new();
        for _ in 0..5000 {
            // Bias toward smaller orders so we exercise splits and merges
            // rather than immediately exhausting the pool.
            let coin = next() & 0xff;
            let free = coin < 0x80 && !live.is_empty();
            if free {
                let i = (next() as usize) % live.len();
                p.buddy.free(live.swap_remove(i));
            } else {
                let order = ((next() & 0x7) as u8).min(5);
                if let Some(f) = p.buddy.alloc(order) {
                    live.push(f);
                }
            }
        }
        // Drain remaining allocations.
        for f in live.drain(..) {
            p.buddy.free(f);
        }
        let end = p.buddy.stats();
        assert_eq!(
            end, baseline,
            "random alloc/free stream must restore the initial free count and largest order"
        );
    }

    #[test]
    fn double_free_is_detected() {
        let mut p = Pool::new(16);
        let pa = p.buddy.alloc(0).unwrap().into_entry();
        // SAFETY: `from_entry`'s contract; `pa` came from `into_entry` of an
        // order-0 token no other record holds, established here.
        p.buddy.free(unsafe { Frames::from_entry(pa, 0) });
        // A forged second token for the same frame: `free` forgets it
        // before it checks, so the panic leaks nothing.
        let res = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            // SAFETY: forges a token on purpose; `free` refuses it with a
            // panic before touching the lists, established here.
            p.buddy.free(unsafe { Frames::from_entry(pa, 0) });
        }));
        assert!(res.is_err(), "double free must panic");
    }

    #[test]
    fn free_outside_span_is_refused() {
        let mut p = Pool::new(16);
        let res = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            // SAFETY: forges a token on purpose; `free` refuses a block
            // outside its span with a panic before touching the lists,
            // established here.
            p.buddy.free(unsafe { Frames::from_entry(p.phys_end, 0) });
        }));
        assert!(res.is_err(), "a block above the span must panic");
        assert_eq!(p.buddy.stats().free_frames, 16);
    }

    #[test]
    fn dropped_frames_leak_and_stay_allocated() {
        // The only host test that drops a `Frames`: the leak counter is
        // one static shared by every test in the process.
        let mut p = Pool::new(16);
        let line = line!() + 1;
        let f = p.buddy.alloc(0).unwrap();
        let base = f.base();
        let res = panic::catch_unwind(panic::AssertUnwindSafe(|| drop(f)));
        assert_eq!(res.is_err(), cfg!(debug_assertions));
        if let Err(e) = res {
            let msg = e
                .downcast_ref::<std::string::String>()
                .map(|s| s.as_str())
                .unwrap_or("");
            assert!(msg.contains(&std::format!("{base:#x} order 0")), "{msg}");
            assert!(msg.contains(&std::format!("{}:{line}:", file!())), "{msg}");
        }
        assert_eq!(leaked_frames(), 1);
        let s = p.buddy.stats();
        assert_eq!(s.free_frames, s.total_frames - 1);
        // SAFETY: `covered_by_free_block`'s contract; only the buddy wrote
        // the pool's lists (invariant I46, established here).
        assert!(!unsafe { p.buddy.covered_by_free_block(base, 0) });
    }

    #[test]
    fn alloc_constrained_respects_max_phys() {
        let mut p = Pool::new(64);
        let s0 = p.buddy.stats();
        // Only the first frame ends at or below base + 4 KiB.
        let low = p
            .buddy
            .alloc_constrained(0, TEST_PHYS_BASE + PAGE_SIZE)
            .unwrap();
        assert_eq!(low.base(), TEST_PHYS_BASE);
        assert_eq!(low.order(), 0);
        assert!(
            p.buddy
                .alloc_constrained(0, TEST_PHYS_BASE + PAGE_SIZE)
                .is_none()
        );
        // Nothing ends at or below the pool's base.
        assert!(p.buddy.alloc_constrained(0, TEST_PHYS_BASE).is_none());
        // A limit inside the pool: every block ends at or below it.
        let limit = TEST_PHYS_BASE + 16 * PAGE_SIZE;
        let mut got = Vec::new();
        while let Some(f) = p.buddy.alloc_constrained(1, limit) {
            assert!(f.base() + (PAGE_SIZE << 1) <= limit);
            assert_eq!(f.base() & (2 * PAGE_SIZE - 1), 0);
            got.push(f);
        }
        // 16 frames below the limit, one taken: 7 order-1 blocks fit.
        assert_eq!(got.len(), 7);
        let big = p.buddy.alloc_constrained(4, u64::MAX).unwrap();
        assert!(big.base() >= limit);
        assert!(
            p.buddy
                .alloc_constrained(MAX_ORDER as u8 + 1, u64::MAX)
                .is_none()
        );
        p.buddy.free(big);
        for f in got {
            p.buddy.free(f);
        }
        p.buddy.free(low);
        assert_eq!(p.buddy.stats(), s0);
    }

    #[test]
    fn from_entry_round_trip() {
        let mut p = Pool::new(16);
        let s0 = p.buddy.stats();
        let f = p.buddy.alloc(1).unwrap();
        let base = f.base();
        let pa = f.into_entry();
        assert_eq!(pa, base);
        assert_eq!(p.buddy.stats().free_frames, 14);
        // SAFETY: `from_entry`'s contract; `pa` came from `into_entry` of
        // the order-1 token above and nothing else names it, established
        // here.
        let back = unsafe { Frames::from_entry(pa, 1) };
        assert_eq!(back.base(), base);
        assert_eq!(back.order(), 1);
        p.buddy.free(back);
        assert_eq!(p.buddy.stats(), s0);
    }

    #[test]
    fn frames_accessors() {
        let mut p = Pool::new(64);
        let f = p.buddy.alloc(3).unwrap();
        assert_eq!(f.order(), 3);
        assert_eq!(f.count(), 8);
        assert_eq!(f.base() & (8 * PAGE_SIZE - 1), 0);
        assert!(p.contains(f.base()));
        assert_eq!(
            core::mem::size_of::<Option<Frames>>(),
            core::mem::size_of::<Frames>()
        );
        p.buddy.free(f);
    }

    #[test]
    fn insert_region_skips_frame_zero() {
        // A region nominally starting at phys 0 must drop frame 0. Use a
        // fresh buddy configured so phys 0 maps to mem[0].
        let words = (32 * PAGE_SIZE as usize) / 8;
        let mem: Vec<u64> = vec![0u64; words];
        let ptr = mem.as_ptr() as u64;
        let mut buddy = Buddy::new(ptr);
        // SAFETY: `insert_region`'s contract; `mem` is 32 pages of host
        // memory this test owns, reached at `ptr` (invariant I46,
        // established here).
        unsafe { buddy.insert_region(0, 32 * PAGE_SIZE) };
        assert_eq!(buddy.stats().total_frames, 31);
    }

    #[test]
    fn insert_region_trims_unaligned_edges() {
        // 10-page window sized in the vec, but caller hands us a range
        // whose bounds fall inside pages. Only the aligned interior — 8
        // whole pages — should register.
        let words = (10 * PAGE_SIZE as usize) / 8;
        let mem: Vec<u64> = vec![0u64; words];
        let ptr = mem.as_ptr() as u64;
        let mut buddy = Buddy::new(ptr.wrapping_sub(TEST_PHYS_BASE));
        // SAFETY: `insert_region`'s contract; `mem` is 10 pages of host
        // memory this test owns, and the range lies inside it (invariant
        // I46, established here).
        unsafe {
            buddy.insert_region(TEST_PHYS_BASE + 100, TEST_PHYS_BASE + 9 * PAGE_SIZE + 200);
        }
        assert_eq!(buddy.stats().total_frames, 8);
    }

    #[test]
    fn constrained_alloc_align_and_boundary() {
        assert_eq!(Buddy::order_for(1, PAGE_SIZE), Some(0));
        assert_eq!(Buddy::order_for(0x1800, 0x2000), Some(1));
        assert!(Buddy::order_for(0, PAGE_SIZE).is_none());
        assert!(crosses_boundary(0xFFFF_F800, 0x1000, 1u64 << 32));
        assert!(!crosses_boundary(0x1000, 0x1000, 1u64 << 32));
        // A 4 KiB request aligned to 8 KiB is an order-1 block; the
        // boundary rule lives in `dma::DmaAlloc::order`.
        let order = Buddy::order_for(0x1000, 0x2000).unwrap();
        assert_eq!(order, 1);
        let mut p = Pool::new(64);
        let f = p.buddy.alloc_constrained(order, 1u64 << 32).unwrap();
        assert_eq!(f.base() & 0x1FFF, 0);
        assert!(!crosses_boundary(f.base(), 0x1000, 1u64 << 32));
        p.buddy.free(f);
        assert_eq!(p.buddy.stats().free_frames, 64);
    }

    // ---- AP trampoline page and usable-range clipping (DESIGN §7.3, §2.4) ----

    /// Limine memory map types, as `limine::memmap` numbers them.
    const USABLE: u64 = 0;
    const RESERVED: u64 = 1;
    const BOOTLOADER_RECLAIMABLE: u64 = 5;

    /// The usable ranges of a `(base, end, type)` map.
    fn usable(map: &[(u64, u64, u64)]) -> impl Iterator<Item = core::ops::Range<u64>> + '_ {
        map.iter()
            .filter(|e| e.2 == USABLE)
            .map(|&(base, end, _)| base..end)
    }

    /// Below 1 MiB, the map the pinned Limine reports under QEMU's SeaBIOS
    /// with 128 MiB (frame 0 is in no entry), and the first usable range
    /// above it.
    const SEABIOS_MAP: &[(u64, u64, u64)] = &[
        (0x1000, 0x52000, BOOTLOADER_RECLAIMABLE),
        (0x52000, 0x9F000, USABLE),
        (0x9FC00, 0xA0000, RESERVED),
        (0xF0000, 0x10_0000, RESERVED),
        (0x10_0000, 0x7FE_0000, USABLE),
    ];

    /// Below 1 MiB, the map the pinned Limine reports under the OVMF
    /// (edk2) `make test-e2e-uefi` boots with 128 MiB.
    const OVMF_MAP: &[(u64, u64, u64)] = &[
        (0x0, 0x1000, BOOTLOADER_RECLAIMABLE),
        (0x1000, 0xA0000, USABLE),
        (0x10_0000, 0x80_0000, USABLE),
    ];

    #[test]
    fn trampoline_page_seabios_map() {
        assert_eq!(choose_trampoline_page(usable(SEABIOS_MAP)), Some(0x52000));
    }

    #[test]
    fn trampoline_page_ovmf_map() {
        assert_eq!(choose_trampoline_page(usable(OVMF_MAP)), Some(0x1000));
        // DESIGN §7.3's OVMF map, with frame 0 usable: frame 0 is skipped.
        let older = [(0x0, 0x87000, USABLE), (0x10_0000, 0x80_0000, USABLE)];
        assert_eq!(choose_trampoline_page(usable(&older)), Some(0x1000));
    }

    #[test]
    fn trampoline_page_skips_reserved_0x8000() {
        let map = [
            (0x1000, 0x8000, BOOTLOADER_RECLAIMABLE),
            (0x8000, 0x9000, RESERVED),
            (0x9000, 0x9F000, USABLE),
            (0x10_0000, 0x80_0000, USABLE),
        ];
        assert_eq!(choose_trampoline_page(usable(&map)), Some(0x9000));
        // Unsorted, and the lower range unaligned: rounded inward.
        let ranges = [0x9000..0x9F000, 0x7800..0x8000, 0x2800..0x4000];
        assert_eq!(choose_trampoline_page(ranges), Some(0x3000));
        // A partial page never counts.
        assert_eq!(
            choose_trampoline_page(core::iter::once(0x2001..0x3FFF)),
            None
        );
    }

    #[test]
    #[allow(
        clippy::reversed_empty_ranges,
        reason = "a wrapped range is the untrusted input under test"
    )]
    fn trampoline_page_none_below_1mib() {
        let map = [
            (0x0, 0x1000, USABLE),
            (0x1000, 0x9F000, BOOTLOADER_RECLAIMABLE),
            (0xF_F800, 0x10_0000, USABLE),
            (0x10_0000, 0x80_0000, USABLE),
        ];
        assert_eq!(choose_trampoline_page(usable(&map)), None);
        // A range straddling 1 MiB offers the pages wholly below it.
        assert_eq!(
            choose_trampoline_page(core::iter::once(0xFF000..0x20_0000)),
            Some(0xFF000)
        );
        // Untrusted input: wrapped, empty and top-of-space ranges.
        assert_eq!(
            choose_trampoline_page([0x9000..0x1000, 0x5000..0x5000, u64::MAX - 5..u64::MAX]),
            None
        );
    }

    /// Clip `r` against `excl` into a vector.
    fn clipped(r: core::ops::Range<u64>, excl: &[core::ops::Range<u64>]) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        clip_usable(r, || excl.iter().cloned(), |p| out.push((p.start, p.end)));
        out
    }

    #[test]
    fn clip_usable_eight_framebuffers() {
        const M: u64 = 0x10_0000;
        // Frame 0, the trampoline page, the kernel, and eight framebuffers:
        // eleven exclusions, more than the old 8-entry list held.
        let mut excl = vec![0..PAGE_SIZE, 0x52000..0x53000, 2 * M..4 * M];
        for i in 0..8u64 {
            let base = 16 * M + i * 4 * M;
            excl.push(base..base + 3 * M + 100);
        }
        let out = clipped(0..64 * M, &excl);
        let mut want = vec![(PAGE_SIZE, 0x52000), (0x53000, 2 * M), (4 * M, 16 * M)];
        for i in 0..8u64 {
            let end = 16 * M + i * 4 * M + 3 * M + 100;
            let next = if i == 7 {
                64 * M
            } else {
                16 * M + (i + 1) * 4 * M
            };
            want.push((end.next_multiple_of(PAGE_SIZE), next));
        }
        want.retain(|&(a, b)| a < b);
        assert_eq!(out, want);
        let kept: u64 = out.iter().map(|&(a, b)| b - a).sum();
        for (a, b) in &out {
            assert!(a % PAGE_SIZE == 0 && b % PAGE_SIZE == 0);
            assert!(excl.iter().all(|e| *b <= e.start || *a >= e.end));
        }
        assert!(kept < 64 * M);
    }

    #[test]
    #[allow(
        clippy::reversed_empty_ranges,
        reason = "a wrapped exclusion is an input under test"
    )]
    fn clip_usable_unsorted_overlapping() {
        const P: u64 = PAGE_SIZE;
        // Unsorted, overlapping, nested, empty, wrapped, and unaligned
        // exclusions, one reaching the top of the address space.
        let excl = [
            30 * P..35 * P,
            5 * P + 1..6 * P - 1,
            10 * P..20 * P,
            12 * P..25 * P,
            14 * P..15 * P,
            40 * P..40 * P,
            50 * P..45 * P,
            u64::MAX - 3..u64::MAX,
        ];
        assert_eq!(
            clipped(0..64 * P, &excl),
            vec![
                (0, 5 * P),
                (6 * P, 10 * P),
                (25 * P, 30 * P),
                (35 * P, 64 * P)
            ]
        );
        // Wholly excluded, and a range above every exclusion.
        assert_eq!(clipped(11 * P..24 * P, &excl), vec![]);
        assert_eq!(clipped(100 * P..101 * P, &excl), vec![(100 * P, 101 * P)]);
        // An exclusion running past the range's end, and one to the top.
        assert_eq!(
            clipped(60 * P..70 * P, core::slice::from_ref(&(65 * P..u64::MAX))),
            vec![(60 * P, 65 * P)]
        );
        // No exclusions at all.
        assert_eq!(clipped(3 * P..4 * P, &[]), vec![(3 * P, 4 * P)]);
    }

    /// F103: above 2^63 `next_power_of_two` overflowed; the size is now
    /// refused before it rounds.
    #[test]
    fn order_for_u64_max_is_none() {
        assert_eq!(Buddy::order_for(u64::MAX, 0), None);
        assert_eq!(Buddy::order_for(u64::MAX, PAGE_SIZE), None);
    }

    #[test]
    fn order_for_above_2_pow_63_is_none() {
        assert_eq!(Buddy::order_for((1 << 63) + 1, 0), None);
        assert_eq!(Buddy::order_for((1 << 63) + 1, PAGE_SIZE), None);
    }

    #[test]
    fn order_for_largest_block_edge() {
        let largest = PAGE_SIZE << MAX_ORDER;
        assert_eq!(Buddy::order_for(largest, 0), Some(MAX_ORDER as u8));
        assert_eq!(Buddy::order_for(largest + 1, 0), None);
    }
}

// ------------------ Kani proofs (ROADMAP §10.8) ------------------

#[cfg(kani)]
mod kani_proofs;

/// Host tests of the top-order runs (ROADMAP §10.2).
#[cfg(test)]
mod run_tests;
