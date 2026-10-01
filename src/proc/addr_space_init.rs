//! Kernel address space: the counted object (DESIGN §2.11), the buddy and
//! the PT lock. ROADMAP §9.2, §10.6.
//!
//! A space is a two-count object (ROADMAP §10.6, F019): [`Space`] is one
//! `users` reference, held by the process's thread and by each pin, whose
//! last put runs [`SpaceCore`]'s teardown (every user leaf and user page
//! table back to the buddy); [`CoreRef`] is one core reference, held by
//! each region and each `users` holder, whose last put frees the root,
//! after asserting that no CPU and no TCB still holds it (invariant I44).
//! Code reaches a running space only through a scoped guard
//! (`proc_init::with_current_space`), never a `&'static`.

use core::fmt;
use core::ops::Deref;
use core::sync::atomic::Ordering;

use vibeos::addr_space::{AsError, Backing, BrkPlan, FrameFree, MmapReq, Region, UserPerms};
use vibeos::arch::PageTable;
use vibeos::kalloc::{CoreArc, Teardown, UsersArc};
use vibeos::paging::{FrameAlloc, PAGE_SIZE_4K, PhysAddr, VirtAddr};
use vibeos::pmm::Frames;
use vibeos::proc::fill::FILL_BATCH;
use vibeos::thread::ThreadId;

use crate::arch::current::{AddressSpace, Arch};
use crate::paging_init;
use crate::per_cpu_init;
use crate::pmm_init;
use crate::sync::blocking_init::{BlockingMutex, BlockingMutexGuard};
use crate::thread_init;

struct BuddyPool;

// SAFETY: the buddy hands out order-0 frames of RAM, which the physmap maps
// writable at the HHDM offset, as `paging::FrameAlloc` requires; established
// by `paging_init::install`.
unsafe impl FrameAlloc for BuddyPool {
    fn alloc_frame(&mut self) -> Option<Frames> {
        pmm_init::with_buddy(|b| b.alloc(0))
    }
}

// SAFETY: every frame an address space holds came from the buddy through
// this pool's `alloc_frame`, and `free_frame` gives it back to the buddy,
// as `addr_space::FrameFree` requires; established here.
unsafe impl FrameFree for BuddyPool {
    fn free_frame(&mut self, f: Frames) {
        pmm_init::with_buddy(|b| b.free(f));
    }
}

/// The portable space as the kernel keeps it: each region holds a
/// [`CoreRef`].
pub type Mm = AddressSpace<CoreRef>;

/// A space's core: its root table and, under the `mm` lock, the portable
/// space over it. ROADMAP §12.1 and §12.3 add the page-table lock and the
/// CPU set here. Its drop frees the root and runs no teardown.
pub struct SpaceCore {
    /// The root table's frame; `None` only inside [`Drop::drop`].
    root: Option<Frames>,
    /// The regions and the page tables below the root. A sleeping lock:
    /// it ranks before `PT`, which each chunk of work under it takes.
    mm: BlockingMutex<Mm>,
}

/// One `users` reference to a space: the process's thread's, or a pin's.
pub type Space = UsersArc<SpaceCore>;

/// One core reference to a space: a region's, or a test's memory-only one.
pub type CoreRef = CoreArc<SpaceCore>;

/// A space no thread runs yet. Only [`create`] makes one and
/// [`NewSpace::publish`] consumes it, so syscall code, which reaches a
/// space only as `&Space` through a scoped guard, cannot hand a running
/// space to the code that fills a new one.
pub struct NewSpace(Space);

impl NewSpace {
    /// Hand the filled space to the process that will run it.
    pub fn publish(self) -> Space {
        self.0
    }
}

impl Deref for NewSpace {
    type Target = Space;
    fn deref(&self) -> &Space {
        &self.0
    }
}

impl SpaceCore {
    /// The root table's physical address.
    pub fn root(&self) -> PhysAddr {
        PhysAddr(self.root.as_ref().map_or(0, Frames::base))
    }

