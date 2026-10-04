//! Portable page-table types and 4-level walk. DESIGN §4.3.
//!
//! The kernel builds its own PML4 from buddy frames and never edits
//! Limine's tables again. The pieces that touch CR3/EFER/`invlpg` live
//! in the binary crate (`src/mm/paging_init.rs`, `src/arch/x86_64/cpu.rs`); everything
//! algorithmic (index math, PTE encoding, walk, split-into-2MiB) lives
//! here so `cargo test --lib` covers it.
//!
//! ## Design notes
//!
//! - Three page sizes: 4 KiB at level 1, 2 MiB at level 2, 1 GiB at
//!   level 3. `map_range` still picks 4 KiB and 2 MiB; 1 GiB is only
//!   through `map_page(Size1G)` (the physmap builder).
//! - Table frames come from a caller-supplied `FrameAlloc` as `pmm::Frames`
//!   tokens, so host tests can back the allocator with a `Vec`-backed
//!   buddy (`pmm::testing::Pool`). In the kernel it pulls from the buddy
//!   PMM. A table's token is consumed into the entry that points at it
//!   and rebuilt only by the code that clears that entry.
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
//! User half is `0x0`..`USER_END` (DESIGN §4.1). Address spaces live in
//! [`crate::addr_space`]. Demand paging stays phase 12.
//!
//! Real TLB shootdown IPIs: `tlb_shootdown_others` is a hook the
//! kernel installs (DESIGN §4.3 / §7.9). Host tests leave it unset.

#![allow(clippy::identity_op)] // PTE masks read as `x << n` even when n is 0

use core::marker::PhantomData;

use crate::atomic::statics::{AtomicPtr, Ordering};
use crate::ipi::ShootRange;
use crate::pmm::Frames;

pub const PAGE_SHIFT: u32 = 12;
pub const PAGE_SIZE_4K: u64 = 1 << PAGE_SHIFT;
pub const PAGE_SIZE_2M: u64 = 1 << 21;
pub const PAGE_SIZE_1G: u64 = 1 << 30;

/// User canonical half, exclusive end. DESIGN §4.1.
pub const USER_END: u64 = 0x0000_8000_0000_0000;
pub const USER_MAX: u64 = USER_END - 1;
/// Exclusive end of what a user mapping may cover: the top page of the
/// canonical half stays unmapped, so a `syscall` in the last mapped page
/// cannot leave a non-canonical return RIP (DESIGN §4.1, C-USERMAPEND).
/// The ELF loader and every user range check use it.
pub const USER_MAP_END: u64 = USER_END - PAGE_SIZE_4K;
const _: () = assert!(USER_MAP_END == 0x0000_7FFF_FFFF_F000);
/// First page of the user half stays unmapped (null deref). ROADMAP §9.2.
pub const NULL_GUARD_LEN: u64 = PAGE_SIZE_4K;

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
}

/// Which page size a leaf entry maps.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PageSize {
    Size4K,
    Size2M,
    Size1G,
}

impl PageSize {
    pub const fn bytes(self) -> u64 {
        match self {
            PageSize::Size4K => PAGE_SIZE_4K,
            PageSize::Size2M => PAGE_SIZE_2M,
            PageSize::Size1G => PAGE_SIZE_1G,
        }
    }

    pub const fn leaf_level(self) -> u8 {
        match self {
            PageSize::Size4K => 1,
            PageSize::Size2M => 2,
            PageSize::Size1G => 3,
        }
    }
}

#[inline]
const fn page_size_at(level: u8) -> PageSize {
    match level {
        1 => PageSize::Size4K,
        2 => PageSize::Size2M,
        _ => PageSize::Size1G,
    }
}

/// PTE flag set. Held as a raw u64 so table entries round-trip losslessly
/// (including the OS-reserved bits phase 12 will want for e.g. CoW). The
/// values are x86_64's, so that port's `PageTable` encoding is the identity
/// on them; another port maps them to its own bits in `make_entry` and
/// `entry_flags` (PORTABILITY §11.1).
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
    /// Contiguous hint (aarch64 descriptor bit 52). Software-available on x86.
    pub const CONTIGUOUS: u64 = 1 << 52;
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

