//! Address space: user half of a PML4 plus region tracking. ROADMAP §9.2.
//!
//! Kernel half (PML4[256..512)) is shared by copying the kernel's upper
//! entries so they point at the same PDPTs. VA 0 is never mapped. The
//! kernel's low identity window is torn down after `smp: done`, global TLB
//! entries flushed on every CPU, so a user map in its range aliases
//! nothing; only the trampoline page stays, not global.

use crate::paging::{
    FrameAlloc, KERNEL_PML4_FIRST, MapError, MapMode, Mapper, NULL_GUARD_LEN, PAGE_SIZE_4K,
    PTES_PER_TABLE, PageFlags, PageSize, PhysAddr, Probe, USER_MAP_END, VirtAddr, is_canonical,
    user_leaf_flags,
};
use crate::pmm::Frames;
use crate::proc::uaccess::user_range_ok;

pub use crate::limits::MAX_REGIONS;

// `prot` bits, as Linux's include/uapi/asm-generic/mman-common.h defines
// them.
pub const PROT_READ: u64 = 0x1;
pub const PROT_WRITE: u64 = 0x2;
pub const PROT_EXEC: u64 = 0x4;

// `mmap` flags. The map types are Linux's include/uapi/linux/mman.h;
// `MAP_NORESERVE` is include/uapi/asm-generic/mman.h; the rest are
// include/uapi/asm-generic/mman-common.h.
pub const MAP_SHARED: u64 = 0x1;
pub const MAP_PRIVATE: u64 = 0x2;
/// The map-type field (`MAP_TYPE` in include/uapi/linux/mman.h).
pub const MAP_TYPE: u64 = 0xf;
pub const MAP_FIXED: u64 = 0x10;
pub const MAP_ANONYMOUS: u64 = 0x20;
pub const MAP_NORESERVE: u64 = 0x4000;
pub const MAP_POPULATE: u64 = 0x8000;
pub const MAP_STACK: u64 = 0x2_0000;
pub const MAP_FIXED_NOREPLACE: u64 = 0x10_0000;

/// Every flag `mmap` accepts; any other bit is `EINVAL`.
const MAP_ACCEPTED: u64 = MAP_TYPE
    | MAP_FIXED
    | MAP_ANONYMOUS
    | MAP_NORESERVE
    | MAP_POPULATE
    | MAP_STACK
    | MAP_FIXED_NOREPLACE;

/// Top of the `mmap` area: 128 MiB below `USER_MAP_END`, where Linux puts
/// its mmap base with randomization off. Placement runs down from here.
pub const MMAP_TOP: u64 = USER_MAP_END - (128 << 20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserPerms {
    pub write: bool,
    pub exec: bool,
}

impl UserPerms {
    pub const READ: Self = Self {
        write: false,
        exec: false,
    };
    pub const RW: Self = Self {
        write: true,
        exec: false,
    };
    pub const RX: Self = Self {
        write: false,
        exec: true,
    };
    pub const RWX: Self = Self {
        write: true,
        exec: true,
    };

    pub const fn flags(self) -> PageFlags {
        user_leaf_flags(self.write, self.exec)
    }

    pub const fn from_elf(write: bool, exec: bool) -> Self {
        Self { write, exec }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backing {
    Anonymous,
    /// `PROT_NONE`: the range is taken, and no frame backs it.
    Reserved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub start: u64,
    pub len: u64,
    pub perms: UserPerms,
    pub backing: Backing,
}

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsError {
    Misaligned,
    Overflow,
    KernelRange,
    NullGuard,
    Overlap,
    OutOfFrames,
    AlreadyMapped,
    NoRegionSlot,
    NotMapped,
    /// No free range for an `mmap` placement.
    NoVaSpace,
    Map(MapError),
}

/// An address-space error's errno: Linux's for each condition, as `mmap` returns it.
impl From<AsError> for crate::kerror::KError {
    fn from(e: AsError) -> Self {
        match e {
            AsError::Misaligned | AsError::Overflow | AsError::KernelRange | AsError::NotMapped => {
                Self::Inval
            }
            AsError::NullGuard => Self::Perm,
            AsError::Overlap | AsError::AlreadyMapped => Self::Exist,
            AsError::OutOfFrames | AsError::NoRegionSlot | AsError::NoVaSpace => Self::NoMem,
            AsError::Map(m) => Self::from(m),
        }
    }
}

/// What `brk(want)` does, from [`AddressSpace::brk_plan`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrkPlan {
    /// Leave the break where it is and return it.
    Current,
    /// Move the break within its last page: no page changes.
    SamePage,
    /// Map `[va, va+len)` onto the heap.
    Grow { va: u64, len: u64 },
    /// Unmap `[va, va+len)` from the heap's top.
    Shrink { va: u64, len: u64 },
}

/// How a request treats its `addr`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fixed {
    /// A hint.
    No,
    /// `MAP_FIXED`.
    Replace,
    /// `MAP_FIXED_NOREPLACE`.
    NoReplace,
}

/// A decoded anonymous `mmap`: `len` is page-rounded, and `perms` is `None`
/// for `PROT_NONE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MmapReq {
    pub addr: u64,
    pub len: u64,
    pub perms: Option<UserPerms>,
    pub fixed: Fixed,
}

