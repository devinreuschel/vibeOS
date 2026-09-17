//! Kernel page tables. Portable half.
//!
//! DESIGN §4.1 owns the address map, §4.3 owns the mapping rules. This
//! file is the mechanism: a 4-level x86_64 walker (`PageTables`), typed
//! `PhysAddr` / `VirtAddr` / PTE flag constants, `map_page`, `map_2m`,
//! `unmap_page`, `translate`, and the MMIO-attribute patch that
//! `paging_init` calls after CR3 install.
//!
//! The walker never reaches out to hardware directly. Sub-tables are read
//! and written through `hhdm_offset`, so the same code that maps a real
//! page under Limine also drives a `Vec`-backed pool in the host tests
//! (same trick as the buddy allocator in `pmm.rs`).
//!
//! What is deliberately deferred:
//! - Contiguous-block coalescing on unmap. Slice B never unmaps at scale;
//!   phase 3+ scheduler stack teardown gets the real machinery.
//! - PCID / global-flush accounting. `invlpg` is enough for one CPU
//!   (`kernel mappings are GLOBAL` so a CR3 reload does not drop them,
//!   DESIGN §4.3). Phase 4 replaces the single-CPU shootdown no-op in
//!   `paging_init` with the real IPI.
//! - Splitting a 2 MiB entry into 4 KiB entries. DESIGN §1.3 explicitly
//!   forbids splitting the physmap for MMIO patches; if a later subsystem
//!   needs sub-page granularity somewhere else, it goes through the
//!   `ioremap` window instead of tearing up the physmap.

#![allow(clippy::identity_op)]

pub type PhysAddr = u64;
pub type VirtAddr = u64;

pub const PAGE_BITS: u32 = 12;
pub const PAGE_BITS_2M: u32 = 21;
pub const PAGE_SIZE: u64 = 1 << PAGE_BITS;
pub const PAGE_SIZE_2M: u64 = 1 << PAGE_BITS_2M;
pub const ENTRIES_PER_TABLE: usize = 512;

// PTE flag bits. Intel SDM Vol 3A §4.5.
pub const PTE_PRESENT: u64 = 1 << 0;
pub const PTE_WRITABLE: u64 = 1 << 1;
pub const PTE_USER: u64 = 1 << 2;
pub const PTE_PWT: u64 = 1 << 3;
pub const PTE_PCD: u64 = 1 << 4;
pub const PTE_ACCESSED: u64 = 1 << 5;
pub const PTE_DIRTY: u64 = 1 << 6;
/// PS at PDPT/PD level marks a huge (1 GiB / 2 MiB) leaf.
pub const PTE_HUGE: u64 = 1 << 7;
pub const PTE_GLOBAL: u64 = 1 << 8;
pub const PTE_NX: u64 = 1 << 63;

/// Bits 12..=51 of a PTE hold the frame physical address. Everything else
/// is flags or reserved. Same range at every level, huge or not.
pub const PTE_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// PCD+PWT together encode "uncacheable" (UC) in the default PAT layout.
/// DESIGN §4.3 / §9.2: LAPIC, I/O APIC, HPET, and every device MMIO page
/// gets these bits. QEMU tolerates write-back MMIO; real hardware does
/// not.
pub const PTE_UC: u64 = PTE_PCD | PTE_PWT;

// ---- Address map (DESIGN §4.1) ----

/// Low identity window. First 2 MiB is executable so the AP trampoline
/// at 0x8000 can run in real mode after paging is enabled (DESIGN §7.3).
pub const LOW_IDENT_LIMIT: u64 = 512 * 1024 * 1024;
pub const LOW_IDENT_EXEC_LIMIT: u64 = 2 * 1024 * 1024;

/// Physmap cap. Firmware sometimes describes MMIO BARs as multi-terabyte
/// regions; walking that at boot does not finish (DESIGN §9.2). Real
/// `map_end` is derived from usable-RAM high water, kernel image end, and
/// framebuffer extent, then capped here.
pub const PHYSMAP_CAP: u64 = 8 * 1024 * 1024 * 1024;