/// The page-table format, the root register and this CPU's TLB: the
/// `PageTable` seam trait, which `crate::arch` re-exports (PORTABILITY
/// §11.1).
///
/// The format is the port's pure half (`arch::<name>::paging`), which the
/// portable `Mapper` walks through these items; `PageFlags` keeps x86_64's
/// bit values, so on that port `make_entry` and `entry_flags` pass the flag
/// bits through unchanged.
pub trait PageTable {
    /// Levels of the walk; the root is level `LEVELS`, leaves of the
    /// smallest page are level 1.
    const LEVELS: u8;
    /// Entries in one table.
    const ENTRIES: usize;
    /// Root slots `KERNEL_ROOT_FIRST..ENTRIES` are the kernel half every
    /// address space shares.
    const KERNEL_ROOT_FIRST: usize;
    /// First kernel-half VA (TTBR1 / high canonical).
    const KERNEL_VA_START: u64;
    /// UXN bit in `PageFlags` for kernel-half leaves. Zero on x86_64.
    const KERNEL_UXN: u64;
    /// The index `va` selects in a table at `level`, below `ENTRIES`.
    fn index(va: VirtAddr, level: u8) -> usize;
    /// A leaf that maps `pa` at `va` with `flags`.
    fn make_entry(va: VirtAddr, pa: PhysAddr, flags: PageFlags) -> u64;
    /// An interior table descriptor pointing at `pa`. No leaf permissions.
    fn make_table(pa: PhysAddr) -> u64;
    /// Whether `va` is canonical for this port.
    fn va_ok(va: u64) -> bool;
    /// Whether `va` is in the kernel half.
    fn is_kernel_va(va: VirtAddr) -> bool;
    /// The physical address `entry` points at.
    fn entry_phys(entry: u64) -> PhysAddr;
    /// The flags of `entry`.
    fn entry_flags(entry: u64) -> PageFlags;
    /// The root this CPU runs on.
    fn root() -> PhysAddr;
    /// Switch this CPU to `root`.
    ///
    /// # Safety
    ///
    /// `root` is a complete top-level table that maps this CPU's code, its
    /// stack, and everything it touches after the switch.
    unsafe fn set_root(root: PhysAddr);
    /// Drop this CPU's translation of the page holding `va`.
    fn flush_local(va: VirtAddr);
    /// Drop this CPU's non-global translations.
    fn flush_local_all();
}

/// Frame allocator abstraction. Hands out order-0 [`Frames`], one
/// `PAGE_SIZE_4K` frame each. Trait rather than a closure so the mapper
/// can call it from multiple methods without lifetime gymnastics.
///
/// # Safety
/// Implementations return order-0 tokens whose frame's virtual mapping
/// through `Mapper::hhdm_offset` is writable.
pub unsafe trait FrameAlloc {
    fn alloc_frame(&mut self) -> Option<Frames>;
}

/// Errors from map / unmap / translate.
#[must_use]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MapError {
    /// Address not aligned to the requested page size.
    Misaligned,
    /// A leaf entry already covers this VA and the caller did not ask
    /// for `Remap` or `Invalidate`.
    AlreadyMapped,
    /// The VA has no present leaf.
    NotMapped,
    /// `Remap` would change a live leaf's type, output address, size,
    /// Contiguous bit, or make nG global. Invalidate first (ROADMAP §11.2).
    LiveChange,
    /// Frame allocator returned `None` while walking down.
    OutOfFrames,
    /// A 2 MiB request landed under a 4 KiB leaf, or vice versa.
    /// DESIGN §4.3 forbids implicit splits.
    PageSizeMismatch,
    /// Virtual address is non-canonical.
    NonCanonical,
}

/// A mapping request's errno: Linux's for each condition.
impl From<MapError> for crate::kerror::KError {
    fn from(e: MapError) -> Self {
        match e {
            MapError::OutOfFrames => Self::NoMem,
            MapError::AlreadyMapped => Self::Exist,
            MapError::Misaligned
            | MapError::NotMapped
            | MapError::PageSizeMismatch
            | MapError::NonCanonical
            | MapError::LiveChange => Self::Inval,
        }
    }
}

/// Whether `map_page` may overwrite an existing present leaf.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MapMode {
    /// Fail with `MapError::AlreadyMapped` if the target PTE is present.
    /// The default; matches DESIGN §4.3's "assert on overlap" rule.
    Fresh,
    /// Permission-only overwrite, or global → nG. A live type, PA, size,
    /// Contiguous, or nG → global change is `LiveChange`.
    Remap,
    /// Break-before-make: clear the leaf, flush this CPU, then store.
    Invalidate,
}