/// Why [`mmap_request`] refused a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MmapError {
    /// `EINVAL`.
    Inval,
    /// `ENOMEM`.
    NoMem,
    /// A file mapping (no `MAP_ANONYMOUS`): `EBADF` or `ENODEV`.
    NotAnon,
}

/// An `mmap` request's errno; a file mapping, which `sys_mmap` refines by the fd, is no device.
impl From<MmapError> for crate::kerror::KError {
    fn from(e: MmapError) -> Self {
        match e {
            MmapError::Inval => Self::Inval,
            MmapError::NoMem => Self::NoMem,
            MmapError::NotAnon => Self::NoDev,
        }
    }
}

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserMemError {
    NonCanonical,
    Kernel,
    Overflow,
    Unmapped,
    NullGuard,
}

/// A user range that is not the caller's is `EFAULT`: user misuse, never a panic.
impl From<UserMemError> for crate::kerror::KError {
    fn from(e: UserMemError) -> Self {
        match e {
            UserMemError::NonCanonical
            | UserMemError::Kernel
            | UserMemError::Overflow
            | UserMemError::Unmapped
            | UserMemError::NullGuard => Self::Fault,
        }
    }
}

impl UserMemError {
    /// Which bound a range that `uaccess::user_range_ok` refused breaks.
    /// Only meaningful for a refused range.
    pub const fn refused(ptr: u64, len: u64) -> Self {
        if len == 0 {
            return if is_canonical(ptr) {
                Self::Kernel
            } else {
                Self::NonCanonical
            };
        }
        let Some(end) = ptr.checked_add(len) else {
            return Self::Overflow;
        };
        if !is_canonical(ptr) || !is_canonical(end.wrapping_sub(1)) {
            return Self::NonCanonical;
        }
        if ptr >= USER_MAP_END || end > USER_MAP_END {
            return Self::Kernel;
        }
        Self::NullGuard
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TeardownStats {
    pub user_frames: usize,
    pub pt_frames: usize,
}

pub struct AddressSpace {
    mapper: Mapper,
    regions: [Option<Region>; MAX_REGIONS],
    user_frames: usize,
    pt_frames: usize,
    /// Page after the image's highest `PT_LOAD`: where the heap starts.
    brk_start: u64,
    /// The program break; the heap region ends at `page_up(brk)`.
    brk: u64,
}

struct Counting<'a, A: FrameAlloc> {
    inner: &'a mut A,
    n: &'a mut usize,
}

// SAFETY: every frame comes from `inner` unchanged, so `Counting` keeps
// `inner`'s promise of order-0 tokens writable through the HHDM
// (`paging::FrameAlloc`); the count is the only thing added, here.
unsafe impl<A: FrameAlloc> FrameAlloc for Counting<'_, A> {
    fn alloc_frame(&mut self) -> Option<Frames> {
        let f = self.inner.alloc_frame()?;
        *self.n += 1;
        Some(f)
    }
}

impl AddressSpace {
    /// New PML4, kernel half shared from `kernel`. `alloc` supplies the
    /// PML4 frame, whose token the mapper holds until [`teardown`] frees
    /// it last. `pt_frames` starts at 1 (the root).
    ///
    /// [`teardown`]: AddressSpace::teardown
    ///
    /// # Safety
    /// `kernel` is the live kernel mapper. `alloc` returns owned frames
    /// reachable through `kernel.hhdm_offset()`.
    pub unsafe fn new<A: FrameAlloc>(kernel: &Mapper, alloc: &mut A) -> Option<Self> {
        let root = PhysAddr(alloc.alloc_frame()?.into_entry());
        // SAFETY: `root` is an owned frame from `alloc`, writable through
        // `kernel`'s HHDM offset (this fn's contract), and this space owns it
        // until `teardown`; the mapper walks nothing before the zeroing
        // below, so the root is zeroed before first use, as
        // `paging::Mapper::new` requires.
        let mapper = unsafe { Mapper::new(root, kernel.hhdm_offset()) };
        // SAFETY: `root` is the owned frame just taken from `alloc`,
        // reachable through the HHDM offset (this fn's contract), as
        // `paging::Mapper::zero_frame` requires; established here.
        unsafe { mapper.zero_frame(root) };
        let mut space = Self {
            mapper,
            regions: [None; MAX_REGIONS],
            user_frames: 0,
            pt_frames: 1,
            brk_start: 0,
            brk: 0,
        };
        space.mapper.copy_kernel_half_from(kernel);
        Some(space)
    }

