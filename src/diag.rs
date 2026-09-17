//! Boot-time memory diagnostics. DESIGN §1.7 / ROADMAP §1.7.
//!
//! Shell-less `meminfo` on the boot log: PMM totals, heap used/capacity,
//! KVA used, plus a coalesced page-table range dump.

use core::fmt::Write;

use crate::heap_init;
use crate::kva_init;
use crate::paging_init;
use crate::pmm_init;
use crate::serial::Serial;

pub fn meminfo() {
    let pmm = unsafe { pmm_init::with_buddy(|b| b.stats()) };
    let heap = heap_init::stats();
    let kva = kva_init::stats();
    let used_frames = pmm.total_frames.saturating_sub(pmm.free_frames);
    let largest = match pmm.largest_free_order {
        Some(o) => o as i32,
        None => -1,
    };
    let _ = writeln!(
        Serial,
        "vibeOS: meminfo: total {} frames, free {}, used {}, largest order {}",
        pmm.total_frames, pmm.free_frames, used_frames, largest
    );
    let _ = writeln!(
        Serial,
        "vibeOS: meminfo: heap used {} B / capacity {} B",
        heap.used, heap.capacity
    );
    let _ = writeln!(Serial, "vibeOS: meminfo: kva used {} B", kva.used);
    paging_init::dump_ranges();
}
