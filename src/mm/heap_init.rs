//! Kernel-side heap: map the DESIGN §4.1 window, `GlobalAlloc`, error handler.
//!
//! Portable free-list logic is `vibeos::heap`. This module pulls order-0
//! frames from the buddy, maps them writable+NX, and grows in page
//! increments up to the 64 MiB cap. The heap ranks first (heap → page
//! tables → buddy, DESIGN §2.1); growth still drops the heap lock before it
//! takes PT and then the buddy, so the frame allocation can enter direct
//! reclaim with nothing held (DESIGN §4.4 rule 1).

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

use vibeos::heap::{HEAP_END, HEAP_INITIAL, HEAP_SIZE, HEAP_START, Heap, HeapStats, PAGE_SIZE};
use vibeos::lock::RANK_HEAP;
use vibeos::paging::{PhysAddr, VirtAddr, heap_flags};
use vibeos::pmm::Frames;

use crate::boot;
use crate::paging_init;
use crate::pmm_init;
use crate::sync_init::SpinMutex;

/// The heap-failure hook's path (C-FAILAFTER); it lives in `mm::ktest`.
#[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
pub(crate) use super::ktest::fail_after;

struct LockedHeap(Heap);
// SAFETY: `Heap` holds only pointers into the kernel heap window, which the
// kernel half maps the same on every CPU and only `Heap` writes (invariant
// I47, established at `mm::heap_init::grow_for`); `HEAP`'s lock gives one
// CPU at a time the `&mut`, so moving it between CPUs is sound.
unsafe impl Send for LockedHeap {}

static HEAP: SpinMutex<LockedHeap> = SpinMutex::with_rank(LockedHeap(Heap::empty()), RANK_HEAP);

/// Map the initial 1 MiB and hand it to the free-list. Must run after
/// paging install, before any `alloc` use.
///
/// # Safety
/// Single-CPU, IRQs off, live page tables already ours.
pub unsafe fn init() {
    paging_init::assert_unmapped(VirtAddr(HEAP_START), VirtAddr(HEAP_END));
    let mut mapped = 0usize;
    while mapped < HEAP_INITIAL as usize {
        if map_one(HEAP_START + mapped as u64).is_err() {
            boot::halt_with("vibeOS: heap: initial map failed");
        }
        mapped += PAGE_SIZE;
    }
    // SAFETY: `Heap::init`'s contract; the loop above mapped
    // `[HEAP_START, HEAP_START + HEAP_INITIAL)` writable from fresh buddy
    // frames, `assert_unmapped` proved nothing else maps the window, and
    // `HEAP_START` is page aligned (invariant I47, established here).
    unsafe {
        HEAP.lock().0.init(
            HEAP_START as usize,
            HEAP_INITIAL as usize,
            HEAP_SIZE as usize,
        );
    }
}

pub fn stats() -> HeapStats {
    HEAP.lock().0.stats()
}

/// Map one heap page. Its frame's `Frames` is consumed into the leaf: the
/// heap never unmaps a heap-window page (DESIGN §4.4), so that entry is
/// the frame's owner record for good.
fn map_one(va: u64) -> Result<(), ()> {
    let va = VirtAddr(va);
    let r = paging_init::with_pt(|pt| {
        let pa = pmm_init::with_buddy(|b| b.alloc(0)).ok_or(())?.into_entry();
        // SAFETY: `map_4k_locked`'s contract (`Mapper::map_page`'s): `pa` is a
        // fresh buddy frame nothing else maps, and the heap window below
        // `va` is the heap's alone (invariant I47, established here); `pt`
        // is the page-table lock (invariant I48, established at
        // `mm::paging_init::current_mapper`).
        unsafe {
            paging_init::map_4k_locked(pt, va, PhysAddr(pa), heap_flags()).map_err(|_| {
                // SAFETY: `pa` is the order-0 `into_entry` above, and the
                // failed map wrote no entry, so nothing else names it
                // (the contract `pmm::Frames::from_entry` states, met here).
                let f = Frames::from_entry(pa, 0);
                pmm_init::with_buddy(|b| b.free(f));
            })
        }
    });
    if r.is_ok() {
        vibeos::paging::tlb_shootdown_others(va);
    }
    r
}

fn page_present(va: u64) -> bool {
    paging_init::translate(VirtAddr(va)).is_some()
}