    pub fn mapper(&self) -> &Mapper {
        &self.mapper
    }

    pub fn root(&self) -> PhysAddr {
        self.mapper.root()
    }

    pub fn user_frames(&self) -> usize {
        self.user_frames
    }

    pub fn pt_frames(&self) -> usize {
        self.pt_frames
    }

    pub fn regions(&self) -> impl Iterator<Item = Region> + '_ {
        self.regions.iter().filter_map(|r| *r)
    }

    /// # Safety
    /// `alloc` returns owned frames; `va` is the user half and not already mapped.
    pub unsafe fn map_anon<A: FrameAlloc + FrameFree>(
        &mut self,
        va: u64,
        len: u64,
        perms: UserPerms,
        alloc: &mut A,
    ) -> Result<(), AsError> {
        self.check_new_region(va, len)?;
        // SAFETY: `check_new_region` found `[va, va+len)` in the user half
        // and clear of every region, and every leaf in this space belongs to
        // a region (invariant of `addr_space::AddressSpace::insert_region`),
        // so no page there is mapped; `alloc` hands out owned frames (this
        // fn's contract).
        unsafe { self.map_pages(va, len, perms, alloc)? };
        let region = Region {
            start: va,
            len,
            perms,
            backing: Backing::Anonymous,
        };
        if let Err(e) = self.insert_region(region) {
            // SAFETY: `map_pages` just mapped every page of the range with
            // a frame from `alloc`, and nothing else has seen them, here.
            unsafe { self.unmap_pages(va, len, alloc, &mut |_| {})? };
            return Err(e);
        }
        Ok(())
    }

    /// The checks a new region at `[va, va+len)` passes before anything is
    /// mapped: an aligned user-half range, no overlap with a region, and a
    /// free region slot.
    pub fn check_new_region(&self, va: u64, len: u64) -> Result<(), AsError> {
        check_map_range(va, len)?;
        if self.overlaps(va, len) {
            return Err(AsError::Overlap);
        }
        if !self.regions.iter().any(|r| r.is_none()) {
            return Err(AsError::NoRegionSlot);
        }
        Ok(())
    }

    /// Record `r` in a free slot. Every mapped user leaf lies in a region
    /// once its mapping call returns.
    pub fn insert_region(&mut self, r: Region) -> Result<(), AsError> {
        let slot = self
            .regions
            .iter_mut()
            .find(|s| s.is_none())
            .ok_or(AsError::NoRegionSlot)?;
        *slot = Some(r);
        Ok(())
    }

    /// Map each page of `[va, va+len)` to a fresh frame from `alloc`, with
    /// `perms`. Records no region and does not zero the frames. Counts
    /// `user_frames` per leaf mapped and `pt_frames` per table allocated,
    /// on success and on failure alike. On any failure it unmaps and frees
    /// every leaf this call mapped and returns the error (F009).
    ///
    /// # Safety
    /// No page of `[va, va+len)` is mapped in this space, and `alloc`
    /// returns owned frames.
    pub unsafe fn map_pages<A: FrameAlloc + FrameFree>(
        &mut self,
        va: u64,
        len: u64,
        perms: UserPerms,
        alloc: &mut A,
    ) -> Result<(), AsError> {
        check_map_range(va, len)?;
        let flags = perms.flags();
        let mut mapped = 0u64;
        while mapped < len {
            let page_va = VirtAddr(va + mapped);
            let err = match alloc.alloc_frame() {
                None => AsError::OutOfFrames,
                Some(leaf) => {
                    // The leaf's token moves into the entry `map_page`
                    // writes; `unmap_pages` and `teardown` take it back.
                    let pa = PhysAddr(leaf.into_entry());
                    let mut n_pt = 0usize;
                    let rc = {
                        let mut c = Counting {
                            inner: &mut *alloc,
                            n: &mut n_pt,
                        };
                        // SAFETY: `page_va` is unmapped (this fn's contract)
                        // and `pa` is the frame just taken from `alloc`,
                        // which nothing else names, here.
                        unsafe {
                            self.mapper.map_page(
                                page_va,
                                pa,
                                flags,
                                PageSize::Size4K,
                                MapMode::Fresh,
                                &mut c,
                            )
                        }
                    };
                    // Tables `map_page` linked in stay in the tree, and
                    // `teardown` frees them, whether or not the leaf landed.
                    self.pt_frames += n_pt;
                    match rc {
                        Ok(()) => {
                            self.user_frames += 1;
                            mapped += PAGE_SIZE_4K;
                            continue;
                        }
                        Err(e) => {
                            // SAFETY: `pa` is the order-0 `into_entry`
                            // above, and a failed `map_page` writes no leaf,
                            // so no entry holds it (the contract
                            // `pmm::Frames::from_entry` states), here.
                            alloc.free_frame(unsafe { Frames::from_entry(pa.as_u64(), 0) });
                            match e {
                                MapError::OutOfFrames => AsError::OutOfFrames,
                                MapError::AlreadyMapped => AsError::AlreadyMapped,
                                other => AsError::Map(other),
                            }
                        }
                    }
                }
            };
            // SAFETY: this call mapped every page of `[va, va+mapped)` with
            // a frame from `alloc`, here.
            unsafe { self.unmap_pages(va, mapped, alloc, &mut |_| {})? };
            return Err(err);
        }
        Ok(())
    }

    /// Clear every present leaf in `[va, va+len)` and give its frame to
    /// `pool`. `flush(va)` runs after each leaf's PTE is cleared and before
    /// its frame is freed. Pages with no leaf are skipped. Touches no
    /// region.
    ///
    /// # Safety
    /// Every leaf in the range holds an order-0 token that `pool` may take
    /// back, and `flush` removes the page from every TLB that may hold it.
    pub unsafe fn unmap_pages<A: FrameFree>(
        &mut self,
        va: u64,
        len: u64,
        pool: &mut A,
        flush: &mut dyn FnMut(u64),
    ) -> Result<(), AsError> {
        check_map_range(va, len)?;
        let mut off = 0u64;
        while off < len {
            let page = VirtAddr(va + off);
            match self.mapper.translate(page) {
                None => {}
                Some((_, PageSize::Size2M, _)) => {
                    return Err(AsError::Map(MapError::PageSizeMismatch));
                }
                Some((_, PageSize::Size4K, _)) => {
                    // SAFETY: the TLB entry for `page` goes through `flush`
                    // below, before its frame is freed (this fn's contract,
                    // `addr_space::AddressSpace::unmap_pages`).
                    if let Some((pa, _)) = unsafe { self.mapper.unmap_page(page) } {
                        flush(page.as_u64());
                        // SAFETY: `unmap_page` just cleared this user leaf,
                        // which held an order-0 token that
                        // `addr_space::AddressSpace::map_pages` consumed into
                        // it (the contract `pmm::Frames::from_entry` states).
                        pool.free_frame(unsafe { Frames::from_entry(pa.as_u64(), 0) });
                        self.user_frames = self.user_frames.saturating_sub(1);
                    }
                }
            }
            off += PAGE_SIZE_4K;
        }
        Ok(())
    }

    /// The program break.
    pub fn brk(&self) -> u64 {
        self.brk
    }

    /// Where the heap starts; 0 before a loader sets it.
    pub fn brk_start(&self) -> u64 {
        self.brk_start
    }

    /// Start the heap, empty, at `va` rounded up to a page. The loader calls
    /// it after mapping the image.
    pub fn set_brk_start(&mut self, va: u64) {
        let start = page_up(va).unwrap_or(USER_MAP_END);
        self.brk_start = start;
        self.brk = start;
    }

    /// Move the break after its pages changed as [`brk_plan`] said.
    ///
    /// [`brk_plan`]: AddressSpace::brk_plan
    pub fn set_brk(&mut self, want: u64) {
        self.brk = want;
    }

    /// What `brk(want)` does. `brk(0)`, a value below the start, an end past
    /// `USER_MAP_END`, and growth into another region leave the break
    /// where it is.
    pub fn brk_plan(&self, want: u64) -> BrkPlan {
        if self.brk_start == 0 || want == 0 || want < self.brk_start || want > USER_MAP_END {
            return BrkPlan::Current;
        }
        let (Some(top), Some(new_top)) = (page_up(self.brk), page_up(want)) else {
            return BrkPlan::Current;
        };
        if new_top == top {
            BrkPlan::SamePage
        } else if new_top > top {
            let len = new_top - top;
            if self.overlaps(top, len) {
                BrkPlan::Current
            } else {
                BrkPlan::Grow { va: top, len }
            }
        } else {
            BrkPlan::Shrink {
                va: new_top,
                len: top - new_top,
            }
        }
    }

    /// Before a heap growth maps `[va, va+len)`: the range is free, and a
    /// slot is free when the heap has no region yet.
    pub fn heap_grow_check(&self, va: u64, len: u64) -> Result<(), AsError> {
        check_map_range(va, len)?;
        if self.overlaps(va, len) {
            return Err(AsError::Overlap);
        }
        if self.heap_slot(va).is_none() && !self.regions.iter().any(|r| r.is_none()) {
            return Err(AsError::NoRegionSlot);
        }
        Ok(())
    }

    /// After `[va, va+len)` is mapped: extend the heap region that ends at
    /// `va` over it (or record the range as a new region) and set the break
    /// to `want`.
    pub fn heap_grow_commit(&mut self, va: u64, len: u64, want: u64) -> Result<(), AsError> {
        match self.heap_slot(va) {
            Some(i) => {
                if let Some(r) = self.regions[i].as_mut() {
                    r.len = r.len.checked_add(len).ok_or(AsError::Overflow)?;
                }
            }
            None => self.insert_region(Region {
                start: va,
                len,
                perms: UserPerms::RW,
                backing: Backing::Anonymous,
            })?,
        }
        self.brk = want;
        Ok(())
    }

    /// The slot of the heap region a growth at `top` extends: the anonymous
    /// read-write region at or above the heap's start that ends at `top`.
    /// `None` when the heap is empty, or a `munmap` removed its top page.
    fn heap_slot(&self, top: u64) -> Option<usize> {
        self.regions.iter().position(|r| {
            r.is_some_and(|r| {
                r.start >= self.brk_start
                    && r.start.saturating_add(r.len) == top
                    && r.backing == Backing::Anonymous
                    && r.perms == UserPerms::RW
            })
        })
    }

    /// Where an `mmap` of `req` goes. A fixed request below `NULL_GUARD_LEN`
    /// is `NullGuard`, one past `USER_MAP_END` is `KernelRange`, and one
    /// over a region is `Overlap`. Otherwise a free hint, rounded down to a
    /// page, is used; otherwise the highest free range below `MMAP_TOP`,
    /// or `NoVaSpace`.
    pub fn mmap_place(&self, req: &MmapReq) -> Result<u64, AsError> {
        let len = req.len;
        if req.fixed != Fixed::No {
            if req.addr < NULL_GUARD_LEN {
                return Err(AsError::NullGuard);
            }
            let end = req.addr.checked_add(len).ok_or(AsError::KernelRange)?;
            if end > USER_MAP_END {
                return Err(AsError::KernelRange);
            }
            if self.overlaps(req.addr, len) {
                return Err(AsError::Overlap);
            }
            return Ok(req.addr);
        }
        let hint = req.addr & !(PAGE_SIZE_4K - 1);
        if hint >= NULL_GUARD_LEN
            && hint
                .checked_add(len)
                .is_some_and(|e| e <= USER_MAP_END && !self.overlaps(hint, len))
        {
            return Ok(hint);
        }
        let mut end = MMAP_TOP;
        loop {
            let start = end
                .checked_sub(len)
                .filter(|&s| s >= NULL_GUARD_LEN)
                .ok_or(AsError::NoVaSpace)?;
            let below = self
                .regions
                .iter()
                .flatten()
                .filter(|r| r.start < end && start < r.start.saturating_add(r.len))
                .map(|r| r.start)
                .min();
            match below {
                None => return Ok(start),
                Some(s) => end = s,
            }
        }
    }

    /// Free every user leaf + user PT page + the PML4, the root last.
    /// Kernel-half PDPTs are not touched. `free` must return frames to
    /// `alloc`.
    ///
    /// # Safety
    /// The root is not loaded in any CR3, and nothing uses this space
    /// again after it returns.
    pub unsafe fn teardown<F>(&mut self, free: &mut F) -> TeardownStats
    where
        F: FnMut(Frames),
    {
        // SAFETY: every user-half entry holds a token `map_page` (tables)
        // or `map_pages` (leaves) consumed with `into_entry`, as
        // `paging::Mapper::free_user_half` requires; established by
        // `addr_space::AddressSpace::map_pages`.
        let walked = unsafe { self.mapper.free_user_half(free) };
        // SAFETY: `AddressSpace::new` consumed the root's order-0 token
        // into this mapper, and this space is not used again (this fn's
        // `# Safety`), so the mapper's reference is the entry being
        // cleared (the contract `pmm::Frames::from_entry` states).
        free(unsafe { Frames::from_entry(self.mapper.root().as_u64(), 0) });
        let stats = TeardownStats {
            user_frames: walked.leaves,
            pt_frames: walked.tables + 1,
        };
        self.regions = [None; MAX_REGIONS];
        self.user_frames = 0;
        self.pt_frames = 0;
        let _ = walked;
        stats
    }

    /// Range check + present USER mapping. Never panics.
    pub fn check_user_range(&self, ptr: u64, len: u64) -> Result<(), UserMemError> {
        if !user_range_ok(ptr, len) {
            return Err(UserMemError::refused(ptr, len));
        }
        if len == 0 {
            return Ok(());
        }
        let end = ptr.wrapping_add(len);
        let mut va = ptr;
        while va < end {
            match self.mapper.probe(VirtAddr(va)) {
                Probe::Skip(_) => return Err(UserMemError::Unmapped),
                Probe::Mapped { flags, size, .. } => {
                    if !flags.contains(PageFlags::USER) {
                        return Err(UserMemError::Unmapped);
                    }
                    let span = size.bytes();
                    let next = (va & !(span - 1)).saturating_add(span);
                    if next <= va {
                        break;
                    }
                    va = next;
                }
            }
        }
        Ok(())
    }

    /// Copy through HHDM after [`check_user_range`]. All-or-nothing.
    pub fn read_bytes(&self, src: u64, dst: &mut [u8]) -> Result<(), UserMemError> {
        if dst.is_empty() {
            return Ok(());
        }
        self.check_user_range(src, dst.len() as u64)?;
        // SAFETY: `dst` is `dst.len()` live, writable bytes, and
        // `check_user_range` just found every page of the source mapped in
        // this space, as `addr_space::AddressSpace::copy_via_hhdm` requires;
        // established here.
        unsafe { self.copy_via_hhdm(src, dst.as_mut_ptr(), dst.len(), false) }
    }

    /// Copy through HHDM after [`check_user_range`]. All-or-nothing.
    /// Does not require the PTE to be writable (ELF load onto RX pages).
    pub fn write_bytes(&self, dst: u64, src: &[u8]) -> Result<(), UserMemError> {
        if src.is_empty() {
            return Ok(());
        }
        self.check_user_range(dst, src.len() as u64)?;
        // SAFETY: `src` is `src.len()` live bytes that a copy to user only
        // reads (`to_user` is true, so the `*mut` is never written through),
        // and `check_user_range` just found every page of the destination
        // mapped in this space, as `addr_space::AddressSpace::copy_via_hhdm`
        // requires; established here.
        unsafe { self.copy_via_hhdm(dst, src.as_ptr() as *mut u8, src.len(), true) }
    }

    pub fn zero_bytes(&self, dst: u64, len: u64) -> Result<(), UserMemError> {
        if len == 0 {
            return Ok(());
        }
        self.check_user_range(dst, len)?;
        let mut off = 0u64;
        while off < len {
            let va = dst + off;
            let Some((pa, size, _)) = self.mapper.translate(VirtAddr(va)) else {
                return Err(UserMemError::Unmapped);
            };
            let span = size.bytes();
            let page_off = va & (span - 1);
            let chunk = (span - page_off).min(len - off);
            let ptr = (pa.as_u64().wrapping_add(self.mapper.hhdm_offset())) as *mut u8;
            // SAFETY: `translate` found `va` mapped to `pa` in a page of
            // `span` bytes, `chunk` stops at that page's end, and the HHDM
            // maps every frame writable (`paging::FrameAlloc`), so
            // `[ptr, ptr+chunk)` is one live frame's bytes; established here.
            unsafe { core::ptr::write_bytes(ptr, 0, chunk as usize) };
            off += chunk;
        }
        Ok(())
    }

    /// # Safety
    /// `buf` is `len` live bytes; `va` is mapped in this space.
    unsafe fn copy_via_hhdm(
        &self,
        va: u64,
        buf: *mut u8,
        len: usize,
        to_user: bool,
    ) -> Result<(), UserMemError> {
        let mut off = 0usize;
        while off < len {
            let cur = va + off as u64;
            let Some((pa, size, _)) = self.mapper.translate(VirtAddr(cur)) else {
                return Err(UserMemError::Unmapped);
            };
            let span = size.bytes();
            let page_off = cur & (span - 1);
            let chunk = (span - page_off).min((len - off) as u64) as usize;
            let user = (pa.as_u64().wrapping_add(self.mapper.hhdm_offset())) as *mut u8;
            // SAFETY: `off + chunk <= len`, so `buf.add(off)` and its `chunk`
            // bytes lie in the caller's `len` live bytes (this fn's contract,
            // `addr_space::AddressSpace::copy_via_hhdm`); `translate` found
            // `cur` mapped and `chunk` stops at its page's end, so `user` is
            // `chunk` bytes of one frame through the HHDM; a user frame is
            // never a kernel object, so the two do not overlap.
            unsafe {
                if to_user {
                    core::ptr::copy_nonoverlapping(buf.add(off) as *const u8, user, chunk);
                } else {
                    core::ptr::copy_nonoverlapping(user as *const u8, buf.add(off), chunk);
                }
            }
            off += chunk;
        }
        Ok(())
    }

    fn overlaps(&self, va: u64, len: u64) -> bool {
        let end = va.saturating_add(len);
        self.regions.iter().flatten().any(|r| {
            let r_end = r.start.saturating_add(r.len);
            va < r_end && r.start < end
        })
    }
}

