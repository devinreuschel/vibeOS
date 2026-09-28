//! Boot-time memory diagnostics. ROADMAP §1.7 / §2.8 / §4.11.
//!
//! `meminfo` / `uptime` / `cpus` write to any `fmt::Write` so the boot
//! log and the shell share one implementation.

use core::fmt::Write;
use core::sync::atomic::Ordering;

use crate::apic_init;
use crate::heap_init;
use crate::kva_init;
use crate::paging_init;
use crate::per_cpu_init;
use crate::pmm_init;
use crate::serial::Serial;
use crate::time_init;

pub fn meminfo() {
    meminfo_to(&mut Serial);
}

pub fn meminfo_to(w: &mut impl Write) {
    let pmm = pmm_init::with_buddy(|b| b.stats());
    let heap = heap_init::stats();
    let kva = kva_init::stats();
    let used_frames = pmm.total_frames.saturating_sub(pmm.free_frames);
    let largest = match pmm.largest_free_order {
        Some(o) => o as i32,
        None => -1,
    };
    let _ = writeln!(
        w,
        "vibeOS: meminfo: total {} frames, free {}, used {}, largest order {}",
        pmm.total_frames, pmm.free_frames, used_frames, largest
    );
    let _ = writeln!(
        w,
        "vibeOS: meminfo: leaked {} frames",
        vibeos::pmm::leaked_frames()
    );
    let _ = writeln!(
        w,
        "vibeOS: meminfo: heap used {} B / capacity {} B",
        heap.used, heap.capacity
    );
    let _ = writeln!(w, "vibeOS: meminfo: kva used {} B", kva.used);
    paging_init::dump_ranges_to(w);
}

/// Tick milliseconds and TSC microseconds side by side. ROADMAP §2.8.
pub fn uptime() {
    uptime_to(&mut Serial);
}

pub fn uptime_to(w: &mut impl Write) {
    let tick = time_init::uptime_ms();
    let us = time_init::now_us();
    let _ = writeln!(w, "vibeOS: uptime: tick {tick} ms, tsc {us} us");
}

/// Logical id, APIC id, online mask, timer mode, ticks, ready depth,
/// switches. ROADMAP §4.11.
pub fn cpus() {
    cpus_to(&mut Serial);
}

pub fn cpus_to(w: &mut impl Write) {
    let mask = per_cpu_init::online_mask();
    let n = per_cpu_init::cpu_count();
    let _ = writeln!(w, "vibeOS: cpus: n={n} online={mask:#x}");
    // Every CPU copies the BSP's timer mode at bring-up.
    let timer = apic_init::timer_mode();
    let mut i = 0u32;
    while (i as usize) < n {
        let Some(c) = per_cpu_init::cpu(i) else {
            i += 1;
            continue;
        };
        let _ = writeln!(
            w,
            "vibeOS: cpus: cpu{} apic={} ticks={} switches={} ready={} timer={}",
            i,
            c.apic_id.load(Ordering::Relaxed),
            c.ticks.load(Ordering::Relaxed),
            c.switches.load(Ordering::Relaxed),
            c.runq_len.load(Ordering::Relaxed),
            timer.as_str()
        );
        i += 1;
    }
}
