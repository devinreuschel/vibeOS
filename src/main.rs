//! vibeOS kernel entry.
//!
//! Boot order is DESIGN §3.3: serial, Limine, PMM, paging, heap, KVA,
//! then a shell-less meminfo dump. The `kernel_tests` build runs the
//! in-guest registry after that and exits through isa-debug-exit.

#![no_std]
#![no_main]
#![feature(alloc_error_handler)]
// The panic-test build gates the entire non-panic tail behind
// `#[cfg(not(feature = "panic-test"))]`, which leaves the Limine
// requests, paging init, and helpers technically dead. That is
// deliberate — silence the noise so a real warning is not lost.
#![cfg_attr(feature = "panic-test", allow(dead_code, unused_imports))]

extern crate alloc;

mod diag;
mod heap_init;
mod kva_init;
mod paging_init;
mod panic;
mod pmm_init;
mod serial;
mod x86;

#[cfg(feature = "kernel_tests")]
mod ktest;

use core::fmt::Write;

use limine::request::{
    ExecutableAddressRequest, FramebufferRequest, HhdmRequest, MemmapRequest, RsdpRequest,
};
use limine::{BaseRevision, RequestsEndMarker, RequestsStartMarker};

use vibeos::marker;

// The linker groups these three into `.limine_requests` (see linker.ld).
// Limine walks between the start and end markers to find our requests.

#[used]
#[unsafe(link_section = ".limine_requests_start")]
static REQ_START: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static BASE_REV: BaseRevision = BaseRevision::with_revision(3);

#[used]
#[unsafe(link_section = ".limine_requests")]
static HHDM: HhdmRequest = HhdmRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static MEMMAP: MemmapRequest = MemmapRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static RSDP: RsdpRequest = RsdpRequest::new();

// Executable address: physical + virtual base of the loaded kernel image.
// The PMM subtracts this from the free lists so we do not hand our own
// code and data back out as regular RAM.
#[used]
#[unsafe(link_section = ".limine_requests")]
static EXEC_ADDR: ExecutableAddressRequest = ExecutableAddressRequest::new();

// Framebuffer: same reasoning, plus Limine's memmap already marks the
// framebuffer non-USABLE on most firmwares, but DESIGN §4.2 asks for
// an explicit exclude so a stray USABLE entry from a quirky BIOS cannot
// hand us the scanout region.
#[used]
#[unsafe(link_section = ".limine_requests")]
pub(crate) static FRAMEBUFFER: FramebufferRequest = FramebufferRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests_end")]
static REQ_END: RequestsEndMarker = RequestsEndMarker::new();

#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    // Step 1: serial. Nothing before this is debuggable.
    serial::Serial::init();
    serial::line(marker::SERIAL_ONLINE);

    // Step 2: base revision. DESIGN §3.3 puts this immediately after serial.
    // Missing / older Limine responds by not clearing the request, and
    // `is_supported()` returns false.
    if !BASE_REV.is_supported() {
        serial::line("vibeOS: limine: base revision unsupported");
        x86::halt();
    }
    serial::line(marker::LIMINE_OK);

    // Optional smoke checks: log presence of the responses we asked for so
    // future phases have signal when a Limine upgrade drops a response
    // silently. Not part of the marker contract.
    if RSDP.response().is_some() {
        serial::line("vibeOS: limine: rsdp present");
    }

    // With `--features panic-test`, prove the panic path end to end.
    // Kept before PMM init so the panic path still exercises only the
    // minimum machinery it needs to be diagnostic. Guarding both this
    // branch and the "normal path" tail avoids `unreachable_code`
    // warnings in the panic-test build.
    #[cfg(feature = "panic-test")]
    {
        serial::line("vibeOS: boot: panic-test armed");
        panic!("intentional panic-test trip");
    }

    #[cfg(not(feature = "panic-test"))]
    {
        normal_boot_tail();
        x86::halt();
    }
}

