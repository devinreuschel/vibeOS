//! Kernel-side wiring: turn Limine's memory map into a running Buddy
//! allocator. Portable buddy logic lives in `vibeos::pmm`; this module is
//! the binary-crate half that pokes hardware/limine.
//!
//! DESIGN §4.2 lists the regions that must not enter the free lists:
//!   - physical frame 0
//!   - the loaded kernel image span
//!   - the AP trampoline page at 0x8000
//!   - the framebuffer
//!   - anything not marked `USABLE`, including bootloader- and
//!     ACPI-reclaimable
//!
//! Frame 0 is dropped inside `Buddy::insert_region`. The other three are
//! sorted into an "excludes" list and subtracted from every USABLE range
//! before it is inserted.

use core::cell::UnsafeCell;
use core::fmt::Write;

use limine::memmap::{Entry, MEMMAP_USABLE};

use vibeos::pmm::{Buddy, PAGE_SIZE, PmmStats};

use crate::serial::Serial;

/// AP trampoline page. DESIGN §7.3 fixes the SIPI vector at 0x08, which
/// means the entry point lives at physical 0x8000.
const AP_TRAMPOLINE_PHYS: u64 = 0x8000;

/// Small pre-boot cell: single-CPU, pre-interrupts, so an UnsafeCell with
/// a Sync claim is fine. Phase 4 replaces this with a real IRQ-aware
/// mutex once concurrency arrives.
struct BootCell<T>(UnsafeCell<T>);
unsafe impl<T> Sync for BootCell<T> {}
impl<T> BootCell<T> {
    const fn new(v: T) -> Self {
        Self(UnsafeCell::new(v))
    }
    /// # Safety
    /// Caller must ensure no other reference is live. Only sound during
    /// single-threaded, IRQs-off boot.
    #[allow(clippy::mut_from_ref)]
    unsafe fn get_mut(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
}

static BUDDY: BootCell<Buddy> = BootCell::new(Buddy::new());

/// Post-init access to the global buddy. Paging bring-up pulls table
/// frames through this; later slices add heap-backing and KVA calls.
///
/// # Safety
/// Same rule as [`init`]: single-threaded, interrupts off. Phase 4
/// wraps the underlying storage in a real IRQ-aware mutex.
pub unsafe fn with_buddy<R>(f: impl FnOnce(&mut Buddy) -> R) -> R {
    f(unsafe { BUDDY.get_mut() })
}

// Bounds of the loaded kernel image. Declared in linker.ld (DESIGN §3.4).
// These are virtual addresses; the physical span is derived from Limine's
// executable-address response.
unsafe extern "C" {
    static __kernel_vma_start: u8;
    static __kernel_vma_end: u8;
}

/// One entry in the sorted excludes list, [start, end).
#[derive(Copy, Clone, Debug)]
struct Range {
    start: u64,
    end: u64,
}

/// Fixed-size excludes buffer. Slice A carries three static excludes
/// (kernel image, AP trampoline, framebuffer) plus one slot of slack.
/// Frame 0 is handled implicitly by `Buddy::insert_region`.
const MAX_EXCLUDES: usize = 8;

struct Excludes {
    ranges: [Range; MAX_EXCLUDES],
    len: usize,
}

impl Excludes {
    const fn new() -> Self {
        Self {
            ranges: [Range { start: 0, end: 0 }; MAX_EXCLUDES],
            len: 0,
        }
    }

    fn push(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        if self.len >= MAX_EXCLUDES {
            // Slice A never fills MAX_EXCLUDES. If a future subsystem
            // adds more, bump the cap rather than silently dropping.
            let _ = writeln!(
                Serial,
                "vibeOS: pmm: excludes overflow, dropping {start:#x}..{end:#x}"
            );
            return;
        }
        self.ranges[self.len] = Range { start, end };
        self.len += 1;
    }

    fn sort(&mut self) {
        // Insertion sort by start. len <= MAX_EXCLUDES so O(n^2) is nothing.
        let s = &mut self.ranges[..self.len];
        for i in 1..s.len() {
            let mut j = i;
            while j > 0 && s[j - 1].start > s[j].start {
                s.swap(j - 1, j);
                j -= 1;
            }
        }
    }

