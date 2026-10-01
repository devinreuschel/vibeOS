//! The fill API's portable half (ROADMAP §10.6): writes into an address
//! space that no thread runs, the ELF loader's segments, zero fill, TLS and
//! initial stack, and `fork`'s copy until §12.3's copy-on-write.
//!
//! Each driver walks its range in batches that [`batches`] plans: at most
//! [`FILL_BATCH`] pages, never across a 2 MiB boundary, so one batch's
//! translations live in one leaf table. A batch's frames are looked up with
//! the page-table lock held ([`PtHold::hold`]), and their contents written
//! through the physmap after it is dropped, so a large copy never holds off
//! another CPU's page-table work, or its own tick, for longer than one
//! leaf-table lookup (DESIGN §2.9 rule 2). The physmap primitives are this
//! file's private fns; `scripts/check_user_access.py` fails on a call to
//! one anywhere outside this module and its kernel half, `proc::fill_init`.

use crate::arch::PageTable;
use crate::kerror::KError;
use crate::paging::{PAGE_SIZE_4K, PageFlags, VirtAddr};
use crate::proc::addr_space::{AddressSpace, AsError, Backing, Region, UserPerms};
use crate::proc::uaccess::user_range_ok;

/// Pages one batch covers at most: a 512-byte frame array on the stack.
pub const FILL_BATCH: usize = 64;

/// One leaf table's span: a batch never crosses a multiple of it.
pub const LEAF_SPAN: u64 = 512 * PAGE_SIZE_4K;

/// Why a fill failed.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillError {
    /// The range is not inside the user half, or wraps.
    Range,
    /// A page of the range has no present user leaf.
    Unmapped,
    /// A frame or the heap ran out building the new space.
    NoMem,
}

/// A fill into a new space is `EFAULT` for a bad range and `ENOMEM` when
/// memory ran out, as `execve` and `fork` return them.
impl From<FillError> for KError {
    fn from(e: FillError) -> Self {
        match e {
            FillError::Range | FillError::Unmapped => Self::Fault,
            FillError::NoMem => Self::NoMem,
        }
    }
}

/// The space errors a fill meets: running out is `NoMem`, anything else a
/// bad range.
impl From<AsError> for FillError {
    fn from(e: AsError) -> Self {
        match e {
            AsError::OutOfFrames | AsError::NoRegionSlot | AsError::NoVaSpace => Self::NoMem,
            _ => Self::Range,
        }
    }
}

/// One batch: `pages` pages from the page-aligned `va`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Batch {
    pub va: u64,
    pub pages: usize,
}

/// The batches over the pages `[va, va+len)` touches, in order: each at
/// most [`FILL_BATCH`] pages, none across a [`LEAF_SPAN`] boundary. Empty
/// for an empty or wrapping range.
pub fn batches(va: u64, len: u64) -> Batches {
    let start = va & !(PAGE_SIZE_4K - 1);
    let end = va
        .checked_add(len)
        .filter(|_| len != 0)
        .and_then(|e| e.checked_add(PAGE_SIZE_4K - 1))
        .map_or(start, |e| e & !(PAGE_SIZE_4K - 1));
    Batches { cur: start, end }
}

/// The iterator [`batches`] returns.
#[derive(Clone, Debug)]
pub struct Batches {
    cur: u64,
    end: u64,
}

impl Iterator for Batches {
    type Item = Batch;

    fn next(&mut self) -> Option<Batch> {
        if self.cur >= self.end {
            return None;
        }
        let leaf_end = (self.cur & !(LEAF_SPAN - 1)).saturating_add(LEAF_SPAN);
        let cap = self.cur.saturating_add(FILL_BATCH as u64 * PAGE_SIZE_4K);
        let stop = leaf_end.min(cap).min(self.end);
        let b = Batch {
            va: self.cur,
            pages: ((stop - self.cur) / PAGE_SIZE_4K) as usize,
        };
        self.cur = stop;
        Some(b)
    }
}

/// The page-table lock, as the drivers take it. The kernel's takes `PT`;
/// a host test's counts.
pub trait PtHold {
    /// Run `f`, which looks up `batch`'s translations, with the lock held.
    fn hold<R>(&mut self, batch: Batch, f: impl FnOnce() -> R) -> R;

