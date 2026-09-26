//! Kernel-side KVA: range allocator plus mapping. DESIGN §4.5 / §7.9.
//!
//! `vibeos::kva::Kva` hands out VA. This module maps order-0 frames into
//! the reserved ranges and leaves the guard page unmapped. A dead thread's
//! stack waits on its CPU's dead list, linked through the stacks themselves
//! ([`park_on_list`]), until that CPU's worker frees it ([`free_parked`]).
//! `vmap` hands out a move-only [`Vmap`] that holds its `Frames`, and
//! `vunmap` takes it back, so the span unmapped is the span freed.
//! The KVA free-list lives under the page-table lock. Unmap, drop PT,
//! shootdown, then free VA to the tail.
#![allow(dead_code)]

use vibeos::kva::{KVA_END, KVA_SIZE, KVA_START, Kva, KvaError, KvaStats, PAGE_SIZE};
use vibeos::paging::{MapError, PhysAddr, VirtAddr, heap_flags, stack_flags};
use vibeos::pmm::Frames;
pub use vibeos::thread::GuardedStack;
use vibeos::thread::MAX_STACK_PAGES;

use crate::cell::IrqCell;
use crate::paging_init;
use crate::pmm_init;

static KVA: IrqCell<Kva> = IrqCell::new(Kva::empty());

use vibeos::limits::MAX_UNMAP_PAGES as MAX_UNMAP;

// `free_stack` unmaps a whole stack in one `unmap_shootdown` batch.
const _: () = assert!(MAX_STACK_PAGES <= MAX_UNMAP);

/// Claim the 64 GiB window. Must run after paging + heap.
///
/// # Safety
/// Single-CPU, IRQs off, live page tables already ours.
pub unsafe fn init() {
    paging_init::assert_unmapped(VirtAddr(KVA_START), VirtAddr(KVA_END));
    paging_init::with_pt(|_pt| KVA.with(|k| k.init(KVA_START, KVA_SIZE).expect("kva: init")));
}

pub fn stats() -> KvaStats {
    paging_init::with_pt(|_pt| KVA.with(|k| k.stats()))
}

pub fn alloc_va(len: u64) -> Option<VirtAddr> {
    paging_init::with_pt(|_pt| KVA.with(|k| k.alloc(len)).map(VirtAddr))
}

pub fn free_va(va: VirtAddr, len: u64) {
    paging_init::with_pt(|_pt| KVA.with(|k| k.free(va.as_u64(), len).expect("kva: free-list")));
}