/// `x` rounded up to a page, or `None` on overflow.
fn page_up(x: u64) -> Option<u64> {
    Some(x.checked_add(PAGE_SIZE_4K - 1)? & !(PAGE_SIZE_4K - 1))
}

/// Decode an anonymous `mmap(addr, len, prot, flags, fd, off)`, checking in
/// the order Linux's mmap(2) does: an unaligned `off`, then a file mapping,
/// then a zero `len`, a map type other than `MAP_PRIVATE` or a flag
/// outside the accepted set, a `prot` bit other than R, W or X, a `len`
/// that rounds past `USER_MAP_END`, and a fixed request's unaligned `addr`.
pub fn mmap_request(
    addr: u64,
    len: u64,
    prot: u64,
    flags: u64,
    off: u64,
) -> Result<MmapReq, MmapError> {
    if !off.is_multiple_of(PAGE_SIZE_4K) {
        return Err(MmapError::Inval);
    }
    if flags & MAP_ANONYMOUS == 0 {
        return Err(MmapError::NotAnon);
    }
    if len == 0 {
        return Err(MmapError::Inval);
    }
    if flags & MAP_TYPE != MAP_PRIVATE || flags & !MAP_ACCEPTED != 0 {
        return Err(MmapError::Inval);
    }
    if prot & !(PROT_READ | PROT_WRITE | PROT_EXEC) != 0 {
        return Err(MmapError::Inval);
    }
    let len = page_up(len)
        .filter(|&l| l <= USER_MAP_END)
        .ok_or(MmapError::NoMem)?;
    let fixed = if flags & MAP_FIXED_NOREPLACE != 0 {
        Fixed::NoReplace
    } else if flags & MAP_FIXED != 0 {
        Fixed::Replace
    } else {
        Fixed::No
    };
    if fixed != Fixed::No && !addr.is_multiple_of(PAGE_SIZE_4K) {
        return Err(MmapError::Inval);
    }
    let perms = (prot != 0).then_some(UserPerms {
        write: prot & PROT_WRITE != 0,
        exec: prot & PROT_EXEC != 0,
    });
    Ok(MmapReq {
        addr,
        len,
        perms,
        fixed,
    })
}

