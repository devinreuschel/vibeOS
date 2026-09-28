//! Address space: user half of a PML4 plus region tracking. ROADMAP §9.2.
//!
//! Kernel half (PML4[256..512)) is shared by copying the kernel's upper
//! entries so they point at the same PDPTs. VA 0 is never mapped. The
//! kernel's 512 MiB low identity window is GLOBAL; user maps in that
//! range can alias a stale TLB until identity is torn down.

use crate::paging::{
    FrameAlloc, KERNEL_PML4_FIRST, MapError, MapMode, Mapper, NULL_GUARD_LEN, PAGE_SIZE_4K,
    PTES_PER_TABLE, PageFlags, PageSize, PhysAddr, Probe, USER_MAP_END, VirtAddr, is_canonical,
    user_leaf_flags,
};
use crate::pmm::Frames;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserMemError {
    NonCanonical,
    Kernel,
    Overflow,
    Unmapped,
    NullGuard,
}

impl UserMemError {
    /// Linux `EFAULT`. User misuse, never a panic.
    pub const EFAULT: i32 = 14;

    pub const fn errno(self) -> i32 {
        match self {
            Self::NonCanonical
            | Self::Kernel
            | Self::Overflow
            | Self::Unmapped
            | Self::NullGuard => Self::EFAULT,
        }
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
        let mapper = unsafe { Mapper::new(root, kernel.hhdm_offset()) };
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
                    // below, before its frame is freed (this fn's contract).
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
        if self.heap_slot().is_none() && !self.regions.iter().any(|r| r.is_none()) {
            return Err(AsError::NoRegionSlot);
        }
        Ok(())
    }

    /// After `[va, va+len)` is mapped: extend the heap region over it (or
    /// record it as the heap region) and set the break to `want`.
    pub fn heap_grow_commit(&mut self, va: u64, len: u64, want: u64) -> Result<(), AsError> {
        match self.heap_slot() {
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

    /// The heap region's slot: the region that starts at the heap's start.
    fn heap_slot(&self) -> Option<usize> {
        self.regions
            .iter()
            .position(|r| r.is_some_and(|r| r.start == self.brk_start))
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
        if len == 0 {
            if !is_canonical(ptr) {
                return Err(UserMemError::NonCanonical);
            }
            if ptr >= USER_MAP_END {
                return Err(UserMemError::Kernel);
            }
            return Ok(());
        }
        let end = ptr.checked_add(len).ok_or(UserMemError::Overflow)?;
        if !is_canonical(ptr) || !is_canonical(end.wrapping_sub(1)) {
            return Err(UserMemError::NonCanonical);
        }
        if ptr >= USER_MAP_END || end > USER_MAP_END {
            return Err(UserMemError::Kernel);
        }
        if ptr < NULL_GUARD_LEN {
            return Err(UserMemError::NullGuard);
        }
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
        unsafe { self.copy_via_hhdm(src, dst.as_mut_ptr(), dst.len(), false) }
    }

    /// Copy through HHDM after [`check_user_range`]. All-or-nothing.
    /// Does not require the PTE to be writable (ELF load onto RX pages).
    pub fn write_bytes(&self, dst: u64, src: &[u8]) -> Result<(), UserMemError> {
        if src.is_empty() {
            return Ok(());
        }
        self.check_user_range(dst, src.len() as u64)?;
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
            if to_user {
                unsafe { core::ptr::copy_nonoverlapping(buf.add(off) as *const u8, user, chunk) };
            } else {
                unsafe { core::ptr::copy_nonoverlapping(user as *const u8, buf.add(off), chunk) };
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
                // order-0 tokens from `pool` (this fn's contract).
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
                let _ = unsafe { dst.teardown_pool(alloc) };
                Err(e)
            }
        }
    }
}

// Host tests give frames back to the shared buddy pool.
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
mod tests {
    use super::*;
    use crate::paging::{PTE_ADDR_MASK, UserFreeStats, physmap_flags};
    use crate::pmm::testing::Pool;

    /// Frames the pool has handed out and not had back.
    fn used(pool: &Pool) -> usize {
        let s = pool.buddy.stats();
        s.total_frames - s.free_frames
    }

    /// The root's token moves into the mapper, which is never torn down.
    fn kernel_mapper(pool: &mut Pool) -> Mapper {
        let root = PhysAddr(pool.alloc_frame().unwrap().into_entry());
        let mapper = unsafe { Mapper::new(root, pool.hhdm()) };
        unsafe { mapper.zero_frame(root) };
        mapper
    }

