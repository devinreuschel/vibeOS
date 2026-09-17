//! Portable page-table types and 4-level walk. DESIGN §4.3.
//!
//! The kernel builds its own PML4 from buddy frames and never edits
//! Limine's tables again. The pieces that touch CR3/EFER/`invlpg` live
//! in the binary crate (`src/paging_init.rs`, `src/x86.rs`); everything
//! algorithmic (index math, PTE encoding, walk, split-into-2MiB) lives
//! here so `cargo test --lib` covers it.
//!
//! ## Design notes
//!
//! - Two page sizes: 4 KiB leaf at level 1 and 2 MiB leaf at level 2.
//!   Bigger pages (1 GiB / L3) are not used; DESIGN §4.3 only names the
//!   two we implement.
//! - Table frames come from a caller-supplied `FrameAlloc` so host tests
//!   can back the allocator with a `Vec`. In the kernel the callback
//!   pulls from the buddy PMM.
//! - Physical to writable-virtual translation uses a caller-supplied
//!   `hhdm_offset`. Same trick as `pmm::Buddy`: `virt = phys + offset`.
//!   In the kernel this is Limine's HHDM offset (valid until we install
//!   our own PML4 — after that, the same offset still works because our
//!   physmap sits at HHDM_OFFSET too, per DESIGN §4.1).
//! - `map_page` refuses to overwrite an existing present leaf unless the
//!   caller opts in with `MapMode::Remap`. That is DESIGN §4.3's "assert
//!   on mapping over an existing present entry unless explicitly
//!   remapping".
//!
//! ## Deliberately out of scope for Slice B
//!
//! - 1 GiB pages, per-process address-space objects, and the demand-paging
//!   fault handler (all phase 9/10).
//! - Real TLB shootdown IPIs: `tlb_shootdown_others` is a documented
//!   no-op that phase 4 replaces without touching call sites (DESIGN §4.3
//!   TLB section, §7.9).

#![allow(clippy::identity_op)]

pub const PAGE_SHIFT: u32 = 12;
pub const PAGE_SIZE_4K: u64 = 1 << PAGE_SHIFT;
pub const PAGE_SIZE_2M: u64 = 1 << 21;
pub const PTES_PER_TABLE: usize = 512;

/// Physical byte address. Newtype so a physical value cannot silently
/// stand in for a virtual one, and vice versa (DESIGN §1.1).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct PhysAddr(pub u64);

impl PhysAddr {
    pub const fn new(v: u64) -> Self {
        Self(v)
    }
    pub const fn as_u64(self) -> u64 {
        self.0
    }
    pub const fn is_aligned(self, align: u64) -> bool {
        (self.0 & (align - 1)) == 0
    }
}

/// Virtual byte address. Same reasoning as `PhysAddr`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct VirtAddr(pub u64);

impl VirtAddr {
    pub const fn new(v: u64) -> Self {
        Self(v)
    }
    pub const fn as_u64(self) -> u64 {
        self.0
    }
    pub const fn is_aligned(self, align: u64) -> bool {
        (self.0 & (align - 1)) == 0
    }

    /// PML4/PDPT/PD/PT indices for this VA. Level 4 down to 1.
    pub const fn index(self, level: u8) -> usize {
        let shift = 12 + 9 * (level as u32 - 1);
        ((self.0 >> shift) & 0x1FF) as usize
    }
}

/// Which page size a leaf entry maps.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PageSize {
    Size4K,
    Size2M,
}

impl PageSize {
    pub const fn bytes(self) -> u64 {
        match self {
            PageSize::Size4K => PAGE_SIZE_4K,
            PageSize::Size2M => PAGE_SIZE_2M,
        }
    }
}

/// PTE flag set. Held as a raw u64 so table entries round-trip losslessly
/// (including the OS-reserved bits phase 10 will want for e.g. CoW).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct PageFlags(pub u64);

impl PageFlags {
    pub const PRESENT: u64 = 1 << 0;
    pub const WRITABLE: u64 = 1 << 1;
    pub const USER: u64 = 1 << 2;
    pub const PWT: u64 = 1 << 3;
    pub const PCD: u64 = 1 << 4;
    pub const ACCESSED: u64 = 1 << 5;
    pub const DIRTY: u64 = 1 << 6;
    /// PS on L2/L3 (huge page). Aliases PAT bit on 4K leaves; we do not
    /// use PAT anywhere so treating it as PS is fine.
    pub const HUGE: u64 = 1 << 7;
    pub const GLOBAL: u64 = 1 << 8;
    pub const NX: u64 = 1 << 63;