/// Dedicated MMIO window for devices that should not go through the
/// physmap (DESIGN §4.1: 256 MiB starting at 0xFFFF_E000_0000_0000).
pub const MMIO_WINDOW_START: u64 = 0xFFFF_E000_0000_0000;
pub const MMIO_WINDOW_END: u64 = 0xFFFF_E000_1000_0000;
pub const MMIO_WINDOW_LEN: u64 = MMIO_WINDOW_END - MMIO_WINDOW_START;

// ---- Address decomposition ----

#[inline]
pub const fn pml4_index(v: VirtAddr) -> usize {
    ((v >> 39) & 0x1FF) as usize
}
#[inline]
pub const fn pdpt_index(v: VirtAddr) -> usize {
    ((v >> 30) & 0x1FF) as usize
}
#[inline]
pub const fn pd_index(v: VirtAddr) -> usize {
    ((v >> 21) & 0x1FF) as usize
}
#[inline]
pub const fn pt_index(v: VirtAddr) -> usize {
    ((v >> 12) & 0x1FF) as usize
}
#[inline]
pub const fn page_offset(v: VirtAddr) -> u64 {
    v & (PAGE_SIZE - 1)
}
#[inline]
pub const fn page_offset_2m(v: VirtAddr) -> u64 {
    v & (PAGE_SIZE_2M - 1)
}

/// Sign-extend a 48-bit linear address. The CPU faults on non-canonical
/// pointers; producing them by accident (arithmetic that overflows bit 47)
/// is one of the classes of bugs the address map's non-canonical hole
/// exists to catch.
#[inline]
pub const fn canonical(v: u64) -> u64 {
    let shift = 64 - 48;
    ((v << shift) as i64 >> shift) as u64
}

#[inline]
pub const fn is_canonical(v: u64) -> bool {
    canonical(v) == v
}

// ---- Errors ----

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum MapError {
    /// The frame allocator returned `None`.
    NoFrame,
    /// The 4 KiB request lands inside an existing HUGE entry, or a 2 MiB
    /// request lands inside an existing 4 KiB entry. Slice B never
    /// splits, so both are hard failures rather than an automatic split.
    Overlap,
    /// The target entry is already present and `remap` was not set.
    AlreadyMapped,
    /// `virt` or `phys` was not aligned to the requested page size.
    Misaligned,
    /// Virtual address is not canonical.
    NonCanonical,
}

// ---- Frame allocator hook ----

/// Backing for fresh page-table frames. The buddy allocator (with a small
/// wrapper that zeroes the page via HHDM) is the production impl; host
/// tests supply a `Vec`-backed pool.
pub trait FrameAllocator {
    /// Return a fresh, zeroed 4 KiB frame. The address must be reachable
    /// through the `hhdm_offset` the `PageTables` was built with.
    fn alloc_zeroed(&mut self) -> Option<PhysAddr>;
}

// ---- The walker ----

pub struct PageTables {
    pml4_phys: PhysAddr,
    hhdm_offset: u64,
}

impl PageTables {
    /// Wrap a pre-zeroed PML4 frame. Caller supplies the frame so the
    /// buddy allocator and the paging bootstrap can hand out the same
    /// kind of memory.
    ///
    /// # Safety
    /// `pml4_phys` must point to a zeroed 4 KiB page reachable through
    /// `hhdm_offset`, and must not be freed while this `PageTables` is
    /// alive.
    pub unsafe fn new(pml4_phys: PhysAddr, hhdm_offset: u64) -> Self {
        Self {
            pml4_phys,
            hhdm_offset,
        }
    }

    #[inline]
    pub fn pml4_phys(&self) -> PhysAddr {
        self.pml4_phys
    }

    #[inline]
    pub fn hhdm_offset(&self) -> u64 {
        self.hhdm_offset
    }