    #[test]
    fn top_page_is_not_mappable() {
        let mut pool = Pool::new(64);
        let kernel = kernel_mapper(&mut pool);
        let mut aspace = unsafe { AddressSpace::new(&kernel, &mut pool) }.unwrap();
        assert_eq!(
            unsafe { aspace.map_anon(USER_MAP_END, PAGE_SIZE_4K, UserPerms::RW, &mut pool) },
            Err(AsError::KernelRange)
        );
        let below = USER_MAP_END - PAGE_SIZE_4K;
        assert!(unsafe { aspace.map_anon(below, PAGE_SIZE_4K, UserPerms::RW, &mut pool) }.is_ok());
        assert!(aspace.check_user_range(below, PAGE_SIZE_4K).is_ok());
        assert!(aspace.check_user_range(USER_MAP_END, 1).is_err());
        assert!(aspace.check_user_range(below, PAGE_SIZE_4K + 1).is_err());
        unsafe { aspace.teardown_pool(&mut pool) };
    }

    #[test]
    fn null_guard_and_kernel_rejected() {
        let mut pool = Pool::new(64);
        let kernel = kernel_mapper(&mut pool);
        let before = used(&pool);
        let mut aspace = unsafe { AddressSpace::new(&kernel, &mut pool) }.unwrap();
        assert_eq!(
            unsafe { aspace.map_anon(0, PAGE_SIZE_4K, UserPerms::RW, &mut pool) },
            Err(AsError::NullGuard)
        );
        assert_eq!(
            unsafe {
                aspace.map_anon(
                    crate::paging::USER_END,
                    PAGE_SIZE_4K,
                    UserPerms::RW,
                    &mut pool,
                )
            },
            Err(AsError::KernelRange)
        );
        assert_eq!(
            unsafe {
                aspace.map_anon(
                    0xFFFF_8000_0000_0000,
                    PAGE_SIZE_4K,
                    UserPerms::RW,
                    &mut pool,
                )
            },
            Err(AsError::KernelRange)
        );
        unsafe { aspace.teardown_pool(&mut pool) };
        assert_eq!(used(&pool), before);
    }

    #[test]
    fn map_unmap_teardown_balances_frames() {
        let mut pool = Pool::new(128);
        let mut kernel = kernel_mapper(&mut pool);
        let kva = VirtAddr(0xFFFF_C000_0010_0000);
        unsafe {
            kernel
                .map_page(
                    kva,
                    PhysAddr(0x0080_0000),
                    physmap_flags(),
                    PageSize::Size4K,
                    MapMode::Fresh,
                    &mut pool,
                )
                .unwrap();
        }
        let before = used(&pool);
        let mut aspace = unsafe { AddressSpace::new(&kernel, &mut pool) }.unwrap();
        assert_eq!(
            aspace.mapper().pml4_entry(KERNEL_PML4_FIRST),
            kernel.pml4_entry(KERNEL_PML4_FIRST)
        );
        let user_va = 0x0000_0000_0040_0000u64;
        unsafe {
            aspace
                .map_anon(user_va, PAGE_SIZE_4K * 2, UserPerms::RW, &mut pool)
                .unwrap();
        }
        assert_eq!(aspace.user_frames(), 2);
        assert!(aspace.pt_frames() >= 2);
        assert!(aspace.check_user_range(user_va, 16).is_ok());
        unsafe {
            aspace
                .unmap_free(user_va, PAGE_SIZE_4K * 2, &mut pool, &mut |_| {})
                .unwrap();
        }
        assert_eq!(aspace.user_frames(), 0);
        assert_eq!(
            aspace.check_user_range(user_va, 16),
            Err(UserMemError::Unmapped)
        );
        let st = unsafe { aspace.teardown_pool(&mut pool) };
        assert_eq!(st.user_frames, 0);
        assert!(st.pt_frames >= 1);
        assert_eq!(used(&pool), before);
        assert!(kernel.translate(kva).is_some());
    }