    pub const fn empty() -> Self {
        Self(0)
    }
    pub const fn present() -> Self {
        Self(Self::PRESENT)
    }
    pub const fn contains(self, bits: u64) -> bool {
        (self.0 & bits) == bits
    }
    pub const fn with(self, bits: u64) -> Self {
        Self(self.0 | bits)
    }
    pub const fn without(self, bits: u64) -> Self {
        Self(self.0 & !bits)
    }
}

/// Physical-address mask for a PTE. Bits 12..=51 are the frame address on
/// current hardware; bits 0..12 and 52..63 are flags / reserved / NX.
pub const PTE_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// Compose a raw PTE from `(phys, flags)`.
#[inline]
pub const fn make_pte(phys: PhysAddr, flags: PageFlags) -> u64 {
    (phys.0 & PTE_ADDR_MASK) | (flags.0 & !PTE_ADDR_MASK)
}

/// Extract the physical frame address a PTE points at.
#[inline]
pub const fn pte_phys(entry: u64) -> PhysAddr {
    PhysAddr(entry & PTE_ADDR_MASK)
}

/// Extract just the flag bits from a PTE.
#[inline]
pub const fn pte_flags(entry: u64) -> PageFlags {
    PageFlags(entry & !PTE_ADDR_MASK)
}

/// Frame allocator abstraction. Returns page-aligned frames of exactly
/// `PAGE_SIZE_4K`. Trait rather than a closure so the mapper can call it
/// from multiple methods without lifetime gymnastics.
///
/// # Safety
/// Implementations must return non-overlapping, currently-unused frames.
/// The returned frame's virtual mapping through `Mapper::hhdm_offset`
/// must be writable.
pub unsafe trait FrameAlloc {
    fn alloc_frame(&mut self) -> Option<PhysAddr>;
}

/// Errors from map / unmap / translate.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MapError {
    /// Address not aligned to the requested page size.
    Misaligned,
    /// A leaf entry already covers this VA and the caller did not ask
    /// for `Remap`.
    AlreadyMapped,
    /// The VA has no present leaf. Returned by operations that expect
    /// to walk an existing mapping (e.g. `patch_physmap_uc`).
    NotMapped,
    /// Frame allocator returned `None` while walking down.
    OutOfFrames,
    /// A 2 MiB request landed under a 4 KiB leaf, or vice versa.
    /// DESIGN §4.3 forbids implicit splits.
    PageSizeMismatch,
    /// Virtual address is non-canonical.
    NonCanonical,
}

/// Whether `map_page` may overwrite an existing present leaf.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MapMode {
    /// Fail with `MapError::AlreadyMapped` if the target PTE is present.
    /// The default; matches DESIGN §4.3's "assert on overlap" rule.
    Fresh,
    /// Overwrite. Used by `patch_physmap_uc` and by phase-10 remap paths.
    Remap,
}

/// Canonicalization check: bits 48..64 must all equal bit 47.
#[inline]
pub const fn is_canonical(va: u64) -> bool {
    let hi = va >> 47;
    hi == 0 || hi == 0x1_FFFF
}

/// Mapper over a single PML4 root.
///
/// Not `Sync`: the design uses one root per address space and a lock at
/// the call site (DESIGN §2.1 puts page tables first in the lock order).
pub struct Mapper {
    root: PhysAddr,
    hhdm_offset: u64,
}

impl Mapper {
    /// # Safety
    /// `root` must point at a zeroed, page-aligned PML4 frame that the
    /// caller owns for the lifetime of this `Mapper`. `hhdm_offset` must
    /// map every table frame's phys addr to a writable virt.
    pub const unsafe fn new(root: PhysAddr, hhdm_offset: u64) -> Self {
        Self { root, hhdm_offset }
    }

    pub fn root(&self) -> PhysAddr {
        self.root
    }
    pub fn hhdm_offset(&self) -> u64 {
        self.hhdm_offset
    }

    #[inline]
    fn table_ptr(&self, phys: PhysAddr) -> *mut u64 {
        // Wrapping so callers can supply any offset value (kernel uses a
        // Limine-provided HHDM; host tests use `base_ptr - phys_base`).
        (phys.0.wrapping_add(self.hhdm_offset) as usize) as *mut u64
    }