/// Canonicalization check: bits 48..64 must all equal bit 47.
#[inline]
pub const fn is_canonical(va: u64) -> bool {
    let hi = va >> 47;
    hi == 0 || hi == 0x1_FFFF
}

/// Mapper over a single root table, in the format port `A`'s pure half
/// encodes (PORTABILITY §11.1).
///
/// Auto-`Send + Sync`, since it holds only a root address and an offset,
/// so nothing in the type serializes a root: callers do, through the
/// page-table lock (DESIGN §2.1 puts page tables first in the lock order).
pub struct Mapper<A: PageTable> {
    root: PhysAddr,
    hhdm_offset: u64,
    _port: PhantomData<fn() -> A>,
}

impl<A: PageTable> Mapper<A> {
    /// # Safety
    /// `root` must point at a zeroed, page-aligned PML4 frame that the
    /// caller owns for the lifetime of this `Mapper`. `hhdm_offset` must
    /// map every table frame's phys addr to a writable virt.
    pub const unsafe fn new(root: PhysAddr, hhdm_offset: u64) -> Self {
        Self {
            root,
            hhdm_offset,
            _port: PhantomData,
        }
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
    pub unsafe fn map_page<F: FrameAlloc>(
        &mut self,
        va: VirtAddr,
        pa: PhysAddr,
        flags: PageFlags,
        size: PageSize,
        mode: MapMode,
        alloc: &mut F,
    ) -> Result<(), MapError> {
        if !A::va_ok(va.0) {
            return Err(MapError::NonCanonical);
        }
        let step = size.bytes();
        if !va.is_aligned(step) || !pa.is_aligned(step) {
            return Err(MapError::Misaligned);
        }

        let mut table_phys = self.root;
        let leaf_level = size.leaf_level();

        let mut level = A::LEVELS;
        while level > leaf_level {
            let idx = A::index(va, level);
            // SAFETY: `table_phys` is this root or a table the walk found
            // under it, and every table this root reaches is a frame reached at `phys + hhdm_offset` (`mm::paging::Mapper::new`'s contract; invariant I14 for the kernel's); `idx < A::ENTRIES` (`arch::PageTable::index`) stays inside it.
            let entry_ptr = unsafe { self.table_ptr(table_phys).add(idx) };
            // SAFETY: `entry_ptr` points into a live table, as above
            // (`mm::paging::Mapper::new`'s contract).
            let entry = unsafe { entry_ptr.read_volatile() };

            if !A::entry_flags(entry).contains(PageFlags::PRESENT) {
                let f = alloc.alloc_frame().ok_or(MapError::OutOfFrames)?;
                debug_assert_eq!(f.order(), 0, "paging: FrameAlloc gave order {}", f.order());
                // The table's token moves into the entry written below;
                // `free_level` takes it back when it clears that entry.
                let new = PhysAddr(f.into_entry());
                // SAFETY: `zero_frame`'s contract; `new` is the order-0
                // frame the token above owned, reached at `hhdm_offset` as
                // `FrameAlloc`'s contract (`mm::paging::FrameAlloc`) says.
                unsafe { self.zero_frame(new) };
                // SAFETY: `entry_ptr` points into a live table (`mm::paging::Mapper::new`'s contract);
                // `&mut self` makes this the root's one writer, and for the
                // kernel root the page-table lock is held (invariant I48,
                // established at `mm::paging_init::current_mapper`).
                unsafe { entry_ptr.write_volatile(A::make_table(new)) };
                table_phys = new;
            } else {
                if A::entry_flags(entry).contains(PageFlags::HUGE) {
                    return Err(MapError::PageSizeMismatch);
                }
                table_phys = A::entry_phys(entry);
            }
            level -= 1;
        }

        let idx = A::index(va, leaf_level);
        // SAFETY: `table_phys` is a table the walk above reached, and
        // every table this root reaches is a frame reached at `phys + hhdm_offset` (`mm::paging::Mapper::new`'s contract; invariant I14 for the kernel's); `idx < A::ENTRIES` (`arch::PageTable::index`).
        let entry_ptr = unsafe { self.table_ptr(table_phys).add(idx) };
        // SAFETY: `entry_ptr` points into a live table (`mm::paging::Mapper::new`'s contract).
        let existing = unsafe { entry_ptr.read_volatile() };
        let present = A::entry_flags(existing).contains(PageFlags::PRESENT);
        if present {
            match mode {
                MapMode::Fresh => return Err(MapError::AlreadyMapped),
                MapMode::Remap => {
                    let existing_huge = A::entry_flags(existing).contains(PageFlags::HUGE);
                    let want_huge = !matches!(size, PageSize::Size4K);
                    if existing_huge != want_huge {
                        return Err(MapError::PageSizeMismatch);
                    }
                }
                MapMode::Invalidate => {}
            }
        }

        let mut leaf_flags = flags.with(PageFlags::PRESENT);
        if !matches!(size, PageSize::Size4K) {
            leaf_flags = leaf_flags.with(PageFlags::HUGE);
        }
        if A::is_kernel_va(va) {
            leaf_flags = leaf_flags.with(A::KERNEL_UXN);
        }

        if present
            && mode == MapMode::Remap
            && live_change(
                A::entry_phys(existing),
                A::entry_flags(existing),
                pa,
                leaf_flags,
            )
        {
            return Err(MapError::LiveChange);
        }
        if present && mode == MapMode::Invalidate {
            // SAFETY: as the write below; this is the invalid store of
            // break-before-make (ROADMAP §11.2), established here.
            unsafe { entry_ptr.write_volatile(0) };
            A::flush_local(va);
        }

        // SAFETY: `entry_ptr` points into a live table (`mm::paging::Mapper::new`'s contract); the
        // caller vouches for `pa` (this fn's `# Safety` contract,
        // established here), and the page-table lock covers a kernel-root
        // write (invariant I48, established at
        // `mm::paging_init::current_mapper`).
        unsafe { entry_ptr.write_volatile(A::make_entry(va, pa, leaf_flags)) };
        Ok(())
    }

    /// Raw leaf word at `va`, if present.
    pub fn leaf_raw(&self, va: VirtAddr) -> Option<u64> {
        let mut table_phys = self.root;
        let mut level = A::LEVELS;
        while level >= 1 {
            let idx = A::index(va, level);
            // SAFETY: `table_phys` is this root or a table the walk found
            // under it (`mm::paging::Mapper::new`'s contract); `idx < A::ENTRIES`.
            let entry = unsafe { self.table_ptr(table_phys).add(idx).read_volatile() };
            if !A::entry_flags(entry).contains(PageFlags::PRESENT) {
                return None;
            }
            if level == 1 || A::entry_flags(entry).contains(PageFlags::HUGE) {
                return Some(entry);
            }
            table_phys = A::entry_phys(entry);
            level -= 1;
        }
        None
    }

    /// Map `[va, va+len)` to `[pa, pa+len)` using the largest natural
    /// pages that fit alignment and length. Both endpoints must be
    /// aligned to at least 4 KiB.
    ///
    /// # Safety
    /// Same contract as `map_page`.
    pub unsafe fn map_range<F: FrameAlloc>(
        &mut self,
        va: VirtAddr,
        pa: PhysAddr,
        len: u64,
        flags: PageFlags,
        mode: MapMode,
        alloc: &mut F,
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
            // SAFETY: `map_page`'s contract, which this fn's `# Safety`
            // passes on for the whole range, established here.
            unsafe { self.map_page(cur_va, cur_pa, flags, size, mode, alloc)? };
            off += step;
        }
        Ok(())
    }

