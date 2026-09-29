//! Kernel heap: address-ordered free-list, portable half. DESIGN §4.4.
//!
//! Lives at `HEAP_START` (DESIGN §4.1). The library half only manages a
//! contiguous VA window: `init` with some prefix already mapped, `extend`
//! when the kernel maps more pages, `alloc`/`dealloc`/`realloc` over that
//! window. Backing frames, PTE flags, and the `GlobalAlloc` wrapper live
//! in the binary crate.
//!
//! Layout of a block:
//!
//! ```text
//! [size: usize][next: *mut  | padding][back: usize][payload...]
//!  ^ block_start                           ^ user pointer
//! ```
//!
//! `size` is the whole block. `next` is only meaningful while free.
//! `back` (immediately before the user pointer) is the byte offset from
//! `user` back to `block_start`, so free can recover the header at any
//! alignment up to a page.
//!
//! Free list is sorted by address so adjacent frees coalesce. First-fit.

use core::alloc::Layout;
use core::ptr::{self, NonNull};

/// DESIGN §4.1: 64 MiB kernel heap window.
pub const HEAP_START: u64 = 0xFFFF_C000_0000_0000;
pub const HEAP_SIZE: u64 = 64 * 1024 * 1024;
pub const HEAP_END: u64 = HEAP_START + HEAP_SIZE;
/// Initial mapped size. Grows in page increments from here.
pub const HEAP_INITIAL: u64 = 1024 * 1024;

pub const PAGE_SIZE: usize = 4096;

const HEADER: usize = core::mem::size_of::<FreeBlock>();
const BACKPTR: usize = core::mem::size_of::<usize>();
/// Smallest leftover we will split off. Must fit a free header.
const MIN_SPLIT: usize = HEADER + BACKPTR + 16;

#[repr(C)]
struct FreeBlock {
    size: usize,
    next: Option<NonNull<FreeBlock>>,
}

/// Snapshot for `meminfo`. `capacity` is the currently mapped window,
/// not the 64 MiB region cap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeapStats {
    pub used: usize,
    pub capacity: usize,
    pub cap_max: usize,
    pub free: usize,
}

pub struct Heap {
    base: usize,
    mapped: usize,
    cap: usize,
    free_head: Option<NonNull<FreeBlock>>,
    free_bytes: usize,
}

impl Heap {
    pub const fn empty() -> Self {
        Self {
            base: 0,
            mapped: 0,
            cap: 0,
            free_head: None,
            free_bytes: 0,
        }
    }

    /// Take ownership of `[base, base+mapped)` as one free block.
    /// `mapped` must be at least `MIN_SPLIT` and 16-byte aligned; `cap`
    /// is the hard ceiling `extend` will not cross.
    ///
    /// # Safety
    /// `[base, base+mapped)` must be mapped writable, unused, and not
    /// overlap any live allocation, and from here on only this heap writes
    /// its free blocks (invariant I225). `base` 16-aligned.
    pub unsafe fn init(&mut self, base: usize, mapped: usize, cap: usize) {
        assert!(base.is_multiple_of(core::mem::align_of::<FreeBlock>()));
        assert!(mapped >= MIN_SPLIT);
        assert!(mapped <= cap);
        self.base = base;
        self.mapped = mapped;
        self.cap = cap;
        self.free_head = None;
        self.free_bytes = 0;
        // SAFETY: `insert_free`'s contract; `[base, base + mapped)` is
        // mapped, writable and unused by this fn's `# Safety` contract
        // (invariant I225, established here).
        unsafe { self.insert_free(base as *mut u8, mapped) };
    }

    pub fn mapped(&self) -> usize {
        self.mapped
    }
    pub fn cap(&self) -> usize {
        self.cap
    }
    pub fn base(&self) -> usize {
        self.base
    }

    pub fn stats(&self) -> HeapStats {
        HeapStats {
            used: self.mapped.saturating_sub(self.free_bytes),
            capacity: self.mapped,
            cap_max: self.cap,
            free: self.free_bytes,
        }
    }