    /// # Safety
    /// `phys` must be a frame reachable via HHDM. Returns a raw pointer
    /// into the table; caller must not alias mutable references.
    #[inline]
    unsafe fn table_at(&self, phys: PhysAddr) -> *mut u64 {
        (phys.wrapping_add(self.hhdm_offset) as usize) as *mut u64
    }

    /// Map a single 4 KiB page. See `MapOptions`-less signature: keep it
    /// simple; call sites pass `flags` directly.
    ///
    /// # Safety
    /// See module-level rules. Caller vouches that:
    /// - `virt` and `phys` are 4 KiB aligned
    /// - `phys` names real RAM (or a device the caller has decided to map)
    /// - the range is not already claimed by a conflicting subsystem
    ///   (DESIGN §4.1: every region asserts its VA is free)
    pub unsafe fn map_page(
        &mut self,
        virt: VirtAddr,
        phys: PhysAddr,
        flags: u64,
        remap: bool,
        alloc: &mut impl FrameAllocator,
    ) -> Result<(), MapError> {
        if !is_canonical(virt) {
            return Err(MapError::NonCanonical);
        }
        if page_offset(virt) != 0 || page_offset(phys) != 0 {
            return Err(MapError::Misaligned);
        }
        unsafe {
            let pt = self.walk_to_pt(virt, alloc)?;
            let slot = pt.add(pt_index(virt));
            let existing = slot.read();
            if existing & PTE_PRESENT != 0 && !remap {
                return Err(MapError::AlreadyMapped);
            }
            // If the entry is a huge leaf higher up we would have caught
            // it in `walk_to_pt`. Here at the PT level all leaves are 4 KiB.
            slot.write((phys & PTE_ADDR_MASK) | (flags & !PTE_HUGE) | PTE_PRESENT);
        }
        Ok(())
    }

    /// Map a single 2 MiB page.
    ///
    /// # Safety
    /// Same rules as `map_page`, with 2 MiB alignment on both `virt` and
    /// `phys`.
    pub unsafe fn map_2m(
        &mut self,
        virt: VirtAddr,
        phys: PhysAddr,
        flags: u64,
        remap: bool,
        alloc: &mut impl FrameAllocator,
    ) -> Result<(), MapError> {
        if !is_canonical(virt) {
            return Err(MapError::NonCanonical);
        }
        if page_offset_2m(virt) != 0 || page_offset_2m(phys) != 0 {
            return Err(MapError::Misaligned);
        }
        unsafe {
            let pd = self.walk_to_pd(virt, alloc)?;
            let slot = pd.add(pd_index(virt));
            let existing = slot.read();
            if existing & PTE_PRESENT != 0 {
                if !remap {
                    return Err(MapError::AlreadyMapped);
                }
                // Do not silently reclaim the sub-PT frame if this entry
                // used to be a 4 KiB PT parent. Slice B does not need it;
                // fail loud (DESIGN §4.3 asserts).
                if existing & PTE_HUGE == 0 {
                    return Err(MapError::Overlap);
                }
            }
            slot.write((phys & PTE_ADDR_MASK) | (flags & !PTE_PRESENT) | PTE_PRESENT | PTE_HUGE);
        }
        Ok(())
    }

    /// Map `len` bytes starting at `virt -> phys` using 4 KiB pages. `len`
    /// is rounded up to a multiple of `PAGE_SIZE`.
    ///
    /// # Safety
    /// See `map_page`.
    pub unsafe fn map_range_4k(
        &mut self,
        virt: VirtAddr,
        phys: PhysAddr,
        len: u64,
        flags: u64,
        remap: bool,
        alloc: &mut impl FrameAllocator,
    ) -> Result<(), MapError> {
        let end = align_up(len, PAGE_SIZE);
        let mut off = 0u64;
        while off < end {
            unsafe { self.map_page(virt + off, phys + off, flags, remap, alloc)? };
            off += PAGE_SIZE;
        }
        Ok(())
    }