    #[test]
    fn user_ptr_helpers() {
        let mut pool = Pool::new(64);
        let kernel = kernel_mapper(&mut pool);
        let before = used(&pool);
        let mut aspace = unsafe { AddressSpace::new(&kernel, &mut pool) }.unwrap();
        let va = 0x0000_0000_0040_0000u64;
        unsafe {
            aspace
                .map_anon(va, PAGE_SIZE_4K, UserPerms::RW, &mut pool)
                .unwrap();
        }
        assert_eq!(aspace.check_user_range(0, 8), Err(UserMemError::NullGuard));
        assert_eq!(
            aspace.check_user_range(0xFFFF_8000_0000_1000, 8),
            Err(UserMemError::Kernel)
        );
        assert_eq!(
            aspace.check_user_range(u64::MAX, 2),
            Err(UserMemError::Overflow)
        );
        assert_eq!(
            aspace.check_user_range(0x0000_8000_0000_0000, 8),
            Err(UserMemError::NonCanonical)
        );
        assert_eq!(
            aspace.check_user_range(va + PAGE_SIZE_4K, 8),
            Err(UserMemError::Unmapped)
        );
        assert!(aspace.check_user_range(va, 8).is_ok());
        assert!(aspace.check_user_range(va, 0).is_ok());
        aspace.write_bytes(va, b"abcd").unwrap();
        let mut got = [0u8; 4];
        aspace.read_bytes(va, &mut got).unwrap();
        assert_eq!(&got, b"abcd");
        aspace.zero_bytes(va, 2).unwrap();
        aspace.read_bytes(va, &mut got).unwrap();
        assert_eq!(&got, b"\0\0cd");
        assert_eq!(aspace.write_bytes(0, b"x"), Err(UserMemError::NullGuard));
        let _ = UserMemError::Kernel.errno();
        unsafe { aspace.teardown_pool(&mut pool) };
        assert_eq!(used(&pool), before);
        let _ = PTE_ADDR_MASK;
        let _ = UserFreeStats {
            leaves: 0,
            tables: 0,
        };
    }

    #[test]
    fn clone_anon_copies_bytes_not_frames() {
        let mut pool = Pool::new(128);
        let kernel = kernel_mapper(&mut pool);
        let before = used(&pool);
        let mut src = unsafe { AddressSpace::new(&kernel, &mut pool) }.unwrap();
        let va = 0x0000_0000_0040_0000u64;
        unsafe {
            src.map_anon(va, PAGE_SIZE_4K, UserPerms::RW, &mut pool)
                .unwrap();
        }
        src.write_bytes(va, b"fork-me").unwrap();
        let dst = unsafe { src.clone_anon(&kernel, &mut pool) }.unwrap();
        let mut got = [0u8; 7];
        dst.read_bytes(va, &mut got).unwrap();
        assert_eq!(&got, b"fork-me");
        src.write_bytes(va, b"parent!").unwrap();
        dst.read_bytes(va, &mut got).unwrap();
        assert_eq!(&got, b"fork-me");
        unsafe {
            let mut src = src;
            src.teardown_pool(&mut pool);
            let mut dst = dst;
            dst.teardown_pool(&mut pool);
        }
        assert_eq!(used(&pool), before);
    }

    #[test]
    fn kernel_half_not_owned() {
        let mut pool = Pool::new(64);
        let mut kernel = kernel_mapper(&mut pool);
        unsafe {
            kernel
                .map_page(
                    VirtAddr(0xFFFF_8000_0020_0000),
                    PhysAddr(0x0040_0000),
                    physmap_flags(),
                    PageSize::Size2M,
                    MapMode::Fresh,
                    &mut pool,
                )
                .unwrap();
        }
        let before = used(&pool);
        let mut aspace = unsafe { AddressSpace::new(&kernel, &mut pool) }.unwrap();
        assert_eq!(
            aspace.mapper().pml4_entry(256) & PTE_ADDR_MASK,
            kernel.pml4_entry(256) & PTE_ADDR_MASK
        );
        unsafe { aspace.teardown_pool(&mut pool) };
        // The shared kernel-half tables stay allocated; only the root went.
        assert_eq!(used(&pool), before);
        assert!(kernel.translate(VirtAddr(0xFFFF_8000_0020_0000)).is_some());
    }

    #[test]
    fn user_mem_error_is_efault() {
        for e in [
            UserMemError::NonCanonical,
            UserMemError::Kernel,
            UserMemError::Overflow,
            UserMemError::Unmapped,
            UserMemError::NullGuard,
        ] {
            match e {
                UserMemError::NonCanonical
                | UserMemError::Kernel
                | UserMemError::Overflow
                | UserMemError::Unmapped
                | UserMemError::NullGuard => assert_eq!(e.errno(), 14),
            }
        }
    }

