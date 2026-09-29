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
use vibeos::pmm::Frames;

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

/// Map one heap page. Its frame's `Frames` is consumed into the leaf: the
/// heap never unmaps a heap-window page (DESIGN §4.4), so that entry is
/// the frame's owner record for good.
fn map_one(va: u64) -> Result<(), ()> {
    let va = VirtAddr(va);
    let r = paging_init::with_pt(|pt| {
        let pa = pmm_init::with_buddy(|b| b.alloc(0)).ok_or(())?.into_entry();
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
    /// # Safety
    /// `layout` is a valid allocation request.
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        #[cfg(feature = "kernel_tests")]
        if fail_after::refuse() {
            return ptr::null_mut();
        }
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

    /// # Safety
    /// `ptr` came from `alloc` with the same `layout`.
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { HEAP.lock().0.dealloc(ptr, layout) };
    }

    /// # Safety
    /// `ptr` came from `alloc` with `layout`; the returned pointer replaces it.
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        #[cfg(feature = "kernel_tests")]
        if fail_after::refuse() {
            return ptr::null_mut();
        }
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

/// A `kernel_tests` hook that fails every counted heap allocation after a
/// budget (ROADMAP §10.4, C-FAILAFTER). `KernelAlloc::alloc` and `realloc`
/// ask [`fail_after::refuse`] before they take the heap lock or grow the
/// heap, so a refused allocation maps no frame; `dealloc` never asks.
#[cfg(feature = "kernel_tests")]
pub(crate) mod fail_after {
    use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};

    use vibeos::thread::ThreadId;

    use crate::x86::InterruptGuard;
    use crate::{per_cpu_init, syscall_init, thread_init};

    /// Which allocations the hook counts.
    #[derive(Clone, Copy)]
    pub(crate) enum Scope {
        /// Allocations on a process thread (pid != 0) whose
        /// `Tcb.syscall_count` is at least `from_syscall`: 1 counts from
        /// the thread's first syscall, 2 skips it.
        Processes { from_syscall: u64 },
        /// Allocations on one thread, a kernel thread included.
        Thread(ThreadId),
    }

    /// What the hook saw while armed: `counted` in-scope allocations, of
    /// which it refused the last `refused`.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) struct Seen {
        pub counted: usize,
        pub refused: usize,
    }

    const KIND_PROCESSES: u8 = 0;
    const KIND_THREAD: u8 = 1;

    static ARMED: AtomicBool = AtomicBool::new(false);
    static KIND: AtomicU8 = AtomicU8::new(KIND_PROCESSES);
    /// `from_syscall`, or the thread id.
    static ARG: AtomicU64 = AtomicU64::new(0);
    static BUDGET: AtomicUsize = AtomicUsize::new(0);
    static COUNTED: AtomicUsize = AtomicUsize::new(0);
    static REFUSED: AtomicUsize = AtomicUsize::new(0);

    /// The first `budget` allocations in `scope` succeed; every later one
    /// returns null. One arm at a time.
    pub(crate) fn arm(budget: usize, scope: Scope) {
        assert!(!ARMED.load(Ordering::Acquire), "fail_after: armed twice");
        let (kind, arg) = match scope {
            Scope::Processes { from_syscall } => (KIND_PROCESSES, from_syscall),
            Scope::Thread(id) => (KIND_THREAD, u64::from(id.0)),
        };
        KIND.store(kind, Ordering::Relaxed);
        ARG.store(arg, Ordering::Relaxed);
        BUDGET.store(budget, Ordering::Relaxed);
        COUNTED.store(0, Ordering::Relaxed);
        REFUSED.store(0, Ordering::Relaxed);
        // Publishes the fields above to `refuse`'s Acquire load.
        ARMED.store(true, Ordering::Release);
    }

    /// Stop counting and return what the hook saw since [`arm`].
    pub(crate) fn disarm() -> Seen {
        ARMED.store(false, Ordering::Release);
        Seen {
            counted: COUNTED.load(Ordering::Acquire),
            refused: REFUSED.load(Ordering::Acquire),
        }
    }

    /// Whether this allocation is refused. Atomics only; never allocates.
    pub(super) fn refuse() -> bool {
        if !ARMED.load(Ordering::Acquire) {
            return false;
        }
        // One thread's pid, id and count: no switch between the reads.
        let _irq = InterruptGuard::enter();
        if per_cpu_init::current_thread().is_null() {
            return false;
        }
        let arg = ARG.load(Ordering::Relaxed);
        let in_scope = match KIND.load(Ordering::Relaxed) {
            KIND_PROCESSES => {
                thread_init::current_pid() != 0 && syscall_init::syscall_count() >= arg
            }
            _ => u64::from(thread_init::current_id().0) == arg,
        };
        if !in_scope {
            return false;
        }
        let n = COUNTED.fetch_add(1, Ordering::AcqRel);
        if n < BUDGET.load(Ordering::Relaxed) {
            return false;
        }
        REFUSED.fetch_add(1, Ordering::AcqRel);
        true
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