    /// Map one page. Panics on non-canonical addresses to fail loud
    /// (DESIGN §1.1: "Fail loud, fail early").
    ///
    /// # Safety
    /// `phys` must be real, non-conflicting memory. Caller vouches that
    /// no other mapping already reaches this range unless `mode` is
    /// `Remap`.
    pub unsafe fn map_page<A: FrameAlloc>(
        &mut self,
        va: VirtAddr,
        pa: PhysAddr,
        flags: PageFlags,
        size: PageSize,
        mode: MapMode,
        alloc: &mut A,
    ) -> Result<(), MapError> {
        if !is_canonical(va.0) {
            return Err(MapError::NonCanonical);
        }
        let step = size.bytes();
        if !va.is_aligned(step) || !pa.is_aligned(step) {
            return Err(MapError::Misaligned);
        }

        let mut table_phys = self.root;
        // Walk down to the level that holds the leaf. 4K leaf sits at L1
        // (walk L4->L3->L2->L1), 2M leaf sits at L2 (walk L4->L3->L2 and
        // stop).
        let leaf_level: u8 = match size {
            PageSize::Size4K => 1,
            PageSize::Size2M => 2,
        };

        // Walk levels 4 down to leaf_level+1: at each, ensure a child
        // table exists.
        let mut level = 4;
        while level > leaf_level {
            let idx = va.index(level);
            let entry_ptr = unsafe { self.table_ptr(table_phys).add(idx) };
            let entry = unsafe { entry_ptr.read_volatile() };

            if entry & PageFlags::PRESENT == 0 {
                let new = alloc.alloc_frame().ok_or(MapError::OutOfFrames)?;
                unsafe { self.zero_frame(new) };
                // Interior tables always writable, always non-NX.
                // Setting USER here is fine — permission is masked by
                // the leaf's flags for kernel VAs.
                let interior = PageFlags(
                    PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER,
                );
                unsafe { entry_ptr.write_volatile(make_pte(new, interior)) };
                table_phys = new;
            } else {
                if entry & PageFlags::HUGE != 0 {
                    // The walk hit an existing huge/large leaf where we
                    // expected an interior table. That means the caller
                    // is trying to map a 4 KiB page inside an existing
                    // 2 MiB region without splitting.
                    return Err(MapError::PageSizeMismatch);
                }
                table_phys = pte_phys(entry);
            }
            level -= 1;
        }

        // Now at the level holding the leaf slot.
        let idx = va.index(leaf_level);
        let entry_ptr = unsafe { self.table_ptr(table_phys).add(idx) };
        let existing = unsafe { entry_ptr.read_volatile() };
        if existing & PageFlags::PRESENT != 0 {
            match mode {
                MapMode::Fresh => return Err(MapError::AlreadyMapped),
                MapMode::Remap => {}
            }
            // If existing leaf was a 4K entry but we're placing a 2M leaf
            // (or the other way round), refuse. Not our job to split.
            let existing_huge = (existing & PageFlags::HUGE) != 0;
            let want_huge = matches!(size, PageSize::Size2M);
            if existing_huge != want_huge {
                return Err(MapError::PageSizeMismatch);
            }
        }

        let mut leaf_flags = flags.with(PageFlags::PRESENT);
        if matches!(size, PageSize::Size2M) {
            leaf_flags = leaf_flags.with(PageFlags::HUGE);
        }
        unsafe { entry_ptr.write_volatile(make_pte(pa, leaf_flags)) };
        Ok(())
    }

    /// Map `[va, va+len)` to `[pa, pa+len)` using the largest natural
    /// pages that fit alignment and length. Both endpoints must be
    /// aligned to at least 4 KiB.
    ///
    /// # Safety
    /// Same contract as `map_page`.
    pub unsafe fn map_range<A: FrameAlloc>(
        &mut self,
        va: VirtAddr,
        pa: PhysAddr,
        len: u64,
        flags: PageFlags,
        mode: MapMode,
        alloc: &mut A,
    ) -> Result<(), MapError> {
        if len == 0 {
            return Ok(());
        }
        if !va.is_aligned(PAGE_SIZE_4K) || !pa.is_aligned(PAGE_SIZE_4K) {
            return Err(MapError::Misaligned);
        }
        let mut off: u64 = 0;
        while off < len {
            let cur_va = VirtAddr(va.0 + off);
            let cur_pa = PhysAddr(pa.0 + off);
            let remaining = len - off;
            let use_2m = cur_va.is_aligned(PAGE_SIZE_2M)
                && cur_pa.is_aligned(PAGE_SIZE_2M)
                && remaining >= PAGE_SIZE_2M;
            let (size, step) = if use_2m {
                (PageSize::Size2M, PAGE_SIZE_2M)
            } else {
                (PageSize::Size4K, PAGE_SIZE_4K)
            };
            unsafe { self.map_page(cur_va, cur_pa, flags, size, mode, alloc)? };
            off += step;
        }
        Ok(())
    }