fn check_map_range(va: u64, len: u64) -> Result<(), AsError> {
    if len == 0 {
        return Ok(());
    }
    if !va.is_multiple_of(PAGE_SIZE_4K) || !len.is_multiple_of(PAGE_SIZE_4K) {
        return Err(AsError::Misaligned);
    }
    let end = va.checked_add(len).ok_or(AsError::Overflow)?;
    if va < NULL_GUARD_LEN {
        return Err(AsError::NullGuard);
    }
    if va >= USER_MAP_END || end > USER_MAP_END {
        return Err(AsError::KernelRange);
    }
    if !is_canonical(va) || !is_canonical(end - 1) {
        return Err(AsError::KernelRange);
    }
    Ok(())
}

/// Allocator that can take frames back. Kernel buddy and host tests.
///
/// # Safety
/// `free_frame` returns the token to the allocator that handed it out.
pub unsafe trait FrameFree {
    fn free_frame(&mut self, f: Frames);
}

impl AddressSpace {
    /// `munmap` of `[va, va+len)`: every region in the range is trimmed,
    /// split or removed, and each of its leaves is cleared, flushed with
    /// `flush(va)` after its PTE is cleared, and freed to `pool`. Holes and
    /// an empty range are fine. The range is page-aligned and ends at or
    /// below `USER_MAP_END`; it may start below `NULL_GUARD_LEN`. When a
    /// split needs a region slot and none is free, returns `NoRegionSlot`
    /// with nothing changed. Allocates nothing.
    ///
    /// # Safety
    /// Every leaf in the range holds an order-0 token that `pool` may take
    /// back, and `flush` removes the page from every TLB that may hold it.
    pub unsafe fn unmap_free<A: FrameFree>(
        &mut self,
        va: u64,
        len: u64,
        pool: &mut A,
        flush: &mut dyn FnMut(u64),
    ) -> Result<(), AsError> {
        if !va.is_multiple_of(PAGE_SIZE_4K) || !len.is_multiple_of(PAGE_SIZE_4K) {
            return Err(AsError::Misaligned);
        }
        let end = va.checked_add(len).ok_or(AsError::Overflow)?;
        if end > USER_MAP_END {
            return Err(AsError::KernelRange);
        }
        if len == 0 {
            return Ok(());
        }
        // Regions do not overlap, so at most one strictly contains the
        // range and needs a second slot for its upper part.
        let split = self
            .regions
            .iter()
            .flatten()
            .any(|r| r.start < va && end < r.start.saturating_add(r.len));
        if split && !self.regions.iter().any(|r| r.is_none()) {
            return Err(AsError::NoRegionSlot);
        }
        let mut i = 0;
        while i < MAX_REGIONS {
            let Some(r) = self.regions[i] else {
                i += 1;
                continue;
            };
            let r_end = r.start.saturating_add(r.len);
            if r_end <= va || end <= r.start {
                i += 1;
                continue;
            }
            let lo = r.start.max(va);
            let hi = r_end.min(end);
            if r.backing != Backing::Reserved {
                // SAFETY: `[lo, hi)` lies in this region, whose leaves hold
                // order-0 tokens from `pool`, and `flush` covers every TLB
                // (this fn's contract, `addr_space::AddressSpace::unmap_free`).
                unsafe { self.unmap_pages(lo, hi - lo, pool, flush)? };
            }
            let below = (r.start < va).then(|| Region {
                len: va - r.start,
                ..r
            });
            let above = (end < r_end).then(|| Region {
                start: end,
                len: r_end - end,
                ..r
            });
            self.regions[i] = below.or(above);
            if let (Some(_), Some(up)) = (below, above) {
                // The first pass found a free slot for this split.
                self.insert_region(up)?;
            }
            i += 1;
        }
        Ok(())
    }

