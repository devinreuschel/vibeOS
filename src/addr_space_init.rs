#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
//! Kernel AddressSpace: buddy + PT lock. ROADMAP §9.2.

use vibeos::addr_space::{AddressSpace, AsError, FrameFree, TeardownStats, UserPerms};
use vibeos::paging::{FrameAlloc, PAGE_SIZE_4K, PhysAddr};

use crate::paging_init;
use crate::per_cpu_init;
use crate::pmm_init;
use crate::x86;

struct BuddyPool;

unsafe impl FrameAlloc for BuddyPool {
    fn alloc_frame(&mut self) -> Option<PhysAddr> {
        pmm_init::with_buddy(|b| b.allocate_frame()).map(PhysAddr)
    }
}

unsafe impl FrameFree for BuddyPool {
    /// # Safety
    /// `pa` is an owned frame this pool may return to the buddy.
    unsafe fn free_frame(&mut self, pa: PhysAddr) {
        pmm_init::with_buddy(|b| unsafe { b.deallocate_frame(pa.as_u64()) });
    }
}

/// New user address space. Kernel half shared from the boot PML4.
pub fn create() -> Option<AddressSpace> {
    paging_init::with_pt(|| {
        let kernel = paging_init::current_mapper();
        let mut pool = BuddyPool;
        unsafe { AddressSpace::new(&kernel, &mut pool) }
    })
}

/// # Safety
/// Same contract as `AddressSpace::map_anon`.
pub unsafe fn map_anon(
    space: &mut AddressSpace,
    va: u64,
    len: u64,
    perms: UserPerms,
) -> Result<(), AsError> {
    paging_init::with_pt(|| {
        let mut pool = BuddyPool;
        unsafe { space.map_anon(va, len, perms, &mut pool) }
    })?;
    shootdown_user(space, va, len);
    Ok(())
}

/// # Safety
/// Same contract as `AddressSpace::unmap_free`.
pub unsafe fn unmap(space: &mut AddressSpace, va: u64, len: u64) -> Result<(), AsError> {
    paging_init::with_pt(|| {
        let mut pool = BuddyPool;
        unsafe { space.unmap_free(va, len, &mut pool) }
    })?;
    shootdown_user(space, va, len);
    Ok(())
}

pub fn teardown(mut space: AddressSpace) -> TeardownStats {
    paging_init::with_pt(|| {
        let mut pool = BuddyPool;
        unsafe { space.teardown_pool(&mut pool) }
    })
}

fn shootdown_user(space: &AddressSpace, va: u64, len: u64) {
    let cur = x86::read_cr3() & vibeos::paging::PTE_ADDR_MASK;
    if cur != space.root().as_u64() {
        return;
    }
    let mut off = 0u64;
    while off < len {
        x86::invlpg(va + off);
        off += PAGE_SIZE_4K;
    }
}

pub fn load_cr3(space: &AddressSpace) {
    let want = space.root().as_u64();
    per_cpu_init::with_current(|cpu| {
        if cpu.as_cr3 == want {
            return;
        }
        unsafe { x86::write_cr3(want) };
        cpu.as_cr3 = want;
    });
}

pub fn load_cr3_u64(want: u64) {
    per_cpu_init::with_current(|cpu| {
        if cpu.as_cr3 == want || want == 0 {
            return;
        }
        unsafe { x86::write_cr3(want) };
        cpu.as_cr3 = want;
    });
}

pub fn load_kernel_cr3() {
    load_cr3_u64(paging_init::kernel_cr3());
}

/// Full AS copy for fork. Caller must not be running on `src`'s CR3
/// teardown path; clone allocates a new PML4.
pub fn clone_full(src: &vibeos::addr_space::AddressSpace) -> Option<AddressSpace> {
    paging_init::with_pt(|| {
        let kernel = paging_init::current_mapper();
        let mut pool = BuddyPool;
        unsafe { src.clone_anon(&kernel, &mut pool) }.ok()
    })
}

pub fn cr3_was_skipped(space: &AddressSpace) -> bool {
    crate::per_cpu_init::current().as_cr3 == space.root().as_u64()
}