    /// Unmap one 4 KiB or 2 MiB page. Returns the physical frame that
    /// was mapped, or `None` if the VA was not present.
    ///
    /// Interior tables are NOT freed — that requires refcounting and
    /// belongs in phase 10.
    ///
    /// # Safety
    /// Caller is responsible for TLB invalidation via
    /// `tlb_invalidate_page` (single CPU) or the shootdown protocol
    /// (SMP, DESIGN §7.9).
    pub unsafe fn unmap_page(&mut self, va: VirtAddr) -> Option<(PhysAddr, PageSize)> {
        let mut table_phys = self.root;
        let mut level = 4;
        while level >= 1 {
            let idx = va.index(level);
            let entry_ptr = unsafe { self.table_ptr(table_phys).add(idx) };
            let entry = unsafe { entry_ptr.read_volatile() };
            if entry & PageFlags::PRESENT == 0 {
                return None;
            }
            let is_leaf = level == 1 || (entry & PageFlags::HUGE) != 0;
            if is_leaf {
                let size = if level == 1 {
                    PageSize::Size4K
                } else {
                    PageSize::Size2M
                };
                let phys = pte_phys(entry);
                unsafe { entry_ptr.write_volatile(0) };
                return Some((phys, size));
            }
            table_phys = pte_phys(entry);
            level -= 1;
        }
        None
    }

    /// Return the physical address a VA translates to, or `None` if
    /// unmapped. Does not consult the TLB (host-testable).
    pub fn translate(&self, va: VirtAddr) -> Option<(PhysAddr, PageSize, PageFlags)> {
        let mut table_phys = self.root;
        let mut level = 4;
        while level >= 1 {
            let idx = va.index(level);
            let entry = unsafe { self.table_ptr(table_phys).add(idx).read_volatile() };
            if entry & PageFlags::PRESENT == 0 {
                return None;
            }
            let is_leaf = level == 1 || (entry & PageFlags::HUGE) != 0;
            if is_leaf {
                let size = if level == 1 {
                    PageSize::Size4K
                } else {
                    PageSize::Size2M
                };
                let page_base = pte_phys(entry).0;
                let offset = va.0 & (size.bytes() - 1);
                return Some((PhysAddr(page_base + offset), size, pte_flags(entry)));
            }
            table_phys = pte_phys(entry);
            level -= 1;
        }
        None
    }

    /// Add PCD+PWT to every leaf covering `[phys, phys+len)` in the
    /// physmap portion of the address space, preserving 2 MiB page
    /// size — DESIGN §4.3 forbids splitting. Callers pass `hhdm_start`,
    /// the VA at which their physmap begins.
    ///
    /// Returns the number of leaf PTEs touched. Zero means the region
    /// wasn't in the physmap and the caller should have used `ioremap`.
    ///
    /// # Safety
    /// Only sound after CR3 install; the walked PTEs must be the ones
    /// the CPU is currently using.
    pub unsafe fn patch_physmap_uc(
        &mut self,
        hhdm_start: VirtAddr,
        phys: PhysAddr,
        len: u64,
    ) -> Result<usize, MapError> {
        if len == 0 {
            return Ok(0);
        }
        let mut touched = 0usize;
        let mut off: u64 = 0;
        while off < len {
            let va = VirtAddr(hhdm_start.0 + phys.0 + off);
            let (_, size, _) = self.translate(va).ok_or(MapError::NotMapped)?;
            // Walk to the leaf slot, add the bits, write back.
            let leaf_level: u8 = match size {
                PageSize::Size4K => 1,
                PageSize::Size2M => 2,
            };
            let mut table_phys = self.root;
            let mut level = 4;
            while level > leaf_level {
                let idx = va.index(level);
                let entry = unsafe { self.table_ptr(table_phys).add(idx).read_volatile() };
                table_phys = pte_phys(entry);
                level -= 1;
            }
            let idx = va.index(leaf_level);
            let entry_ptr = unsafe { self.table_ptr(table_phys).add(idx) };
            let entry = unsafe { entry_ptr.read_volatile() };
            let patched = entry | PageFlags::PCD | PageFlags::PWT;
            unsafe { entry_ptr.write_volatile(patched) };
            touched += 1;
            off = off.saturating_add(size.bytes());
        }
        Ok(touched)
    }