    /// Unmap one 4 KiB or 2 MiB page. Returns the physical frame that
    /// was mapped, or `None` if the VA was not present.
    ///
    /// Interior tables are NOT freed — that requires refcounting and
    /// belongs in phase 12.
    ///
    /// # Safety
    /// Caller is responsible for TLB invalidation: `invlpg` on this CPU and,
    /// for a kernel-half VA, `tlb_shootdown_others` (DESIGN §4.3, §7.9).
    pub unsafe fn unmap_page(&mut self, va: VirtAddr) -> Option<(PhysAddr, PageSize)> {
        let mut table_phys = self.root;
        let mut level = A::LEVELS;
        while level >= 1 {
            let idx = A::index(va, level);
            // SAFETY: `table_phys` is this root or a table the walk found
            // under it, and every table this root reaches is a frame reached at `phys + hhdm_offset` (`mm::paging::Mapper::new`'s contract; invariant I14 for the kernel's); `idx < A::ENTRIES` (`arch::PageTable::index`).
            let entry_ptr = unsafe { self.table_ptr(table_phys).add(idx) };
            // SAFETY: `entry_ptr` points into a live table (`mm::paging::Mapper::new`'s contract).
            let entry = unsafe { entry_ptr.read_volatile() };
            if !A::entry_flags(entry).contains(PageFlags::PRESENT) {
                return None;
            }
            let is_leaf = level == 1 || A::entry_flags(entry).contains(PageFlags::HUGE);
            if is_leaf {
                let size = page_size_at(level);
                let phys = A::entry_phys(entry);
                // SAFETY: `entry_ptr` points into a live table (`mm::paging::Mapper::new`'s contract),
                // and the page-table lock covers a kernel-root write
                // (invariant I48, established at
                // `mm::paging_init::current_mapper`).
                unsafe { entry_ptr.write_volatile(0) };
                return Some((phys, size));
            }
            table_phys = A::entry_phys(entry);
            level -= 1;
        }
        None
    }

