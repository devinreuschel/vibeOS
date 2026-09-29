//! Test-only hooks over console's kernel half (kernel_tests only, Q2): the
//! backend switches, the 8042's config and injection commands, and pixel
//! and cursor reads, which no production path needs.

use core::sync::atomic::Ordering;

use vibeos::console::BackendId;
use vibeos::kbd::{CMD_READ_CFG, CMD_WRITE_KBD_OUT, DecodedKey};

use crate::console::{console_init, fb_init, kbd_init};
use crate::x86::InterruptGuard;

// --- console_init

/// `console_init::init` has run.
pub(crate) fn live() -> bool {
    console_init::LIVE.load(Ordering::Acquire)
}

/// Turn backend `id` on or off; the framebuffer only turns on once it is
/// ready.
pub(crate) fn set_enabled(id: BackendId, on: bool) {
    match id {
        BackendId::Serial => console_init::SERIAL_ON.store(on, Ordering::Release),
        BackendId::Framebuffer => {
            if on && !fb_init::ready() {
                return;
            }
            console_init::FB_ON.store(on, Ordering::Release);
        }
    }
}

/// Backend `id` is on.
pub(crate) fn enabled(id: BackendId) -> bool {
    match id {
        BackendId::Serial => console_init::SERIAL_ON.load(Ordering::Acquire),
        BackendId::Framebuffer => console_init::FB_ON.load(Ordering::Acquire),
    }
}

// --- kbd_init

/// The keyboard is initialized and routed.
pub(crate) fn kbd_live() -> bool {
    kbd_init::LIVE.load(Ordering::Acquire)
}

/// The keyboard's GSI, when routed through the I/O APIC.
pub(crate) fn gsi() -> Option<u32> {
    let g = kbd_init::GSI.load(Ordering::Acquire);
    if g == kbd_init::GSI_NONE {
        None
    } else {
        Some(g)
    }
}

/// The keyboard fell back to the 8259's IRQ1.
pub(crate) fn pic_fallback() -> bool {
    kbd_init::PIC_FALLBACK.load(Ordering::Acquire)
}

/// Queue `k` as if the keyboard had sent it.
pub(crate) fn push_for_test(k: DecodedKey) {
    kbd_init::with_kbd(|kbd| kbd.ring.push(k));
}

/// Read the 8042 config byte. IF off, so the IRQ1 handler cannot steal it.
pub(crate) fn read_cfg() -> Option<u8> {
    let _irq = InterruptGuard::enter();
    kbd_init::flush_obf();
    if !kbd_init::write_cmd(CMD_READ_CFG) {
        return None;
    }
    kbd_init::read_data()
}

/// Present `sc` as a keyboard byte (cmd 0xD2). IRQ1 runs after this
/// returns if INT1 is armed and the GSI is unmasked. Not the device clock:
/// that is `cfg_clock1_on` / QEMU `sendkey`.
pub(crate) fn inject_scancode(sc: u8) -> bool {
    let _irq = InterruptGuard::enter();
    kbd_init::flush_obf();
    kbd_init::write_cmd(CMD_WRITE_KBD_OUT) && kbd_init::write_data(sc)
}

// --- fb_init

/// Write `color` at `(x, y)`; false with no framebuffer or off its edge.
pub(crate) fn put_pixel(x: u32, y: u32, color: u32) -> bool {
    let c = fb_init::CONSOLE.lock();
    let Some(fb) = c.fb.as_ref() else {
        return false;
    };
    if fb.pixel_ptr(x, y).is_none() {
        return false;
    }
    fb.put_pixel(x, y, color);
    true
}

/// The pixel at `(x, y)`.
pub(crate) fn get_pixel(x: u32, y: u32) -> Option<u32> {
    let c = fb_init::CONSOLE.lock();
    c.fb.as_ref()?.get_pixel(x, y)
}

/// The framebuffer's pitch in bytes.
pub(crate) fn pitch() -> Option<u64> {
    let c = fb_init::CONSOLE.lock();
    c.fb.as_ref().map(|f| f.pitch)
}

/// The framebuffer's width in pixels.
pub(crate) fn width() -> Option<u32> {
    let c = fb_init::CONSOLE.lock();
    c.fb.as_ref().map(|f| f.width)
}

/// The text cursor's `(col, row)`, with a framebuffer.
pub(crate) fn cursor() -> Option<(u32, u32)> {
    let c = fb_init::CONSOLE.lock();
    c.fb.as_ref().map(|_| c.grid.cursor())
}
