//! Kernel-side KVA: range allocator plus mapping. DESIGN §4.5 / §7.9.
//!
//! `vibeos::kva::Kva` hands out VA. This module maps order-0 frames into
//! the reserved ranges, leaves the guard page unmapped, and keeps a
//! deferred-free list for stacks that cannot be unmapped from themselves.
//! The KVA free-list lives under the page-table lock. Unmap, drop PT,
//! shootdown, then free VA to the tail.
#![allow(dead_code)]

use core::cell::UnsafeCell;

use vibeos::kva::{KVA_END, KVA_SIZE, KVA_START, Kva, KvaStats, PAGE_SIZE};
use vibeos::paging::{PhysAddr, VirtAddr, heap_flags, stack_flags};

use crate::paging_init;
use crate::pmm_init;

struct BootCell<T>(UnsafeCell<T>);
unsafe impl<T> Sync for BootCell<T> {}
impl<T> BootCell<T> {
    const fn new(v: T) -> Self {
        Self(UnsafeCell::new(v))
    }
    /// # Safety
    /// Exclusive boot/IRQ-off access; cell is initialized.
    #[allow(clippy::mut_from_ref)] // boot cell, IRQ-off exclusive
    unsafe fn get_mut(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
    /// # Safety
    /// Cell is initialized; shared reads are ok after boot.
    unsafe fn get(&self) -> &T {
        unsafe { &*self.0.get() }
    }
}

static KVA: BootCell<Kva> = BootCell::new(Kva::empty());

#[derive(Clone, Copy, Debug)]
pub struct GuardedStack {
    pub guard: VirtAddr,
    pub pages: usize,
}

impl GuardedStack {
    #[allow(dead_code)]
    pub fn mapped_base(self) -> VirtAddr {
        VirtAddr(self.guard.as_u64() + PAGE_SIZE)
    }
    pub fn top(self) -> VirtAddr {
        VirtAddr(self.guard.as_u64() + (self.pages as u64 + 1) * PAGE_SIZE)
    }
}

const MAX_DEFERRED: usize = 8;
static DEFERRED: BootCell<[Option<GuardedStack>; MAX_DEFERRED]> =
    BootCell::new([None; MAX_DEFERRED]);

const MAX_UNMAP: usize = 32;

/// Claim the 64 GiB window. Must run after paging + heap.
///
/// # Safety
/// Single-CPU, IRQs off, live page tables already ours.
pub unsafe fn init() {
    paging_init::assert_unmapped(VirtAddr(KVA_START), VirtAddr(KVA_END));
    paging_init::with_pt(|| unsafe { KVA.get_mut().init(KVA_START, KVA_SIZE) });
}

pub fn stats() -> KvaStats {
    paging_init::with_pt(|| unsafe { KVA.get().stats() })
}

pub fn alloc_va(len: u64) -> Option<VirtAddr> {
    paging_init::with_pt(|| unsafe { KVA.get_mut().alloc(len) }.map(VirtAddr))
}

pub fn free_va(va: VirtAddr, len: u64) {
    paging_init::with_pt(|| unsafe { KVA.get_mut().free(va.as_u64(), len) });
}

/// Reserve `pages+1` VA, map the upper `pages` from separate order-0
/// frames, leave the bottom page unmapped.
pub fn alloc_guarded_stack(pages: usize) -> Option<GuardedStack> {
    if pages == 0 || pages > MAX_UNMAP {
        return None;
    }
    let mut vas = [VirtAddr(0); MAX_UNMAP];
    let mut failed_pas = [0u64; MAX_UNMAP];
    let mut failed_n = 0usize;
    let mut failed_mapped = 0usize;
    let stack = paging_init::with_pt(|| {
        let guard_u = unsafe { KVA.get_mut().alloc_guarded(pages) }?;
        let guard = VirtAddr(guard_u);
        for (i, slot) in vas.iter_mut().take(pages).enumerate() {
            let va = VirtAddr(guard_u + PAGE_SIZE * (i as u64 + 1));
            let Some(pa) = pmm_init::with_buddy(|b| b.allocate_frame()) else {
                failed_mapped = i;
                failed_n = unsafe { unwind_stack_locked(guard, i, pages, &mut failed_pas) };
                return None;
            };
            if unsafe { paging_init::map_4k_locked(va, PhysAddr(pa), stack_flags()) }.is_err() {
                pmm_init::with_buddy(|b| unsafe { b.deallocate_frame(pa) });
                failed_mapped = i;
                failed_n = unsafe { unwind_stack_locked(guard, i, pages, &mut failed_pas) };
                return None;
            }
            *slot = va;
        }
        Some(GuardedStack { guard, pages })
    });
    let Some(stack) = stack else {
        let mut j = 0;
        while j < failed_mapped {
            vibeos::paging::tlb_shootdown_others(vas[j]);
            j += 1;
        }
        if failed_n != 0 {
            pmm_init::with_buddy(|b| {
                let mut j = 0;
                while j < failed_n {
                    unsafe { b.deallocate_frame(failed_pas[j]) };
                    j += 1;
                }
            });
        }
        return None;
    };
    let mut i = 0;
    while i < pages {
        vibeos::paging::tlb_shootdown_others(vas[i]);
        i += 1;
    }
    Some(stack)
}

pub fn free_stack(stack: GuardedStack) {
    unsafe { free_stack_shootdown(stack) };
}

/// Park a stack on the deferred list. Drain from a context that is not
/// running on it.
pub fn defer_free(stack: GuardedStack) {
    paging_init::with_pt(|| {
        let slots = unsafe { DEFERRED.get_mut() };
        for slot in slots.iter_mut() {
            if slot.is_none() {
                *slot = Some(stack);
                return;
            }
        }
        panic!("kva: deferred free list full");
    });
}

pub fn drain_deferred() {
    let mut pending = [None; MAX_DEFERRED];
    paging_init::with_pt(|| {
        let slots = unsafe { DEFERRED.get_mut() };
        let mut i = 0;
        while i < MAX_DEFERRED {
            pending[i] = slots[i].take();
            i += 1;
        }
    });
    let mut i = 0;
    while i < MAX_DEFERRED {
        if let Some(s) = pending[i] {
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
    let va = paging_init::with_pt(|| {
        let len = n as u64 * PAGE_SIZE;
        let va_u = unsafe { KVA.get_mut().alloc(len) }?;
        for (i, &pa) in frames.iter().enumerate() {
            let page = VirtAddr(va_u + i as u64 * PAGE_SIZE);
            if unsafe { paging_init::map_4k_locked(page, pa, heap_flags()) }.is_err() {
                unsafe { unmap_only_locked(VirtAddr(va_u), i) };
                unsafe { KVA.get_mut().free(va_u, len) };
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
    unmap_shootdown(va, nframes, false);
    paging_init::with_pt(|| unsafe { KVA.get_mut().free(va.as_u64(), nframes as u64 * PAGE_SIZE) });
}

/// # Safety
/// `stack` was allocated by this KVA; not the running stack.
unsafe fn free_stack_shootdown(stack: GuardedStack) {
    let base = stack.mapped_base();
    unmap_shootdown(base, stack.pages, true);
    paging_init::with_pt(|| unsafe {
        KVA.get_mut()
            .free(stack.guard.as_u64(), (stack.pages as u64 + 1) * PAGE_SIZE);
    });
}

/// Unmap `n` pages, drop PT, shootdown. If `free_frames`, return them to
/// the buddy after the shootdown.
fn unmap_shootdown(va: VirtAddr, n: usize, free_frames: bool) {
    let mut pas = [0u64; MAX_UNMAP];
    let mut np = 0usize;
    let n = n.min(MAX_UNMAP);
    paging_init::with_pt(|| {
        let mut i = 0;
        while i < n {
            let page = VirtAddr(va.as_u64() + i as u64 * PAGE_SIZE);
            if let Some((pa, _)) = unsafe { paging_init::unmap_4k_locked(page) } {
                pas[np] = pa.as_u64();
                np += 1;
            }
            i += 1;
        }
    });
    let mut i = 0;
    while i < n {
        vibeos::paging::tlb_shootdown_others(VirtAddr(va.as_u64() + i as u64 * PAGE_SIZE));
        i += 1;
    }
    if free_frames {
        pmm_init::with_buddy(|b| {
            let mut j = 0;
            while j < np {
                unsafe { b.deallocate_frame(pas[j]) };
                j += 1;
            }
        });
    }
}

/// # Safety
/// Caller holds the PT lock.
unsafe fn unmap_only_locked(va: VirtAddr, n: usize) {
    let mut i = 0;
    while i < n {
        let page = VirtAddr(va.as_u64() + i as u64 * PAGE_SIZE);
        let _ = unsafe { paging_init::unmap_4k_locked(page) };
        i += 1;
    }
}

/// Unmap `n` pages under PT. Caller shootdowns, then frees frames.
///
/// # Safety
/// Caller holds the PT lock; `out` is large enough.
unsafe fn unmap_collect_locked(va: VirtAddr, n: usize, out: &mut [u64; MAX_UNMAP]) -> usize {
    let mut np = 0usize;
    let mut i = 0;
    while i < n {
        let page = VirtAddr(va.as_u64() + i as u64 * PAGE_SIZE);
        if let Some((pa, _)) = unsafe { paging_init::unmap_4k_locked(page) } {
            out[np] = pa.as_u64();
            np += 1;
        }
        i += 1;
    }
    np
}

/// Partial guarded-stack construction failed after `mapped` upper pages.
/// Caller holds PT. Unmap + free VA; frames are returned after shootdown.
///
/// # Safety
/// Caller holds the PT lock; `guard` is the failed allocation.
unsafe fn unwind_stack_locked(
    guard: VirtAddr,
    mapped: usize,
    pages: usize,
    out_pas: &mut [u64; MAX_UNMAP],
) -> usize {
    let np = unsafe { unmap_collect_locked(VirtAddr(guard.as_u64() + PAGE_SIZE), mapped, out_pas) };
    unsafe {
        KVA.get_mut()
            .free(guard.as_u64(), (pages as u64 + 1) * PAGE_SIZE)
    };
    np
}
