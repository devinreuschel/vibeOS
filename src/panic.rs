//! Panic handler. Re-inits serial from scratch (the panic may have come from
//! inside the serial path), prints the location and message, then halts.
//! No unwinding: `panic = "abort"` in both profiles.

use core::fmt::Write;
use core::panic::PanicInfo;

use crate::serial::Serial;
use crate::x86;
use vibeos::marker;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    Serial::init();

    // Banner first so a grep for `vibeOS: panic:` finds every panic even
    // if the follow-up printing itself faults.
    Serial::write_bytes(marker::PANIC_BANNER.as_bytes());
    Serial::write_bytes(b"\n");

    if let Some(loc) = info.location() {
        let _ = writeln!(
            Serial,
            "vibeOS: panic: at {}:{}:{}",
            loc.file(),
            loc.line(),
            loc.column()
        );
    } else {
        Serial::write_bytes(b"vibeOS: panic: at <unknown>\n");
    }

    // `PanicInfo::message()` is stable and returns a `fmt::Arguments`.
    let _ = writeln!(Serial, "vibeOS: panic: msg: {}", info.message());

    Serial::write_bytes(b"vibeOS: panic: halted\n");

    x86::halt();
}