fn grow_for(layout: Layout) -> bool {
    let extra = align_up(Heap::block_bytes(layout), PAGE_SIZE);
    if extra == 0 {
        return false;
    }
    let (old, cap) = {
        let h = HEAP.lock();
        (h.0.mapped(), h.0.cap())
    };
    let Some(want) = old.checked_add(extra) else {
        return false;
    };
    if want > cap {
        return false;
    }
    let mut mapped = old;
    while mapped < want {
        let va = HEAP_START + mapped as u64;
        if page_present(va) {
            mapped += PAGE_SIZE;
            continue;
        }
        match map_one(va) {
            Ok(()) => mapped += PAGE_SIZE,
            Err(()) => {
                if page_present(va) {
                    mapped += PAGE_SIZE;
                    continue;
                }
                break;
            }
        }
    }
    if mapped <= old {
        return false;
    }
    // `page_present` takes PT with HEAP dropped, as the maps above did:
    // growth holds nothing across a frame allocation (DESIGN §4.4 rule 1).
    let (cur, cap) = {
        let h = HEAP.lock();
        (h.0.mapped(), h.0.cap())
    };
    let mut n = cur;
    while n < mapped && n < cap {
        if !page_present(HEAP_START + n as u64) {
            break;
        }
        n += PAGE_SIZE;
    }
    if n > cur {
        let mut h = HEAP.lock();
        let now = h.0.mapped();
        if n > now && n <= h.0.cap() {
            // SAFETY: `Heap::extend`'s contract; `page_present` found every
            // page of `[now, n)` mapped, and only this heap maps heap-window
            // pages (`map_one`), so the span is writable and unused
            // (invariant I47, established here).
            unsafe { h.0.extend(n) };
        }
    }
    n >= want
}

struct KernelAlloc;

/// Grow rounds before giving up. Cap is 64 MiB; this is a fuse, not the
/// real bound (`grow_for` returns false at the window cap).
const GROW_ROUNDS: u32 = 4096;

// SAFETY: `GlobalAlloc`'s contract; every block comes from `Heap`, which
// hands out disjoint blocks of at least the layout's size and alignment
// inside memory mapped before `Heap::extend` takes it (invariant I47,
// established at `mm::heap_init::grow_for`), and a failure returns null.
unsafe impl GlobalAlloc for KernelAlloc {
    /// # Safety
    /// `layout` is a valid allocation request.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
        if fail_after::refuse() {
            return ptr::null_mut();
        }
        let mut n = 0u32;
        loop {
            {
                let mut h = HEAP.lock();
                // SAFETY: `Heap::alloc`'s contract; the caller treats the
                // block as `layout` until `dealloc` (`GlobalAlloc::alloc`'s
                // contract, established here).
                let p = unsafe { h.0.alloc(layout) };
                if !p.is_null() {
                    return p;
                }
            }
            if n >= GROW_ROUNDS || !grow_for(layout) {
                return ptr::null_mut();
            }
            n += 1;
        }
    }

    /// # Safety
    /// `ptr` came from `alloc` with the same `layout`.
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `Heap::dealloc`'s contract; `ptr` came from `alloc` with
        // `layout` (this fn's `# Safety` contract, established here).
        unsafe { HEAP.lock().0.dealloc(ptr, layout) };
    }

    /// # Safety
    /// `ptr` came from `alloc` with `layout`; the returned pointer replaces it.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
        if fail_after::refuse() {
            return ptr::null_mut();
        }
        let mut n = 0u32;
        loop {
            {
                let mut h = HEAP.lock();
                // SAFETY: `Heap::realloc`'s contract; `ptr` came from `alloc`
                // with `layout`, and `new_size` rounded to its alignment
                // fits `isize` (`GlobalAlloc::realloc`'s contract,
                // established here).
                let p = unsafe { h.0.realloc(ptr, layout, new_size) };
                if !p.is_null() || new_size == 0 {
                    return p;
                }
            }
            // SAFETY: `layout.align()` is a power of two, and
            // `GlobalAlloc::realloc`'s contract bounds `new_size`,
            // established here.
            let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
            if n >= GROW_ROUNDS || !grow_for(new_layout) {
                return ptr::null_mut();
            }
            n += 1;
        }
    }
}

#[global_allocator]
static GLOBAL: KernelAlloc = KernelAlloc;

#[alloc_error_handler]
#[allow(
    clippy::panic,
    reason = "DESIGN §4.4: only an infallible allocation reaches this handler, and those run only before irq: enabled"
)]
fn on_alloc_error(layout: Layout) -> ! {
    #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
    crate::arch::catch::on_alloc_error(layout);
    panic!(
        "alloc error: size={} align={}",
        layout.size(),
        layout.align()
    );
}

#[inline]
const fn align_up(x: usize, a: usize) -> usize {
    (x + a - 1) & !(a - 1)
}