    /// `batch`'s contents are written, with the lock dropped: the boundary
    /// before the next hold.
    fn filled(&mut self, _batch: Batch) {}
}

/// The frames under `b`'s pages in `space`: `out[i]` is page `i`'s
/// physical address. Every page needs a present user leaf.
fn lookup<A: PageTable, C>(
    space: &AddressSpace<A, C>,
    b: Batch,
    out: &mut [u64; FILL_BATCH],
) -> Result<(), FillError> {
    let mut i = 0usize;
    while i < b.pages {
        let va = b.va + i as u64 * PAGE_SIZE_4K;
        match space.mapper().translate(VirtAddr(va)) {
            Some((pa, _, flags)) if flags.contains(PageFlags::USER) => out[i] = pa.as_u64(),
            _ => return Err(FillError::Unmapped),
        }
        i += 1;
    }
    Ok(())
}

/// The part of page `i` of batch `b` that `[va, end)` covers: its offset
/// in the page, and its length.
fn span(b: Batch, i: usize, va: u64, end: u64) -> (u64, u64) {
    let page = b.va + i as u64 * PAGE_SIZE_4K;
    let lo = page.max(va);
    let hi = (page + PAGE_SIZE_4K).min(end);
    (lo - page, hi.saturating_sub(lo))
}

/// Copy `src` to byte `off` of the frame at `pa`, through the physmap.
///
/// # Safety
/// `pa` is a frame of RAM reachable at `hhdm + pa`, `off + src.len()` is at
/// most a page, and nothing else accesses those bytes meanwhile.
unsafe fn physmap_write(hhdm: u64, pa: u64, off: u64, src: &[u8]) {
    let dst = hhdm.wrapping_add(pa).wrapping_add(off) as *mut u8;
    // SAFETY: `dst` is `src.len()` bytes inside one frame through the
    // physmap, which no kernel object overlaps (this fn's contract,
    // `proc::fill::physmap_write`).
    unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len()) };
}

/// Zero `len` bytes from byte `off` of the frame at `pa`, through the
/// physmap.
///
/// # Safety
/// As for [`physmap_write`], for `off + len`.
unsafe fn physmap_zero(hhdm: u64, pa: u64, off: u64, len: u64) {
    let dst = hhdm.wrapping_add(pa).wrapping_add(off) as *mut u8;
    // SAFETY: `dst` is `len` bytes inside one frame through the physmap
    // (this fn's contract, `proc::fill::physmap_zero`).
    unsafe { core::ptr::write_bytes(dst, 0, len as usize) };
}

/// Copy `len` bytes at byte `off` of frame `from` to the same bytes of
/// frame `to`, through the physmap.
///
/// # Safety
/// As for [`physmap_write`], for both frames, which differ.
unsafe fn physmap_copy(hhdm: u64, from: u64, to: u64, off: u64, len: u64) {
    let src = hhdm.wrapping_add(from).wrapping_add(off) as *const u8;
    let dst = hhdm.wrapping_add(to).wrapping_add(off) as *mut u8;
    // SAFETY: both are `len` bytes inside one frame each through the
    // physmap, and the frames differ, so they do not overlap (this fn's
    // contract, `proc::fill::physmap_copy`).
    unsafe { core::ptr::copy_nonoverlapping(src, dst, len as usize) };
}

/// The range check every driver makes first.
fn check(va: u64, len: u64) -> Result<u64, FillError> {
    if !user_range_ok(va, len) {
        return Err(FillError::Range);
    }
    Ok(va + len)
}

/// Write `src` at user address `va` of `space`, a space no thread runs.
/// Pages need not be writable: the loader fills read-only segments too.
pub fn write<A: PageTable, C, H: PtHold>(
    space: &AddressSpace<A, C>,
    hold: &mut H,
    va: u64,
    src: &[u8],
) -> Result<(), FillError> {
    let end = check(va, src.len() as u64)?;
    let hhdm = space.mapper().hhdm_offset();
    let mut pas = [0u64; FILL_BATCH];
    let mut done = 0usize;
    for b in batches(va, src.len() as u64) {
        hold.hold(b, || lookup(space, b, &mut pas))?;
        let mut i = 0usize;
        while i < b.pages {
            let (off, n) = span(b, i, va, end);
            let bytes = src.get(done..done + n as usize).ok_or(FillError::Range)?;
            // SAFETY: `pas[i]` is the user leaf `lookup` found for this page
            // of a space no thread runs, and `off + n` stays in the page
            // (`proc::fill::span`), as `physmap_write` requires; established
            // here.
            unsafe { physmap_write(hhdm, pas[i], off, bytes) };
            done += n as usize;
            i += 1;
        }
        hold.filled(b);
    }
    Ok(())
}

