//! Kernel-side heap: map the DESIGN §4.1 window, `GlobalAlloc`, error handler.
//!
//! Portable free-list logic is `vibeos::heap`. This module pulls order-0
//! frames from the buddy, maps them writable+NX, and grows in page
//! increments up to the 64 MiB cap.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;

use vibeos::heap::{Heap, HeapStats, HEAP_END, HEAP_INITIAL, HEAP_SIZE, HEAP_START, PAGE_SIZE};
use vibeos::paging::{heap_flags, PhysAddr, VirtAddr};

use crate::paging_init;
use crate::pmm_init;
use crate::x86;

struct BootCell<T>(UnsafeCell<T>);
unsafe impl<T> Sync for BootCell<T> {}
impl<T> BootCell<T> {
    const fn new(v: T) -> Self {
        Self(UnsafeCell::new(v))
    }
    /// # Safety
    /// Single-threaded, interrupts off (the `GlobalAlloc` wrapper holds
    /// an [`InterruptGuard`] for the whole call).
    #[allow(clippy::mut_from_ref)]
    unsafe fn get_mut(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
    unsafe fn get(&self) -> &T {
        unsafe { &*self.0.get() }
    }
}

static HEAP: BootCell<Heap> = BootCell::new(Heap::empty());

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
        HEAP.get_mut()
            .init(HEAP_START as usize, HEAP_INITIAL as usize, HEAP_SIZE as usize)
    };
}

pub fn stats() -> HeapStats {
    let _g = x86::InterruptGuard::enter();
    unsafe { HEAP.get().stats() }
}

fn map_one(va: u64) -> Result<(), ()> {
    let pa = unsafe { pmm_init::with_buddy(|b| b.allocate_frame()) }.ok_or(())?;
    unsafe {
        paging_init::map_4k(VirtAddr(va), PhysAddr(pa), heap_flags()).map_err(|_| {
            pmm_init::with_buddy(|b| b.deallocate_frame(pa));
            ()
        })
    }
}

fn grow_for(layout: Layout) -> bool {
    let extra = align_up(Heap::block_bytes(layout), PAGE_SIZE);
    if extra == 0 {
        return false;
    }
    let heap = unsafe { HEAP.get_mut() };
    let old = heap.mapped();
    let Some(want) = old.checked_add(extra) else {
        return false;
    };
    if want > heap.cap() {
        return false;
    }
    let mut mapped = old;
    while mapped < want {
        match map_one(HEAP_START + mapped as u64) {
            Ok(()) => mapped += PAGE_SIZE,
            Err(()) => break,
        }
    }
    if mapped <= old {
        return false;
    }
    unsafe { heap.extend(mapped) };
    mapped >= want
}

struct KernelAlloc;

unsafe impl GlobalAlloc for KernelAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _g = x86::InterruptGuard::enter();
        {
            let p = unsafe { HEAP.get_mut().alloc(layout) };
            if !p.is_null() {
                return p;
            }
        }
        if !grow_for(layout) {
            return ptr::null_mut();
        }
        unsafe { HEAP.get_mut().alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let _g = x86::InterruptGuard::enter();
        unsafe { HEAP.get_mut().dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _g = x86::InterruptGuard::enter();
        let p = unsafe { HEAP.get_mut().realloc(ptr, layout, new_size) };
        if !p.is_null() || new_size == 0 {
            return p;
        }
        // In-place failed and the fresh alloc missed; try growing.
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        if !grow_for(new_layout) {
            return ptr::null_mut();
        }
        unsafe { HEAP.get_mut().realloc(ptr, layout, new_size) }
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