    #[test]
    fn map_anon_rolls_back_on_leaf_oom() {
        let mut pool = Pool::new(16);
        let kernel = kernel_mapper(&mut pool);
        let baseline = used(&pool);
        let mut aspace = unsafe { AddressSpace::new(&kernel, &mut pool) }.unwrap();
        let after_new = used(&pool);
        let va = 0x0000_0000_0040_0000u64;
        assert_eq!(
            unsafe { aspace.map_anon(va, 64 * PAGE_SIZE_4K, UserPerms::RW, &mut pool) },
            Err(AsError::OutOfFrames)
        );
        assert_eq!(aspace.user_frames(), 0);
        assert_eq!(aspace.regions().count(), 0);
        assert_eq!(
            aspace.check_user_range(va, PAGE_SIZE_4K),
            Err(UserMemError::Unmapped)
        );
        assert_eq!(
            aspace.check_user_range(va + 8 * PAGE_SIZE_4K, PAGE_SIZE_4K),
            Err(UserMemError::Unmapped)
        );
        assert_eq!(used(&pool), after_new + aspace.pt_frames() - 1);
        unsafe {
            aspace
                .map_anon(va, 2 * PAGE_SIZE_4K, UserPerms::RW, &mut pool)
                .unwrap();
        }
        assert_eq!(aspace.user_frames(), 2);
        assert_eq!(aspace.regions().count(), 1);
        assert_eq!(used(&pool), after_new + aspace.pt_frames() - 1 + 2);
        unsafe { aspace.teardown_pool(&mut pool) };
        assert_eq!(used(&pool), baseline);
    }

    const P: u64 = PAGE_SIZE_4K;
    const BASE: u64 = 0x0000_0000_0040_0000;

    fn nop(_: u64) {}

    #[test]
    fn addr_space_munmap_splits_region() {
        let mut pool = Pool::new(128);
        let kernel = kernel_mapper(&mut pool);
        let before = used(&pool);
        let mut a = unsafe { AddressSpace::new(&kernel, &mut pool) }.unwrap();
        unsafe { a.map_anon(BASE, 3 * P, UserPerms::RW, &mut pool).unwrap() };
        a.write_bytes(BASE, b"one").unwrap();
        a.write_bytes(BASE + 2 * P, b"three").unwrap();
        unsafe { a.unmap_free(BASE + P, P, &mut pool, &mut nop).unwrap() };
        let mut rs: Vec<Region> = a.regions().collect();
        rs.sort_by_key(|r| r.start);
        assert_eq!(rs.len(), 2);
        assert_eq!((rs[0].start, rs[0].len), (BASE, P));
        assert_eq!((rs[1].start, rs[1].len), (BASE + 2 * P, P));
        assert_eq!(a.user_frames(), 2);
        assert_eq!(a.check_user_range(BASE + P, 1), Err(UserMemError::Unmapped));
        let mut c = unsafe { a.clone_anon(&kernel, &mut pool) }.unwrap();
        let mut got = [0u8; 5];
        c.read_bytes(BASE, &mut got[..3]).unwrap();
        assert_eq!(&got[..3], b"one");
        c.read_bytes(BASE + 2 * P, &mut got).unwrap();
        assert_eq!(&got, b"three");
        assert_eq!(c.user_frames(), 2);
        unsafe {
            c.teardown_pool(&mut pool);
            a.teardown_pool(&mut pool);
        }
        assert_eq!(used(&pool), before);
    }