    /// Return the physical address a VA translates to, or `None` if
    /// unmapped. Does not consult the TLB (host-testable).
    pub fn translate(&self, va: VirtAddr) -> Option<(PhysAddr, PageSize, PageFlags)> {
        let mut table_phys = self.root;
        let mut level = A::LEVELS;
        while level >= 1 {
            let idx = A::index(va, level);
            // SAFETY: `table_phys` is this root or a table the walk found
            // under it, and every table this root reaches is a frame reached at `phys + hhdm_offset` (`mm::paging::Mapper::new`'s contract; invariant I14 for the kernel's); `idx < A::ENTRIES` (`arch::PageTable::index`).
            let entry = unsafe { self.table_ptr(table_phys).add(idx).read_volatile() };
            if !A::entry_flags(entry).contains(PageFlags::PRESENT) {
                return None;
            }
            let is_leaf = level == 1 || A::entry_flags(entry).contains(PageFlags::HUGE);
            if is_leaf {
                let size = page_size_at(level);
                let page_base = A::entry_phys(entry).0;
                let offset = va.0 & (size.bytes() - 1);
                return Some((PhysAddr(page_base + offset), size, A::entry_flags(entry)));
            }
            table_phys = A::entry_phys(entry);
            level -= 1;
        }
        None
    }

    /// What covers `va`: a present leaf, or how far we can skip because
    /// a higher table slot is empty. Used by the range walker and by
    /// "assert this region is unmapped" checks (DESIGN §4.1).
    pub fn probe(&self, va: VirtAddr) -> Probe {
        if !A::va_ok(va.0) {
            if va.0 < A::KERNEL_VA_START {
                return Probe::Skip(A::KERNEL_VA_START - va.0);
            }
            return Probe::Skip(PAGE_SIZE_4K);
        }
        let mut table_phys = self.root;
        let mut level: u8 = A::LEVELS;
        loop {
            let idx = A::index(va, level);
            // SAFETY: `table_phys` is this root or a table the walk found
            // under it, and every table this root reaches is a frame reached at `phys + hhdm_offset` (`mm::paging::Mapper::new`'s contract; invariant I14 for the kernel's); `idx < A::ENTRIES` (`arch::PageTable::index`).
            let entry = unsafe { self.table_ptr(table_phys).add(idx).read_volatile() };
            if !A::entry_flags(entry).contains(PageFlags::PRESENT) {
                return Probe::Skip(slot_remaining(va.0, level));
            }
            let is_leaf = level == 1 || A::entry_flags(entry).contains(PageFlags::HUGE);
            if is_leaf {
                let size = page_size_at(level);
                return Probe::Mapped {
                    pa: A::entry_phys(entry),
                    size,
                    flags: A::entry_flags(entry),
                };
            }
            table_phys = A::entry_phys(entry);
            level -= 1;
        }
    }

    /// True iff nothing in `[start, end)` is present. Walks by table
    /// slot so a 64 GiB hole is not 16 million 4 KiB probes.
    pub fn range_unmapped(&self, start: VirtAddr, end: VirtAddr) -> bool {
        let mut va = start.0;
        while va < end.0 {
            match self.probe(VirtAddr(va)) {
                Probe::Skip(n) => {
                    let step = n.max(1);
                    va = va.saturating_add(step);
                    if va <= start.0 {
                        break;
                    }
                }
                Probe::Mapped { .. } => return false,
            }
        }
        true
    }