    /// Worst-case block bytes for `layout`, used by the kernel to decide
    /// how many pages to grow. Inclusive of header, back-pointer, and
    /// worst-case alignment padding.
    pub fn block_bytes(layout: Layout) -> usize {
        if layout.size() == 0 {
            return 0;
        }
        let align = layout.align();
        align_up(HEADER + BACKPTR + layout.size() + align, HEADER_ALIGN)
    }

    /// Grow the managed window to `new_mapped`. Caller has already mapped
    /// `[base+old, base+new)`. The new span is inserted as a free block
    /// (and coalesced if the previous tail was free).
    ///
    /// # Safety
    /// The new span must be mapped writable and unused (invariant I225).
    pub unsafe fn extend(&mut self, new_mapped: usize) {
        assert!(new_mapped > self.mapped);
        assert!(new_mapped <= self.cap);
        let add = new_mapped - self.mapped;
        let start = self.base + self.mapped;
        self.mapped = new_mapped;
        // SAFETY: `insert_free`'s contract; the new span is mapped,
        // writable and unused by this fn's `# Safety` contract (invariant
        // I225, established here).
        unsafe { self.insert_free(start as *mut u8, add) };
    }

    /// # Safety
    /// Caller must treat the returned pointer as `layout`-sized until `dealloc`.
    pub unsafe fn alloc(&mut self, layout: Layout) -> *mut u8 {
        if layout.size() == 0 {
            return dangling(layout.align());
        }
        let need = Self::block_bytes(layout);
        let mut prev: Option<NonNull<FreeBlock>> = None;
        let mut cur = self.free_head;
        while let Some(node) = cur {
            let block = node.as_ptr();
            // SAFETY: `block` is a node on the free list, in mapped memory
            // only the heap writes (invariant I225, established at `mm::heap::Heap::init`).
            let size = unsafe { (*block).size };
            if size >= need {
                // SAFETY: as above, `block` is a free-list node (invariant I225, established at `mm::heap::Heap::init`).
                let next = unsafe { (*block).next };
                // SAFETY: `unlink`'s contract; `prev` is `block`'s
                // predecessor on the free list, or `None` at the head
                // (invariant I225, established at `mm::heap::Heap::init`).
                unsafe { unlink(&mut self.free_head, prev, next) };
                self.free_bytes -= size;
                // SAFETY: `carve`'s contract; `block` was just unlinked and
                // is `size` bytes the heap owns (invariant I225, established at `mm::heap::Heap::init`).
                return unsafe { self.carve(block as *mut u8, size, layout) };
            }
            prev = cur;
            // SAFETY: as above, `block` is a free-list node (invariant I225, established at `mm::heap::Heap::init`).
            cur = unsafe { (*block).next };
        }
        ptr::null_mut()
    }

    /// # Safety
    /// `ptr` came from `alloc` with the same `layout` and is not used after.
    pub unsafe fn dealloc(&mut self, ptr: *mut u8, layout: Layout) {
        if layout.size() == 0 || ptr.is_null() {
            return;
        }
        // SAFETY: `recover`'s contract; `ptr` is a live allocation from
        // this heap by this fn's `# Safety` contract, established here.
        let (start, size) = unsafe { recover(ptr) };
        // SAFETY: `insert_free`'s contract; the caller gives the block back
        // and never uses it again (this fn's `# Safety` contract,
        // established here), so it is unused heap memory (invariant I225).
        unsafe { self.insert_free(start, size) };
    }

