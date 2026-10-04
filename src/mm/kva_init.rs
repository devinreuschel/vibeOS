//! Kernel-side KVA: range allocator plus mapping. DESIGN §4.5 / §7.9.
//!
//! `vibeos::kva::Kva` hands out VA. This module maps order-0 frames into
//! the reserved ranges and leaves the guard page unmapped. A dead thread's
//! stack waits on its CPU's dead list, linked through the stacks themselves
//! ([`park_on_list`]), until that CPU's worker frees it ([`free_parked`]).
//! `vmap` hands out a move-only [`Vmap`] that holds its `Frames`, and
//! `vunmap` takes it back, so the span unmapped is the span freed.
//! The KVA free-list lives under the page-table lock. Unmap, drop PT,
//! shootdown, then free VA to the tail ([`release_va`]).

use vibeos::ipi::{SHOOT_RANGE_PAGES, SHOOT_RANGES, ShootRange};
use vibeos::kva::{
    DEFAULT_STACK_PAGES, KVA_END, KVA_SIZE, KVA_START, Kva, KvaError, KvaStats, PAGE_SIZE,
};
use vibeos::lock::RANK_PT;
use vibeos::paging::{MapError, MapMode, PageFlags, PhysAddr, VirtAddr, heap_flags, stack_flags};
use vibeos::pmm::Frames;
pub use vibeos::thread::GuardedStack;
use vibeos::thread::MAX_STACK_PAGES;

use crate::paging_init;
use crate::pmm_init;
use crate::sync_init::SpinMutex;

static KVA: SpinMutex<Kva> = SpinMutex::with_rank(Kva::empty(), RANK_PT);

/// Run `f` on the KVA free-list, which its callers reach under PT.
pub(super) fn with_kva<R>(f: impl FnOnce(&mut Kva) -> R) -> R {
    // pair order: paging_init::PT, then KVA
    let mut g = KVA.lock_nested(1);
    f(&mut g)
}

use vibeos::limits::MAX_UNMAP_PAGES as MAX_UNMAP;

// `free_stack` unmaps a whole stack in one `unmap_shootdown` batch.
const _: () = assert!(MAX_STACK_PAGES <= MAX_UNMAP);
// One `ShootRange` covers what `unmap_shootdown` unmaps, so it sends one
// round (`shoot_span`).
const _: () = assert!(MAX_UNMAP as u64 <= SHOOT_RANGE_PAGES);

/// Frames one [`free_parked`] batch holds between its unmaps and its
/// shootdown round: [`SHOOT_RANGES`] default stacks, and one stack of any
/// size.
const BATCH_PAGES: usize = SHOOT_RANGES * DEFAULT_STACK_PAGES;
const _: () = assert!(MAX_STACK_PAGES <= BATCH_PAGES);

/// Claim the 64 GiB window. Must run after paging + heap.
///
/// # Safety
/// Single-CPU, IRQs off, live page tables already ours.
#[allow(
    clippy::expect_used,
    reason = "invariant: a fresh pool has MAX_RANGES free slots, so Kva::init cannot fail (mm::kva::Kva::init)"
)]
pub unsafe fn init() {
    paging_init::assert_unmapped(VirtAddr(KVA_START), VirtAddr(KVA_END));
    paging_init::with_pt(|_pt| with_kva(|k| k.init(KVA_START, KVA_SIZE).expect("kva: init")));
}

pub fn stats() -> KvaStats {
    paging_init::with_pt(|_pt| with_kva(|k| k.stats()))
}

/// Give `[va, va + len)` back to the KVA free list. `Kva::free` always
/// finds a node: `Kva::alloc` caps the live ranges below the pool size.
pub(super) fn release_va(va: VirtAddr, len: u64) {
    paging_init::with_pt(|_pt| with_kva(|k| k.free(va.as_u64(), len)));
}