    /// Map `len` bytes with 2 MiB pages. `virt`, `phys` and `len` must be
    /// 2 MiB aligned; a partial trailing page is a caller bug.
    ///
    /// # Safety
    /// See `map_2m`.
    pub unsafe fn map_range_2m(
        &mut self,
        virt: VirtAddr,
        phys: PhysAddr,
        len: u64,
        flags: u64,
        remap: bool,
        alloc: &mut impl FrameAllocator,
    ) -> Result<(), MapError> {
        if page_offset_2m(len) != 0 {
            return Err(MapError::Misaligned);
        }
        let mut off = 0u64;
        while off < len {
            unsafe { self.map_2m(virt + off, phys + off, flags, remap, alloc)? };
            off += PAGE_SIZE_2M;
        }
        Ok(())
    }

    /// Clear the 4 KiB entry covering `virt`. Returns the physical frame
    /// that was mapped, if any. Does not free intermediate tables; slice
    /// B never tears down a subtree, and phase 4 will unify this with the
    /// TLB shootdown path.
    ///
    /// # Safety
    /// Caller vouches that the entry to be cleared was mapped through
    /// this table (not a shared subtree borrowed from Limine).
    pub unsafe fn unmap_page(&mut self, virt: VirtAddr) -> Option<PhysAddr> {
        if !is_canonical(virt) {
            return None;
        }
        unsafe {
            let pt = self.find_pt(virt)?;
            let slot = pt.add(pt_index(virt));
            let entry = slot.read();
            if entry & PTE_PRESENT == 0 {
                return None;
            }
            slot.write(0);
            Some(entry & PTE_ADDR_MASK)
        }
    }

    /// Read-only walk. Resolves `virt` to the mapped physical address,
    /// preserving the offset within a huge page.
    pub fn translate(&self, virt: VirtAddr) -> Option<PhysAddr> {
        if !is_canonical(virt) {
            return None;
        }
        unsafe {
            let pml4 = self.table_at(self.pml4_phys);
            let e = pml4.add(pml4_index(virt)).read();
            if e & PTE_PRESENT == 0 {
                return None;
            }
            let pdpt = self.table_at(e & PTE_ADDR_MASK);
            let e = pdpt.add(pdpt_index(virt)).read();
            if e & PTE_PRESENT == 0 {
                return None;
            }
            if e & PTE_HUGE != 0 {
                // 1 GiB huge page.
                return Some((e & PTE_ADDR_MASK) | (virt & ((1u64 << 30) - 1)));
            }
            let pd = self.table_at(e & PTE_ADDR_MASK);
            let e = pd.add(pd_index(virt)).read();
            if e & PTE_PRESENT == 0 {
                return None;
            }
            if e & PTE_HUGE != 0 {
                return Some((e & PTE_ADDR_MASK) | page_offset_2m(virt));
            }
            let pt = self.table_at(e & PTE_ADDR_MASK);
            let e = pt.add(pt_index(virt)).read();
            if e & PTE_PRESENT == 0 {
                return None;
            }
            Some((e & PTE_ADDR_MASK) | page_offset(virt))
        }
    }

    /// Set PCD+PWT on the physmap PTEs that cover `[phys, phys+len)`.
    /// Preserves 2 MiB pages: an unaligned or short range still patches
    /// the whole 2 MiB entry it lands in, because splitting the physmap
    /// is explicitly ruled out (DESIGN §4.3 / §9.2).
    ///
    /// Returns the number of entries touched. Caller is responsible for
    /// invlpg-ing each patched 2 MiB range.
    ///
    /// # Safety
    /// `physmap_base` must be the HHDM virtual base of the physmap the
    /// caller built.
    pub unsafe fn patch_physmap_uc(
        &mut self,
        physmap_base: VirtAddr,
        phys: PhysAddr,
        len: u64,
    ) -> Result<usize, MapError> {
        if len == 0 {
            return Ok(0);
        }
        let start = align_down(phys, PAGE_SIZE_2M);
        let end = align_up(phys + len, PAGE_SIZE_2M);
        let mut n = 0usize;
        let mut p = start;
        while p < end {
            let virt = physmap_base.wrapping_add(p);
            unsafe { self.set_uc_2m(virt)?; }
            n += 1;
            p += PAGE_SIZE_2M;
        }
        Ok(n)
    }

