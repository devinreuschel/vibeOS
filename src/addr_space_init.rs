#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
//! Kernel AddressSpace: buddy + PT lock. ROADMAP §9.2.

use core::fmt;
use core::sync::atomic::Ordering;

use vibeos::addr_space::{
    AddressSpace, AsError, Backing, FrameFree, Region, TeardownStats, UserPerms,
};
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

/// Pages one hold of `PT` maps or unmaps: one leaf table's worth. A chunk
/// never crosses a 2 MiB boundary either, so it touches one leaf table
/// (ROADMAP §10.6, F009).
pub const CHUNK_PAGES: u64 = 512;

/// End of the chunk that starts at `cur`: at most [`CHUNK_PAGES`] pages,
/// up to the next 2 MiB boundary or `end`, whichever comes first.
fn chunk_end(cur: u64, end: u64) -> u64 {
    let span = CHUNK_PAGES * PAGE_SIZE_4K;
    (cur & !(span - 1)).saturating_add(span).min(end)
}

/// Run `f` with `PT` held, for a chunk of `pages` pages.
fn with_pt_chunk<R>(pages: u64, f: impl FnOnce() -> R) -> R {
    paging_init::with_pt(|_pt| {
        #[cfg(feature = "kernel_tests")]
        testing::note_hold(pages);
        #[cfg(not(feature = "kernel_tests"))]
        let _ = pages;
        f()
    })
}

/// Map `[va, va+len)` with zeroed frames and record it as one region. Takes
/// `PT` once per chunk ([`CHUNK_PAGES`]) and zeroes each chunk with `PT`
/// dropped. On failure every page it mapped is unmapped and freed, also
/// chunk by chunk, and no region is recorded.
///
/// # Safety
/// Same contract as `AddressSpace::map_anon`.
pub unsafe fn map_anon(
    space: &mut AddressSpace,
    va: u64,
    len: u64,
    perms: UserPerms,
) -> Result<(), AsError> {
    space.check_new_region(va, len)?;
    // SAFETY: `check_new_region` found the range free of regions, so no
    // page in it is mapped (invariant of
    // `addr_space::AddressSpace::insert_region`); this fn's contract.
    unsafe { map_chunks(space, va, len, perms)? };
    let region = Region {
        start: va,
        len,
        perms,
        backing: Backing::Anonymous,
    };
    if let Err(e) = space.insert_region(region) {
        // SAFETY: `map_chunks` just mapped the whole range from the buddy,
        // here.
        unsafe { unmap_chunks(space, va, len)? };
        return Err(e);
    }
    Ok(())
}

/// Map and zero `[va, va+len)` chunk by chunk; roll back on failure.
///
/// # Safety
/// No page of `[va, va+len)` is mapped in `space`.
unsafe fn map_chunks(
    space: &mut AddressSpace,
    va: u64,
    len: u64,
    perms: UserPerms,
) -> Result<(), AsError> {
    let end = va.checked_add(len).ok_or(AsError::Overflow)?;
    let mut cur = va;
    while cur < end {
        let ce = chunk_end(cur, end);
        let n = ce - cur;
        let rc = with_pt_chunk(n / PAGE_SIZE_4K, || {
            let mut pool = BuddyPool;
            // SAFETY: `[cur, ce)` is inside the unmapped range this fn's
            // contract names, and the buddy hands out owned frames.
            unsafe { space.map_pages(cur, n, perms, &mut pool) }
        });
        // `map_pages` rolled its own chunk back; a failed zero leaves this
        // chunk mapped, so it is rolled back with the rest.
        let done = match rc {
            Ok(()) => match space.zero_bytes(cur, n) {
                Ok(()) => {
                    cur = ce;
                    continue;
                }
                Err(_) => (ce, AsError::NotMapped),
            },
            Err(e) => (cur, e),
        };
        // SAFETY: this call mapped `[va, done.0)` from the buddy, here.
        unsafe { unmap_chunks(space, va, done.0 - va)? };
        return Err(done.1);
    }
    Ok(())
}