/// Reserve `2S` VA for a power-of-two stack of `pages` pages, map the
/// upper `S`, leave the lower `S` unmapped (DESIGN §4.5). The one
/// constructor of a [`GuardedStack`]; [`free_stack`] takes it back.
pub fn alloc_guarded_stack(pages: usize) -> Result<GuardedStack, KvaError> {
    if pages == 0 || pages > MAX_STACK_PAGES || !pages.is_power_of_two() {
        return Err(KvaError::Size);
    }
    let guard = paging_init::with_pt(|_pt| with_kva(|k| k.alloc_guarded(pages)))
        .map(VirtAddr)
        .ok_or(KvaError::NoVa)?;
    let base = VirtAddr(guard.as_u64() + pages as u64 * PAGE_SIZE);
    let mut frames: [Option<Frames>; MAX_STACK_PAGES] = [const { None }; MAX_STACK_PAGES];
    let mut mapped = 0usize;
    let r = paging_init::with_pt(|pt| {
        while mapped < pages {
            let va = VirtAddr(base.as_u64() + PAGE_SIZE * mapped as u64);
            let f = pmm_init::with_buddy(|b| b.alloc(0)).ok_or(KvaError::NoFrames)?;
            let pa = PhysAddr(f.base());
            frames[mapped] = Some(f);
            // SAFETY: `map_4k_locked`'s contract; `pa` is the fresh frame
            // above and `va` a page of the 2S range `alloc_guarded` just
            // reserved, which nothing else maps; `pt` holds the page-table
            // lock (invariant I48, established at
            // `mm::paging_init::current_mapper`).
            if unsafe { paging_init::map_4k_locked(pt, va, pa, stack_flags()) }.is_err() {
                return Err(KvaError::Map);
            }
            mapped += 1;
        }
        Ok(())
    });
    if let Err(e) = r {
        // Unmap and shoot down what was mapped, and only then free the
        // frames and the VA.
        unmap_shootdown(base, mapped);
        free_frames(&mut frames);
        release_va(guard, Kva::guarded_va_len(pages));
        return Err(e);
    }
    shoot_span(base, pages);
    #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
    fill_stack(base, pages);
    // SAFETY: `[guard, guard + 2S)` came from `Kva::alloc_guarded(pages)`
    // above, page `i` of the stack maps `frames[i]` (the loop above), the
    // other slots are `None`, the guard was never mapped, and this is the
    // only handle built for the range (the contract
    // `GuardedStack::from_raw_parts` states, established here).
    Ok(unsafe { GuardedStack::from_raw_parts(guard, pages, frames) })
}

/// Fill a fresh stack's `pages` mapped pages above `base` with
/// `stack_depth::PATTERN`, for the depth scan (DESIGN §4.5, TESTING §8.2).
#[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
fn fill_stack(base: VirtAddr, pages: usize) {
    let words = pages * (PAGE_SIZE as usize / 8);
    // SAFETY: `alloc_guarded_stack` mapped `[base, base + pages)` writable
    // from fresh frames and shot the span down, and no handle to it exists
    // yet, so nothing else reads or writes it; established here.
    let s = unsafe { core::slice::from_raw_parts_mut(base.as_u64() as *mut u64, words) };
    vibeos::sched::stack_depth::fill(s);
}

/// Fill `stack` with `stack_depth::PATTERN` again, before a thread reuses
/// it (`thread_init::spawn_inner`).
///
/// # Safety
/// No thread runs on `stack` and nothing else reads or writes its pages
/// until the call returns.
#[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
pub unsafe fn refill_stack(stack: &GuardedStack) {
    let words = stack.pages() * (PAGE_SIZE as usize / 8);
    // SAFETY: the handle's `pages` pages above `base` are mapped writable
    // (`mm::kva_init::alloc_guarded_stack`), and the caller keeps every
    // other accessor off them (this fn's contract); established here.
    let s = unsafe { core::slice::from_raw_parts_mut(stack.base().as_u64() as *mut u64, words) };
    vibeos::sched::stack_depth::fill(s);
}

/// Return each held frame to the buddy. Only after the pages that mapped
/// them are unmapped and shot down.
fn free_frames(frames: &mut [Option<Frames>]) {
    pmm_init::with_buddy(|b| {
        for slot in frames.iter_mut() {
            if let Some(f) = slot.take() {
                b.free(f);
            }
        }
    });
}

/// Unmap `stack`, shoot it down, then free its frames and its VA.
pub fn free_stack(stack: GuardedStack) {
    // SAFETY: `free_stack_shootdown`'s contract; the one constructor of a
    // `GuardedStack` is `alloc_guarded_stack`, so this KVA allocated it, and
    // the handle moves here only once no CPU runs on the stack (invariant
    // I10, established at `sched::thread_init::finish_switch`).
    unsafe { free_stack_shootdown(stack) };
}