    /// The portable space, under its lock. Sleeps: never under a spinlock.
    pub fn mm(&self) -> BlockingMutexGuard<'_, Mm> {
        self.mm.lock()
    }
}

impl Teardown for SpaceCore {
    /// The last `users` put: every user leaf and every page-table page
    /// below the root back to the buddy, and the regions cleared, which
    /// drops their core references. The root stays for the core's free.
    fn teardown(&self) {
        let mut mm = self.mm.lock();
        paging_init::with_pt(|_pt| {
            let mut pool = BuddyPool;
            // SAFETY: invariant I44: the last `users` reference is gone, so
            // no thread runs this space and no pin walks it; its process's
            // CR3 was switched away before that put (`proc_init::finish_exit`,
            // `proc_init::sys_execve`), so no CPU walks these tables once they
            // are freed; established by `kalloc::UsersArc`'s drop.
            unsafe { mm.teardown_pool(&mut pool) };
        });
    }
}

impl Drop for SpaceCore {
    /// The core's free: assert that no CPU has the root loaded and no TCB
    /// names it (invariant I44), then free it to the buddy.
    fn drop(&mut self) {
        let Some(root) = self.root.take() else {
            return;
        };
        let pa = root.base();
        if let Some(h) = root_holder(pa) {
            #[allow(
                clippy::panic,
                reason = "invariant I44: a root is freed only after every CPU and TCB has let it go; a holder is a kernel bug"
            )]
            {
                // The root is not freed: a CPU may still walk it.
                core::mem::forget(root);
                panic!("addr_space: free of root {pa:#x} still loaded: {h:?}");
            }
        }
        pmm_init::with_buddy(|b| b.free(root));
    }
}

/// A new user address space: a root from the buddy, the kernel half
/// shared from the kernel's, and the counted object around it. `ENOMEM`
/// (`OutOfFrames`) when a frame or the heap runs out; nothing is left over.
pub fn create() -> Result<NewSpace, AsError> {
    // The region table before PT, which the heap ranks before; declared
    // first, so a table a failure leaves here drops after `kernel`.
    let mut regions = vibeos::addr_space::region_table().ok();
    if regions.is_none() {
        return Err(AsError::OutOfFrames);
    }
    let root = pmm_init::with_buddy(|b| b.alloc(0)).ok_or(AsError::OutOfFrames)?;
    let pa = PhysAddr(root.base());
    let mm = {
        let kernel = paging_init::current_mapper();
        // SAFETY: `kernel` is the live kernel mapper, and `pa` is the order-0
        // frame just taken from the buddy, writable through the HHDM, which
        // the core below owns for the space's life, as
        // `addr_space::AddressSpace::new` requires; established here.
        unsafe { AddressSpace::new(&kernel, pa, &mut regions) }
    };
    let Some(mm) = mm else {
        pmm_init::with_buddy(|b| b.free(root));
        return Err(AsError::OutOfFrames);
    };
    let core = SpaceCore {
        root: Some(root),
        mm: BlockingMutex::new(mm),
    };
    // A failed allocation drops `core`, whose drop frees the root.
    UsersArc::try_new(core)
        .map(NewSpace)
        .map_err(|_| AsError::OutOfFrames)
}

/// Pages one hold of `PT` maps or unmaps: one leaf table's worth. A chunk
/// never crosses a 2 MiB boundary either, so it touches one leaf table
/// (ROADMAP §10.6, F009).
pub const CHUNK_PAGES: u64 = 512;

/// Pages one hold of `PT` maps: the fill API's batch, whose zeroed frames
/// are prepared before the hold, in a 536-byte array on the stack.
pub const MAP_CHUNK_PAGES: u64 = FILL_BATCH as u64;

/// End of the chunk that starts at `cur`: at most [`CHUNK_PAGES`] pages,
/// up to the next 2 MiB boundary or `end`, whichever comes first.
fn chunk_end(cur: u64, end: u64) -> u64 {
    chunk_end_of(cur, end, CHUNK_PAGES)
}