/// Unmap and free every leaf in `[va, va+len)`, chunk by chunk, flushing
/// each page from this CPU's TLB before its frame is freed when `space` is
/// the loaded CR3.
///
/// # Safety
/// Same contract as `AddressSpace::unmap_pages`, whose flush this fn
/// supplies.
unsafe fn unmap_chunks(space: &mut AddressSpace, va: u64, len: u64) -> Result<(), AsError> {
    let end = va.checked_add(len).ok_or(AsError::Overflow)?;
    let mut flush = local_flush(space);
    let mut cur = va;
    while cur < end {
        let ce = chunk_end(cur, end);
        with_pt_chunk((ce - cur) / PAGE_SIZE_4K, || {
            let mut pool = BuddyPool;
            // SAFETY: this fn's contract; `flush` invalidates the page on
            // this CPU, the only one that runs the space's thread
            // (`thread_init::spawn_user` pins it).
            unsafe { space.unmap_pages(cur, ce - cur, &mut pool, &mut flush) }
        })?;
        cur = ce;
    }
    Ok(())
}

/// A flush for `space`'s pages: `invlpg` when it is this CPU's CR3,
/// nothing otherwise.
fn local_flush(space: &AddressSpace) -> impl FnMut(u64) + use<> {
    let loaded = x86::read_cr3() & PTE_ADDR_MASK == space.root().as_u64();
    move |va| {
        if loaded {
            x86::invlpg(va);
        }
    }
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

#[cfg(feature = "kernel_tests")]
pub(crate) mod testing {
    use core::sync::atomic::{AtomicU64, Ordering};

    static HOLDS: AtomicU64 = AtomicU64::new(0);
    static MAX_PAGES: AtomicU64 = AtomicU64::new(0);

    pub(super) fn note_hold(pages: u64) {
        HOLDS.fetch_add(1, Ordering::Relaxed);
        MAX_PAGES.fetch_max(pages, Ordering::Relaxed);
    }

    /// Zero the chunk counters.
    pub(crate) fn reset_chunks() {
        HOLDS.store(0, Ordering::Relaxed);
        MAX_PAGES.store(0, Ordering::Relaxed);
    }

    /// `(holds, max_pages_per_hold)`: how many times the chunked map and
    /// unmap paths took `PT` since [`reset_chunks`], and the most pages one
    /// hold covered.
    pub(crate) fn chunk_stats() -> (u64, u64) {
        (
            HOLDS.load(Ordering::Relaxed),
            MAX_PAGES.load(Ordering::Relaxed),
        )
    }
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

/// Load `want` into CR3 unless this CPU already has it (or it is 0), and
/// record it in this CPU's `PerCpuRemote.as_cr3`.
///
/// # Safety
/// `want` is 0, or the physical address of a PML4 whose kernel half is the
/// kernel's: one that `paging_init::install`, [`create`] or [`clone_full`]
/// built. That PML4 stays allocated while it is loaded. Unless `want` is
/// the kernel root, the caller has recorded it in the current thread's
/// `Tcb.as_cr3` before the call (`execve` does, through
/// `thread_init::set_pid_cr3`), so [`teardown`] sees it (invariant I128).
pub unsafe fn load_cr3_u64(want: u64) {
    per_cpu_init::with_current(|cpu| {
        if cpu.remote.as_cr3.load(Ordering::Relaxed) == want || want == 0 {
            return;
        }
        unsafe { x86::write_cr3(want) };
        cpu.remote.as_cr3.store(want, Ordering::Release);
    });
}

pub fn load_kernel_cr3() {
    // SAFETY: the kernel root comes from `paging_init::kernel_cr3`, which
    // `paging_init::install` published and nothing frees; it is the kernel
    // root, so no TCB need name it.
    unsafe { load_cr3_u64(paging_init::kernel_cr3()) };
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