/// A dead stack on a dead list: the link and the stack's own handle,
/// written into the stack's lowest mapped bytes.
#[repr(C)]
struct Parked {
    next: u64,
    stack: GuardedStack,
}

// A node fits in one page, and a stack maps at least one.
const _: () = assert!(core::mem::size_of::<Parked>() <= PAGE_SIZE as usize);
const _: () = assert!(core::mem::align_of::<Parked>() <= PAGE_SIZE as usize);

/// Push `stack` onto the dead list at `head`, linked through the stack
/// itself: the node is written into its lowest mapped page. Nothing but
/// the list holds it until [`free_parked`] takes it back.
///
/// # Safety
///
/// As [`park_slot_on_list`]'s, for `stack`.
#[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
pub(crate) unsafe fn park_on_list(head: &mut u64, stack: GuardedStack) {
    // SAFETY: invariant I10, established at `thread_init::finish_switch`
    // for every stack it parks, and by this fn's contract for `stack`;
    // `head` is a dead list, as the contract asks.
    unsafe { park_slot_on_list(head, &mut Some(stack)) };
}

/// [`park_on_list`] for the stack in `slot`, which it leaves empty; nothing
/// when `slot` is empty. The handle moves from the slot into the node with
/// one copy, so none of its 520 bytes passes through a frame: the switch
/// tail parks from `PerCpu.dead_stack` this way (ROADMAP §10.2).
///
/// # Safety
///
/// No CPU runs on the slot's stack, now or later (invariant I10): the
/// node overwrites its lowest page. `head` is 0 or a dead list these fns
/// built, whose stacks no CPU runs on.
pub(crate) unsafe fn park_slot_on_list(head: &mut u64, slot: &mut Option<GuardedStack>) {
    let Some(stack) = slot.as_ref() else {
        return;
    };
    let src: *const GuardedStack = stack;
    let node = stack.base().as_u64() as *mut Parked;
    // SAFETY: invariant I10, established at `thread_init::finish_switch`
    // (and for the test hooks in `thread_init::testing`, a stack no thread
    // was given): no CPU runs on the slot's stack, its lowest page is mapped
    // writable (`kva_init::alloc_guarded_stack`), page-aligned and one page
    // holds a node (the const assertions above), and the slot's handle alone
    // names the range, so nothing else reads or writes the node. The copy
    // moves the handle into the node bit for bit, and the write of `None`
    // over the slot forgets the slot's copy without dropping it, so one
    // handle names the range again; established here.
    unsafe {
        core::ptr::addr_of_mut!((*node).next).write(*head);
        core::ptr::copy_nonoverlapping(src, core::ptr::addr_of_mut!((*node).stack), 1);
        core::ptr::write(slot, None);
    }
    *head = node as u64;
}

/// Free every stack on the dead list `head`, which the caller took whole
/// off its owner (`thread_init::take_dead_stacks`). IF=1. Returns how many.
///
/// A batch at a time ([`free_batch`]): one shootdown round per
/// [`SHOOT_RANGES`] stacks, not one per page, so a worker frees stacks
/// faster than a burst of exits, which sends no IPI, parks them (DESIGN
/// §4.5).
///
/// # Safety
///
/// `head` is 0 or a dead list [`park_slot_on_list`] built, taken whole off
/// its owner so nothing else reads or writes it, whose stacks no CPU runs
/// on (invariant I10).
pub(crate) unsafe fn free_parked(mut head: u64) -> usize {
    let mut n = 0usize;
    while head != 0 {
        // SAFETY: invariant I10, established at `thread_init::finish_switch`
        // for every stack on a dead list: `head` is the rest of the list
        // this fn's contract names.
        let (rest, k) = unsafe { free_batch(head) };
        head = rest;
        n += k;
    }
    n
}

