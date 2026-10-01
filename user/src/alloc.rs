//! The heap: a `#[global_allocator]` over `brk` (ROADMAP §10.5), so
//! `alloc`'s `Box`, `Vec` and `String` work in any program that names the
//! `alloc` crate.
//!
//! - Requests of at most 2,048 bytes, alignment included, take a block of
//!   the next power-of-two size class from 16 to 2,048 bytes. Each class
//!   keeps an intrusive free list, and a block is carved from the current
//!   slab, a 64 KiB page run, aligned to its own size.
//! - Larger requests, and alignment above 2,048, take whole pages from an
//!   address-ordered list of free page runs, first fit, which a free
//!   coalesces with its neighbours.
//! - When nothing free fits, the break grows by the pages needed, at least
//!   64 KiB, and what the request does not use joins the free runs.
//! - `dealloc` takes the size from its `Layout`, so blocks carry no header.
//! - Alignment above 4096 is refused, as is anything `brk` refuses: the
//!   call returns null, and the allocator never panics.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::{self, NonNull};

use crate::sys;

const PAGE: usize = 4096;
/// The least the break grows by, and a slab's size.
const STEP: usize = 64 * 1024;
/// The smallest size class; a free block holds a list pointer.
const MIN_CLASS: usize = 16;
/// The largest size class.
const MAX_CLASS: usize = 2048;
/// Classes 16, 32, ..., 2048.
const CLASSES: usize = 8;
/// The largest alignment served.
const MAX_ALIGN: usize = PAGE;

/// A free block of a size class: the next free block of its class.
struct FreeBlock {
    next: Option<NonNull<FreeBlock>>,
}

/// A free page run, kept at its own start: its length in bytes (a page
/// multiple) and the next free run above it.
struct FreeRun {
    len: usize,
    next: Option<NonNull<FreeRun>>,
}

struct State {
    /// The break, once read; 0 before the first allocation.
    top: usize,
    /// The current slab's unused part.
    bump: usize,
    bump_end: usize,
    classes: [Option<NonNull<FreeBlock>>; CLASSES],
    /// Free page runs, lowest address first.
    runs: Option<NonNull<FreeRun>>,
}

/// The runtime's allocator (C-USERRT): see the module docs.
pub struct BrkHeap {
    state: UnsafeCell<State>,
}

// SAFETY: one thread per process until ROADMAP §13.1, and no signal handler
// before §13.8, so no two calls ever reach `state` at once; established
// here: the runtime starts no thread and installs no handler, and the kernel
// gives a process one thread and runs no user handler until then.
// TODO(ROADMAP §13.1): a lock arrives with threads.
unsafe impl Sync for BrkHeap {}

impl BrkHeap {
    pub const fn new() -> Self {
        Self {
            state: UnsafeCell::new(State {
                top: 0,
                bump: 0,
                bump_end: 0,
                classes: [None; CLASSES],
                runs: None,
            }),
        }
    }
}

impl Default for BrkHeap {
    fn default() -> Self {
        Self::new()
    }
}

/// The size class index for a request of `size` bytes aligned to `align`,
/// or None for a page request.
fn class_of(size: usize, align: usize) -> Option<usize> {
    let need = size.max(align).max(MIN_CLASS);
    if need > MAX_CLASS {
        return None;
    }
    let bytes = need.checked_next_power_of_two()?;
    Some((bytes.trailing_zeros() - MIN_CLASS.trailing_zeros()) as usize)
}

const fn class_bytes(i: usize) -> usize {
    MIN_CLASS << i
}

/// `n` rounded up to a page, or None on overflow.
fn page_round(n: usize) -> Option<usize> {
    n.checked_add(PAGE - 1).map(|v| v & !(PAGE - 1))
}

impl State {
    /// A block of class `i`.
    fn alloc_class(&mut self, i: usize) -> *mut u8 {
        if let Some(b) = self.classes[i] {
            // SAFETY: a block on a free list is a free block this heap
            // carved and nothing else refers to; established here by
            // `free_class`.
            self.classes[i] = unsafe { b.as_ref().next };
            return b.as_ptr().cast();
        }
        let size = class_bytes(i);
        let start = (self.bump + size - 1) & !(size - 1);
        if self.bump_end == 0 || start.checked_add(size).is_none_or(|e| e > self.bump_end) {
            let slab = self.alloc_pages(STEP);
            if slab.is_null() {
                return ptr::null_mut();
            }
            self.bump = slab as usize;
            self.bump_end = self.bump + STEP;
            return self.alloc_class(i);
        }
        self.bump = start + size;
        start as *mut u8
    }

    fn free_class(&mut self, p: *mut u8, i: usize) {
        let b = p.cast::<FreeBlock>();
        // SAFETY: `p` is a block of class `i` the caller no longer uses, at
        // least 16 bytes and aligned to its size; established here by the
        // `GlobalAlloc::dealloc` contract.
        unsafe {
            b.write(FreeBlock {
                next: self.classes[i],
            })
        };
        self.classes[i] = NonNull::new(b);
    }