    /// Zero a freshly-allocated frame through the HHDM.
    ///
    /// # Safety
    /// `phys` must be an owned, page-sized frame reachable via
    /// `hhdm_offset`.
    unsafe fn zero_frame(&self, phys: PhysAddr) {
        let ptr = self.table_ptr(phys) as *mut u64;
        for i in 0..PTES_PER_TABLE {
            unsafe { ptr.add(i).write_volatile(0) };
        }
    }
}

/// Reservation over the ioremap window (DESIGN §4.1: `0xFFFF_E000_0000_0000`,
/// 256 MiB). Bump allocator, never freed. Devices call `ioremap` for MMIO
/// that should not be reached through the physmap (typically because it
/// belongs to a device whose physical address is far above `map_end`).
pub const IOREMAP_BASE: u64 = 0xFFFF_E000_0000_0000;
pub const IOREMAP_LEN: u64 = 256 * 1024 * 1024;

pub struct IoremapWindow {
    next: u64,
}

impl IoremapWindow {
    pub const fn new() -> Self {
        Self { next: IOREMAP_BASE }
    }

    /// Reserve `len` bytes at the next natural alignment for `phys`,
    /// return the VA the caller should map onto `[phys, phys+len)`.
    /// The actual `map_range` is the caller's job; this only tracks
    /// window bookkeeping.
    ///
    /// Aligns both VA and length up to 4 KiB. A 2 MiB-aligned request
    /// naturally lines up too since the bump base is 512 GiB aligned.
    pub fn reserve(&mut self, phys: PhysAddr, len: u64) -> Option<VirtAddr> {
        if len == 0 {
            return None;
        }
        let page_off = phys.0 & (PAGE_SIZE_4K - 1);
        let pages = align_up(page_off + len, PAGE_SIZE_4K);
        let va = align_up(self.next, PAGE_SIZE_4K);
        let end = va.checked_add(pages)?;
        if end > IOREMAP_BASE + IOREMAP_LEN {
            return None;
        }
        self.next = end;
        Some(VirtAddr(va + page_off))
    }

    pub fn next(&self) -> u64 {
        self.next
    }
}

impl Default for IoremapWindow {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
const fn align_up(x: u64, a: u64) -> u64 {
    (x + a - 1) & !(a - 1)
}

/// Standard MMIO leaf flags per DESIGN §4.3 PTE flag policy table.
pub const fn mmio_flags() -> PageFlags {
    PageFlags(PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::GLOBAL | PageFlags::NX
              | PageFlags::PCD | PageFlags::PWT)
}

/// Physmap leaf flags: writable, NX, global.
pub const fn physmap_flags() -> PageFlags {
    PageFlags(PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::GLOBAL | PageFlags::NX)
}

/// Kernel `.text` flags: read-only, executable, global.
pub const fn kernel_text_flags() -> PageFlags {
    PageFlags(PageFlags::PRESENT | PageFlags::GLOBAL)
}

/// Kernel `.rodata` flags: read-only, NX, global.
pub const fn kernel_rodata_flags() -> PageFlags {
    PageFlags(PageFlags::PRESENT | PageFlags::GLOBAL | PageFlags::NX)
}

/// Kernel `.data` / `.bss` flags: writable, NX, global.
pub const fn kernel_data_flags() -> PageFlags {
    PageFlags(PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::GLOBAL | PageFlags::NX)
}

/// TLB shootdown hook. Single-CPU no-op today; phase 4 replaces this
/// with the IPI 0xFC path (DESIGN §7.9). Every place that changes a
/// kernel PTE calls this after the local `invlpg` so call sites are
/// already correct when APs arrive.
///
/// Intentionally free-standing (not a method on `Mapper`) so architecture
/// code can call it after any leaf edit including MMIO patches.
#[inline]
pub fn tlb_shootdown_others(_va: VirtAddr) {
    // no-op: single CPU
}

// ------------------ host tests ------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::vec;
    use std::vec::Vec;