/// Free stacks from the dead list `head` on, at most [`SHOOT_RANGES`] of
/// them and [`BATCH_PAGES`] pages, and at least one: unmap each, then one
/// shootdown round for all of them, then their frames and their VA.
/// Returns the rest of the list and how many it freed.
///
/// # Safety
///
/// As [`free_parked`]'s; `head` is not 0.
unsafe fn free_batch(mut head: u64) -> (u64, usize) {
    let mut frames: [Option<Frames>; BATCH_PAGES] = [const { None }; BATCH_PAGES];
    let mut nf = 0usize;
    let mut ranges = [ShootRange::page(0); SHOOT_RANGES];
    let mut nr = 0usize;
    let mut vas = [(VirtAddr(0), 0usize); SHOOT_RANGES];
    let mut k = 0usize;
    while head != 0 && k < SHOOT_RANGES {
        // SAFETY: invariant I10, established at `thread_init::finish_switch`:
        // `head` is a node `park_on_list` wrote into a stack no CPU runs on,
        // and the list was taken whole, so nothing else reads or writes it;
        // this reads the handle's page count in place.
        let pages = unsafe { (*(head as *const Parked)).stack.pages() };
        if nf + pages > BATCH_PAGES {
            break;
        }
        // SAFETY: invariant I10, established at `thread_init::finish_switch`,
        // as above; this is the one move out of the node, and the link and
        // the handle leave it before the stack is unmapped.
        let Parked { next, stack } = unsafe { core::ptr::read(head as *const Parked) };
        head = next;
        // SAFETY: `GuardedStack::into_raw_parts`'s contract; the range is
        // unmapped just below and shot down by the round after this loop,
        // and its frames and VA are freed only after that round, established
        // here.
        let (guard, pages, parts) = unsafe { stack.into_raw_parts() };
        let base = VirtAddr(guard.as_u64() + pages as u64 * PAGE_SIZE);
        // SAFETY: `unmap_only_locked`'s contract; no CPU runs on the stack
        // (invariant I10, established at `thread_init::finish_switch`), and
        // the round below completes before its frames and VA are freed.
        paging_init::with_pt(|pt| unsafe { unmap_only_locked(pt, base, pages) });
        match ShootRange::new(base.as_u64(), pages as u64) {
            Some(r) => {
                ranges[nr] = r;
                nr += 1;
            }
            None => shoot_span(base, pages),
        }
        // A stack's frames sit in its first `pages` slots
        // (`GuardedStack::from_raw_parts`), and the check above left room.
        for f in parts.into_iter().flatten() {
            frames[nf] = Some(f);
            nf += 1;
        }
        vas[k] = (guard, pages);
        k += 1;
    }
    vibeos::paging::tlb_shootdown_ranges(&ranges[..nr]);
    free_frames(&mut frames[..nf]);
    for &(guard, pages) in &vas[..k] {
        release_va(guard, Kva::guarded_va_len(pages));
    }
    (head, k)
}

/// One `Frames` block mapped contiguously into KVA. Move-only: only
/// [`vmap`] builds one and [`vunmap`] takes it back, returning the
/// `Frames`. Dropping one leaks its span and its `Frames` (DESIGN §4.5).
#[must_use]
pub struct Vmap {
    base: VirtAddr,
    frames: Frames,
}

impl Vmap {
    /// First mapped VA.
    #[cfg_attr(
        any(
            not(feature = "kernel_tests"),
            all(target_arch = "aarch64", feature = "kernel_tests")
        ),
        expect(
            dead_code,
            reason = "ROADMAP §10.3: `kva_init::vmap` returns a move-only handle; only in-guest tests call it until a driver does"
        )
    )]
    pub fn base(&self) -> VirtAddr {
        self.base
    }

    /// Mapped span in bytes: the frame count times the page size.
    #[cfg_attr(
        any(
            not(feature = "kernel_tests"),
            all(target_arch = "aarch64", feature = "kernel_tests")
        ),
        expect(
            dead_code,
            reason = "ROADMAP §10.3: `kva_init::vmap` returns a move-only handle; only in-guest tests call it until a driver does"
        )
    )]
    pub fn len(&self) -> u64 {
        self.frames.count() as u64 * PAGE_SIZE
    }

    /// Always false: a `Vmap` maps at least one frame.
    #[expect(
        dead_code,
        reason = "ROADMAP §10.3: `kva_init::vmap` returns a move-only handle; only in-guest tests call it until a driver does; clippy's len_without_is_empty pairs it with len"
    )]
    pub fn is_empty(&self) -> bool {
        false
    }
}

const _: fn(Vmap) -> Frames = vunmap;
crate::cell::assert_not_impl!(Vmap: Clone);
crate::cell::assert_not_impl!(Vmap: Copy);