/// [`chunk_end`] for chunks of at most `pages` pages.
fn chunk_end_of(cur: u64, end: u64, pages: u64) -> u64 {
    let span = CHUNK_PAGES * PAGE_SIZE_4K;
    let leaf = (cur & !(span - 1)).saturating_add(span);
    leaf.min(cur.saturating_add(pages * PAGE_SIZE_4K)).min(end)
}

/// Frames a map chunk may take: one per page, and the three tables (PDPT,
/// PD, PT) a chunk inside one leaf table can need below the root.
const ZEROED_CAP: usize = FILL_BATCH + 3;

/// Zeroed frames for one map chunk, taken from the buddy and zeroed with
/// `PT` not held, so the chunk's hold only links them in (ROADMAP §10.6,
/// DESIGN §2.9 rule 2). A frame is zeroed before any mapping names it, so
/// no running space sees the write. Leftovers go back on drop.
struct Zeroed {
    pa: [u64; ZEROED_CAP],
    n: usize,
    next: usize,
}

impl Zeroed {
    /// Up to `want` zeroed frames; fewer when the buddy runs out, and the
    /// map then fails as it would have.
    fn take(want: usize) -> Self {
        let mut z = Zeroed {
            pa: [0; ZEROED_CAP],
            n: 0,
            next: 0,
        };
        while z.n < want.min(ZEROED_CAP) {
            let Some(f) = pmm_init::with_buddy(|b| b.alloc(0)) else {
                break;
            };
            let pa = f.into_entry();
            zero_frame(pa);
            z.pa[z.n] = pa;
            z.n += 1;
        }
        z
    }
}

/// Zero the order-0 frame at `pa` through the physmap. Not a write to any
/// address space: no mapping names the frame yet.
fn zero_frame(pa: u64) {
    let p = paging_init::HHDM_BASE.wrapping_add(pa) as *mut u8;
    // SAFETY: `pa` is a buddy frame this caller just allocated and nothing
    // else names, and the physmap maps every buddy frame writable at
    // `HHDM_BASE`; established by `paging_init::install`.
    unsafe { core::ptr::write_bytes(p, 0, PAGE_SIZE_4K as usize) };
}

// SAFETY: every frame `Zeroed` hands out is an order-0 buddy frame of RAM,
// writable through the physmap (`addr_space_init::Zeroed::take`, or the
// buddy for a fallback), as `paging::FrameAlloc` requires.
unsafe impl FrameAlloc for Zeroed {
    fn alloc_frame(&mut self) -> Option<Frames> {
        if self.next < self.n {
            let pa = self.pa[self.next];
            self.next += 1;
            // SAFETY: `pa` came from `into_entry` of an order-0 token in
            // `Zeroed::take`, and only this slot held it, the contract
            // `pmm::Frames::from_entry` states; established by
            // `addr_space_init::Zeroed::take`.
            return Some(unsafe { Frames::from_entry(pa, 0) });
        }
        // More than the chunk could need: zero it here, under the hold.
        let f = pmm_init::with_buddy(|b| b.alloc(0))?;
        zero_frame(f.base());
        Some(f)
    }
}

// SAFETY: `free_frame` gives a frame back to the buddy, which handed out
// every frame `Zeroed` holds, as `addr_space::FrameFree` requires;
// established here.
unsafe impl FrameFree for Zeroed {
    fn free_frame(&mut self, f: Frames) {
        pmm_init::with_buddy(|b| b.free(f));
    }
}