/// Zero `[va, va+len)` of `space`, a space no thread runs.
pub fn zero<A: PageTable, C, H: PtHold>(
    space: &AddressSpace<A, C>,
    hold: &mut H,
    va: u64,
    len: u64,
) -> Result<(), FillError> {
    let end = check(va, len)?;
    let hhdm = space.mapper().hhdm_offset();
    let mut pas = [0u64; FILL_BATCH];
    for b in batches(va, len) {
        hold.hold(b, || lookup(space, b, &mut pas))?;
        let mut i = 0usize;
        while i < b.pages {
            let (off, n) = span(b, i, va, end);
            // SAFETY: as in `write`: a user leaf of a space no thread runs,
            // and `off + n` stays in the page; established here.
            unsafe { physmap_zero(hhdm, pas[i], off, n) };
            i += 1;
        }
        hold.filled(b);
    }
    Ok(())
}

/// Copy `[va, va+len)` of `from` to the same range of `to`, a space no
/// thread runs. `from` is the parent's, which its own thread, the caller,
/// keeps still while it forks.
pub fn copy<A: PageTable, C, D, H: PtHold>(
    from: &AddressSpace<A, C>,
    to: &AddressSpace<A, D>,
    hold: &mut H,
    va: u64,
    len: u64,
) -> Result<(), FillError> {
    let end = check(va, len)?;
    let hhdm = to.mapper().hhdm_offset();
    let mut src = [0u64; FILL_BATCH];
    let mut dst = [0u64; FILL_BATCH];
    for b in batches(va, len) {
        hold.hold(b, || {
            lookup(from, b, &mut src)?;
            lookup(to, b, &mut dst)
        })?;
        let mut i = 0usize;
        while i < b.pages {
            let (off, n) = span(b, i, va, end);
            if src[i] == dst[i] {
                return Err(FillError::Range);
            }
            // SAFETY: both are user leaves `lookup` found, the destination
            // in a space no thread runs, they differ (checked just above),
            // and `off + n` stays in the page; established here.
            unsafe { physmap_copy(hhdm, src[i], dst[i], off, n) };
            i += 1;
        }
        hold.filled(b);
    }
    Ok(())
}