    /// In-place if the existing block fits `new_size` at the same align;
    /// otherwise allocate / copy / free. Returns null on failure and
    /// leaves the original allocation live (GlobalAlloc contract).
    ///
    /// # Safety
    /// `ptr` came from `alloc` with `layout`; the returned pointer replaces it.
    /// `new_size`, rounded up to `layout.align()`, does not overflow `isize`
    /// (`GlobalAlloc::realloc`'s rule).
    pub unsafe fn realloc(&mut self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ptr.is_null() {
            // SAFETY: `layout.align()` is a power of two (it came from a
            // `Layout`), and this fn's `# Safety` contract bounds `new_size`
            // and has the caller treat the result as that layout,
            // established here.
            return unsafe {
                self.alloc(Layout::from_size_align_unchecked(new_size, layout.align()))
            };
        }
        if new_size == 0 {
            // SAFETY: `dealloc`'s contract; `ptr` came from `alloc` with
            // `layout` and the caller drops it for the result (this fn's
            // `# Safety` contract, established here).
            unsafe { self.dealloc(ptr, layout) };
            return dangling(layout.align());
        }
        // SAFETY: `layout.align()` is a power of two (it came from a
        // `Layout`), and this fn's `# Safety` contract bounds `new_size`,
        // established here.
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        // SAFETY: `recover`'s contract; `ptr` is a live allocation from this
        // heap by this fn's `# Safety` contract, established here.
        let (start, old_block) = unsafe { recover(ptr) };
        let user_off = ptr as usize - start as usize;
        let old_payload_cap = old_block - user_off;
        if new_size <= old_payload_cap {
            // Shrink: split the tail back onto the free list when it is
            // worth a free block of its own.
            let used = align_up(user_off + new_size, HEADER_ALIGN);
            if old_block - used >= MIN_SPLIT {
                // SAFETY: `start` is the live block `recover` found, and
                // its tail past `used` holds no payload byte, so it is
                // unused heap memory (invariant I225, established at
                // `mm::heap::Heap::init`).
                unsafe {
                    self.resize_block(start, used);
                    self.insert_free((start as usize + used) as *mut u8, old_block - used);
                }
            }
            return ptr;
        }
        // SAFETY: `alloc`'s contract; the new block replaces `ptr` as a
        // `new_layout` allocation, established here.
        let new_ptr = unsafe { self.alloc(new_layout) };
        if new_ptr.is_null() {
            return ptr::null_mut();
        }
        let copy = layout.size().min(new_size);
        // SAFETY: both blocks are live and distinct and hold at least
        // `copy` bytes, established here.
        unsafe { ptr::copy_nonoverlapping(ptr, new_ptr, copy) };
        // SAFETY: `dealloc`'s contract; `ptr` came from `alloc` with
        // `layout` and `new_ptr` replaces it, established here.
        unsafe { self.dealloc(ptr, layout) };
        new_ptr
    }

    /// # Safety
    /// `start` is a live block header this heap owns.
    unsafe fn resize_block(&mut self, start: *mut u8, new_size: usize) {
        // SAFETY: `start` is a live block header this heap owns, by this
        // fn's `# Safety` contract, established here.
        unsafe { (start as *mut usize).write(new_size) };
    }

    /// Place `layout` inside `[block, block+size)`, splitting the tail.
    ///
    /// # Safety
    /// `block` is an unlinked free block of `size` bytes this heap owns.
    unsafe fn carve(&mut self, block: *mut u8, size: usize, layout: Layout) -> *mut u8 {
        let user = align_up(block as usize + HEADER + BACKPTR, layout.align());
        assert!(
            user + layout.size() <= block as usize + size,
            "heap: carve past its block"
        );
        let back = user - BACKPTR;
        assert!(
            back >= block as usize + HEADER,
            "heap: carve backpointer in its header"
        );
        // SAFETY: the asserts above put `back` inside `block`, which this
        // fn's `# Safety` contract hands over (invariant I225, established
        // here).
        unsafe { (back as *mut usize).write(user - block as usize) };

        let used = align_up(user + layout.size() - block as usize, HEADER_ALIGN);
        if size - used >= MIN_SPLIT {
            // SAFETY: `block`'s header is the heap's (invariant I225,
            // established here).
            unsafe { (block as *mut usize).write(used) };
            // SAFETY: `insert_free`'s contract; the tail past `used` is
            // unused memory of the block this fn owns (invariant I225,
            // established here).
            unsafe { self.insert_free((block as usize + used) as *mut u8, size - used) };
        } else {
            // SAFETY: `block`'s header is the heap's (invariant I225,
            // established here).
            unsafe { (block as *mut usize).write(size) };
        }
        user as *mut u8
    }