    #[test]
    fn munmap_trims_spans_and_holes() {
        let mut pool = Pool::new(128);
        let kernel = kernel_mapper(&mut pool);
        let before = used(&pool);
        let mut a = unsafe { AddressSpace::new(&kernel, &mut pool) }.unwrap();
        let r2 = BASE + 8 * P;
        unsafe {
            a.map_anon(BASE, 4 * P, UserPerms::RW, &mut pool).unwrap();
            a.map_anon(r2, 4 * P, UserPerms::RW, &mut pool).unwrap();
        }
        let base_used = used(&pool) - a.user_frames();
        // Head of the first region.
        unsafe { a.unmap_free(BASE, P, &mut pool, &mut nop).unwrap() };
        // Tail of the second.
        unsafe { a.unmap_free(r2 + 3 * P, P, &mut pool, &mut nop).unwrap() };
        assert_eq!(a.user_frames(), 6);
        // A span over the first's tail, the hole, and the second's head.
        unsafe {
            a.unmap_free(BASE + 2 * P, 8 * P, &mut pool, &mut nop)
                .unwrap()
        };
        let mut rs: Vec<(u64, u64)> = a.regions().map(|r| (r.start, r.len)).collect();
        rs.sort();
        assert_eq!(rs, [(BASE + P, P), (r2 + 2 * P, P)]);
        assert_eq!(a.user_frames(), 2);
        // A hole, an empty range, and a range below the null guard.
        unsafe {
            a.unmap_free(BASE + 4 * P, 4 * P, &mut pool, &mut nop)
                .unwrap();
            a.unmap_free(BASE, 0, &mut pool, &mut nop).unwrap();
            a.unmap_free(0, P, &mut pool, &mut nop).unwrap();
        }
        assert_eq!(a.user_frames(), 2);
        assert_eq!(used(&pool), base_used + a.user_frames());
        assert_eq!(
            unsafe { a.unmap_free(BASE + 1, P, &mut pool, &mut nop) },
            Err(AsError::Misaligned)
        );
        assert_eq!(
            unsafe { a.unmap_free(USER_MAP_END, P, &mut pool, &mut nop) },
            Err(AsError::KernelRange)
        );
        unsafe { a.teardown_pool(&mut pool) };
        assert_eq!(used(&pool), before);
    }

    #[test]
    fn munmap_split_needs_slot() {
        let mut pool = Pool::new(128);
        let kernel = kernel_mapper(&mut pool);
        let mut a = unsafe { AddressSpace::new(&kernel, &mut pool) }.unwrap();
        unsafe { a.map_anon(BASE, 3 * P, UserPerms::RW, &mut pool).unwrap() };
        let mut va = BASE + 4 * P;
        while a.regions().count() < MAX_REGIONS {
            unsafe { a.map_anon(va, P, UserPerms::RW, &mut pool).unwrap() };
            va += 2 * P;
        }
        let frames = a.user_frames();
        let used_before = used(&pool);
        assert_eq!(
            unsafe { a.unmap_free(BASE + P, P, &mut pool, &mut nop) },
            Err(AsError::NoRegionSlot)
        );
        assert_eq!(a.user_frames(), frames);
        assert_eq!(used(&pool), used_before);
        assert!(a.check_user_range(BASE, 3 * P).is_ok());
        // A trim needs no slot.
        unsafe { a.unmap_free(BASE, P, &mut pool, &mut nop).unwrap() };
        assert_eq!(a.user_frames(), frames - 1);
        unsafe { a.teardown_pool(&mut pool) };
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Ev {
        Flush(u64),
        Free(u64),
    }

    /// A pool that logs each frame it takes back into a log it shares with
    /// the flush.
    struct Logging<'a> {
        pool: &'a mut Pool,
        log: &'a core::cell::RefCell<Vec<Ev>>,
    }

    unsafe impl FrameFree for Logging<'_> {
        fn free_frame(&mut self, f: Frames) {
            self.log.borrow_mut().push(Ev::Free(f.base()));
            self.pool.buddy.free(f);
        }
    }

    #[test]
    fn munmap_flush_before_free() {
        let mut pool = Pool::new(128);
        let kernel = kernel_mapper(&mut pool);
        let mut a = unsafe { AddressSpace::new(&kernel, &mut pool) }.unwrap();
        unsafe { a.map_anon(BASE, 4 * P, UserPerms::RW, &mut pool).unwrap() };
        let pas: Vec<u64> = (0..4)
            .map(|i| {
                a.mapper()
                    .translate(VirtAddr(BASE + i * P))
                    .unwrap()
                    .0
                    .as_u64()
            })
            .collect();
        let log = core::cell::RefCell::new(Vec::new());
        {
            let mut lp = Logging {
                pool: &mut pool,
                log: &log,
            };
            let mut flush = |va: u64| log.borrow_mut().push(Ev::Flush(va));
            unsafe { a.unmap_free(BASE, 4 * P, &mut lp, &mut flush).unwrap() };
        }
        let log = log.into_inner();
        assert_eq!(log.len(), 8);
        for i in 0..4u64 {
            let f = log
                .iter()
                .position(|e| *e == Ev::Flush(BASE + i * P))
                .unwrap();
            let r = log
                .iter()
                .position(|e| *e == Ev::Free(pas[i as usize]))
                .unwrap();
            assert!(f < r, "page {i}: flush at {f}, free at {r}");
        }
        unsafe { a.teardown_pool(&mut pool) };
    }