/// Give `to`, a new space, `from`'s layout for `fork`: its break and every
/// region, each holding a clone of `core`. A reservation is recorded as it
/// is; an anonymous region is mapped through `map(to, va, len, perms,
/// core)`, the caller's install, and its bytes are left for [`copy`]. On a
/// failure `to` keeps what was mapped, for its owner to tear down.
pub fn clone_layout<A: PageTable, C, D: Clone>(
    from: &AddressSpace<A, C>,
    to: &mut AddressSpace<A, D>,
    core: &D,
    mut map: impl FnMut(&mut AddressSpace<A, D>, u64, u64, UserPerms, D) -> Result<(), AsError>,
) -> Result<(), FillError> {
    to.set_brk_start(from.brk_start());
    to.set_brk(from.brk());
    for r in from.regions() {
        if r.len == 0 {
            continue;
        }
        if r.backing == Backing::Reserved {
            to.check_new_region(r.start, r.len)?;
            to.insert_region(Region {
                start: r.start,
                len: r.len,
                perms: r.perms,
                backing: r.backing,
                core: core.clone(),
            })?;
            continue;
        }
        map(to, r.start, r.len, r.perms, core.clone())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::arch::stub::Arch;
    use crate::paging::{FrameAlloc, Mapper, PhysAddr};
    use crate::pmm::Frames;
    use crate::pmm::testing::Pool;
    use crate::proc::addr_space::{FrameFree, region_table};
    use std::vec::Vec;

    fn kernel_mapper(pool: &mut Pool) -> Mapper<Arch> {
        let root = PhysAddr(pool.alloc_frame().unwrap().into_entry());
        // SAFETY: `root` is an owned frame fresh from `pool`, zeroed on the next line before
        // any walk, and `pool.hhdm()` maps every pool frame writable; established here.
        let mapper = unsafe { Mapper::new(root, pool.hhdm()) };
        // SAFETY: `root` is an owned pool frame reachable through `pool.hhdm()`; established
        // here.
        unsafe { mapper.zero_frame(root) };
        mapper
    }

    fn new_space(kernel: &Mapper<Arch>, pool: &mut Pool) -> AddressSpace<Arch> {
        let root = PhysAddr(pool.alloc_frame().unwrap().into_entry());
        // SAFETY: `kernel` is this test's kernel mapper, and `root` an owned pool frame only
        // this space uses; established here.
        unsafe { AddressSpace::new(kernel, root, &mut region_table().ok()) }.unwrap()
    }

    fn map(a: &mut AddressSpace<Arch>, pool: &mut Pool, va: u64, pages: u64) {
        // SAFETY: `map_anon` refuses a range outside the user half or over a region before it
        // maps anything, and `pool` hands out owned frames; established here.
        unsafe { a.map_anon(va, pages * PAGE_SIZE_4K, UserPerms::RW, (), pool) }.unwrap();
    }

    fn free(mut a: AddressSpace<Arch>, pool: &mut Pool) {
        // SAFETY: a host test loads no CR3 and `a` is not used again, as
        // `addr_space::AddressSpace::teardown_pool` requires; established here.
        unsafe { a.teardown_pool(pool) };
        // SAFETY: `new_space` took the root's order-0 token with `into_entry` and the space
        // is torn down, the contract `pmm::Frames::from_entry` states; established by
        // `fill::tests::new_space`.
        pool.free_frame(unsafe { Frames::from_entry(a.root().as_u64(), 0) });
    }

    /// Read `buf.len()` bytes at `va` of `a`, one page at a time, for the
    /// checks; not a fill primitive.
    fn peek(a: &AddressSpace<Arch>, va: u64, buf: &mut [u8]) {
        for (k, byte) in buf.iter_mut().enumerate() {
            let at = va + k as u64;
            let (pa, _, _) = a.mapper().translate(VirtAddr(at)).unwrap();
            let p = a.mapper().hhdm_offset().wrapping_add(pa.as_u64()) as *const u8;
            // SAFETY: `translate` found `at` mapped to `pa`, a pool frame reachable through
            // the pool's HHDM; established here.
            *byte = unsafe { p.read() };
        }
    }

    /// Counts holds and the pages each covered, and logs the events.
    #[derive(Default)]
    struct Fake {
        holds: Vec<Batch>,
        events: Vec<char>,
    }

    impl PtHold for Fake {
        fn hold<R>(&mut self, batch: Batch, f: impl FnOnce() -> R) -> R {
            self.holds.push(batch);
            self.events.push('H');
            f()
        }

        fn filled(&mut self, _batch: Batch) {
            self.events.push('F');
        }
    }

    #[test]
    fn batches_stay_in_one_leaf_table() {
        let cases = [
            (0x40_0000u64, 1u64),
            (0x40_0123, 3 * PAGE_SIZE_4K),
            (0x5f_f000, 2 * PAGE_SIZE_4K),
            (0x41_0000, 8 << 20),
            (0x7f_e800, LEAF_SPAN + 5),
        ];
        for (va, len) in cases {
            let bs: Vec<Batch> = batches(va, len).collect();
            let mut next = va & !(PAGE_SIZE_4K - 1);
            for b in &bs {
                assert_eq!(b.va, next, "batches are contiguous");
                assert!(b.pages >= 1 && b.pages <= FILL_BATCH);
                let last = b.va + (b.pages as u64 - 1) * PAGE_SIZE_4K;
                assert_eq!(b.va / LEAF_SPAN, last / LEAF_SPAN, "one leaf table");
                next = b.va + b.pages as u64 * PAGE_SIZE_4K;
            }
            assert!(next >= va + len && next - (va + len) < PAGE_SIZE_4K);
        }
        assert_eq!(batches(0x40_0000, 0).count(), 0);
        assert_eq!(batches(u64::MAX - 10, 100).count(), 0);
    }

    #[test]
    fn write_zero_copy_bytes() {
        let mut pool = Pool::new(64);
        let kernel = kernel_mapper(&mut pool);
        let mut a = new_space(&kernel, &mut pool);
        let va = 0x40_0000u64;
        map(&mut a, &mut pool, va, 3);
        let msg: Vec<u8> = (0..(2 * PAGE_SIZE_4K as usize + 10))
            .map(|i| i as u8 | 1)
            .collect();
        write(&a, &mut Fake::default(), va + 7, &msg).unwrap();
        let mut got = std::vec![0u8; msg.len()];
        peek(&a, va + 7, &mut got);
        assert_eq!(got, msg);
        zero(&a, &mut Fake::default(), va + 100, 5000).unwrap();
        peek(&a, va + 7, &mut got);
        for (k, b) in got.iter().enumerate() {
            let at = va + 7 + k as u64;
            let want = if (va + 100..va + 5100).contains(&at) {
                0
            } else {
                msg[k]
            };
            assert_eq!(*b, want, "byte at {at:#x}");
        }
        assert_eq!(
            write(&a, &mut Fake::default(), 0, b"x"),
            Err(FillError::Range)
        );
        free(a, &mut pool);
    }

    #[test]
    fn copy_copies_bytes_not_frames() {
        let mut pool = Pool::new(128);
        let kernel = kernel_mapper(&mut pool);
        let mut a = new_space(&kernel, &mut pool);
        let mut b = new_space(&kernel, &mut pool);
        let va = 0x40_0000u64;
        map(&mut a, &mut pool, va, 2);
        map(&mut b, &mut pool, va, 2);
        write(&a, &mut Fake::default(), va + 4090, b"fork-me").unwrap();
        copy(&a, &b, &mut Fake::default(), va, 2 * PAGE_SIZE_4K).unwrap();
        let mut got = [0u8; 7];
        peek(&b, va + 4090, &mut got);
        assert_eq!(&got, b"fork-me");
        write(&a, &mut Fake::default(), va + 4090, b"parent!").unwrap();
        peek(&b, va + 4090, &mut got);
        assert_eq!(&got, b"fork-me", "the copy has its own frames");
        assert_eq!(
            copy(&a, &a, &mut Fake::default(), va, PAGE_SIZE_4K),
            Err(FillError::Range),
            "a copy onto the same frames is refused"
        );
        free(a, &mut pool);
        free(b, &mut pool);
    }

    #[test]
    fn pt_hold_covers_one_batch() {
        let pages = 3 * FILL_BATCH as u64 + 5;
        let mut pool = Pool::new(2 * pages as usize + 64);
        let kernel = kernel_mapper(&mut pool);
        let mut a = new_space(&kernel, &mut pool);
        let mut b = new_space(&kernel, &mut pool);
        // Starts 3 pages below a leaf-table boundary.
        let va = 0x60_0000u64 - 3 * PAGE_SIZE_4K;
        map(&mut a, &mut pool, va, pages);
        map(&mut b, &mut pool, va, pages);
        let mut fake = Fake::default();
        copy(&a, &b, &mut fake, va, pages * PAGE_SIZE_4K).unwrap();
        let total: usize = fake.holds.iter().map(|h| h.pages).sum();
        assert_eq!(total as u64, pages);
        assert_eq!(
            fake.holds.first().map(|h| h.pages),
            Some(3),
            "stops at the leaf table"
        );
        for h in &fake.holds {
            assert!(h.pages <= FILL_BATCH);
        }
        // Every hold is followed by its copy, outside the hold.
        let want: Vec<char> = fake.holds.iter().flat_map(|_| ['H', 'F']).collect();
        assert_eq!(fake.events, want);
        free(a, &mut pool);
        free(b, &mut pool);
    }

    #[test]
    fn unmapped_page_is_an_error() {
        let mut pool = Pool::new(64);
        let kernel = kernel_mapper(&mut pool);
        let mut a = new_space(&kernel, &mut pool);
        let va = 0x40_0000u64;
        map(&mut a, &mut pool, va, 1);
        assert_eq!(
            write(&a, &mut Fake::default(), va + PAGE_SIZE_4K - 2, b"abcd"),
            Err(FillError::Unmapped)
        );
        assert_eq!(
            zero(&a, &mut Fake::default(), va + PAGE_SIZE_4K, 1),
            Err(FillError::Unmapped)
        );
        free(a, &mut pool);
    }
}