    /// Insert `[ptr, ptr+size)` into the address-sorted free list,
    /// coalescing with immediate neighbours. Panics on overlap (double
    /// free or a wild pointer).
    ///
    /// # Safety
    /// `[ptr, ptr+size)` is unused heap memory this allocator owns.
    unsafe fn insert_free(&mut self, ptr: *mut u8, size: usize) {
        assert!(size >= HEADER);
        let start = ptr as usize;
        let end = start + size;

        let mut prev: Option<NonNull<FreeBlock>> = None;
        let mut cur = self.free_head;
        while let Some(node) = cur {
            let nstart = node.as_ptr() as usize;
            // SAFETY: `node` is a free-list node (invariant I225, established at `mm::heap::Heap::init`).
            let nsize = unsafe { (*node.as_ptr()).size };
            let nend = nstart + nsize;
            assert!(
                end <= nstart || start >= nend,
                "heap: overlapping free {start:#x}..{end:#x} with {nstart:#x}..{nend:#x}"
            );
            if nstart >= start {
                break;
            }
            prev = cur;
            // SAFETY: `node` is a free-list node (invariant I225, established at `mm::heap::Heap::init`).
            cur = unsafe { (*node.as_ptr()).next };
        }

        let mut block_start = start;
        let mut block_size = size;
        let mut reuse_prev = false;
        if let Some(p) = prev {
            let pstart = p.as_ptr() as usize;
            // SAFETY: `p` is a free-list node (invariant I225, established at `mm::heap::Heap::init`).
            let psize = unsafe { (*p.as_ptr()).size };
            if pstart + psize == start {
                block_start = pstart;
                block_size += psize;
                self.free_bytes -= psize;
                reuse_prev = true;
            }
        }
        if let Some(n) = cur {
            let nstart = n.as_ptr() as usize;
            if block_start + block_size == nstart {
                // SAFETY: `n` is a free-list node (invariant I225, established at `mm::heap::Heap::init`).
                let nsize = unsafe { (*n.as_ptr()).size };
                block_size += nsize;
                self.free_bytes -= nsize;
                // SAFETY: `n` is a free-list node (invariant I225, established at `mm::heap::Heap::init`).
                let nnext = unsafe { (*n.as_ptr()).next };
                // SAFETY: `unlink`'s contract; `prev` is `n`'s predecessor
                // on the free list, or `None` at the head (invariant I225, established at `mm::heap::Heap::init`).
                unsafe { unlink(&mut self.free_head, prev, nnext) };
                cur = nnext;
            }
        }

        if reuse_prev && let Some(p) = prev {
            // SAFETY: `p` is a free-list node (invariant I225, established at `mm::heap::Heap::init`).
            unsafe {
                (*p.as_ptr()).size = block_size;
                (*p.as_ptr()).next = cur;
            }
            self.free_bytes += block_size;
            return;
        }

        let node = block_start as *mut FreeBlock;
        // SAFETY: `[block_start, block_start + block_size)` is unused heap
        // memory this fn's `# Safety` contract hands over, merged with the
        // free neighbour the loop unlinked (invariant I225, established
        // here).
        unsafe {
            (*node).size = block_size;
            (*node).next = cur;
        }
        // SAFETY: `node` is `ptr` or a later address in the heap window,
        // never null, established here.
        let nn = unsafe { NonNull::new_unchecked(node) };
        if let Some(p) = prev {
            // SAFETY: `p` is a free-list node (invariant I225, established at `mm::heap::Heap::init`).
            unsafe { (*p.as_ptr()).next = Some(nn) };
        } else {
            self.free_head = Some(nn);
        }
        self.free_bytes += block_size;
    }
}

const HEADER_ALIGN: usize = 16;

fn dangling(align: usize) -> *mut u8 {
    align as *mut u8
}

/// # Safety
/// `user` is a live allocation from this heap.
unsafe fn recover(user: *mut u8) -> (*mut u8, usize) {
    let back = (user as usize - BACKPTR) as *const usize;
    // SAFETY: `user` is a live allocation by this fn's `# Safety` contract,
    // so `carve` wrote its back-pointer just below it, established here.
    let off = unsafe { back.read() };
    assert!(off >= HEADER + BACKPTR, "heap: bad back-pointer");
    let start = (user as usize - off) as *mut u8;
    // SAFETY: `start` is that live block's header, established here.
    let size = unsafe { (start as *const usize).read() };
    assert!(size >= off, "heap: truncated block");
    (start, size)
}