    /// Vec-backed physical memory + a bump `FrameAlloc` for host tests.
    /// Handing out frames sequentially from a big slab of zeros is enough
    /// coverage for the walk logic — the buddy allocator has its own
    /// dedicated tests.
    struct TestPool {
        _mem: Vec<u64>,
        hhdm_offset: u64,
        next_frame: u64,
        end: u64,
    }

    impl TestPool {
        fn new(frames: usize) -> Self {
            let words = (frames * PAGE_SIZE_4K as usize) / 8;
            let mem: Vec<u64> = vec![0u64; words];
            let ptr = mem.as_ptr() as u64;
            let phys_base = 0x0100_0000; // 16 MiB, arbitrary
            let hhdm_offset = ptr.wrapping_sub(phys_base);
            Self {
                _mem: mem,
                hhdm_offset,
                next_frame: phys_base,
                end: phys_base + (frames as u64) * PAGE_SIZE_4K,
            }
        }
    }

    unsafe impl FrameAlloc for TestPool {
        fn alloc_frame(&mut self) -> Option<PhysAddr> {
            if self.next_frame >= self.end {
                return None;
            }
            let p = self.next_frame;
            self.next_frame += PAGE_SIZE_4K;
            Some(PhysAddr(p))
        }
    }

    fn fresh_mapper(pool: &mut TestPool) -> Mapper {
        let root = <TestPool as FrameAlloc>::alloc_frame(pool).unwrap();
        // Zero via HHDM.
        let ptr = root.0.wrapping_add(pool.hhdm_offset) as *mut u64;
        for i in 0..PTES_PER_TABLE {
            unsafe { ptr.add(i).write_volatile(0) };
        }
        unsafe { Mapper::new(root, pool.hhdm_offset) }
    }

    #[test]
    fn indexing_matches_hardware_layout() {
        // 0x1_2345_6789_abcd: pick out per-level indices.
        let v = VirtAddr(0x0000_1234_5678_9abc);
        // l1 = bits 12..21, l2 = 21..30, l3 = 30..39, l4 = 39..48.
        assert_eq!(v.index(1), ((0x0000_1234_5678_9abcu64 >> 12) & 0x1FF) as usize);
        assert_eq!(v.index(2), ((0x0000_1234_5678_9abcu64 >> 21) & 0x1FF) as usize);
        assert_eq!(v.index(3), ((0x0000_1234_5678_9abcu64 >> 30) & 0x1FF) as usize);
        assert_eq!(v.index(4), ((0x0000_1234_5678_9abcu64 >> 39) & 0x1FF) as usize);
    }

    #[test]
    fn canonical_check() {
        assert!(is_canonical(0x0000_7FFF_FFFF_FFFF));
        assert!(is_canonical(0xFFFF_8000_0000_0000));
        assert!(is_canonical(0));
        // Middle of the non-canonical hole.
        assert!(!is_canonical(0x0000_8000_0000_0000));
        assert!(!is_canonical(0xFFFF_7FFF_FFFF_FFFF));
    }

    #[test]
    fn make_pte_round_trip() {
        let phys = PhysAddr(0x0000_0000_ABCD_E000);
        let f = PageFlags::present().with(PageFlags::WRITABLE | PageFlags::NX);
        let entry = make_pte(phys, f);
        assert_eq!(pte_phys(entry), phys);
        assert_eq!(pte_flags(entry).0, f.0);
        // NX bit lives at 63 and must survive the round trip.
        assert!(pte_flags(entry).contains(PageFlags::NX));
    }

    #[test]
    fn map_translate_unmap_4k() {
        let mut pool = TestPool::new(64);
        let mut m = fresh_mapper(&mut pool);
        let va = VirtAddr(0xFFFF_C000_0010_0000);
        let pa = PhysAddr(0x0080_0000);
        unsafe {
            m.map_page(va, pa, physmap_flags(), PageSize::Size4K, MapMode::Fresh, &mut pool)
                .unwrap();
        }
        let (got, size, flags) = m.translate(va).unwrap();
        assert_eq!(got, pa);
        assert_eq!(size, PageSize::Size4K);
        assert!(flags.contains(PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::NX));
        // Offset within the page carries through.
        let (got_mid, _, _) = m.translate(VirtAddr(va.0 + 0x123)).unwrap();
        assert_eq!(got_mid.0, pa.0 + 0x123);
        // Unmap returns the frame and translate is None.
        let (unm_pa, unm_size) = unsafe { m.unmap_page(va).unwrap() };
        assert_eq!(unm_pa, pa);
        assert_eq!(unm_size, PageSize::Size4K);
        assert!(m.translate(va).is_none());
    }