    /// Walk `[start, end)` coalescing adjacent leaves that share flags
    /// and page size. `visit(va, len, flags, size)`.
    pub fn walk_ranges<F>(&self, start: VirtAddr, end: VirtAddr, mut visit: F)
    where
        F: FnMut(u64, u64, PageFlags, PageSize),
    {
        let mut va = start.0;
        let mut run: Option<(u64, u64, PageFlags, PageSize)> = None;
        while va < end.0 {
            match self.probe(VirtAddr(va)) {
                Probe::Skip(n) => {
                    if let Some((rva, rlen, rf, rs)) = run.take() {
                        visit(rva, rlen, rf, rs);
                    }
                    let step = n.max(1);
                    let next = va.saturating_add(step);
                    if next <= va {
                        break;
                    }
                    va = next;
                }
                Probe::Mapped { size, flags, .. } => {
                    let span = size.bytes();
                    let leaf_base = va & !(span - 1);
                    let leaf_end = leaf_base.saturating_add(span);
                    // A/D bits are hardware-updated and must not split a
                    // run of otherwise identical leaves (ROADMAP §1.7).
                    const AD: u64 = PageFlags::ACCESSED | PageFlags::DIRTY;
                    match run {
                        Some((rva, rlen, rf, rs))
                            if rs == size
                                && (rf.0 & !AD) == (flags.0 & !AD)
                                && rva + rlen == leaf_base =>
                        {
                            run = Some((rva, rlen + span, rf, rs));
                        }
                        Some((rva, rlen, rf, rs)) => {
                            visit(rva, rlen, rf, rs);
                            run = Some((leaf_base, span, flags, size));
                        }
                        None => run = Some((leaf_base, span, flags, size)),
                    }
                    if leaf_end <= va {
                        break;
                    }
                    va = leaf_end;
                }
            }
        }
        if let Some((rva, rlen, rf, rs)) = run {
            visit(rva, rlen, rf, rs);
        }
    }

    /// Copy root slots `A::KERNEL_ROOT_FIRST..` from `src`. Those entries point
    /// at the same kernel PDPTs; later leaf maps in the kernel half are
    /// visible to every address space. Do not copy `0..A::KERNEL_ROOT_FIRST`
    /// (low identity stays on the kernel CR3 only).
    pub fn copy_kernel_half_from(&mut self, src: &Mapper<A>) {
        let src_ptr = src.table_ptr(src.root);
        let dst_ptr = self.table_ptr(self.root);
        let mut i = A::KERNEL_ROOT_FIRST;
        while i < A::ENTRIES {
            // SAFETY: `src`'s root is a live PML4 reached at its offset
            // (`mm::paging::Mapper::new`'s contract), and `i < A::ENTRIES`.
            let e = unsafe { src_ptr.add(i).read_volatile() };
            // SAFETY: this root is a live PML4 reached at `hhdm_offset`
            // (`mm::paging::Mapper::new`'s contract), and `&mut self` makes this its one writer;
            // `i < A::ENTRIES`.
            unsafe { dst_ptr.add(i).write_volatile(e) };
            i += 1;
        }
    }

    pub fn pml4_entry(&self, idx: usize) -> u64 {
        assert!(idx < A::ENTRIES, "paging: pml4_entry index {idx}");
        // SAFETY: the root is a live PML4 reached at `hhdm_offset`
        // (`mm::paging::Mapper::new`'s contract), and the assert keeps `idx` inside it.
        unsafe { self.table_ptr(self.root).add(idx).read_volatile() }
    }

    /// Walk the user half (root slots `0..A::KERNEL_ROOT_FIRST`), clear every
    /// present entry and hand `free` the frame each one held, leaves and
    /// interior tables alike; leave kernel-half entries untouched. Does
    /// not free `self.root`.
    ///
    /// # Safety
    /// Every present user-half entry holds an order-0 frame whose
    /// [`Frames`] was consumed into it with `into_entry` (interior tables
    /// by `map_page`, leaves by their owner), and no other token names
    /// it. User leaves are 4 KiB; a 2 MiB leaf is a kernel bug.
    pub unsafe fn free_user_half<F>(&mut self, free: &mut F) -> UserFreeStats
    where
        F: FnMut(Frames),
    {
        let mut stats = UserFreeStats {
            leaves: 0,
            tables: 0,
        };
        // SAFETY: `free_level`'s contract; the root is this mapper's live
        // PML4 (`mm::paging::Mapper::new`'s contract), and this fn's `# Safety` contract covers
        // its user half, established here.
        unsafe { self.free_level(self.root, A::LEVELS, true, free, &mut stats) };
        stats
    }