    // ---- private helpers ----

    /// Walk to the PD level, allocating intermediate tables. Fails if the
    /// path is blocked by an unexpected huge entry higher up.
    unsafe fn walk_to_pd(
        &mut self,
        virt: VirtAddr,
        alloc: &mut impl FrameAllocator,
    ) -> Result<*mut u64, MapError> {
        unsafe {
            let pml4 = self.table_at(self.pml4_phys);
            let pdpt = self.ensure_next(pml4.add(pml4_index(virt)), alloc)?;
            let e = pdpt.add(pdpt_index(virt)).read();
            if e & PTE_PRESENT != 0 && e & PTE_HUGE != 0 {
                return Err(MapError::Overlap);
            }
            let pd = self.ensure_next(pdpt.add(pdpt_index(virt)), alloc)?;
            Ok(pd)
        }
    }

    unsafe fn walk_to_pt(
        &mut self,
        virt: VirtAddr,
        alloc: &mut impl FrameAllocator,
    ) -> Result<*mut u64, MapError> {
        unsafe {
            let pd = self.walk_to_pd(virt, alloc)?;
            let e = pd.add(pd_index(virt)).read();
            if e & PTE_PRESENT != 0 && e & PTE_HUGE != 0 {
                return Err(MapError::Overlap);
            }
            let pt = self.ensure_next(pd.add(pd_index(virt)), alloc)?;
            Ok(pt)
        }
    }

    /// Read `*slot`; if it does not point at a child table, allocate one
    /// and write it back. Returns the child table pointer via HHDM.
    unsafe fn ensure_next(
        &self,
        slot: *mut u64,
        alloc: &mut impl FrameAllocator,
    ) -> Result<*mut u64, MapError> {
        let e = unsafe { slot.read() };
        if e & PTE_PRESENT != 0 {
            // Intermediate entries carry the union of child permissions:
            // PRESENT | WRITABLE | USER. NX only meaningful on leaves.
            return Ok(unsafe { self.table_at(e & PTE_ADDR_MASK) });
        }
        let frame = alloc.alloc_zeroed().ok_or(MapError::NoFrame)?;
        // Intermediate entries: PRESENT + WRITABLE so any leaf below
        // can be writable. GLOBAL bit only lives on leaves per Intel SDM.
        // We deliberately do NOT set NX on the parent so parents can host
        // both X and NX leaves.
        unsafe {
            slot.write((frame & PTE_ADDR_MASK) | PTE_PRESENT | PTE_WRITABLE);
        }
        Ok(unsafe { self.table_at(frame) })
    }

    /// Read-only walk to the PT (no allocation). Returns `None` if any
    /// intermediate entry is missing or is a huge leaf.
    unsafe fn find_pt(&self, virt: VirtAddr) -> Option<*mut u64> {
        unsafe {
            let pml4 = self.table_at(self.pml4_phys);
            let e = pml4.add(pml4_index(virt)).read();
            if e & PTE_PRESENT == 0 {
                return None;
            }
            let pdpt = self.table_at(e & PTE_ADDR_MASK);
            let e = pdpt.add(pdpt_index(virt)).read();
            if e & PTE_PRESENT == 0 || e & PTE_HUGE != 0 {
                return None;
            }
            let pd = self.table_at(e & PTE_ADDR_MASK);
            let e = pd.add(pd_index(virt)).read();
            if e & PTE_PRESENT == 0 || e & PTE_HUGE != 0 {
                return None;
            }
            Some(self.table_at(e & PTE_ADDR_MASK))
        }
    }