    #[test]
    fn map_translate_2m() {
        let mut pool = TestPool::new(64);
        let mut m = fresh_mapper(&mut pool);
        let va = VirtAddr(0xFFFF_8000_0020_0000);
        let pa = PhysAddr(0x0040_0000);
        unsafe {
            m.map_page(va, pa, physmap_flags(), PageSize::Size2M, MapMode::Fresh, &mut pool)
                .unwrap();
        }
        let (got, size, _) = m.translate(va).unwrap();
        assert_eq!(got, pa);
        assert_eq!(size, PageSize::Size2M);
        // Offset well inside the 2M page.
        let mid = VirtAddr(va.0 + 0x0010_0000);
        let (mid_pa, _, _) = m.translate(mid).unwrap();
        assert_eq!(mid_pa.0, pa.0 + 0x0010_0000);
    }

    #[test]
    fn misaligned_2m_rejected() {
        let mut pool = TestPool::new(64);
        let mut m = fresh_mapper(&mut pool);
        let va = VirtAddr(0xFFFF_8000_0020_1000); // not 2M-aligned
        let pa = PhysAddr(0x0040_0000);
        let err = unsafe {
            m.map_page(va, pa, physmap_flags(), PageSize::Size2M, MapMode::Fresh, &mut pool)
                .unwrap_err()
        };
        assert_eq!(err, MapError::Misaligned);
    }

    #[test]
    fn map_range_selects_2m_when_aligned() {
        let mut pool = TestPool::new(256);
        let mut m = fresh_mapper(&mut pool);
        let va = VirtAddr(0xFFFF_8000_0000_0000);
        let pa = PhysAddr(0);
        // 6 MiB: three whole 2M pages, no tail.
        unsafe {
            m.map_range(va, pa, 6 * PAGE_SIZE_2M, physmap_flags(), MapMode::Fresh, &mut pool)
                .unwrap();
        }
        for i in 0..3 {
            let (got, size, _) = m.translate(VirtAddr(va.0 + i * PAGE_SIZE_2M)).unwrap();
            assert_eq!(size, PageSize::Size2M, "block {i} should be 2M");
            assert_eq!(got.0, pa.0 + i * PAGE_SIZE_2M);
        }
    }

    #[test]
    fn map_range_splits_head_tail_to_4k() {
        let mut pool = TestPool::new(1024);
        let mut m = fresh_mapper(&mut pool);
        // Start at a 4K boundary that isn't 2M aligned, so the first
        // stretch has to be 4K until we roll into a 2M boundary; then
        // 2M pages; then 4K tail.
        let va = VirtAddr(0xFFFF_8000_0000_0000 + PAGE_SIZE_4K);
        let pa = PhysAddr(PAGE_SIZE_4K);
        let len = 3 * PAGE_SIZE_2M;
        unsafe {
            m.map_range(va, pa, len, physmap_flags(), MapMode::Fresh, &mut pool)
                .unwrap();
        }
        // Spot check a page in the head (first 4K), one deep inside a
        // 2M block, and one in the tail.
        assert_eq!(m.translate(va).unwrap().0, pa);
        let mid = VirtAddr(0xFFFF_8000_0000_0000 + PAGE_SIZE_2M);
        assert_eq!(m.translate(mid).unwrap().1, PageSize::Size2M);
        let end = VirtAddr(va.0 + len - PAGE_SIZE_4K);
        assert!(m.translate(end).is_some());
    }

    #[test]
    fn overlap_without_remap_errors() {
        let mut pool = TestPool::new(64);
        let mut m = fresh_mapper(&mut pool);
        let va = VirtAddr(0xFFFF_C000_0000_0000);
        let pa = PhysAddr(0x0080_0000);
        unsafe {
            m.map_page(va, pa, physmap_flags(), PageSize::Size4K, MapMode::Fresh, &mut pool)
                .unwrap();
        }
        let err = unsafe {
            m.map_page(va, PhysAddr(0x0090_0000), physmap_flags(),
                       PageSize::Size4K, MapMode::Fresh, &mut pool)
                .unwrap_err()
        };
        assert_eq!(err, MapError::AlreadyMapped);
        // Remap replaces the leaf.
        unsafe {
            m.map_page(va, PhysAddr(0x0090_0000), physmap_flags(),
                       PageSize::Size4K, MapMode::Remap, &mut pool)
                .unwrap();
        }
        assert_eq!(m.translate(va).unwrap().0, PhysAddr(0x0090_0000));
    }

