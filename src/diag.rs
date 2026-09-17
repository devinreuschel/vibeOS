//! Boot-time memory diagnostics. DESIGN §1.7 / ROADMAP §1.7 / §2.8 / §4.11.
//!
//! Shell-less `meminfo` on the boot log: PMM totals, heap used/capacity,
//! KVA used, plus a coalesced page-table range dump. `uptime` prints
//! tick milliseconds next to TSC microseconds so divergence is visible.
//! `cpus` dumps per-CPU identity and runq depth.

use core::fmt::Write;

use crate::heap_init;
use crate::kva_init;
use crate::paging_init;
use crate::per_cpu_init;
use crate::pmm_init;
use crate::serial::Serial;
use crate::time_init;

pub fn meminfo() {
    let pmm = pmm_init::with_buddy(|b| b.stats());
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

/// Tick milliseconds and TSC microseconds side by side. ROADMAP §2.8.
pub fn uptime() {
    let tick = time_init::uptime_ms();
    let us = time_init::now_us();
    let _ = writeln!(
        Serial,
        "vibeOS: uptime: tick {tick} ms, tsc {us} us"
    );
}

/// Logical id, APIC id, online mask, timer mode, ticks, ready depth,
/// switches. ROADMAP §4.11.
pub fn cpus() {
    let mask = per_cpu_init::online_mask();
    let n = per_cpu_init::cpu_count();
    let _ = writeln!(Serial, "vibeOS: cpus: n={n} online={mask:#x}");
    let mut i = 0u32;
    while (i as usize) < n {
        let Some(c) = per_cpu_init::cpu(i) else {
            i += 1;
            continue;
        };
        let _ = writeln!(
            Serial,
            "vibeOS: cpus: cpu{} apic={} ticks={} switches={} ready={} timer={}",
            c.cpu_id,
            c.apic_id,
            c.ticks,
            c.switches,
            c.runq.len(),
            c.timer_mode.as_str()
        );
        i += 1;
    }
}
