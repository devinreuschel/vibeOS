//! Kernel-side KVA: range allocator plus mapping. DESIGN §4.5.
//!
//! `vibeos::kva::Kva` hands out VA. This module maps order-0 frames into
//! the reserved ranges, leaves the guard page unmapped, and keeps a
//! deferred-free list for stacks that cannot be unmapped from themselves.
//
// Production boot only needs init/stats/stack probe. The rest is the
// ktest + phase-3 API; keep it compiled so a missing symbol is a type
// error rather than a surprise in the test ISO.
#![allow(dead_code)]

use core::cell::UnsafeCell;

use vibeos::kva::{Kva, KvaStats, KVA_END, KVA_SIZE, KVA_START, PAGE_SIZE};
use vibeos::paging::{heap_flags, stack_flags, PhysAddr, VirtAddr};

use crate::paging_init;
use crate::pmm_init;
use crate::x86;

struct BootCell<T>(UnsafeCell<T>);
unsafe impl<T> Sync for BootCell<T> {}
impl<T> BootCell<T> {
    const fn new(v: T) -> Self {
        Self(UnsafeCell::new(v))
    }
    #[allow(clippy::mut_from_ref)]
    unsafe fn get_mut(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
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

/// Claim the 64 GiB window. Must run after paging + heap.
///
/// # Safety
/// Single-CPU, IRQs off, live page tables already ours.
pub unsafe fn init() {
    paging_init::assert_unmapped(VirtAddr(KVA_START), VirtAddr(KVA_END));
    unsafe { KVA.get_mut().init(KVA_START, KVA_SIZE) };
}

pub fn stats() -> KvaStats {
    let _g = x86::InterruptGuard::enter();
    unsafe { KVA.get().stats() }
}

pub fn alloc_va(len: u64) -> Option<VirtAddr> {
    let _g = x86::InterruptGuard::enter();
    unsafe { KVA.get_mut().alloc(len) }.map(VirtAddr)
}

pub fn free_va(va: VirtAddr, len: u64) {
    let _g = x86::InterruptGuard::enter();
    unsafe { KVA.get_mut().free(va.as_u64(), len) };
}

/// Reserve `pages+1` VA, map the upper `pages` from separate order-0
/// frames, leave the bottom page unmapped.
pub fn alloc_guarded_stack(pages: usize) -> Option<GuardedStack> {
    if pages == 0 {
        return None;
    }
    let _g = x86::InterruptGuard::enter();
    let guard_u = unsafe { KVA.get_mut().alloc_guarded(pages) }?;
    let guard = VirtAddr(guard_u);
    for i in 0..pages {
        let va = VirtAddr(guard_u + PAGE_SIZE * (i as u64 + 1));
        let Some(pa) = (unsafe { pmm_init::with_buddy(|b| b.allocate_frame()) }) else {
            unsafe { unwind_stack(guard, i, pages) };
            return None;
        };
        if unsafe { paging_init::map_4k(va, PhysAddr(pa), stack_flags()) }.is_err() {
            unsafe { pmm_init::with_buddy(|b| b.deallocate_frame(pa)) };
            unsafe { unwind_stack(guard, i, pages) };
            return None;
        }
    }
    Some(GuardedStack { guard, pages })
}

pub fn free_stack(stack: GuardedStack) {
    let _g = x86::InterruptGuard::enter();
    unsafe { free_stack_locked(stack) };
}

/// Park a stack on the deferred list. Drain from a context that is not
/// running on it.
pub fn defer_free(stack: GuardedStack) {
    let _g = x86::InterruptGuard::enter();
    let slots = unsafe { DEFERRED.get_mut() };
    for slot in slots.iter_mut() {
        if slot.is_none() {
            *slot = Some(stack);
            return;
        }
    }
    panic!("kva: deferred free list full");
}

pub fn drain_deferred() {
    let _g = x86::InterruptGuard::enter();
    let slots = unsafe { DEFERRED.get_mut() };
    for slot in slots.iter_mut() {
        if let Some(s) = slot.take() {
            unsafe { free_stack_locked(s) };
        }
    }
}

/// Present `frames` contiguously. Caller keeps the frames; `vunmap`
/// only releases the VA.
pub fn vmap(frames: &[PhysAddr]) -> Option<VirtAddr> {
    if frames.is_empty() {
        return None;
    }
    let _g = x86::InterruptGuard::enter();
    let len = frames.len() as u64 * PAGE_SIZE;
    let va_u = unsafe { KVA.get_mut().alloc(len) }?;
    for (i, &pa) in frames.iter().enumerate() {
        let va = VirtAddr(va_u + i as u64 * PAGE_SIZE);
        if unsafe { paging_init::map_4k(va, pa, heap_flags()) }.is_err() {
            unsafe { unmap_only(VirtAddr(va_u), i) };
            unsafe { KVA.get_mut().free(va_u, len) };
            return None;
        }
    }
    Some(VirtAddr(va_u))
}

pub fn vunmap(va: VirtAddr, nframes: usize) {
    let _g = x86::InterruptGuard::enter();
    unsafe { unmap_only(va, nframes) };
    unsafe {
        KVA.get_mut()
            .free(va.as_u64(), nframes as u64 * PAGE_SIZE)
    };
}

unsafe fn free_stack_locked(stack: GuardedStack) {
    unsafe { unmap_and_free(stack.mapped_base(), stack.pages) };
    unsafe {
        KVA.get_mut()
            .free(stack.guard.as_u64(), (stack.pages as u64 + 1) * PAGE_SIZE)
    };
}

unsafe fn unmap_only(va: VirtAddr, n: usize) {
    for i in 0..n {
        let page = VirtAddr(va.as_u64() + i as u64 * PAGE_SIZE);
        let _ = unsafe { paging_init::unmap_4k(page) };
    }
}

unsafe fn unmap_and_free(va: VirtAddr, n: usize) {
    for i in 0..n {
        let page = VirtAddr(va.as_u64() + i as u64 * PAGE_SIZE);
        if let Some((pa, _)) = unsafe { paging_init::unmap_4k(page) } {
            unsafe { pmm_init::with_buddy(|b| b.deallocate_frame(pa.as_u64())) };
        }
    }
}

/// Partial guarded-stack construction failed after `mapped` upper pages.
/// The original reservation was `pages + 1`.
unsafe fn unwind_stack(guard: VirtAddr, mapped: usize, pages: usize) {
    unsafe { unmap_and_free(VirtAddr(guard.as_u64() + PAGE_SIZE), mapped) };
    unsafe {
        KVA.get_mut()
            .free(guard.as_u64(), (pages as u64 + 1) * PAGE_SIZE)
    };
}