    /// Set PCD+PWT on the 2 MiB entry that covers `virt`. Fails if the
    /// mapping is not a 2 MiB leaf.
    unsafe fn set_uc_2m(&mut self, virt: VirtAddr) -> Result<(), MapError> {
        unsafe {
            let pml4 = self.table_at(self.pml4_phys);
            let e = pml4.add(pml4_index(virt)).read();
            if e & PTE_PRESENT == 0 {
                return Err(MapError::AlreadyMapped);
            }
            let pdpt = self.table_at(e & PTE_ADDR_MASK);
            let e = pdpt.add(pdpt_index(virt)).read();
            if e & PTE_PRESENT == 0 || e & PTE_HUGE != 0 {
                return Err(MapError::Overlap);
            }
            let pd = self.table_at(e & PTE_ADDR_MASK);
            let slot = pd.add(pd_index(virt));
            let cur = slot.read();
            if cur & PTE_PRESENT == 0 || cur & PTE_HUGE == 0 {
                return Err(MapError::Overlap);
            }
            slot.write(cur | PTE_UC);
            Ok(())
        }
    }
}

// ---- utilities ----

#[inline]
pub const fn align_up(x: u64, a: u64) -> u64 {
    (x + a - 1) & !(a - 1)
}

#[inline]
pub const fn align_down(x: u64, a: u64) -> u64 {
    x & !(a - 1)
}

// ---- host tests ----

#[cfg(test)]
mod tests {
    use super::*;

    use std::vec;
    use std::vec::Vec;

    // Simple Vec-backed frame pool. Each call to `alloc_zeroed` reserves
    // one 4 KiB slot and returns its "physical" address. hhdm_offset maps
    // that address to the actual host pointer for table reads/writes.
    struct Pool {
        buf: Vec<u8>,
        base_phys: u64,
        used_frames: usize,
        cap_frames: usize,
        hhdm_offset: u64,
    }

    impl Pool {
        fn new(cap_frames: usize) -> Self {
            let base_phys: u64 = 0x0040_0000;
            let mut buf = vec![0u8; cap_frames * PAGE_SIZE as usize];
            // Poison so a walker reading past its allocation is visible.
            for b in buf.iter_mut() {
                *b = 0xCC;
            }
            let ptr = buf.as_ptr() as u64;
            Self {
                buf,
                base_phys,
                used_frames: 0,
                cap_frames,
                hhdm_offset: ptr.wrapping_sub(base_phys),
            }
        }
    }

    impl FrameAllocator for Pool {
        fn alloc_zeroed(&mut self) -> Option<PhysAddr> {
            if self.used_frames >= self.cap_frames {
                return None;
            }
            let idx = self.used_frames;
            self.used_frames += 1;
            let start = idx * PAGE_SIZE as usize;
            let end = start + PAGE_SIZE as usize;
            for b in &mut self.buf[start..end] {
                *b = 0;
            }
            Some(self.base_phys + (idx as u64) * PAGE_SIZE)
        }
    }

    fn make_tables(pool: &mut Pool) -> PageTables {
        let pml4 = pool.alloc_zeroed().expect("pml4 frame");
        unsafe { PageTables::new(pml4, pool.hhdm_offset) }
    }

    #[test]
    fn indices_cover_all_levels() {
        let v: u64 = (1u64 << 47) | (0x123 << 30) | (0x0AB << 21) | (0x055 << 12) | 0x321;
        // pml4 index uses bits 39..=47, so this has bit 47 set: 1 << 8 = 0x100.
        assert_eq!(pml4_index(v), 0x100);
        assert_eq!(pdpt_index(v), 0x123);
        assert_eq!(pd_index(v), 0x0AB);
        assert_eq!(pt_index(v), 0x055);
        assert_eq!(page_offset(v), 0x321);
    }