/// Map `frames` contiguously into KVA. A block of more than `MAX_UNMAP`
/// frames is refused with `KvaError::Size`, so [`vunmap`] always unmaps
/// the whole span. On any failure the frames go back to the buddy.
#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(
        dead_code,
        reason = "ROADMAP §10.3: `kva_init::vmap` returns a move-only handle; only in-guest tests call it until a driver does"
    )
)]
#[cfg_attr(
    all(target_arch = "aarch64", feature = "kernel_tests"),
    expect(dead_code, reason = "boot-CPU S7; unused on this path")
)]
pub fn vmap(frames: Frames) -> Result<Vmap, KvaError> {
    let n = frames.count();
    if n > MAX_UNMAP {
        pmm_init::with_buddy(|b| b.free(frames));
        return Err(KvaError::Size);
    }
    let len = n as u64 * PAGE_SIZE;
    let pa0 = frames.base();
    let mut mapped = 0usize;
    let r = paging_init::with_pt(|pt| {
        let va = with_kva(|k| k.alloc(len)).ok_or((None, KvaError::NoVa))?;
        while mapped < n {
            let off = mapped as u64 * PAGE_SIZE;
            let page = VirtAddr(va + off);
            // SAFETY: `map_4k_locked`'s contract; frame `mapped` of the
            // `frames` block this fn owns goes to a page of the range
            // `k.alloc` just reserved, which nothing else maps; `pt` holds
            // the page-table lock (invariant I48, established at
            // `mm::paging_init::current_mapper`).
            let one =
                unsafe { paging_init::map_4k_locked(pt, page, PhysAddr(pa0 + off), heap_flags()) };
            if let Err(e) = one {
                let e = match e {
                    MapError::OutOfFrames => KvaError::NoFrames,
                    _ => KvaError::Map,
                };
                return Err((Some(VirtAddr(va)), e));
            }
            mapped += 1;
        }
        Ok(VirtAddr(va))
    });
    match r {
        Ok(base) => {
            shoot_span(base, n);
            Ok(Vmap { base, frames })
        }
        Err((va, e)) => {
            // Unmap and shoot down what was mapped, and only then free the
            // VA and the frames.
            if let Some(va) = va {
                unmap_shootdown(va, mapped);
                release_va(va, len);
            }
            pmm_init::with_buddy(|b| b.free(frames));
            Err(e)
        }
    }
}

/// Unmap `v`'s span, shoot it down, free exactly that span of VA, and
/// return its `Frames`.
pub fn vunmap(v: Vmap) -> Frames {
    let Vmap { base, frames } = v;
    let n = frames.count();
    unmap_shootdown(base, n);
    release_va(base, n as u64 * PAGE_SIZE);
    frames
}

/// # Safety
/// `stack` was allocated by this KVA, and no CPU runs on it: invariant
/// I10, a dead thread's stack is freed only after its CPU has switched off
/// it (`thread_init::finish_switch`).
unsafe fn free_stack_shootdown(stack: GuardedStack) {
    // SAFETY: `unmap_shootdown` below unmaps the range and shoots it down
    // before the frames and the VA are freed (the contract
    // `GuardedStack::into_raw_parts` states, met here).
    let (guard, pages, mut frames) = unsafe { stack.into_raw_parts() };
    unmap_shootdown(VirtAddr(guard.as_u64() + pages as u64 * PAGE_SIZE), pages);
    free_frames(&mut frames);
    release_va(guard, Kva::guarded_va_len(pages));
}

/// Unmap `n` pages, drop PT, shootdown. Frees nothing: the caller owns
/// the frames and frees them after this returns. `n` is at most
/// `MAX_UNMAP`, a kernel invariant no input reaches: `vmap` rejects a
/// larger block and a stack is at most `MAX_STACK_PAGES` (the const
/// assertion above).
pub(super) fn unmap_shootdown(va: VirtAddr, n: usize) {
    assert!(
        n <= MAX_UNMAP,
        "kva: unmap_shootdown of {n} pages, over MAX_UNMAP"
    );
    // SAFETY: `unmap_only_locked`'s contract; the shootdown below runs
    // before this fn returns, and every caller frees the frames and the VA
    // only after that, established here.
    paging_init::with_pt(|pt| unsafe { unmap_only_locked(pt, va, n) });
    shoot_span(va, n);
}

