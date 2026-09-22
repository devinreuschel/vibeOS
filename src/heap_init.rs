//! Kernel-side heap: map the DESIGN §4.1 window, `GlobalAlloc`, error handler.
//!
//! Portable free-list logic is `vibeos::heap`. This module pulls order-0
//! frames from the buddy, maps them writable+NX, and grows in page
//! increments up to the 64 MiB cap. Heap lock is dropped before PT/buddy
//! on grow (lock order: page tables → buddy → heap).

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

use vibeos::heap::{HEAP_END, HEAP_INITIAL, HEAP_SIZE, HEAP_START, Heap, HeapStats, PAGE_SIZE};
use vibeos::lock::RANK_HEAP;
use vibeos::paging::{PhysAddr, VirtAddr, heap_flags};

use crate::paging_init;
use crate::pmm_init;
use crate::sync_init::SpinMutex;

struct LockedHeap(Heap);
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
        map_one(HEAP_START + mapped as u64).expect("heap: initial map");
        mapped += PAGE_SIZE;
    }
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

fn map_one(va: u64) -> Result<(), ()> {
    let va = VirtAddr(va);
    let r = paging_init::with_pt(|| {
        let pa = pmm_init::with_buddy(|b| b.allocate_frame()).ok_or(())?;
        unsafe {
            paging_init::map_4k_locked(va, PhysAddr(pa), heap_flags()).map_err(|_| {
                pmm_init::with_buddy(|b| b.deallocate_frame(pa));
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
    // translate takes PT (rank 1); HEAP is rank 3. Do not invert.
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
            unsafe { h.0.extend(n) };
        }
    }
    n >= want
}

struct KernelAlloc;

/// Grow rounds before giving up. Cap is 64 MiB; this is a fuse, not the
/// real bound (`grow_for` returns false at the window cap).
const GROW_ROUNDS: u32 = 4096;

unsafe impl GlobalAlloc for KernelAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut n = 0u32;
        loop {
            {
                let mut h = HEAP.lock();
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

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { HEAP.lock().0.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let mut n = 0u32;
        loop {
            {
                let mut h = HEAP.lock();
                let p = unsafe { h.0.realloc(ptr, layout, new_size) };
                if !p.is_null() || new_size == 0 {
                    return p;
                }
            }
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
fn on_alloc_error(layout: Layout) -> ! {
    #[cfg(feature = "kernel_tests")]
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