    #[test]
    fn canonical_is_idempotent_on_kernel_addresses() {
        let hi = 0xFFFF_FFFF_8000_0000u64;
        assert!(is_canonical(hi));
        assert_eq!(canonical(hi), hi);

        let lo = 0x0000_0000_0000_1000u64;
        assert!(is_canonical(lo));

        let noncan = 0x0001_0000_0000_0000u64;
        assert!(!is_canonical(noncan));
    }

    #[test]
    fn map_translate_unmap_4k_roundtrip() {
        let mut pool = Pool::new(64);
        let mut pt = make_tables(&mut pool);
        let virt: u64 = 0xFFFF_FFFF_8000_0000;
        let phys: u64 = 0x0020_0000;
        unsafe {
            pt.map_page(virt, phys, PTE_WRITABLE | PTE_NX, false, &mut pool)
                .expect("map");
        }
        assert_eq!(pt.translate(virt), Some(phys));
        assert_eq!(pt.translate(virt + 0x10), Some(phys + 0x10));
        // A neighbor is still unmapped.
        assert_eq!(pt.translate(virt + 0x1000), None);
        let unmapped = unsafe { pt.unmap_page(virt) };
        assert_eq!(unmapped, Some(phys));
        assert_eq!(pt.translate(virt), None);
    }

    #[test]
    fn map_2mib_translate_and_offset() {
        let mut pool = Pool::new(64);
        let mut pt = make_tables(&mut pool);
        let virt: u64 = 0xFFFF_8000_0020_0000;
        let phys: u64 = 0x0040_0000;
        unsafe {
            pt.map_2m(virt, phys, PTE_WRITABLE | PTE_NX | PTE_GLOBAL, false, &mut pool)
                .expect("map 2m");
        }
        // Translate at start and near the end of the huge page.
        assert_eq!(pt.translate(virt), Some(phys));
        assert_eq!(
            pt.translate(virt + PAGE_SIZE_2M - 1),
            Some(phys + PAGE_SIZE_2M - 1),
        );
        assert_eq!(pt.translate(virt + PAGE_SIZE_2M), None);
    }

    #[test]
    fn map_range_4k_covers_every_page() {
        let mut pool = Pool::new(64);
        let mut pt = make_tables(&mut pool);
        let virt: u64 = 0xFFFF_FFFF_8000_0000;
        let phys: u64 = 0x0080_0000;
        let len: u64 = 5 * PAGE_SIZE;
        unsafe {
            pt.map_range_4k(virt, phys, len, PTE_WRITABLE, false, &mut pool)
                .expect("range");
        }
        for i in 0..5 {
            let off = i * PAGE_SIZE;
            assert_eq!(pt.translate(virt + off), Some(phys + off));
        }
        assert_eq!(pt.translate(virt + 5 * PAGE_SIZE), None);
    }

    #[test]
    fn overlapping_map_without_remap_rejected() {
        let mut pool = Pool::new(64);
        let mut pt = make_tables(&mut pool);
        let v: u64 = 0xFFFF_FFFF_8000_0000;
        let p: u64 = 0x0020_0000;
        unsafe {
            pt.map_page(v, p, PTE_WRITABLE, false, &mut pool).unwrap();
        }
        let err = unsafe { pt.map_page(v, p + 0x1000, PTE_WRITABLE, false, &mut pool) };
        assert_eq!(err, Err(MapError::AlreadyMapped));
        // Remap succeeds and reports the new frame.
        unsafe {
            pt.map_page(v, p + 0x1000, PTE_WRITABLE, true, &mut pool)
                .expect("remap");
        }
        assert_eq!(pt.translate(v), Some(p + 0x1000));
    }

    #[test]
    fn map_4k_inside_2m_rejected() {
        let mut pool = Pool::new(64);
        let mut pt = make_tables(&mut pool);
        let big: u64 = 0xFFFF_FFFF_8000_0000;
        unsafe {
            pt.map_2m(big, 0, PTE_WRITABLE, false, &mut pool).unwrap();
        }
        let err = unsafe { pt.map_page(big + 0x1000, 0x1000, PTE_WRITABLE, false, &mut pool) };
        assert_eq!(err, Err(MapError::Overlap));
    }