    /// Clear and free every present entry of `table` at `level` (only the
    /// user half when `pml4`), recursing into interior tables first.
    ///
    /// # Safety
    /// `table` is a live table of this mapper at `level`, reached at
    /// `hhdm_offset`, and each present entry it covers (below
    /// `A::KERNEL_ROOT_FIRST` when `pml4`) holds an order-0 frame whose
    /// [`Frames`] was consumed into it with `into_entry` and that no other
    /// token names, as [`Mapper::free_user_half`] requires.
    unsafe fn free_level<F: FnMut(Frames)>(
        &mut self,
        table: PhysAddr,
        level: u8,
        pml4: bool,
        free: &mut F,
        stats: &mut UserFreeStats,
    ) {
        let ptr = self.table_ptr(table);
        let end = if pml4 && level == A::LEVELS {
            A::KERNEL_ROOT_FIRST
        } else {
            A::ENTRIES
        };
        let mut i = 0usize;
        while i < end {
            // SAFETY: `table` is live by this fn's `# Safety` contract,
            // established here, and `i < A::ENTRIES`.
            let e = unsafe { ptr.add(i).read_volatile() };
            if !A::entry_flags(e).contains(PageFlags::PRESENT) {
                i += 1;
                continue;
            }
            let child = A::entry_phys(e);
            let huge = A::entry_flags(e).contains(PageFlags::HUGE);
            let leaf = level == 1 || huge;
            if leaf {
                assert!(level == 1 && !huge, "addrspace: unexpected huge user leaf");
                stats.leaves += 1;
            } else {
                // SAFETY: `free_level`'s contract; `child` is the live
                // interior table entry `i` points at, and this fn's
                // `# Safety` contract covers its entries, established here.
                unsafe { self.free_level(child, level - 1, false, free, stats) };
                stats.tables += 1;
            }
            // Clear the entry before its frame is rebuilt and freed.
            // SAFETY: `table` is live by this fn's `# Safety` contract, and
            // `&mut self` makes this its one writer, established here.
            unsafe { ptr.add(i).write_volatile(0) };
            // SAFETY: entry `i` of this table held `child`, an order-0 frame
            // consumed with `into_entry` (this fn's `# Safety`, from
            // `free_user_half`'s caller), and the store above just cleared
            // it; the contract `mm::pmm::Frames::from_entry` states.
            free(unsafe { Frames::from_entry(child.as_u64(), 0) });
            i += 1;
        }
    }

    /// Zero a freshly-allocated frame through the HHDM physmap.
    ///
    /// # Safety
    /// `phys` must be an owned, page-sized frame reachable via
    /// `hhdm_offset`.
    pub unsafe fn zero_frame(&self, phys: PhysAddr) {
        let ptr = self.table_ptr(phys);
        for i in 0..A::ENTRIES {
            // SAFETY: `phys` is an owned page reached at `hhdm_offset` by
            // this fn's `# Safety` contract, established here; `i < A::ENTRIES`
            // words stay inside it.
            unsafe { ptr.add(i).write_volatile(0) };
        }
    }
}

/// Reservation over the ioremap window (DESIGN §4.1: `0xFFFF_E000_0000_0000`,
/// 256 MiB). Bump allocator: a mapping is never freed, and a failed map
/// that left nothing in its VA gives its reservation back
/// ([`IoremapWindow::unreserve`]). Devices call `ioremap` for MMIO
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

    /// Give back the reservation `reserve` just returned as `va` for
    /// `[phys, phys+len)`, after its map failed. Only the latest
    /// reservation goes back, by moving the cursor down to it; `ioremap`
    /// holds the lock that guards the window from its `reserve` to here,
    /// so nothing was reserved after it. Returns whether it went back.
    pub fn unreserve(&mut self, va: VirtAddr, len: u64) -> bool {
        let page_off = va.0 & (PAGE_SIZE_4K - 1);
        let start = va.0 - page_off;
        let Some(end) = page_off
            .checked_add(len)
            .and_then(|n| n.checked_add(PAGE_SIZE_4K - 1))
            .map(|n| n & !(PAGE_SIZE_4K - 1))
            .and_then(|n| start.checked_add(n))
        else {
            return false;
        };
        if len == 0 || end != self.next || start < IOREMAP_BASE {
            return false;
        }
        self.next = start;
        true
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

/// Result of [`Mapper::probe`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// No present leaf; skip this many bytes (at least one table slot).
    Skip(u64),
    Mapped {
        pa: PhysAddr,
        size: PageSize,
        flags: PageFlags,
    },
}