    #[test]
    fn page_size_mismatch_rejected() {
        let mut pool = TestPool::new(64);
        let mut m = fresh_mapper(&mut pool);
        let va = VirtAddr(0xFFFF_8000_0040_0000);
        // Place a 2M leaf.
        unsafe {
            m.map_page(va, PhysAddr(0x0080_0000), physmap_flags(),
                       PageSize::Size2M, MapMode::Fresh, &mut pool)
                .unwrap();
        }
        // Attempt a 4K page inside the 2M region: the L2 slot is a huge
        // leaf, so the walk to L1 fails with PageSizeMismatch.
        let inside = VirtAddr(va.0 + PAGE_SIZE_4K);
        let err = unsafe {
            m.map_page(inside, PhysAddr(0x0090_0000), physmap_flags(),
                       PageSize::Size4K, MapMode::Fresh, &mut pool)
                .unwrap_err()
        };
        assert_eq!(err, MapError::PageSizeMismatch);
    }

    #[test]
    fn patch_uc_preserves_2m_and_sets_bits() {
        let mut pool = TestPool::new(1024);
        let mut m = fresh_mapper(&mut pool);
        // Fake up a physmap at HHDM_START mapping [0, 4 MiB) with 2M pages.
        let hhdm = VirtAddr(0xFFFF_8000_0000_0000);
        unsafe {
            m.map_range(hhdm, PhysAddr(0), 4 * PAGE_SIZE_2M,
                        physmap_flags(), MapMode::Fresh, &mut pool)
                .unwrap();
        }
        // Patch 8 KiB starting at phys 0x0020_1000: falls inside the
        // second 2M page. patch should touch exactly one 2M leaf and
        // leave it a 2M leaf.
        let touched = unsafe {
            m.patch_physmap_uc(hhdm, PhysAddr(0x0020_1000), 8 * 1024)
                .unwrap()
        };
        assert_eq!(touched, 1);
        let inside = VirtAddr(hhdm.0 + 0x0020_1000);
        let (_, size, flags) = m.translate(inside).unwrap();
        assert_eq!(size, PageSize::Size2M, "must not split");
        assert!(flags.contains(PageFlags::PCD | PageFlags::PWT));
        // A page in an adjacent 2M block still lacks the UC bits.
        let outside = VirtAddr(hhdm.0 + 0x0040_0000);
        let (_, _, oflags) = m.translate(outside).unwrap();
        assert!(!oflags.contains(PageFlags::PCD));
    }

    #[test]
    fn patch_uc_on_unmapped_returns_not_mapped() {
        // patch_physmap_uc walks the physmap PTEs, so a phys address the
        // physmap doesn't cover must surface as `NotMapped` — not the
        // easily-misread `AlreadyMapped`.
        let mut pool = TestPool::new(1024);
        let mut m = fresh_mapper(&mut pool);
        let hhdm = VirtAddr(0xFFFF_8000_0000_0000);
        // Deliberately do NOT map anything into hhdm before patching.
        let err = unsafe { m.patch_physmap_uc(hhdm, PhysAddr(0), PAGE_SIZE_4K).unwrap_err() };
        assert_eq!(err, MapError::NotMapped);
    }

    #[test]
    fn ioremap_bumps_and_bounds() {
        let mut w = IoremapWindow::new();
        // Simple aligned reservation.
        let a = w.reserve(PhysAddr(0xFEE0_0000), 4096).unwrap();
        assert_eq!(a.0 & (PAGE_SIZE_4K - 1), 0);
        // Sub-page tail: reservation still returns the exact phys offset
        // so device access hits the right byte.
        let b = w.reserve(PhysAddr(0xFEC0_0123), 0x100).unwrap();
        assert_eq!(b.0 & 0xFFF, 0x123);
        // Exhaustion returns None.
        let mut w = IoremapWindow::new();
        assert!(w.reserve(PhysAddr(0), IOREMAP_LEN + 4096).is_none());
    }
}