impl Drop for Zeroed {
    fn drop(&mut self) {
        while self.next < self.n {
            let pa = self.pa[self.next];
            self.next += 1;
            // SAFETY: as in `alloc_frame`: an `into_entry` token only this
            // slot held; established by `addr_space_init::Zeroed::take`.
            pmm_init::with_buddy(|b| b.free(unsafe { Frames::from_entry(pa, 0) }));
        }
    }
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

/// Map `[va, va+len)` of `space` with zeroed frames and record it as one
/// region, under the space's `mm` lock. Takes `PT` once per chunk
/// ([`MAP_CHUNK_PAGES`]), after zeroing that chunk's frames with `PT` not
/// held. On failure
/// every page it mapped is unmapped and freed, also chunk by chunk, and no
/// region is recorded.
///
/// # Safety
/// Same contract as `AddressSpace::map_anon`.
pub unsafe fn map_anon(space: &Space, va: u64, len: u64, perms: UserPerms) -> Result<(), AsError> {
    let core = space.core();
    let mut mm = space.mm();
    // SAFETY: this fn's contract, `addr_space_init::map_anon`.
    unsafe { map_anon_mm(&mut mm, core, va, len, perms) }
}

/// [`map_anon`] on a space whose `mm` lock the caller holds; the region
/// holds `core`.
///
/// # Safety
/// Same contract as `AddressSpace::map_anon`.
pub(crate) unsafe fn map_anon_mm(
    space: &mut Mm,
    core: CoreRef,
    va: u64,
    len: u64,
    perms: UserPerms,
) -> Result<(), AsError> {
    space.check_new_region(va, len)?;
    // SAFETY: `check_new_region` found the range free of regions, so no
    // page in it is mapped (invariant of
    // `vibeos::addr_space::AddressSpace::insert_region`); this fn's
    // contract, `addr_space_init::map_anon`.
    unsafe { map_chunks(space, va, len, perms)? };
    let region = Region {
        start: va,
        len,
        perms,
        backing: Backing::Anonymous,
        core,
    };
    if let Err(e) = space.insert_region(region) {
        // SAFETY: `map_chunks` just mapped the whole range from the buddy,
        // here.
        unsafe { unmap_chunks(space, va, len)? };
        return Err(e);
    }
    Ok(())
}

/// Map `[va, va+len)` with zeroed frames, chunk by chunk ([`MAP_CHUNK_PAGES`]
/// within one leaf table); roll back on failure.
///
/// # Safety
/// No page of `[va, va+len)` is mapped in `space`.
unsafe fn map_chunks(space: &mut Mm, va: u64, len: u64, perms: UserPerms) -> Result<(), AsError> {
    let end = va.checked_add(len).ok_or(AsError::Overflow)?;
    let mut cur = va;
    while cur < end {
        let ce = chunk_end_of(cur, end, MAP_CHUNK_PAGES);
        let n = ce - cur;
        let pages = n / PAGE_SIZE_4K;
        // The chunk's frames, zeroed before the hold; unused ones go back
        // when `pool` drops, after it.
        let mut pool = Zeroed::take(pages as usize + 3);
        let rc = with_pt_chunk(pages, || {
            // SAFETY: `[cur, ce)` is inside the unmapped range this fn's
            // contract names (`addr_space_init::map_chunks`), and `pool`
            // hands out owned, zeroed buddy frames
            // (`addr_space_init::Zeroed`).
            unsafe { space.map_pages(cur, n, perms, &mut pool) }
        });
        drop(pool);
        if let Err(e) = rc {
            // `map_pages` rolled its own chunk back.
            // SAFETY: this call mapped `[va, cur)` from the buddy, here.
            unsafe { unmap_chunks(space, va, cur - va)? };
            return Err(e);
        }
        cur = ce;
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
unsafe fn unmap_chunks(space: &mut Mm, va: u64, len: u64) -> Result<(), AsError> {
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
fn local_flush(space: &Mm) -> impl FnMut(u64) + use<> {
    let loaded = Arch::root() == space.root();
    move |va| {
        if loaded {
            Arch::flush_local(VirtAddr(va));
        }
    }
}

/// `munmap` of `[va, va+len)` over `AddressSpace::unmap_free`, taking `PT`
/// once per chunk ([`CHUNK_PAGES`]). Each page leaves this CPU's TLB before
/// its frame is freed when `space` is the loaded CR3. Only the first chunk
/// can split a region, so a full region table fails it before anything is
/// unmapped.
///
/// # Safety
/// Same contract as `AddressSpace::unmap_free`, whose flush this fn
/// supplies.
pub unsafe fn unmap(space: &Space, va: u64, len: u64) -> Result<(), AsError> {
    let mut mm = space.mm();
    // SAFETY: this fn's contract, `addr_space_init::unmap`.
    unsafe { unmap_mm(&mut mm, va, len) }
}

/// [`unmap`] on a space whose `mm` lock the caller holds.
///
/// # Safety
/// Same contract as [`unmap`].
unsafe fn unmap_mm(space: &mut Mm, va: u64, len: u64) -> Result<(), AsError> {
    let end = va.checked_add(len).ok_or(AsError::Overflow)?;
    if len == 0 {
        // SAFETY: an empty range touches no leaf, which
        // `vibeos::addr_space::AddressSpace::unmap_free` checks first.
        return paging_init::with_pt(|_pt| unsafe {
            space.unmap_free(va, 0, &mut BuddyPool, &mut |_| {})
        });
    }
    let mut flush = local_flush(space);
    let mut cur = va;
    while cur < end {
        let ce = chunk_end(cur, end);
        with_pt_chunk((ce - cur) / PAGE_SIZE_4K, || {
            let mut pool = BuddyPool;
            // SAFETY: this fn's contract; `flush` invalidates the page on
            // this CPU, the only one that runs the space's thread
            // (`thread_init::spawn_user` pins it).
            unsafe { space.unmap_free(cur, ce - cur, &mut pool, &mut flush) }
        })?;
        cur = ce;
    }
    Ok(())
}

/// `brk(want)` on `space`: returns the new break, or the current one when
/// `want` is 0, below the heap's start, past `USER_MAP_END`, into another
/// region, or more than the free frames cover. Growth maps zeroed pages in
/// chunks like [`map_anon`]; shrinking unmaps the whole pages above the new
/// break.
pub fn brk(space: &Space, want: u64) -> u64 {
    let core = space.core();
    let mut mm = space.mm();
    let space = &mut *mm;
    let moved = match space.brk_plan(want) {
        BrkPlan::Current => return space.brk(),
        BrkPlan::SamePage => Ok(()),
        BrkPlan::Grow { va, len } => brk_grow(space, core, va, len, want),
        // SAFETY: the heap's leaves were mapped from the buddy by
        // `brk_grow`, here, and `unmap_mm` supplies the flush.
        BrkPlan::Shrink { va, len } => unsafe { unmap_mm(space, va, len) },
    };
    // A failed move leaves the break where it was, as brk(2) returns it.
    if moved.is_ok() {
        space.set_brk(want);
    }
    space.brk()
}

fn brk_grow(space: &mut Mm, core: CoreRef, va: u64, len: u64, want: u64) -> Result<(), AsError> {
    space.heap_grow_check(va, len)?;
    // SAFETY: `heap_grow_check` found `[va, va+len)` in the user half and
    // clear of every region, so no page in it is mapped (invariant of
    // `vibeos::addr_space::AddressSpace::insert_region`).
    unsafe { map_chunks(space, va, len, UserPerms::RW)? };
    if let Err(e) = space.heap_grow_commit(va, len, want, core) {
        // SAFETY: `map_chunks` just mapped the range from the buddy, here.
        unsafe { unmap_chunks(space, va, len)? };
        return Err(e);
    }
    Ok(())
}

/// Anonymous `mmap` of `req` in `space`: places it (`AddressSpace::
/// mmap_place`), then maps zeroed pages as [`map_anon`] does, or for
/// `PROT_NONE` records a reservation with no frames. Returns the address.
pub fn mmap(space: &Space, req: &MmapReq) -> Result<u64, AsError> {
    let core = space.core();
    let mut mm = space.mm();
    let space = &mut *mm;
    let va = space.mmap_place(req)?;
    match req.perms {
        None => {
            space.check_new_region(va, req.len)?;
            space.insert_region(Region {
                start: va,
                len: req.len,
                perms: UserPerms::READ,
                backing: Backing::Reserved,
                core,
            })?;
        }
        // SAFETY: `map_anon_mm` checks the range is in the user half and
        // clear of every region, and maps from the buddy, which hands out
        // owned frames (`addr_space_init::BuddyPool`).
        Some(perms) => unsafe { map_anon_mm(space, core, va, req.len, perms)? },
    }
    Ok(va)
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

/// What still holds a root that a core's free was asked to free.
enum RootHolder {
    /// CPU `cpu` has it loaded (CR3; TTBR0 on aarch64), or last recorded
    /// loading it.
    Loaded { cpu: u32 },
    /// That thread's `Tcb.as_cr3` names it.
    Tcb(ThreadId),
}

impl fmt::Debug for RootHolder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RootHolder::Loaded { cpu } => write!(f, "Loaded {{ cpu: {cpu} }}"),
            RootHolder::Tcb(id) => write!(f, "Tcb({})", id.0),
        }
    }
}

/// Who still holds `root`: this CPU's live CR3, any CPU's recorded root,
/// or any TCB's saved root. Returns with no lock held, so the caller's
/// assertion never fires under PT or SCHED.
fn root_holder(root: u64) -> Option<RootHolder> {
    // One IF=0 stretch: the id and CR3 name one CPU.
    let (here, live) = {
        let _irq = crate::arch::current::InterruptGuard::enter();
        (
            per_cpu_init::try_current().map_or(0, |c| c.cpu_id),
            Arch::root().as_u64(),
        )
    };
    if live == root {
        return Some(RootHolder::Loaded { cpu: here });
    }
    let mut id = 0u32;
    while (id as usize) < per_cpu_init::cpu_count() {
        if let Some(r) = per_cpu_init::cpu(id) {
            let loaded = r.as_cr3.load(Ordering::Acquire);
            // A recorded root is a table address, as `load_cr3_u64` stores it.
            if loaded != 0 && loaded == root {
                return Some(RootHolder::Loaded { cpu: id });
            }
        }
        id += 1;
    }
    thread_init::tcb_naming_root(root).map(RootHolder::Tcb)
}

/// Load `want` into CR3 unless this CPU already has it (or it is 0), and
/// record it in this CPU's `PerCpuRemote.as_cr3`.
///
/// # Safety
/// `want` is 0, or the physical address of a PML4 whose kernel half is the
/// kernel's: one that `paging_init::install` or [`create`] built. That PML4 stays allocated while it is loaded. Unless `want` is
/// the kernel root, the caller has recorded it in the current thread's
/// `Tcb.as_cr3` before the call (`execve` does, through
/// `thread_init::set_pid_cr3`), so the core's free sees it (invariant
/// I44).
pub unsafe fn load_cr3_u64(want: u64) {
    per_cpu_init::with_current(|cpu| {
        if cpu.remote.as_cr3.load(Ordering::Relaxed) == want || want == 0 {
            return;
        }
        // SAFETY: invariant I44: `want` is a PML4 that shares the kernel
        // half this code and stack run in and stays allocated while loaded
        // (this fn's contract, `addr_space_init::load_cr3_u64`).
        unsafe { Arch::set_root(PhysAddr(want)) };
        cpu.remote.as_cr3.store(want, Ordering::Release);
    });
}

pub fn load_kernel_cr3() {
    // SAFETY: the kernel root comes from `paging_init::kernel_cr3`, which
    // `paging_init::install` published and nothing frees; it is the kernel
    // root, so no TCB need name it.
    unsafe { load_cr3_u64(paging_init::kernel_cr3()) };
}