/// True when `Remap` would change a live leaf in a way that needs BBM.
fn live_change(old_pa: PhysAddr, old: PageFlags, new_pa: PhysAddr, new: PageFlags) -> bool {
    if old_pa != new_pa {
        return true;
    }
    let type_bits = PageFlags::PCD | PageFlags::PWT | PageFlags::HUGE | PageFlags::CONTIGUOUS;
    if (old.0 & type_bits) != (new.0 & type_bits) {
        return true;
    }
    !old.contains(PageFlags::GLOBAL) && new.contains(PageFlags::GLOBAL)
}

/// Bytes from `va` to the end of the page-table slot at `level`.
fn slot_remaining(va: u64, level: u8) -> u64 {
    let span = 1u64 << (12 + 9 * (level as u32 - 1));
    let slot_base = va & !(span - 1);
    let slot_end = slot_base.wrapping_add(span);
    if slot_end > va { slot_end - va } else { span }
}

/// Standard MMIO leaf flags per DESIGN §4.3 PTE flag policy table.
pub const fn mmio_flags() -> PageFlags {
    PageFlags(
        PageFlags::PRESENT
            | PageFlags::WRITABLE
            | PageFlags::GLOBAL
            | PageFlags::NX
            | PageFlags::PCD
            | PageFlags::PWT,
    )
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

/// Heap and kernel-stack leaves: same as data (writable, NX, global).
pub const fn heap_flags() -> PageFlags {
    kernel_data_flags()
}

pub const fn stack_flags() -> PageFlags {
    kernel_data_flags()
}

/// User leaf: present + U. `write` / `exec` as requested. Never GLOBAL.
pub const fn user_leaf_flags(write: bool, exec: bool) -> PageFlags {
    let mut f = PageFlags::PRESENT | PageFlags::USER;
    if write {
        f |= PageFlags::WRITABLE;
    }
    if !exec {
        f |= PageFlags::NX;
    }
    PageFlags(f)
}

/// Frames reclaimed from the user half of a PML4. `tables` excludes the
/// PML4 itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UserFreeStats {
    pub leaves: usize,
    pub tables: usize,
}

/// TLB shootdown hook. Kernel installs the IPI 0xFC path (DESIGN §7.9).
/// Host tests and pre-SMP boot leave this unset (local `invlpg` is enough).
static SHOOTDOWN_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

pub fn set_tlb_shootdown_hook(f: fn(&[ShootRange])) {
    // Release: pairs with the Acquire load in `tlb_shootdown_ranges`.
    SHOOTDOWN_HOOK.store(f as *mut (), Ordering::Release);
}

/// Intentionally free-standing (not a method on `Mapper`) so architecture
/// code can call it after any leaf edit including MMIO patches.
#[inline]
pub fn tlb_shootdown_others(va: VirtAddr) {
    tlb_shootdown_ranges(&[ShootRange::page(va.as_u64())]);
}

/// Invalidate every page of `ranges` on every other CPU, one round per
/// `ipi::SHOOT_RANGES` ranges (DESIGN §7.9), after the caller has cleared
/// their PTEs and run its local `invlpg`s.
#[inline]
pub fn tlb_shootdown_ranges(ranges: &[ShootRange]) {
    // Acquire: pairs with the Release store in `set_tlb_shootdown_hook`.
    let p = SHOOTDOWN_HOOK.load(Ordering::Acquire);
    if p.is_null() {
        return;
    }
    // SAFETY: the only non-null value `SHOOTDOWN_HOOK` ever holds is a
    // `fn(&[ShootRange])` cast to a pointer, stored by
    // `mm::paging::set_tlb_shootdown_hook`; a fn pointer and `*mut ()` have
    // the same size.
    let f: fn(&[ShootRange]) = unsafe { core::mem::transmute(p) };
    f(ranges);
}

// Host tests take their table frames from the shared buddy pool.
// SAFETY: `FrameAlloc`'s contract; the pool's buddy hands out order-0
// frames of host memory it owns, writable at the pool's offset, the one
// every test `Mapper` is built with (`mm::pmm::Buddy::alloc` on the
// pool's buddy).
#[cfg(test)]
unsafe impl FrameAlloc for crate::pmm::testing::Pool {
    fn alloc_frame(&mut self) -> Option<Frames> {
        self.buddy.alloc(0)
    }
}

// ------------------ host tests ------------------

#[cfg(test)]
#[path = "paging_tests.rs"]
mod tests;
