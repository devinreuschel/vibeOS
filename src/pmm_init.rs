//! Kernel-side wiring: turn [`BootInfo`]'s usable RAM into a running
//! Buddy allocator. Portable buddy logic lives in `vibeos::pmm`; this
//! module is the binary-crate half.
//!
//! DESIGN §4.2 lists the regions that must not enter the free lists:
//!   - physical frame 0
//!   - the loaded kernel image span
//!   - the AP trampoline page at 0x8000
//!   - the framebuffer
//!   - anything not marked `USABLE`, including bootloader- and
//!     ACPI-reclaimable
//!   - anything above the 8 GiB physmap cap
//!
//! Frame 0 is dropped inside `Buddy::insert_region`. Kernel, trampoline,
//! and framebuffer are sorted into an "excludes" list and subtracted from
//! every USABLE range before it is inserted.

use vibeos::lock::RANK_BUDDY;
use vibeos::pmm::{Buddy, PAGE_SIZE, PmmStats};

use crate::boot::BootInfo;
use crate::paging_init::{HHDM_BASE, PHYSMAP_CAP};
use crate::sync_init::SpinMutex;

/// AP trampoline page. DESIGN §7.3 fixes the SIPI vector at 0x08, which
/// means the entry point lives at physical 0x8000.
const AP_TRAMPOLINE_PHYS: u64 = 0x8000;

static BUDDY: SpinMutex<Buddy> = SpinMutex::with_rank(Buddy::new(HHDM_BASE), RANK_BUDDY);

/// Post-init access to the global buddy. IRQ-aware, rank buddy.
pub fn with_buddy<R>(f: impl FnOnce(&mut Buddy) -> R) -> R {
    let mut g = BUDDY.lock();
    f(&mut g)
}

/// One entry in the sorted excludes list, [start, end).
#[derive(Copy, Clone, Debug)]
struct Range {
    start: u64,
    end: u64,
}

/// Fixed-size excludes buffer: kernel image, AP trampoline, one per
/// framebuffer. Frame 0 is handled implicitly by `Buddy::insert_region`.
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

    /// Rounded out to pages so no partial page leaks in.
    fn push(&mut self, start: u64, end: u64) {
        let (start, end) = (align_down(start, PAGE_SIZE), align_up(end, PAGE_SIZE));
        if start >= end {
            return;
        }
        if self.len >= MAX_EXCLUDES {
            // Bump the cap rather than silently dropping.
            crate::marker!("vibeOS: pmm: excludes overflow, dropping {start:#x}..{end:#x}");
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

/// Ingest usable RAM into the global buddy. Returns the resulting stats
/// snapshot. Call once.
///
/// # Safety
/// - Limine's HHDM must still map every USABLE range, so the buddy can
///   write free-list nodes into it at `phys + HHDM_BASE`.
/// - Single CPU, before interrupts are enabled.
pub unsafe fn init(info: &BootInfo) -> PmmStats {
    let mut buddy = BUDDY.lock();

    let mut excl = Excludes::new();
    // AP trampoline. DESIGN §2.4 keeps 0x8000 reserved forever, even
    // after all APs are up.
    excl.push(AP_TRAMPOLINE_PHYS, AP_TRAMPOLINE_PHYS + PAGE_SIZE);
    excl.push(info.kernel_phys.start, info.kernel_phys.end);
    // Limine's memmap already marks framebuffers non-USABLE on most
    // firmware. DESIGN §4.2 excludes them anyway so a quirky BIOS cannot
    // hand us the scanout region.
    for fb in info.framebuffers() {
        excl.push(fb.phys, fb.phys + fb.size);
    }
    excl.sort();

    // Free-list nodes, page tables, and heap pages are all reached through
    // our physmap once cr3 switches, so RAM above its cap stays out.
    for r in info.usable() {
        let end = r.end.min(PHYSMAP_CAP);
        if r.start < end {
            unsafe { insert_clipped(&mut buddy, r.start, end, excl.as_slice()) };
        }
    }

    buddy.stats()
}

/// Feed `[base, end)` into the buddy, skipping every byte covered by any
/// exclude in `sorted`.
///
/// # Safety
/// Same contract as `Buddy::insert_region`: caller vouches for the
/// physical pages being real, writable memory accessible via the HHDM physmap.
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
const fn align_up(x: u64, a: u64) -> u64 {
    (x + a - 1) & !(a - 1)
}
#[inline]
const fn align_down(x: u64, a: u64) -> u64 {
    x & !(a - 1)
}