    /// # Safety
    /// `pool` may recycle every user/PT/PML4 frame this space still owns.
    pub unsafe fn teardown_pool<A: FrameFree>(&mut self, pool: &mut A) -> TeardownStats {
        let mut free = |f: Frames| pool.free_frame(f);
        // SAFETY: the caller hands every frame this space owns to `pool`
        // and uses the space no more (this fn's contract,
        // `addr_space::AddressSpace::teardown_pool`), which is
        // `teardown`'s contract.
        unsafe { self.teardown(&mut free) }
    }

    /// Full copy of user regions (Phase 9 fork). New frames, same bytes.
    ///
    /// # Safety
    /// `kernel` is the live kernel mapper. `alloc` supplies owned frames.
    pub unsafe fn clone_anon<A: FrameAlloc + FrameFree>(
        &self,
        kernel: &Mapper,
        alloc: &mut A,
    ) -> Result<AddressSpace, AsError> {
        // SAFETY: `kernel` is the live kernel mapper and `alloc` hands out
        // owned frames (this fn's contract,
        // `addr_space::AddressSpace::clone_anon`), as `new` requires.
        let mut dst = unsafe { AddressSpace::new(kernel, alloc) }.ok_or(AsError::OutOfFrames)?;
        dst.brk_start = self.brk_start;
        dst.brk = self.brk;
        let rc = (|| {
            let mut buf = [0u8; 256];
            for r in self.regions() {
                if r.len == 0 {
                    continue;
                }
                if r.backing == Backing::Reserved {
                    dst.check_new_region(r.start, r.len)?;
                    dst.insert_region(r)?;
                    continue;
                }
                // SAFETY: `dst` is new, so `[r.start, r.start+r.len)` is
                // unmapped in it, and `alloc` hands out owned frames (this
                // fn's contract, `addr_space::AddressSpace::clone_anon`).
                unsafe { dst.map_anon(r.start, r.len, r.perms, alloc)? };
                let mut off = 0u64;
                while off < r.len {
                    let n = (r.len - off).min(buf.len() as u64) as usize;
                    self.read_bytes(r.start + off, &mut buf[..n])
                        .map_err(|_| AsError::NotMapped)?;
                    dst.write_bytes(r.start + off, &buf[..n])
                        .map_err(|_| AsError::NotMapped)?;
                    off += n as u64;
                }
            }
            Ok(())
        })();
        match rc {
            Ok(()) => Ok(dst),
            Err(e) => {
                // SAFETY: `dst` was never loaded in a CR3 and is dropped
                // here, and every frame it holds came from `alloc`, which may
                // take it back (`addr_space::FrameFree`).
                let _ = unsafe { dst.teardown_pool(alloc) };
                Err(e)
            }
        }
    }
}

// Host tests give frames back to the shared buddy pool.
// SAFETY: every frame a host test's space holds came from this pool's
// buddy, and `free_frame` gives it back to that buddy; established here.
#[cfg(test)]
unsafe impl FrameFree for crate::pmm::testing::Pool {
    fn free_frame(&mut self, f: Frames) {
        self.buddy.free(f);
    }
}

const _: () = {
    assert!(KERNEL_PML4_FIRST == 256);
    assert!(PTES_PER_TABLE == 512);
};

#[cfg(test)]
mod tests;
