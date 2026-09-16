//! vibeOS phase 0 kernel entry.
//!
//! This is intentionally minimal: verify Limine's base revision, bring up
//! COM1, print a fixed set of markers, optionally trip a panic under the
//! `panic-test` feature, then halt. Everything else is later phases.

#![no_std]
#![no_main]

mod panic;
mod serial;
mod x86;

use limine::request::{HhdmRequest, MemmapRequest, RsdpRequest};
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
    // silently. Not part of the phase 0 marker contract.
    if HHDM.response().is_some() {
        serial::line("vibeOS: limine: hhdm present");
    }
    if MEMMAP.response().is_some() {
        serial::line("vibeOS: limine: memmap present");
    }
    if RSDP.response().is_some() {
        serial::line("vibeOS: limine: rsdp present");
    }

    // With `--features panic-test`, prove the panic path end to end.
    #[cfg(feature = "panic-test")]
    {
        serial::line("vibeOS: boot: panic-test armed");
        panic!("intentional panic-test trip");
    }

    #[cfg(not(feature = "panic-test"))]
    {
        serial::line(marker::BOOT_DONE);
        x86::halt();
    }
}