/// The non-panic-test tail of `_start`. Kept as a fn so a `#[cfg]` on
/// the call site silences `unreachable_code` in panic-test builds
/// without duplicating markers.
#[cfg(not(feature = "panic-test"))]
fn normal_boot_tail() {
    // ---- Phase 1 slice A: physical memory manager. ----
    let hhdm = HHDM
        .response()
        .unwrap_or_else(|| halt_with("vibeOS: limine: hhdm missing"));
    // Slice B pins the physmap VA at `paging_init::HHDM_BASE`. If Limine
    // drifts to a different offset, buddy free-list nodes (reached via
    // `phys + hhdm_offset`) fault the moment we install our own PML4.
    // Fail loud here instead of chasing that later.
    paging_init::assert_limine_hhdm(hhdm.offset);
    let memmap = MEMMAP
        .response()
        .unwrap_or_else(|| halt_with("vibeOS: limine: memmap missing"));
    let exec = EXEC_ADDR
        .response()
        .unwrap_or_else(|| halt_with("vibeOS: limine: executable_address missing"));

    let stats = unsafe {
        pmm_init::init(memmap.entries(), hhdm.offset, exec.physical_base)
    };

    // Exit-gate marker for phase 1 slice A. DESIGN §2.6 marker shape.
    let _ = writeln!(
        serial::Serial,
        "vibeOS: pmm: {} free 4KiB frames",
        stats.free_frames
    );

    // Diagnostic follow-up: totals and largest available order. Not part
    // of the exit-gate contract, but useful when the free count is
    // surprising.
    let largest = match stats.largest_free_order {
        Some(o) => o as i32,
        None => -1,
    };
    let _ = writeln!(
        serial::Serial,
        "vibeOS: pmm: {} total, largest order {}",
        stats.total_frames, largest
    );

    // ---- Phase 1 slice B: page tables + MMIO attributes. ----
    // Feed the physmap extent computation from what we already have:
    // usable-RAM high water from the memmap, plus each framebuffer's
    // `base + size` so scanout lands inside the physmap. DESIGN §4.1
    // caps at 8 GiB regardless.
    let ram_high_water = memmap_high_water(memmap.entries());
    let fb_phys_end = framebuffer_phys_end(hhdm.offset);
    let paging_report = unsafe {
        paging_init::install(exec.physical_base, ram_high_water, fb_phys_end)
    };
    paging_init::report(&paging_report);

    // ---- Phase 1 slice C: heap, KVA, diagnostics. ----
    unsafe { heap_init::init() };
    {
        let probe = alloc::boxed::Box::new(0xC0FFEEu64);
        if *probe != 0xC0FFEE {
            panic!("heap probe mismatch");
        }
    }
    serial::line(marker::HEAP_OK);

    unsafe { kva_init::init() };
    {
        let stack = kva_init::alloc_guarded_stack(4).expect("kva stack probe");
        unsafe {
            (stack.mapped_base().as_u64() as *mut u64).write_volatile(0x5A5A_5A5A_5A5A_5A5A)
        };
        kva_init::free_stack(stack);
    }
    serial::line(marker::KVA_READY);

    diag::meminfo();

    serial::line(marker::BOOT_DONE);

    #[cfg(feature = "kernel_tests")]
    crate::ktest::run();
}

/// Highest end address of any USABLE memmap entry, in physical bytes.
/// Zero when the map has no USABLE entries (unreachable in practice).
#[cfg(not(feature = "panic-test"))]
fn memmap_high_water(entries: &[&limine::memmap::Entry]) -> u64 {
    let mut hi = 0u64;
    for e in entries {
        if e.type_ == limine::memmap::MEMMAP_USABLE {
            let end = e.base + e.length;
            if end > hi {
                hi = end;
            }
        }
    }
    hi
}

/// Highest `base + size` across all framebuffers, in physical bytes.
/// Zero when Limine returns no framebuffers.
#[cfg(not(feature = "panic-test"))]
fn framebuffer_phys_end(hhdm_offset: u64) -> u64 {
    let Some(resp) = FRAMEBUFFER.response() else {
        return 0;
    };
    let mut hi = 0u64;
    for fb in resp.framebuffers() {
        let virt = fb.address() as u64;
        if virt == 0 {
            continue;
        }
        let phys = virt.wrapping_sub(hhdm_offset);
        let end = phys + fb.size() as u64;
        if end > hi {
            hi = end;
        }
    }
    hi
}

/// Halt with a serial line. Used when a Limine response we depend on is
/// missing; nothing after this point would work without it.
#[cfg(not(feature = "panic-test"))]
fn halt_with(msg: &str) -> ! {
    serial::line(msg);
    x86::halt();
}
