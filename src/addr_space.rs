//! Address space: user half of a PML4 plus region tracking. ROADMAP §9.2.
//!
//! Kernel half (PML4[256..512)) is shared by copying the kernel's upper
//! entries so they point at the same PDPTs. VA 0 is never mapped. The
//! kernel's 512 MiB low identity window is GLOBAL; user maps in that
//! range can alias a stale TLB until identity is torn down.

use crate::paging::{
    is_canonical, user_leaf_flags, FrameAlloc, MapError, MapMode, Mapper, PageFlags, PageSize,
    PhysAddr, Probe, VirtAddr, KERNEL_PML4_FIRST, NULL_GUARD_LEN, PAGE_SIZE_4K,
    PTES_PER_TABLE, USER_END,
};

pub const MAX_REGIONS: usize = 32;

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
    Map(MapError),
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
}

struct Counting<'a, A: FrameAlloc> {
    inner: &'a mut A,
    n: &'a mut usize,
}

unsafe impl<A: FrameAlloc> FrameAlloc for Counting<'_, A> {
    fn alloc_frame(&mut self) -> Option<PhysAddr> {
        let p = self.inner.alloc_frame()?;
        *self.n += 1;
        Some(p)
    }
}

impl AddressSpace {
    /// New PML4, kernel half shared from `kernel`. `alloc` supplies the
    /// PML4 frame. `pt_frames` starts at 1 (the root).
    ///
    /// # Safety
    /// `kernel` is the live kernel mapper. `alloc` returns owned frames
    /// reachable through `kernel.hhdm_offset()`.
    pub unsafe fn new<A: FrameAlloc>(kernel: &Mapper, alloc: &mut A) -> Option<Self> {
        let root = alloc.alloc_frame()?;
        let mapper = unsafe { Mapper::new(root, kernel.hhdm_offset()) };
        unsafe { mapper.zero_frame(root) };
        let mut space = Self {
            mapper,
            regions: [None; MAX_REGIONS],
            user_frames: 0,
            pt_frames: 1,
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

    pub unsafe fn map_anon<A: FrameAlloc + FrameFree>(
        &mut self,
        va: u64,
        len: u64,
        perms: UserPerms,
        alloc: &mut A,
    ) -> Result<(), AsError> {
        check_map_range(va, len)?;
        if self.overlaps(va, len) {
            return Err(AsError::Overlap);
        }
        let slot = self
            .regions
            .iter()
            .position(|r| r.is_none())
            .ok_or(AsError::NoRegionSlot)?;

        let pages = (len / PAGE_SIZE_4K) as usize;
        let flags = perms.flags();
        let mut extra = 0usize;
        let mut mapped = 0u64;
        while mapped < len {
            let mut n = 0usize;
            let page_va = VirtAddr(va + mapped);
            let pa = {
                let mut c = Counting {
                    inner: &mut *alloc,
                    n: &mut n,
                };
                c.alloc_frame().ok_or(AsError::OutOfFrames)?
            };
            extra += n;
            let mut n_pt = 0usize;
            let map_rc = {
                let mut c = Counting {
                    inner: &mut *alloc,
                    n: &mut n_pt,
                };
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
            extra += n_pt;
            if let Err(e) = map_rc {
                let _ = unsafe { self.mapper.unmap_page(page_va) };
                unsafe { alloc.free_frame(pa) };
                let _ = unsafe { self.unmap_free(va, mapped, alloc) };
                return Err(match e {
                    MapError::OutOfFrames => AsError::OutOfFrames,
                    MapError::AlreadyMapped => AsError::AlreadyMapped,
                    other => AsError::Map(other),
                });
            }
            mapped += PAGE_SIZE_4K;
        }

        self.user_frames += pages;
        self.pt_frames += extra - pages;
        self.regions[slot] = Some(Region {
            start: va,
            len,
            perms,
            backing: Backing::Anonymous,
        });
        Ok(())
    }

    /// Free every user leaf + user PT page + the PML4. Kernel-half
    /// PDPTs are not touched. `free` must return frames to `alloc`.
    pub unsafe fn teardown<F>(&mut self, free: &mut F) -> TeardownStats
    where
        F: FnMut(PhysAddr),
    {
        let walked = unsafe { self.mapper.free_user_half(free) };
        let pml4 = self.mapper.root();
        free(pml4);
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
            if ptr >= USER_END {
                return Err(UserMemError::Kernel);
            }
            return Ok(());
        }
        let end = ptr.checked_add(len).ok_or(UserMemError::Overflow)?;
        if !is_canonical(ptr) || !is_canonical(end.wrapping_sub(1)) {
            return Err(UserMemError::NonCanonical);
        }
        if ptr >= USER_END || end > USER_END {
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

    fn drop_regions_covered(&mut self, va: u64, len: u64) {
        let end = va.saturating_add(len);
        let mut i = 0;
        while i < MAX_REGIONS {
            if let Some(r) = self.regions[i] {
                let r_end = r.start.saturating_add(r.len);
                if va <= r.start && r_end <= end {
                    self.regions[i] = None;
                }
            }
            i += 1;
        }
    }
}

fn check_map_range(va: u64, len: u64) -> Result<(), AsError> {
    if len == 0 {
        return Ok(());
    }
    if va % PAGE_SIZE_4K != 0 || len % PAGE_SIZE_4K != 0 {
        return Err(AsError::Misaligned);
    }
    let end = va.checked_add(len).ok_or(AsError::Overflow)?;
    if va < NULL_GUARD_LEN {
        return Err(AsError::NullGuard);
    }
    if va >= USER_END || end > USER_END {
        return Err(AsError::KernelRange);
    }
    if !is_canonical(va) || !is_canonical(end - 1) {
        return Err(AsError::KernelRange);
    }
    Ok(())
}

/// Allocator that can return frames. Kernel buddy and host tests.
pub unsafe trait FrameFree {
    unsafe fn free_frame(&mut self, pa: PhysAddr);
}

impl AddressSpace {
    pub unsafe fn unmap_free<A>(&mut self, va: u64, len: u64, pool: &mut A) -> Result<(), AsError>
    where
        A: FrameAlloc + FrameFree,
    {
        check_map_range(va, len)?;
        if len == 0 {
            return Ok(());
        }
        let mut off = 0u64;
        while off < len {
            let page = VirtAddr(va + off);
            match unsafe { self.mapper.unmap_page(page) } {
                Some((pa, PageSize::Size4K)) => {
                    unsafe { pool.free_frame(pa) };
                    self.user_frames = self.user_frames.saturating_sub(1);
                }
                Some((_, PageSize::Size2M)) => {
                    return Err(AsError::Map(MapError::PageSizeMismatch));
                }
                None => return Err(AsError::NotMapped),
            }
            off += PAGE_SIZE_4K;
        }
        self.drop_regions_covered(va, len);
        Ok(())
    }

    pub unsafe fn teardown_pool<A: FrameFree>(&mut self, pool: &mut A) -> TeardownStats {
        let mut free = |pa: PhysAddr| unsafe { pool.free_frame(pa) };
        unsafe { self.teardown(&mut free) }
    }
}

const _: () = {
    assert!(KERNEL_PML4_FIRST == 256);
    assert!(PTES_PER_TABLE == 512);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paging::{physmap_flags, PTE_ADDR_MASK, UserFreeStats};
    use std::vec;
    use std::vec::Vec;

    struct TestPool {
        _mem: Vec<u64>,
        hhdm_offset: u64,
        next_frame: u64,
        end: u64,
        live: Vec<u64>,
        recycled: Vec<u64>,
    }

    impl TestPool {
        fn new(frames: usize) -> Self {
            let words = (frames * PAGE_SIZE_4K as usize) / 8;
            let mem: Vec<u64> = vec![0u64; words];
            let ptr = mem.as_ptr() as u64;
            let phys_base = 0x0200_0000;
            let hhdm_offset = ptr.wrapping_sub(phys_base);
            Self {
                _mem: mem,
                hhdm_offset,
                next_frame: phys_base,
                end: phys_base + (frames as u64) * PAGE_SIZE_4K,
                live: Vec::new(),
                recycled: Vec::new(),
            }
        }

        fn live_count(&self) -> usize {
            self.live.len()
        }
    }

    unsafe impl FrameAlloc for TestPool {
        fn alloc_frame(&mut self) -> Option<PhysAddr> {
            let p = if let Some(p) = self.recycled.pop() {
                p
            } else {
                if self.next_frame >= self.end {
                    return None;
                }
                let p = self.next_frame;
                self.next_frame += PAGE_SIZE_4K;
                p
            };
            self.live.push(p);
            Some(PhysAddr(p))
        }
    }

    unsafe impl FrameFree for TestPool {
        unsafe fn free_frame(&mut self, pa: PhysAddr) {
            let p = pa.as_u64();
            let i = self
                .live
                .iter()
                .position(|x| *x == p)
                .expect("double free or wild free");
            self.live.swap_remove(i);
            self.recycled.push(p);
        }
    }

    fn kernel_mapper(pool: &mut TestPool) -> Mapper {
        let root = pool.alloc_frame().unwrap();
        let mapper = unsafe { Mapper::new(root, pool.hhdm_offset) };
        unsafe { mapper.zero_frame(root) };
        mapper
    }

    #[test]
    fn null_guard_and_kernel_rejected() {
        let mut pool = TestPool::new(64);
        let kernel = kernel_mapper(&mut pool);
        let mut aspace = unsafe { AddressSpace::new(&kernel, &mut pool) }.unwrap();
        assert_eq!(
            unsafe { aspace.map_anon(0, PAGE_SIZE_4K, UserPerms::RW, &mut pool) },
            Err(AsError::NullGuard)
        );
        assert_eq!(
            unsafe { aspace.map_anon(USER_END, PAGE_SIZE_4K, UserPerms::RW, &mut pool) },
            Err(AsError::KernelRange)
        );
        assert_eq!(
            unsafe {
                aspace.map_anon(0xFFFF_8000_0000_0000, PAGE_SIZE_4K, UserPerms::RW, &mut pool)
            },
            Err(AsError::KernelRange)
        );
        unsafe { aspace.teardown_pool(&mut pool) };
    }

    #[test]
    fn map_unmap_teardown_balances_frames() {
        let mut pool = TestPool::new(128);
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
        let before = pool.live_count();
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
                .unmap_free(user_va, PAGE_SIZE_4K * 2, &mut pool)
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
        assert_eq!(pool.live_count(), before);
        assert!(kernel.translate(kva).is_some());
    }

    #[test]
    fn user_ptr_helpers() {
        let mut pool = TestPool::new(64);
        let kernel = kernel_mapper(&mut pool);
        let mut aspace = unsafe { AddressSpace::new(&kernel, &mut pool) }.unwrap();
        let va = 0x0000_0000_0040_0000u64;
        unsafe {
            aspace
                .map_anon(va, PAGE_SIZE_4K, UserPerms::RW, &mut pool)
                .unwrap();
        }
        assert_eq!(
            aspace.check_user_range(0, 8),
            Err(UserMemError::NullGuard)
        );
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
        assert_eq!(
            aspace.write_bytes(0, b"x"),
            Err(UserMemError::NullGuard)
        );
        let _ = UserMemError::Kernel.errno();
        unsafe { aspace.teardown_pool(&mut pool) };
        let _ = PTE_ADDR_MASK;
        let _ = UserFreeStats {
            leaves: 0,
            tables: 0,
        };
    }

    #[test]
    fn kernel_half_not_owned() {
        let mut pool = TestPool::new(64);
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
        let mut aspace = unsafe { AddressSpace::new(&kernel, &mut pool) }.unwrap();
        assert_eq!(
            aspace.mapper().pml4_entry(256) & PTE_ADDR_MASK,
            kernel.pml4_entry(256) & PTE_ADDR_MASK
        );
        unsafe { aspace.teardown_pool(&mut pool) };
        assert!(kernel
            .translate(VirtAddr(0xFFFF_8000_0020_0000))
            .is_some());
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
}