    #[test]
    fn addr_space_brk_grow_shrink() {
        let mut pool = Pool::new(128);
        let kernel = kernel_mapper(&mut pool);
        let before = used(&pool);
        let mut a = unsafe { AddressSpace::new(&kernel, &mut pool) }.unwrap();
        assert_eq!(a.brk_plan(BASE + P), BrkPlan::Current);
        a.set_brk_start(BASE + 0x10);
        let b = BASE + P;
        assert_eq!((a.brk_start(), a.brk()), (b, b));
        assert_eq!(a.brk_plan(0), BrkPlan::Current);
        assert_eq!(a.brk_plan(b - 1), BrkPlan::Current);
        assert_eq!(a.brk_plan(USER_MAP_END + 1), BrkPlan::Current);
        assert_eq!(a.brk_plan(b), BrkPlan::SamePage);
        // Grow into the first page, then within it, then across two more.
        let grow = |a: &mut AddressSpace, pool: &mut Pool, want: u64| match a.brk_plan(want) {
            BrkPlan::Grow { va, len } => {
                a.heap_grow_check(va, len).unwrap();
                unsafe { a.map_pages(va, len, UserPerms::RW, pool).unwrap() };
                a.heap_grow_commit(va, len, want).unwrap();
            }
            BrkPlan::SamePage => a.set_brk(want),
            other => panic!("brk({want:#x}): {other:?}"),
        };
        grow(&mut a, &mut pool, b + 0x10);
        assert_eq!(a.brk_plan(b + 0x20), BrkPlan::SamePage);
        grow(&mut a, &mut pool, b + 0x20);
        grow(&mut a, &mut pool, b + 2 * P + 8);
        grow(&mut a, &mut pool, b + 3 * P);
        assert_eq!(a.brk(), b + 3 * P);
        let rs: Vec<Region> = a.regions().collect();
        assert_eq!(rs.len(), 1);
        assert_eq!((rs[0].start, rs[0].len), (b, 3 * P));
        assert_eq!(a.user_frames(), 3);
        // Shrink to one page: the two above go.
        assert_eq!(
            a.brk_plan(b + P),
            BrkPlan::Shrink {
                va: b + P,
                len: 2 * P
            }
        );
        unsafe { a.unmap_free(b + P, 2 * P, &mut pool, &mut nop).unwrap() };
        a.set_brk(b + P);
        assert_eq!(a.user_frames(), 1);
        assert_eq!(a.regions().next().map(|r| r.len), Some(P));
        // Growth into another region leaves the break.
        unsafe { a.map_anon(b + 2 * P, P, UserPerms::RW, &mut pool).unwrap() };
        assert_eq!(a.brk_plan(b + 3 * P), BrkPlan::Current);
        assert_eq!(a.heap_grow_check(b + P, 2 * P), Err(AsError::Overlap));
        // Shrinking to the start removes the heap region; growth adds it back.
        unsafe { a.unmap_free(b, P, &mut pool, &mut nop).unwrap() };
        a.set_brk(b);
        assert_eq!(a.regions().count(), 1);
        grow(&mut a, &mut pool, b + 8);
        assert_eq!(a.regions().count(), 2);
        unsafe { a.teardown_pool(&mut pool) };
        assert_eq!(used(&pool), before);
    }

    fn req(addr: u64, len: u64, fixed: Fixed) -> MmapReq {
        MmapReq {
            addr,
            len,
            perms: Some(UserPerms::RW),
            fixed,
        }
    }

