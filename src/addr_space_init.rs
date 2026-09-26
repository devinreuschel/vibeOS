#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
//! Kernel AddressSpace: buddy + PT lock. ROADMAP §9.2.

use core::fmt;
use core::sync::atomic::Ordering;

use vibeos::addr_space::{AddressSpace, AsError, FrameFree, TeardownStats, UserPerms};
use vibeos::paging::{FrameAlloc, PAGE_SIZE_4K, PTE_ADDR_MASK};
use vibeos::pmm::Frames;
use vibeos::thread::ThreadId;

use crate::paging_init;
use crate::per_cpu_init;
use crate::pmm_init;
use crate::thread_init;
use crate::x86;

struct BuddyPool;

unsafe impl FrameAlloc for BuddyPool {
    fn alloc_frame(&mut self) -> Option<Frames> {
        pmm_init::with_buddy(|b| b.alloc(0))
    }
}

unsafe impl FrameFree for BuddyPool {
    fn free_frame(&mut self, f: Frames) {
        pmm_init::with_buddy(|b| b.free(f));
    }
}

/// New user address space. Kernel half shared from the boot PML4.
pub fn create() -> Option<AddressSpace> {
    let kernel = paging_init::current_mapper();
    let mut pool = BuddyPool;
    unsafe { AddressSpace::new(&kernel, &mut pool) }
}

/// # Safety
/// Same contract as `AddressSpace::map_anon`.
pub unsafe fn map_anon(
    space: &mut AddressSpace,
    va: u64,
    len: u64,
    perms: UserPerms,
) -> Result<(), AsError> {
    paging_init::with_pt(|_pt| {
        let mut pool = BuddyPool;
        unsafe { space.map_anon(va, len, perms, &mut pool) }
    })?;
    shootdown_user(space, va, len);
    Ok(())
}

/// # Safety
/// Same contract as `AddressSpace::unmap_free`.
pub unsafe fn unmap(space: &mut AddressSpace, va: u64, len: u64) -> Result<(), AsError> {
    paging_init::with_pt(|_pt| {
        let mut pool = BuddyPool;
        unsafe { space.unmap_free(va, len, &mut pool) }
    })?;
    shootdown_user(space, va, len);
    Ok(())
}

/// What still holds a root that [`teardown`] was asked to free.
enum RootHolder {
    /// CPU `cpu` has it loaded (CR3; TTBR0 on aarch64), or last recorded
    /// loading it.
    Cr3 { cpu: u32 },
    /// That thread's `Tcb.as_cr3` names it.
    Tcb(ThreadId),
}

impl fmt::Debug for RootHolder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RootHolder::Cr3 { cpu } => write!(f, "Cr3 {{ cpu: {cpu} }}"),
            RootHolder::Tcb(id) => write!(f, "Tcb({})", id.0),
        }
    }
}

/// Who still holds `root`: this CPU's live CR3, any CPU's recorded root,
/// or any TCB's saved root. Returns with no lock held, so the caller's
/// assertion never fires under PT or SCHED.
fn root_holder(root: u64) -> Option<RootHolder> {
    let here = per_cpu_init::try_current().map_or(0, |c| c.cpu_id);
    if x86::read_cr3() & PTE_ADDR_MASK == root {
        return Some(RootHolder::Cr3 { cpu: here });
    }
    let mut id = 0u32;
    while (id as usize) < per_cpu_init::cpu_count() {
        if let Some(r) = per_cpu_init::cpu(id) {
            let loaded = r.as_cr3.load(Ordering::Acquire);
            if loaded != 0 && loaded & PTE_ADDR_MASK == root {
                return Some(RootHolder::Cr3 { cpu: id });
            }
        }
        id += 1;
    }
    thread_init::tcb_naming_root(root).map(RootHolder::Tcb)
}

/// Free `space`'s page tables and frames. Asserts first that no CPU has
/// its root loaded and no TCB names it (invariant I128).
pub fn teardown(mut space: AddressSpace) -> TeardownStats {
    let root = space.root().as_u64();
    if let Some(h) = root_holder(root) {
        panic!("addr_space: teardown of root {root:#x} still loaded: {h:?}");
    }
    paging_init::with_pt(|_pt| {
        let mut pool = BuddyPool;
        // SAFETY: invariant I128, established here: `root_holder` found no
        // CPU with the root loaded and no TCB naming it, so no CPU walks
        // these tables once they are freed.
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
        if cpu.remote.as_cr3.load(Ordering::Relaxed) == want {
            return;
        }
        unsafe { x86::write_cr3(want) };
        cpu.remote.as_cr3.store(want, Ordering::Release);
    });
}

pub fn load_cr3_u64(want: u64) {
    per_cpu_init::with_current(|cpu| {
        if cpu.remote.as_cr3.load(Ordering::Relaxed) == want || want == 0 {
            return;
        }
        unsafe { x86::write_cr3(want) };
        cpu.remote.as_cr3.store(want, Ordering::Release);
    });
}

pub fn load_kernel_cr3() {
    load_cr3_u64(paging_init::kernel_cr3());
}

/// Full AS copy for fork. Caller must not be running on `src`'s CR3
/// teardown path; clone allocates a new PML4.
pub fn clone_full(src: &vibeos::addr_space::AddressSpace) -> Option<AddressSpace> {
    let kernel = paging_init::current_mapper();
    let mut pool = BuddyPool;
    unsafe { src.clone_anon(&kernel, &mut pool) }.ok()
}

pub fn cr3_was_skipped(space: &AddressSpace) -> bool {
    crate::per_cpu_init::current()
        .remote
        .as_cr3
        .load(Ordering::Relaxed)
        == space.root().as_u64()
}