    /// `len` bytes (a page multiple) of page-aligned memory.
    fn alloc_pages(&mut self, len: usize) -> *mut u8 {
        let mut prev: Option<NonNull<FreeRun>> = None;
        let mut cur = self.runs;
        while let Some(r) = cur {
            // SAFETY: a run on the list is free memory this heap owns, with
            // its header at its start; established here by `free_pages`.
            let (rlen, next) = unsafe { (r.as_ref().len, r.as_ref().next) };
            if rlen >= len {
                let rest = rlen - len;
                let link = if rest == 0 {
                    next
                } else {
                    let tail = (r.as_ptr() as usize + len) as *mut FreeRun;
                    // SAFETY: `tail` is inside the free run `r`, page-aligned,
                    // and past what this call hands out; established here.
                    unsafe { tail.write(FreeRun { len: rest, next }) };
                    NonNull::new(tail)
                };
                self.set_next(prev, link);
                return r.as_ptr().cast();
            }
            prev = cur;
            cur = next;
        }
        self.grow(len)
    }

    /// Point `prev`'s link, or the list head, at `to`.
    fn set_next(&mut self, prev: Option<NonNull<FreeRun>>, to: Option<NonNull<FreeRun>>) {
        match prev {
            // SAFETY: `prev` is a run on the free list; established here by
            // `free_pages`.
            Some(mut p) => unsafe { p.as_mut().next = to },
            None => self.runs = to,
        }
    }

    /// Grow the break for `len` bytes and return them; what the growth adds
    /// past them joins the free runs.
    fn grow(&mut self, len: usize) -> *mut u8 {
        if self.top == 0 {
            // SAFETY: `brk(0)` changes nothing; established here.
            let Ok(top) = (unsafe { sys::brk(0) }) else {
                return ptr::null_mut();
            };
            let Some(top) = page_round(top) else {
                return ptr::null_mut();
            };
            self.top = top;
        }
        let Some(step) = page_round(len.max(STEP)) else {
            return ptr::null_mut();
        };
        let Some(end) = self.top.checked_add(step) else {
            return ptr::null_mut();
        };
        // SAFETY: the break only grows here, over pages nothing uses; the
        // kernel maps them or leaves the break where it was; established
        // here.
        match unsafe { sys::brk(end as u64) } {
            Ok(got) if got >= end => {}
            _ => return ptr::null_mut(),
        }
        let base = self.top;
        self.top = end;
        if step > len {
            self.free_pages((base + len) as *mut u8, step - len);
        }
        base as *mut u8
    }

    /// Return `len` bytes (a page multiple) at page-aligned `p` to the free
    /// runs, merging with the runs on either side.
    fn free_pages(&mut self, p: *mut u8, len: usize) {
        let at = p as usize;
        let mut prev: Option<NonNull<FreeRun>> = None;
        let mut cur = self.runs;
        while let Some(r) = cur {
            if r.as_ptr() as usize > at {
                break;
            }
            prev = cur;
            // SAFETY: a run on the free list; established here.
            cur = unsafe { r.as_ref().next };
        }
        let mut len = len;
        let mut next = cur;
        if let Some(n) = next
            && at + len == n.as_ptr() as usize
        {
            // SAFETY: `n` is the free run just above; established here.
            let (nlen, nnext) = unsafe { (n.as_ref().len, n.as_ref().next) };
            len += nlen;
            next = nnext;
        }
        if let Some(mut pr) = prev {
            // SAFETY: `pr` is the free run just below; established here.
            let pr_ref = unsafe { pr.as_mut() };
            if pr.as_ptr() as usize + pr_ref.len == at {
                pr_ref.len += len;
                pr_ref.next = next;
                return;
            }
        }
        let run = p.cast::<FreeRun>();
        // SAFETY: `p` is `len` bytes of page-aligned memory the caller gave
        // back; established here by the `GlobalAlloc::dealloc` contract.
        unsafe { run.write(FreeRun { len, next }) };
        self.set_next(prev, NonNull::new(run));
    }
}

// SAFETY: `alloc` returns null or memory of at least `layout.size()` bytes
// aligned to `layout.align()` that no other allocation overlaps, and
// `dealloc` takes back only what `alloc` gave for the same layout, by the
// size class or page count that layout names; established here.
unsafe impl GlobalAlloc for BrkHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.align() > MAX_ALIGN {
            return ptr::null_mut();
        }
        // SAFETY: no other call reaches `state` while this one runs (the
        // `Sync` impl's invariant); established here.
        let st = unsafe { &mut *self.state.get() };
        match class_of(layout.size(), layout.align()) {
            Some(i) => st.alloc_class(i),
            None => match page_round(layout.size().max(1)) {
                Some(len) => st.alloc_pages(len),
                None => ptr::null_mut(),
            },
        }
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        // SAFETY: as in `alloc`; established here.
        let st = unsafe { &mut *self.state.get() };
        match class_of(layout.size(), layout.align()) {
            Some(i) => st.free_class(p, i),
            None => {
                if let Some(len) = page_round(layout.size().max(1)) {
                    st.free_pages(p, len);
                }
            }
        }
    }
}

/// The heap every program uses.
#[global_allocator]
static HEAP: BrkHeap = BrkHeap::new();