/// Shoot the `n` pages from `va` down on every other CPU, in one round
/// when one [`ShootRange`] holds them: page-aligned KVA and at most
/// `MAX_UNMAP` pages, which every caller passes. Otherwise a round a page.
fn shoot_span(va: VirtAddr, n: usize) {
    if n == 0 {
        return;
    }
    if let Some(r) = ShootRange::new(va.as_u64(), n as u64) {
        vibeos::paging::tlb_shootdown_ranges(&[r]);
        return;
    }
    let mut i = 0;
    while i < n {
        vibeos::paging::tlb_shootdown_others(VirtAddr(va.as_u64() + i as u64 * PAGE_SIZE));
        i += 1;
    }
}

/// Map `[phys, phys+len)` into KVA with `flags`. `memunmap` frees it.
///
/// # Safety
/// `[phys, phys+len)` is memory the kernel may map at this type, and no
/// other mapping of it uses a conflicting type (invariant I17).
pub unsafe fn memremap(phys: PhysAddr, len: u64, flags: PageFlags) -> Option<VirtAddr> {
    if len == 0 {
        return None;
    }
    let page_off = phys.as_u64() & (PAGE_SIZE - 1);
    let span = page_off.checked_add(len)?;
    let pages = span.div_ceil(PAGE_SIZE) * PAGE_SIZE;
    let va = paging_init::with_pt(|_pt| with_kva(|k| k.alloc(pages)))?;
    let base = VirtAddr(va);
    let base_pa = PhysAddr(phys.as_u64() & !(PAGE_SIZE - 1));
    // SAFETY: `map_range_locked`'s contract; `va` is a fresh KVA range and
    // the caller vouches for `phys` (this fn's `# Safety`); established here.
    let r = paging_init::with_pt(|pt| unsafe {
        paging_init::map_range_locked(pt, base, base_pa, pages, flags, MapMode::Fresh)
    });
    if r.is_err() {
        paging_init::with_pt(|pt| {
            let mut off = 0u64;
            while off < pages {
                // SAFETY: these leaves, if any, are the ones `map_range`
                // just placed, unused; established here.
                let _ = unsafe { paging_init::unmap_4k_locked(pt, VirtAddr(va + off)) };
                off = off.saturating_add(PAGE_SIZE);
            }
        });
        let mut off = 0u64;
        while off < pages {
            vibeos::paging::tlb_shootdown_others(VirtAddr(va + off));
            off = off.saturating_add(PAGE_SIZE);
        }
        release_va(base, pages);
        return None;
    }
    let mut off = 0u64;
    while off < pages {
        vibeos::paging::tlb_shootdown_others(VirtAddr(va + off));
        off = off.saturating_add(PAGE_SIZE);
    }
    Some(VirtAddr(va + page_off))
}

/// Unmap a `memremap` range and return its VA to KVA.
///
/// # Safety
/// `va` and `len` are what `memremap` returned / was given, and nothing
/// uses the mapping any more.
pub unsafe fn memunmap(va: VirtAddr, len: u64) {
    if len == 0 {
        return;
    }
    let page_off = va.as_u64() & (PAGE_SIZE - 1);
    let start = va.as_u64() - page_off;
    let Some(span) = page_off.checked_add(len) else {
        return;
    };
    let pages = span.div_ceil(PAGE_SIZE) * PAGE_SIZE;
    paging_init::with_pt(|pt| {
        let mut off = 0u64;
        while off < pages {
            // SAFETY: this fn's `# Safety` contract, established here.
            let _ = unsafe { paging_init::unmap_4k_locked(pt, VirtAddr(start + off)) };
            off = off.saturating_add(PAGE_SIZE);
        }
    });
    let mut off = 0u64;
    while off < pages {
        vibeos::paging::tlb_shootdown_others(VirtAddr(start + off));
        off = off.saturating_add(PAGE_SIZE);
    }
    release_va(VirtAddr(start), pages);
}

/// Unmap `n` pages from `va` through `pt`. Local `invlpg` only.
///
/// # Safety
/// Caller will not use the pages until shootdown.
unsafe fn unmap_only_locked(pt: &mut paging_init::MapperGuard, va: VirtAddr, n: usize) {
    let mut i = 0;
    while i < n {
        let page = VirtAddr(va.as_u64() + i as u64 * PAGE_SIZE);
        // SAFETY: `unmap_4k_locked`'s contract, which this fn's `# Safety`
        // passes on, established here. A page that was never mapped (a
        // partial map's tail) returns `None`, which carries no failure: the
        // caller owns and frees the frames either way.
        let _ = unsafe { paging_init::unmap_4k_locked(pt, page) };
        i += 1;
    }
}