    #[test]
    fn misaligned_addresses_rejected() {
        let mut pool = Pool::new(64);
        let mut pt = make_tables(&mut pool);
        let v: u64 = 0xFFFF_FFFF_8000_0000 | 0x123;
        assert_eq!(
            unsafe { pt.map_page(v, 0, PTE_WRITABLE, false, &mut pool) },
            Err(MapError::Misaligned),
        );
        let bad2m: u64 = 0xFFFF_FFFF_8000_0000 | 0x1000;
        assert_eq!(
            unsafe { pt.map_2m(bad2m, 0, PTE_WRITABLE, false, &mut pool) },
            Err(MapError::Misaligned),
        );
    }

    #[test]
    fn frame_exhaustion_returns_no_frame() {
        let mut pool = Pool::new(2); // 1 for PML4, 1 for one sub-table
        let mut pt = make_tables(&mut pool);
        // First map: uses PDPT+PD+PT = 3 frames. Only 1 remains after PML4.
        let err = unsafe {
            pt.map_page(
                0xFFFF_FFFF_8000_0000,
                0x0020_0000,
                PTE_WRITABLE,
                false,
                &mut pool,
            )
        };
        assert_eq!(err, Err(MapError::NoFrame));
    }

    #[test]
    fn patch_physmap_uc_sets_pcd_pwt() {
        let mut pool = Pool::new(64);
        let mut pt = make_tables(&mut pool);
        let physmap_base: u64 = 0xFFFF_8000_0000_0000;
        let phys: u64 = 0;
        let len: u64 = 8 * PAGE_SIZE_2M;
        // Build a mini physmap of 8 2 MiB pages starting at phys 0.
        unsafe {
            pt.map_range_2m(
                physmap_base,
                phys,
                len,
                PTE_WRITABLE | PTE_NX | PTE_GLOBAL,
                false,
                &mut pool,
            )
            .unwrap();
        }
        // Patch the middle two 2 MiB pages via an unaligned request. The
        // function rounds outward so both are patched.
        let touched = unsafe {
            pt.patch_physmap_uc(physmap_base, 2 * PAGE_SIZE_2M + 100, PAGE_SIZE_2M + 4096)
                .unwrap()
        };
        assert_eq!(touched, 2);

        // Read back the PD entries directly. The physmap base sits at PML4
        // index 256, PDPT index 0, PD entries 2 and 3.
        let pml4 = (pt.pml4_phys().wrapping_add(pool.hhdm_offset) as usize) as *mut u64;
        let pml4_e = unsafe { pml4.add(pml4_index(physmap_base)).read() };
        let pdpt = ((pml4_e & PTE_ADDR_MASK).wrapping_add(pool.hhdm_offset) as usize) as *mut u64;
        let pdpt_e = unsafe { pdpt.add(0).read() };
        let pd = ((pdpt_e & PTE_ADDR_MASK).wrapping_add(pool.hhdm_offset) as usize) as *mut u64;
        for i in 0..8 {
            let e = unsafe { pd.add(i).read() };
            let uc = e & PTE_UC == PTE_UC;
            assert_eq!(uc, i == 2 || i == 3, "index {i} uc={uc}");
        }
    }


    #[test]
    fn mmio_window_constants_agree_with_design() {
        // DESIGN §4.1: 0xFFFF_E000_0000_0000 – 0xFFFF_E000_1000_0000
        assert_eq!(MMIO_WINDOW_START, 0xFFFF_E000_0000_0000);
        assert_eq!(MMIO_WINDOW_END, 0xFFFF_E000_1000_0000);
        assert_eq!(MMIO_WINDOW_LEN, 256 * 1024 * 1024);
        assert!(is_canonical(MMIO_WINDOW_START));
        assert!(is_canonical(MMIO_WINDOW_END - 1));
    }
}
