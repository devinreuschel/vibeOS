//! Physical memory manager: buddy allocator, portable half.
//!
//! DESIGN §4.2. Free blocks of order *k* cover `2^k` contiguous 4 KiB
//! frames. Split on allocation, merge with the buddy on free. Free-list
//! nodes live inside the free pages themselves (intrusive), so the
//! allocator needs no auxiliary bitmap and no allocation to bootstrap.
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
//! In the kernel this is `paging_init::HHDM_BASE`. In host tests it is
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

pub const PAGE_BITS: u32 = 12;
pub const PAGE_SIZE: u64 = 1 << PAGE_BITS;

/// Maximum block order. Order *k* covers `2^k` frames, so `MAX_ORDER = 10`
/// covers 4 MiB blocks — enough for the phase-1 exit gate and slightly
/// bigger than a 2 MiB page-table mapping, so the paging code can pull
/// whole 2 MiB regions from a single allocation.
pub const MAX_ORDER: usize = 10;

pub type PhysAddr = u64;

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

pub struct Buddy {
    heads: [u64; MAX_ORDER + 1],
    counts: [usize; MAX_ORDER + 1],
    total_frames: usize,
    free_frames: usize,
    hhdm_offset: u64,
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
        }
    }

    pub fn hhdm_offset(&self) -> u64 {
        self.hhdm_offset
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

    /// Contribute a [start, end) physical range to the free lists.
    /// Splits into maximally-aligned, maximally-sized buddy blocks.
    /// Any bytes below the first page, or beyond the last page, are
    /// ignored. Frame 0 is skipped unconditionally.
    ///
    /// # Safety
    /// Caller vouches that the entire range is real, writable memory
    /// reachable through `hhdm_offset`, and that no piece of it is
    /// currently in the free lists.
    pub unsafe fn insert_region(&mut self, start: PhysAddr, end: PhysAddr) {
        let start = start.max(PAGE_SIZE);
        let mut a = align_up(start, PAGE_SIZE);
        let b = align_down(end, PAGE_SIZE);
        while a < b {
            let remaining_pages = (b - a) / PAGE_SIZE;
            // Largest order whose block fits: bounded by alignment of `a`
            // and by the pages left in the range.
            let max_by_align = (a.trailing_zeros() - PAGE_BITS) as usize;
            let max_by_len = remaining_pages.ilog2() as usize;
            let k = max_by_align.min(max_by_len).min(MAX_ORDER);
            self.total_frames += 1 << k;
            unsafe { self.push_free(a, k as u8) };
            a += PAGE_SIZE << k;
        }
    }

    /// Allocate a block of `order`. Splits down from a larger order if
    /// none is available at `order` directly. Returns `None` on
    /// exhaustion.
    pub fn allocate(&mut self, order: u8) -> Option<PhysAddr> {
        let target = order as usize;
        if target > MAX_ORDER {
            return None;
        }
        let mut k = target;
        while k <= MAX_ORDER && self.heads[k] == NULL {
            k += 1;
        }
        if k > MAX_ORDER {
            return None;
        }
        let addr = unsafe { self.pop_head(k as u8) };
        // Split down, pushing the right half at each intermediate order.
        while k > target {
            k -= 1;
            let buddy = addr + (PAGE_SIZE << k);
            unsafe { self.push_free(buddy, k as u8) };
        }
        Some(addr)
    }

    /// Free a previously-allocated block at `order`. Coalesces with its
    /// buddy up to `MAX_ORDER`.
    ///
    /// # Safety
    /// Caller must have previously received `phys` from `allocate(order)`
    /// and not yet freed it. Panics on double-free at the same order or on
    /// misalignment.
    pub unsafe fn deallocate(&mut self, mut phys: PhysAddr, mut order: u8) {
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
            !unsafe { self.covered_by_free_block(phys, order) },
            "pmm: double free at {phys:#x} order {order}"
        );

        while (order as usize) < MAX_ORDER {
            let buddy = phys ^ (PAGE_SIZE << order);
            if !unsafe { self.in_free_list(buddy, order) } {
                break;
            }
            unsafe { self.unlink(buddy, order) };
            phys = phys.min(buddy);
            order += 1;
        }
        unsafe { self.push_free(phys, order) };
    }

    /// Convenience: order-0 allocation.
    pub fn allocate_frame(&mut self) -> Option<PhysAddr> {
        self.allocate(0)
    }

    /// Convenience: order-0 free.
    ///
    /// # Safety
    /// Same contract as [`Buddy::deallocate`].
    pub unsafe fn deallocate_frame(&mut self, phys: PhysAddr) {
        unsafe { self.deallocate(phys, 0) }
    }

    /// Order whose block covers `bytes` and is aligned to `align`.
    pub fn order_for(bytes: u64, align: u64) -> Option<u8> {
        if bytes == 0 {
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

    /// Allocate a block that does not straddle `boundary` (0 = none).
    pub fn allocate_constrained(
        &mut self,
        bytes: u64,
        align: u64,
        boundary: u64,
    ) -> Option<(PhysAddr, u8)> {
        let order = Self::order_for(bytes, align)?;
        if boundary != 0 && !boundary.is_power_of_two() {
            return None;
        }
        const MAX_TRY: usize = 16;
        let mut stash = [0u64; MAX_TRY];
        let mut n = 0usize;
        let mut found = None;
        while n < MAX_TRY {
            let Some(phys) = self.allocate(order) else {
                break;
            };
            let ok_align = align <= 1 || phys & (align - 1) == 0;
            if ok_align && !crosses_boundary(phys, bytes, boundary) {
                found = Some((phys, order));
                break;
            }
            stash[n] = phys;
            n += 1;
        }
        let mut i = 0usize;
        while i < n {
            unsafe { self.deallocate(stash[i], order) };
            i += 1;
        }
        found
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
    /// `phys` is an unused frame of `order` that this buddy owns.
    unsafe fn push_free(&mut self, phys: u64, order: u8) {
        let k = order as usize;
        let head = self.heads[k];
        let node = self.node_ptr(phys);
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
        debug_assert!(head != NULL, "pmm: pop_head on empty order {k}");
        let n = self.node_ptr(head);
        let next = unsafe { (*n).next };
        self.heads[k] = next;
        if next != NULL {
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
        let (prev, next) = unsafe { ((*node).prev, (*node).next) };
        if prev == NULL {
            self.heads[k] = next;
        } else {
            unsafe {
                (*self.node_ptr(prev)).next = next;
            }
        }
        if next != NULL {
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
            cur = unsafe { (*self.node_ptr(cur)).next };
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
        for k in (at_least_order as usize)..=MAX_ORDER {
            let block_start = phys & !((PAGE_SIZE << k) - 1);
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
            unsafe { buddy.insert_region(TEST_PHYS_BASE, phys_end) };
            Self {
                _mem: mem,
                phys_base: TEST_PHYS_BASE,
                phys_end,
                buddy,
            }
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

    #[test]
    fn exhaustion_returns_none() {
        let mut p = Pool::new(16);
        // 16 frames, order 0. Drain them one at a time.
        let mut taken = Vec::new();
        while let Some(a) = p.buddy.allocate(0) {
            taken.push(a);
        }
        assert_eq!(taken.len(), 16);
        assert!(p.buddy.allocate(0).is_none());
        assert!(p.buddy.allocate(3).is_none());
        // Now free everything and confirm we're back at 16 free.
        for a in taken {
            unsafe { p.buddy.deallocate(a, 0) };
        }
        assert_eq!(p.buddy.stats().free_frames, 16);
    }

    #[test]
    fn per_order_alignment() {
        let mut p = Pool::new(1024);
        for order in 0u8..=(MAX_ORDER as u8) {
            let block = p.buddy.allocate(order).expect("order fits in 1024 frames");
            let block_size = PAGE_SIZE << order;
            assert!(
                block & (block_size - 1) == 0,
                "order {order} allocation {block:#x} not aligned to {block_size:#x}"
            );
            assert!(p.contains(block));
            unsafe { p.buddy.deallocate(block, order) };
        }
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
        let a = p.buddy.allocate(3).unwrap();
        let s1 = p.buddy.stats();
        assert_eq!(s1.free_frames, 1024 - 8);

        unsafe { p.buddy.deallocate(a, 3) };
        let s2 = p.buddy.stats();
        assert_eq!(s2, s0, "state after alloc+free must equal initial");
    }

    #[test]
    fn coalescing_after_freeing_alternate_blocks() {
        // Start with one order-3 block worth of frames: 8 frames.
        let mut p = Pool::new(8);
        assert_eq!(p.buddy.stats().largest_free_order, Some(3));

        // Split all the way down: allocate all 8 as order-0.
        let mut frames = Vec::new();
        for _ in 0..8 {
            frames.push(p.buddy.allocate(0).unwrap());
        }
        assert_eq!(p.buddy.stats().free_frames, 0);
        assert_eq!(p.buddy.stats().largest_free_order, None);

        // Free every other one. Because buddies are paired by XOR of
        // (PAGE_SIZE << order), freeing frames [0,2,4,6] leaves each with
        // an allocated buddy, so nothing merges.
        frames.sort();
        for &f in &[frames[0], frames[2], frames[4], frames[6]] {
            unsafe { p.buddy.deallocate(f, 0) };
        }
        let mid = p.buddy.stats();
        assert_eq!(mid.free_frames, 4);
        assert_eq!(mid.largest_free_order, Some(0));

        // Now free the odd ones; each free should cascade all the way
        // back up to the single order-3 block we started with.
        for &f in &[frames[1], frames[3], frames[5], frames[7]] {
            unsafe { p.buddy.deallocate(f, 0) };
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
        let a = p.buddy.allocate(2).unwrap(); // splits 4 -> 3 -> 2
        let s = p.buddy.stats();
        assert_eq!(s.free_frames, 12);
        // We should now have one free block at each of orders 2 and 3.
        assert_eq!(s.largest_free_order, Some(3));
        unsafe { p.buddy.deallocate(a, 2) };
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

        let mut live: Vec<(PhysAddr, u8)> = Vec::new();
        for _ in 0..5000 {
            // Bias toward smaller orders so we exercise splits and merges
            // rather than immediately exhausting the pool.
            let coin = next() & 0xff;
            let free = coin < 0x80 && !live.is_empty();
            if free {
                let i = (next() as usize) % live.len();
                let (a, o) = live.swap_remove(i);
                unsafe { p.buddy.deallocate(a, o) };
            } else {
                let order = ((next() & 0x7) as u8).min(5);
                if let Some(a) = p.buddy.allocate(order) {
                    live.push((a, order));
                }
            }
        }
        // Drain remaining allocations.
        for (a, o) in live.drain(..) {
            unsafe { p.buddy.deallocate(a, o) };
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
        let a = p.buddy.allocate(0).unwrap();
        unsafe { p.buddy.deallocate(a, 0) };
        let res = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            unsafe { p.buddy.deallocate(a, 0) };
        }));
        assert!(res.is_err(), "double free must panic");
    }

    #[test]
    fn insert_region_skips_frame_zero() {
        // A region nominally starting at phys 0 must drop frame 0. Use a
        // fresh buddy configured so phys 0 maps to mem[0].
        let words = (32 * PAGE_SIZE as usize) / 8;
        let mem: Vec<u64> = vec![0u64; words];
        let ptr = mem.as_ptr() as u64;
        let mut buddy = Buddy::new(ptr);
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
        let mut p = Pool::new(64);
        let (phys, order) = p
            .buddy
            .allocate_constrained(0x1000, 0x2000, 1u64 << 32)
            .unwrap();
        assert_eq!(order, 1);
        assert_eq!(phys & 0x1FFF, 0);
        unsafe { p.buddy.deallocate(phys, order) };
        // 4K request with a 2K boundary always straddles: refuse.
        assert!(
            p.buddy
                .allocate_constrained(0x1000, PAGE_SIZE, 0x800)
                .is_none()
        );
        assert_eq!(p.buddy.stats().free_frames, 64);
    }
}
