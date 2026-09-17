//! vibeOS kernel entry.
//!
//! Phase 0 brought serial, panic, and the Limine handshake. Phase 1 slice
//! A adds the physical memory manager: walk Limine's memory map, exclude
//! frame 0 / the kernel image / the AP trampoline / the framebuffer,
//! hand the rest to the buddy allocator, and print the phase-1 exit
//! marker. Paging, heap, and KVA come in later slices.

#![no_std]
#![no_main]

mod paging_init;
mod panic;
mod pmm_init;
mod serial;
mod x86;

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
    // minimum machinery it needs to be diagnostic.
    #[cfg(feature = "panic-test")]
    {
        serial::line("vibeOS: boot: panic-test armed");
        panic!("intentional panic-test trip");
    }

    // ---- Phase 1 slice A: physical memory manager. ----
    let hhdm = HHDM
        .response()
        .unwrap_or_else(|| halt_with("vibeOS: limine: hhdm missing"));
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
    // Build a fresh PML4 from buddy frames, install it, then run the
    // MMIO PTE-attribute step. Emits `paging: cr3 ok` and
    // `paging: mmio uc` (DESIGN §3.3 steps 7 and 8). Kept inside the
    // `not(panic-test)` cfg so the panic-test build still trips its
    // panic right after the limine handshake without dragging the whole
    // paging bringup along.
    #[cfg(not(feature = "panic-test"))]
    {
        unsafe {
            paging_init::init(memmap.entries(), hhdm.offset, exec.physical_base);
        }
        serial::line(marker::BOOT_DONE);
        x86::halt();
    }

    #[cfg(feature = "panic-test")]
    x86::halt();
}

/// Halt with a serial line. Used when a Limine response we depend on is
/// missing; nothing after this point would work without it.
fn halt_with(msg: &str) -> ! {
    serial::line(msg);
    x86::halt();
}
