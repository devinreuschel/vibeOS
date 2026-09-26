//! Kernel-side KVA: range allocator plus mapping. DESIGN §4.5 / §7.9.
//!
//! `vibeos::kva::Kva` hands out VA. This module maps order-0 frames into
//! the reserved ranges, leaves the guard page unmapped, and keeps a
//! deferred-free list for stacks that cannot be unmapped from themselves.
//! The KVA free-list lives under the page-table lock. Unmap, drop PT,
//! shootdown, then free VA to the tail.
#![allow(dead_code)]

use vibeos::kva::{KVA_END, KVA_SIZE, KVA_START, Kva, KvaError, KvaStats, PAGE_SIZE};
use vibeos::paging::{PhysAddr, VirtAddr, heap_flags, stack_flags};
use vibeos::pmm::Frames;
pub use vibeos::thread::GuardedStack;
use vibeos::thread::MAX_STACK_PAGES;

use crate::cell::IrqCell;
use crate::paging_init;
use crate::pmm_init;

static KVA: IrqCell<Kva> = IrqCell::new(Kva::empty());

use vibeos::limits::MAX_DEFERRED_STACKS as MAX_DEFERRED;
static DEFERRED: IrqCell<[Option<GuardedStack>; MAX_DEFERRED]> =
    IrqCell::new([const { None }; MAX_DEFERRED]);

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

/// Park a stack on the deferred list. Drain from a context that is not
/// running on it.
pub fn defer_free(stack: GuardedStack) {
    paging_init::with_pt(|_pt| {
        DEFERRED.with(|slots| {
            for slot in slots.iter_mut() {
                if slot.is_none() {
                    *slot = Some(stack);
                    return;
                }
            }
            panic!("kva: deferred free list full");
        });
    });
}

pub fn drain_deferred() {
    let mut pending: [Option<GuardedStack>; MAX_DEFERRED] = [const { None }; MAX_DEFERRED];
    paging_init::with_pt(|_pt| {
        DEFERRED.with(|slots| {
            let mut i = 0;
            while i < MAX_DEFERRED {
                pending[i] = slots[i].take();
                i += 1;
            }
        });
    });
    let mut i = 0;
    while i < MAX_DEFERRED {
        if let Some(s) = pending[i].take() {
            unsafe { free_stack_shootdown(s) };
        }
        i += 1;
    }
}

/// Present `frames` contiguously. Caller keeps the frames; `vunmap`
/// only releases the VA.
pub fn vmap(frames: &[PhysAddr]) -> Option<VirtAddr> {
    if frames.is_empty() || frames.len() > MAX_UNMAP {
        return None;
    }
    let mut vas = [VirtAddr(0); MAX_UNMAP];
    let n = frames.len();
    let va = paging_init::with_pt(|pt| {
        let len = n as u64 * PAGE_SIZE;
        let va_u = KVA.with(|k| k.alloc(len))?;
        for (i, &pa) in frames.iter().enumerate() {
            let page = VirtAddr(va_u + i as u64 * PAGE_SIZE);
            if unsafe { paging_init::map_4k_locked(pt, page, pa, heap_flags()) }.is_err() {
                unsafe { unmap_only_locked(pt, VirtAddr(va_u), i) };
                KVA.with(|k| k.free(va_u, len).expect("kva: free-list"));
                return None;
            }
            vas[i] = page;
        }
        Some(VirtAddr(va_u))
    })?;
    let mut i = 0;
    while i < n {
        vibeos::paging::tlb_shootdown_others(vas[i]);
        i += 1;
    }
    Some(va)
}

pub fn vunmap(va: VirtAddr, nframes: usize) {
    unmap_shootdown(va, nframes);
    paging_init::with_pt(|_pt| {
        KVA.with(|k| {
            k.free(va.as_u64(), nframes as u64 * PAGE_SIZE)
                .expect("kva: free-list")
        })
    });
}

/// # Safety
/// `stack` was allocated by this KVA; not the running stack.
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