/// Reserve `pages+1` VA, map the upper `pages` from separate order-0
/// frames, leave the bottom page unmapped. The one constructor of a
/// [`GuardedStack`]; [`free_stack`] takes it back.
pub fn alloc_guarded_stack(pages: usize) -> Result<GuardedStack, KvaError> {
    if pages == 0 || pages > MAX_STACK_PAGES {
        return Err(KvaError::Size);
    }
    let guard = paging_init::with_pt(|_pt| KVA.with(|k| k.alloc_guarded(pages)))
        .map(VirtAddr)
        .ok_or(KvaError::NoVa)?;
    let base = VirtAddr(guard.as_u64() + PAGE_SIZE);
    let mut frames: [Option<Frames>; MAX_STACK_PAGES] = [const { None }; MAX_STACK_PAGES];
    let mut mapped = 0usize;
    let r = paging_init::with_pt(|pt| {
        while mapped < pages {
            let va = VirtAddr(base.as_u64() + PAGE_SIZE * mapped as u64);
            let f = pmm_init::with_buddy(|b| b.alloc(0)).ok_or(KvaError::NoFrames)?;
            let pa = PhysAddr(f.base());
            frames[mapped] = Some(f);
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
        free_va(guard, (pages as u64 + 1) * PAGE_SIZE);
        return Err(e);
    }
    let mut i = 0;
    while i < pages {
        vibeos::paging::tlb_shootdown_others(VirtAddr(base.as_u64() + PAGE_SIZE * i as u64));
        i += 1;
    }
    // SAFETY: `[guard, guard + (pages + 1) pages)` came from
    // `Kva::alloc_guarded(pages)` above, upper page `i` maps `frames[i]`
    // (the loop above), the other slots are `None`, the guard page was
    // never mapped, and this is the only handle built for the range (the
    // contract `GuardedStack::from_raw_parts` states, established here).
    Ok(unsafe { GuardedStack::from_raw_parts(guard, pages, frames) })
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
/// itself: the node is written into its lowest mapped page. No thread runs
/// on `stack` any more, and nothing but the list holds it until
/// [`free_parked`] takes it back.
pub(crate) fn park_on_list(head: &mut u64, stack: GuardedStack) {
    let node = stack.base().as_u64();
    // SAFETY: invariant I10, established at `thread_init::finish_switch`
    // (and for the test hook `thread_init::testing::park_on_local_list`, a
    // stack no thread was given): no CPU runs on `stack`, its lowest page is
    // mapped writable (`kva_init::alloc_guarded_stack`), page-aligned and
    // one page holds a node (the const assertions above), and this handle
    // alone names the range, so nothing else reads or writes the node.
    unsafe { core::ptr::write(node as *mut Parked, Parked { next: *head, stack }) };
    *head = node;
}

/// Free every stack on the dead list `head`, which the caller took whole
/// off its owner (`thread_init::take_dead_stacks`). IF=1. Returns how many.
pub(crate) fn free_parked(mut head: u64) -> usize {
    let mut n = 0usize;
    while head != 0 {
        // SAFETY: invariant I10, established at `thread_init::finish_switch`:
        // `head` is a node `park_on_list` wrote into a stack no CPU runs on,
        // and the list was taken whole, so this is the one read of it; the
        // link and the handle are moved out before the stack is freed.
        let Parked { next, stack } = unsafe { core::ptr::read(head as *const Parked) };
        free_stack(stack);
        head = next;
        n += 1;
    }
    n
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
    pub fn base(&self) -> VirtAddr {
        self.base
    }

    /// Mapped span in bytes: the frame count times the page size.
    pub fn len(&self) -> u64 {
        self.frames.count() as u64 * PAGE_SIZE
    }

    /// Always false: a `Vmap` maps at least one frame.
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
        let va = KVA.with(|k| k.alloc(len)).ok_or((None, KvaError::NoVa))?;
        while mapped < n {
            let off = mapped as u64 * PAGE_SIZE;
            let page = VirtAddr(va + off);
            if let Err(e) =
                unsafe { paging_init::map_4k_locked(pt, page, PhysAddr(pa0 + off), heap_flags()) }
            {
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
            let mut i = 0;
            while i < n {
                vibeos::paging::tlb_shootdown_others(VirtAddr(
                    base.as_u64() + i as u64 * PAGE_SIZE,
                ));
                i += 1;
            }
            Ok(Vmap { base, frames })
        }
        Err((va, e)) => {
            // Unmap and shoot down what was mapped, and only then free the
            // VA and the frames.
            if let Some(va) = va {
                unmap_shootdown(va, mapped);
                free_va(va, len);
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
    paging_init::with_pt(|_pt| {
        KVA.with(|k| {
            k.free(base.as_u64(), n as u64 * PAGE_SIZE)
                .expect("kva: free-list")
        })
    });
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
    unmap_shootdown(VirtAddr(guard.as_u64() + PAGE_SIZE), pages);
    free_frames(&mut frames);
    free_va(guard, (pages as u64 + 1) * PAGE_SIZE);
}

/// Unmap `n` pages, drop PT, shootdown. Frees nothing: the caller owns
/// the frames and frees them after this returns.
fn unmap_shootdown(va: VirtAddr, n: usize) {
    let n = n.min(MAX_UNMAP);
    paging_init::with_pt(|pt| unsafe { unmap_only_locked(pt, va, n) });
    let mut i = 0;
    while i < n {
        vibeos::paging::tlb_shootdown_others(VirtAddr(va.as_u64() + i as u64 * PAGE_SIZE));
        i += 1;
    }
}

/// Unmap `n` pages from `va` through `pt`. Local `invlpg` only.
///
/// # Safety
/// Caller will not use the pages until shootdown.
unsafe fn unmap_only_locked(pt: &mut paging_init::MapperGuard, va: VirtAddr, n: usize) {
    let mut i = 0;
    while i < n {
        let page = VirtAddr(va.as_u64() + i as u64 * PAGE_SIZE);
        let _ = unsafe { paging_init::unmap_4k_locked(pt, page) };
        i += 1;
    }
}