    #[test]
    fn addr_space_mmap_anon_placement() {
        let mut pool = Pool::new(128);
        let kernel = kernel_mapper(&mut pool);
        let mut a = unsafe { AddressSpace::new(&kernel, &mut pool) }.unwrap();
        assert_eq!(MMAP_TOP, 0x7FFF_F7FF_F000);
        let top = a.mmap_place(&req(0, 4 * P, Fixed::No)).unwrap();
        assert_eq!(top, MMAP_TOP - 4 * P);
        unsafe { a.map_anon(top, 4 * P, UserPerms::RW, &mut pool).unwrap() };
        assert_eq!(a.mmap_place(&req(0, 2 * P, Fixed::No)), Ok(top - 2 * P));
        // A region at the top leaves a gap too small for 2 pages, which
        // placement skips.
        unsafe {
            a.map_anon(top - 3 * P, 2 * P, UserPerms::RW, &mut pool)
                .unwrap()
        };
        assert_eq!(a.mmap_place(&req(0, 2 * P, Fixed::No)), Ok(top - 5 * P));
        assert_eq!(a.mmap_place(&req(0, P, Fixed::No)), Ok(top - P));
        // A free hint is used, rounded down; a taken one is ignored.
        assert_eq!(a.mmap_place(&req(BASE + 5, P, Fixed::No)), Ok(BASE));
        assert_eq!(a.mmap_place(&req(top + 8, P, Fixed::No)), Ok(top - P));
        // Fixed requests.
        assert_eq!(a.mmap_place(&req(BASE, P, Fixed::Replace)), Ok(BASE));
        assert_eq!(
            a.mmap_place(&req(top, P, Fixed::NoReplace)),
            Err(AsError::Overlap)
        );
        assert_eq!(
            a.mmap_place(&req(top, P, Fixed::Replace)),
            Err(AsError::Overlap)
        );
        assert_eq!(
            a.mmap_place(&req(0, P, Fixed::Replace)),
            Err(AsError::NullGuard)
        );
        assert_eq!(
            a.mmap_place(&req(USER_MAP_END, P, Fixed::NoReplace)),
            Err(AsError::KernelRange)
        );
        // Too long for the space below `MMAP_TOP`.
        assert_eq!(
            a.mmap_place(&req(0, MMAP_TOP, Fixed::No)),
            Err(AsError::NoVaSpace)
        );
        unsafe { a.teardown_pool(&mut pool) };
    }

    #[test]
    fn mmap_request_decodes_flags() {
        const PA: u64 = MAP_PRIVATE | MAP_ANONYMOUS;
        const RW: u64 = PROT_READ | PROT_WRITE;
        let ok = |prot, flags| mmap_request(0x1000, 10, prot, flags, 0);
        for extra in [
            0,
            MAP_NORESERVE,
            MAP_POPULATE,
            MAP_STACK,
            MAP_NORESERVE | MAP_POPULATE | MAP_STACK,
        ] {
            let r = ok(RW, PA | extra).unwrap();
            assert_eq!(r.len, P);
            assert_eq!(r.fixed, Fixed::No);
            assert_eq!(
                ok(RW, PA | extra | MAP_FIXED).unwrap().fixed,
                Fixed::Replace
            );
            assert_eq!(
                ok(RW, PA | extra | MAP_FIXED_NOREPLACE).unwrap().fixed,
                Fixed::NoReplace
            );
        }
        for (prot, perms) in [
            (0, None),
            (PROT_READ, Some(UserPerms::READ)),
            (PROT_WRITE, Some(UserPerms::RW)),
            (RW, Some(UserPerms::RW)),
            (PROT_EXEC, Some(UserPerms::RX)),
            (PROT_READ | PROT_EXEC, Some(UserPerms::RX)),
            (RW | PROT_EXEC, Some(UserPerms::RWX)),
        ] {
            assert_eq!(ok(prot, PA).unwrap().perms, perms, "prot {prot}");
        }
        // Each error, and that the earlier check wins.
        use MmapError::*;
        let cases: [(u64, u64, u64, u64, u64, MmapError); 11] = [
            (0, 0, 8, MAP_SHARED, 1, Inval),
            (0, 0, RW, MAP_PRIVATE, 0, NotAnon),
            (0, 0, 8, MAP_SHARED, 0, NotAnon),
            (0, 0, 8, MAP_SHARED | MAP_ANONYMOUS, 0, Inval),
            (0, P, RW, MAP_ANONYMOUS, 0, Inval),
            (0, P, RW, MAP_SHARED | MAP_ANONYMOUS, 0, Inval),
            (0, P, RW, PA | 0x100, 0, Inval),
            (0, P, 8, PA, 0, Inval),
            (0, 1 << 47, 8, PA, 0, Inval),
            (0, 1 << 47, RW, PA, 0, NoMem),
            (0x4000_0001, P, RW, PA | MAP_FIXED, 0, Inval),
        ];
        for (addr, len, prot, flags, off, want) in cases {
            assert_eq!(
                mmap_request(addr, len, prot, flags, off),
                Err(want),
                "addr {addr:#x} len {len:#x} prot {prot} flags {flags:#x} off {off}"
            );
        }
        assert_eq!(mmap_request(0, u64::MAX, RW, PA, 0), Err(NoMem));
        assert_eq!(
            mmap_request(0, USER_MAP_END, RW, PA, 0).map(|r| r.len),
            Ok(USER_MAP_END)
        );
        assert_eq!(
            mmap_request(0x4000_0001, P, RW, PA | MAP_FIXED_NOREPLACE, 0),
            Err(Inval)
        );
        assert_eq!(
            mmap_request(0x4000_0001, P, RW, PA, 0).map(|r| r.addr),
            Ok(0x4000_0001)
        );
    }