    fn as_slice(&self) -> &[Range] {
        &self.ranges[..self.len]
    }
}

/// Ingest the memory map into the global buddy. Returns the resulting
/// stats snapshot. Idempotent-unfriendly: call at most once.
///
/// # Safety
/// - `entries` must be Limine's response, valid for the lifetime of the
///   call.
/// - `hhdm_offset` must be Limine's HHDM offset (so `phys + offset` lands
///   in the higher-half direct map, which Limine set up for us).
/// - Every USABLE range must be real RAM the buddy can safely write
///   free-list nodes into via HHDM.
/// - Must run before interrupts are enabled and before any other CPU is
///   started, since `BUDDY` has no lock.
pub unsafe fn init(
    entries: &[&Entry],
    hhdm_offset: u64,
    kernel_phys_base: u64,
) -> PmmStats {
    let buddy = unsafe { BUDDY.get_mut() };
    buddy.set_hhdm_offset(hhdm_offset);

    // Build the sorted excludes list. Frame 0 is implicit.
    let mut excl = Excludes::new();

    // AP trampoline. DESIGN §2.4 keeps 0x8000 reserved forever, even
    // after all APs are up.
    excl.push(AP_TRAMPOLINE_PHYS, AP_TRAMPOLINE_PHYS + PAGE_SIZE);

    // Kernel image. Size comes from the linker symbols; the physical
    // base is what Limine loaded us at. The range is rounded out to
    // page boundaries so no partial page leaks in.
    let kernel_size = kernel_end_virt().wrapping_sub(kernel_start_virt());
    excl.push(
        align_down(kernel_phys_base, PAGE_SIZE),
        align_up(kernel_phys_base + kernel_size, PAGE_SIZE),
    );

    // Framebuffer, if Limine gave us one. Limine hands back HHDM
    // pointers; subtracting the offset yields the physical base. Every
    // framebuffer's payload is `height * pitch` bytes long.
    if let Some(fb_resp) = crate::FRAMEBUFFER.response() {
        for fb in fb_resp.framebuffers() {
            let virt = fb.address() as u64;
            if virt == 0 {
                continue;
            }
            let phys = virt.wrapping_sub(hhdm_offset);
            let len = fb.size() as u64;
            excl.push(align_down(phys, PAGE_SIZE), align_up(phys + len, PAGE_SIZE));
        }
    }

    excl.sort();

    // Walk USABLE entries and hand each surviving segment to the buddy.
    for entry in entries {
        if entry.type_ != MEMMAP_USABLE {
            continue;
        }
        let base = entry.base;
        let end = base + entry.length;
        unsafe { insert_clipped(buddy, base, end, excl.as_slice()) };
    }

    buddy.stats()
}

/// Feed `[base, end)` into the buddy, skipping every byte covered by any
/// exclude in `sorted`.
///
/// # Safety
/// Same contract as `Buddy::insert_region`: caller vouches for the
/// physical pages being real, writable memory accessible via HHDM.
unsafe fn insert_clipped(buddy: &mut Buddy, base: u64, end: u64, sorted: &[Range]) {
    let mut cur = base;
    for r in sorted {
        if r.end <= cur {
            continue;
        }
        if r.start >= end {
            break;
        }
        let ex_s = r.start.max(cur);
        let ex_e = r.end.min(end);
        if ex_s > cur {
            unsafe { buddy.insert_region(cur, ex_s) };
        }
        cur = ex_e;
        if cur >= end {
            return;
        }
    }
    if cur < end {
        unsafe { buddy.insert_region(cur, end) };
    }
}

#[inline]
fn kernel_start_virt() -> u64 {
    (&raw const __kernel_vma_start) as u64
}
#[inline]
fn kernel_end_virt() -> u64 {
    (&raw const __kernel_vma_end) as u64
}

#[inline]
const fn align_up(x: u64, a: u64) -> u64 {
    (x + a - 1) & !(a - 1)
}
#[inline]
const fn align_down(x: u64, a: u64) -> u64 {
    x & !(a - 1)
}