/// Unlink the node after `prev` (or the head, for `None`) by pointing
/// `prev` at `next`.
///
/// # Safety
/// `prev`, when `Some`, is a node on the free list `head` starts, in heap
/// memory only this heap writes (invariant I225).
unsafe fn unlink(
    head: &mut Option<NonNull<FreeBlock>>,
    prev: Option<NonNull<FreeBlock>>,
    next: Option<NonNull<FreeBlock>>,
) {
    if let Some(p) = prev {
        // SAFETY: `p` is a free-list node by this fn's `# Safety` contract
        // (invariant I225, established here).
        unsafe { (*p.as_ptr()).next = next };
    } else {
        *head = next;
    }
}

#[inline]
const fn align_up(x: usize, a: usize) -> usize {
    (x + a - 1) & !(a - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;
    use std::vec::Vec;

    struct Pool {
        _mem: Vec<u8>,
        heap: Heap,
    }

    impl Pool {
        fn new(mapped: usize, cap: usize) -> Self {
            let mem = vec![0u8; cap + HEADER_ALIGN];
            let base = align_up(mem.as_ptr() as usize, HEADER_ALIGN);
            let mut heap = Heap::empty();
            // SAFETY: `init`'s contract; `mem` is `cap` bytes past `base`,
            // 16-aligned host memory this pool owns (invariant I225,
            // established here).
            unsafe { heap.init(base, mapped, cap) };
            Self { _mem: mem, heap }
        }
    }

    fn layout(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).unwrap()
    }

    /// Holds in release builds too (DESIGN §9.4): `make check` runs it with
    /// debug assertions off. A 64-byte layout in a 32-byte block, inside a
    /// buffer large enough that the unchecked code writes nothing outside it.
    #[test]
    #[should_panic(expected = "heap: carve past its block")]
    fn release_assert_carve_bounds() {
        let mut mem = vec![0u8; 256];
        let block = align_up(mem.as_mut_ptr() as usize, HEADER_ALIGN) as *mut u8;
        let mut heap = Heap::empty();
        // SAFETY: breaks `carve`'s contract on purpose; its `assert!` fires
        // before any write, and `mem` is large enough either way,
        // established here.
        unsafe { heap.carve(block, 32, layout(64, 8)) };
    }

    #[test]
    fn alloc_free_restores_free_bytes() {
        let mut p = Pool::new(64 * 1024, 64 * 1024);
        let s0 = p.heap.stats();
        assert_eq!(s0.used, 0);
        assert_eq!(s0.capacity, 64 * 1024);
        let l = layout(128, 8);
        // SAFETY: `Heap::alloc`'s contract; the test treats the block as its layout until it
        // frees it, established here.
        let a = unsafe { p.heap.alloc(l) };
        assert!(!a.is_null());
        assert!(p.heap.stats().used > 0);
        // SAFETY: `Heap::dealloc`'s contract; each pointer came from `alloc` with this layout
        // and is not used after, established here.
        unsafe { p.heap.dealloc(a, l) };
        assert_eq!(p.heap.stats(), s0);
    }

    #[test]
    fn alignment_1_through_4096() {
        let mut p = Pool::new(256 * 1024, 256 * 1024);
        let mut live = Vec::new();
        let mut align = 1usize;
        while align <= 4096 {
            let l = layout(align, align);
            // SAFETY: `Heap::alloc`'s contract; the test treats the block as its layout until
            // it frees it, established here.
            let a = unsafe { p.heap.alloc(l) };
            assert!(!a.is_null(), "align {align}");
            assert_eq!(a as usize % align, 0, "align {align}");
            // SAFETY: `a` is a live allocation of `align >= 1` bytes,
            // established here.
            unsafe { a.write(0x5A) };
            live.push((a, l));
            align *= 2;
        }
        for (a, l) in live {
            // SAFETY: `Heap::dealloc`'s contract; each pointer came from `alloc` with this
            // layout and is not used after, established here.
            unsafe { p.heap.dealloc(a, l) };
        }
        assert_eq!(p.heap.stats().used, 0);
    }

    #[test]
    fn sandwich_hole_is_reused() {
        let mut p = Pool::new(64 * 1024, 64 * 1024);
        let small = layout(16, 8);
        let l = layout(64, 8);
        // SAFETY: `Heap::alloc`'s contract; the test treats the block as its layout until it
        // frees it, established here.
        let pad = unsafe { p.heap.alloc(small) };
        // SAFETY: `Heap::alloc`'s contract; the test treats the block as its layout until it
        // frees it, established here.
        let a = unsafe { p.heap.alloc(l) };
        // SAFETY: `Heap::alloc`'s contract; the test treats the block as its layout until it
        // frees it, established here.
        let keep = unsafe { p.heap.alloc(l) };
        assert!(!pad.is_null() && !a.is_null() && !keep.is_null());
        // SAFETY: `Heap::dealloc`'s contract; each pointer came from `alloc` with this layout
        // and is not used after, established here.
        unsafe { p.heap.dealloc(a, l) };
        // SAFETY: `Heap::alloc`'s contract; the test treats the block as its layout until it
        // frees it, established here.
        let b = unsafe { p.heap.alloc(l) };
        assert_eq!(a, b, "hole between live neighbours must be first-fit");
        // SAFETY: `Heap::dealloc`'s contract; each pointer came from `alloc` with this layout
        // and is not used after, established here.
        unsafe {
            p.heap.dealloc(b, l);
            p.heap.dealloc(keep, l);
            p.heap.dealloc(pad, small);
        }
    }

    #[test]
    fn alloc_free_realloc_reuses() {
        let mut p = Pool::new(64 * 1024, 64 * 1024);
        let l = layout(64, 8);
        // SAFETY: `Heap::alloc`'s contract; the test treats the block as its layout until it
        // frees it, established here.
        let a = unsafe { p.heap.alloc(l) };
        assert!(!a.is_null());
        // SAFETY: `Heap::dealloc`'s contract; each pointer came from `alloc` with this layout
        // and is not used after, established here.
        unsafe { p.heap.dealloc(a, l) };
        // SAFETY: `Heap::alloc`'s contract; the test treats the block as its layout until it
        // frees it, established here.
        let b = unsafe { p.heap.alloc(l) };
        assert_eq!(a, b, "first-fit must reuse the just-freed block");

        // realloc growing in place when the block has room (we asked
        // for 64 in a much larger initial heap, so the leftover split
        // may or may not leave slack; realloc to a smaller size first
        // then grow back).
        // SAFETY: `Heap::realloc`'s contract; `b` came from `alloc` with
        // `l`, and the result replaces it, established here.
        let c = unsafe { p.heap.realloc(b, l, 32) };
        assert_eq!(c, b);
        // SAFETY: `Heap::realloc`'s contract; the pointer came from `alloc` or the last
        // `realloc` with this layout, and the result replaces it, established here.
        let d = unsafe { p.heap.realloc(c, layout(32, 8), 48) };
        assert!(!d.is_null());
        // SAFETY: `Heap::dealloc`'s contract; each pointer came from `alloc` with this layout
        // and is not used after, established here.
        unsafe { p.heap.dealloc(d, layout(48, 8)) };
    }

    #[test]
    fn oom_returns_null_without_corruption() {
        let mut p = Pool::new(4096, 4096);
        let huge = layout(1024 * 1024, 8);
        // SAFETY: `Heap::alloc`'s contract; the test treats the block as its layout until it
        // frees it, established here.
        let n = unsafe { p.heap.alloc(huge) };
        assert!(n.is_null());
        // Still able to hand out a small block.
        let l = layout(16, 8);
        // SAFETY: `Heap::alloc`'s contract; the test treats the block as its layout until it
        // frees it, established here.
        let a = unsafe { p.heap.alloc(l) };
        assert!(!a.is_null());
        // SAFETY: `Heap::dealloc`'s contract; each pointer came from `alloc` with this layout
        // and is not used after, established here.
        unsafe { p.heap.dealloc(a, l) };
        assert_eq!(p.heap.stats().used, 0);
    }

    #[test]
    fn extend_makes_room() {
        let mut p = Pool::new(4096, 16 * 4096);
        let big = layout(6000, 8);
        // SAFETY: `Heap::alloc`'s contract; the test treats the block as its layout until it
        // frees it, established here.
        assert!(unsafe { p.heap.alloc(big) }.is_null());
        // SAFETY: `Heap::extend`'s contract; `Pool::new` backs all of `cap` with host memory
        // the heap owns (invariant I225, established here).
        unsafe { p.heap.extend(3 * 4096) };
        // SAFETY: `Heap::alloc`'s contract; the test treats the block as its layout until it
        // frees it, established here.
        let a = unsafe { p.heap.alloc(big) };
        assert!(!a.is_null());
        assert!(p.heap.mapped() >= 3 * 4096);
        // SAFETY: `Heap::dealloc`'s contract; each pointer came from `alloc` with this layout
        // and is not used after, established here.
        unsafe { p.heap.dealloc(a, big) };
    }

    #[test]
    fn coalesce_after_freeing_neighbours() {
        let mut p = Pool::new(32 * 1024, 32 * 1024);
        let l = layout(128, 8);
        // SAFETY: `Heap::alloc`'s contract; the test treats the block as its layout until it
        // frees it, established here.
        let a = unsafe { p.heap.alloc(l) };
        // SAFETY: `Heap::alloc`'s contract; the test treats the block as its layout until it
        // frees it, established here.
        let b = unsafe { p.heap.alloc(l) };
        // SAFETY: `Heap::alloc`'s contract; the test treats the block as its layout until it
        // frees it, established here.
        let c = unsafe { p.heap.alloc(l) };
        // SAFETY: `Heap::dealloc`'s contract; each pointer came from `alloc` with this layout
        // and is not used after, established here.
        unsafe { p.heap.dealloc(a, l) };
        // SAFETY: `Heap::dealloc`'s contract; each pointer came from `alloc` with this layout
        // and is not used after, established here.
        unsafe { p.heap.dealloc(c, l) };
        // SAFETY: `Heap::dealloc`'s contract; each pointer came from `alloc` with this layout
        // and is not used after, established here.
        unsafe { p.heap.dealloc(b, l) };
        // One coalesced free region: a request almost the size of the
        // whole heap must succeed.
        let big = layout(16 * 1024, 8);
        // SAFETY: `Heap::alloc`'s contract; the test treats the block as its layout until it
        // frees it, established here.
        let d = unsafe { p.heap.alloc(big) };
        assert!(!d.is_null(), "coalesce failed; 16KiB alloc missed");
        // SAFETY: `Heap::dealloc`'s contract; each pointer came from `alloc` with this layout
        // and is not used after, established here.
        unsafe { p.heap.dealloc(d, big) };
    }

    #[test]
    fn zst_is_non_null_and_noop_free() {
        let mut p = Pool::new(4096, 4096);
        let l = layout(0, 8);
        // SAFETY: `Heap::alloc`'s contract; the test treats the block as its layout until it
        // frees it, established here.
        let a = unsafe { p.heap.alloc(l) };
        assert!(!a.is_null());
        assert_eq!(p.heap.stats().used, 0);
        // SAFETY: `Heap::dealloc`'s contract; each pointer came from `alloc` with this layout
        // and is not used after, established here.
        unsafe { p.heap.dealloc(a, l) };
    }

    #[test]
    fn double_free_panics() {
        let mut p = Pool::new(4096, 4096);
        let l = layout(32, 8);
        // SAFETY: `Heap::alloc`'s contract; the test treats the block as its layout until it
        // frees it, established here.
        let a = unsafe { p.heap.alloc(l) };
        // SAFETY: `dealloc`'s contract; `a` came from `alloc` with `l`,
        // established here.
        unsafe { p.heap.dealloc(a, l) };
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: frees `a` twice on purpose; `insert_free` finds the
            // overlap and panics before it writes, established here.
            unsafe { p.heap.dealloc(a, l) };
        }));
        assert!(res.is_err(), "double free must panic");
    }

    #[test]
    fn block_bytes_covers_page_align() {
        let l = layout(1, 4096);
        assert!(Heap::block_bytes(l) >= 4096 + HEADER);
    }
}