    /// Record a `PROT_NONE` reservation, as the kernel's `mmap` does.
    fn reserve(a: &mut AddressSpace, va: u64, len: u64) -> Result<(), AsError> {
        a.check_new_region(va, len)?;
        a.insert_region(Region {
            start: va,
            len,
            perms: UserPerms::READ,
            backing: Backing::Reserved,
        })
    }

    #[test]
    fn prot_none_reserves_no_frames() {
        let mut pool = Pool::new(64);
        let kernel = kernel_mapper(&mut pool);
        let mut a = unsafe { AddressSpace::new(&kernel, &mut pool) }.unwrap();
        let after_new = used(&pool);
        reserve(&mut a, BASE, 16 * P).unwrap();
        assert_eq!(used(&pool), after_new);
        assert_eq!(a.user_frames(), 0);
        assert_eq!(a.check_user_range(BASE, 1), Err(UserMemError::Unmapped));
        assert_eq!(
            unsafe { a.map_anon(BASE + P, P, UserPerms::RW, &mut pool) },
            Err(AsError::Overlap)
        );
        assert_eq!(
            a.mmap_place(&req(0, 17 * P, Fixed::No)),
            Ok(MMAP_TOP - 17 * P)
        );
        // Unmapping splits the reservation and frees nothing.
        let mut flushes = 0;
        unsafe {
            a.unmap_free(BASE + 4 * P, 4 * P, &mut pool, &mut |_| flushes += 1)
                .unwrap()
        };
        assert_eq!(flushes, 0);
        assert_eq!(a.regions().count(), 2);
        assert_eq!(used(&pool), after_new);
        unsafe { a.teardown_pool(&mut pool) };
    }

    #[test]
    fn clone_keeps_brk_and_reservations() {
        let mut pool = Pool::new(128);
        let kernel = kernel_mapper(&mut pool);
        let before = used(&pool);
        let mut a = unsafe { AddressSpace::new(&kernel, &mut pool) }.unwrap();
        a.set_brk_start(BASE);
        let BrkPlan::Grow { va, len } = a.brk_plan(BASE + 0x1800) else {
            panic!("no grow");
        };
        unsafe { a.map_pages(va, len, UserPerms::RW, &mut pool).unwrap() };
        a.heap_grow_commit(va, len, BASE + 0x1800).unwrap();
        a.write_bytes(BASE + 0x1000, b"heap").unwrap();
        reserve(&mut a, BASE + 16 * P, 4 * P).unwrap();
        let mut c = unsafe { a.clone_anon(&kernel, &mut pool) }.unwrap();
        assert_eq!((c.brk_start(), c.brk()), (BASE, BASE + 0x1800));
        assert_eq!(c.user_frames(), 2);
        let mut got = [0u8; 4];
        c.read_bytes(BASE + 0x1000, &mut got).unwrap();
        assert_eq!(&got, b"heap");
        let mut rs: Vec<(u64, u64, Backing)> =
            c.regions().map(|r| (r.start, r.len, r.backing)).collect();
        rs.sort_by_key(|r| r.0);
        assert_eq!(
            rs,
            [
                (BASE, 2 * P, Backing::Anonymous),
                (BASE + 16 * P, 4 * P, Backing::Reserved)
            ]
        );
        assert_eq!(
            c.check_user_range(BASE + 16 * P, 1),
            Err(UserMemError::Unmapped)
        );
        unsafe {
            c.teardown_pool(&mut pool);
            a.teardown_pool(&mut pool);
        }
        assert_eq!(used(&pool), before);
    }

    #[test]
    fn fixed_tables_match_limits() {
        let mut pool = Pool::new(64);
        let kernel = kernel_mapper(&mut pool);
        let aspace = unsafe { AddressSpace::new(&kernel, &mut pool) }.unwrap();
        assert_eq!(aspace.regions.len(), crate::limits::MAX_REGIONS);
    }
}
