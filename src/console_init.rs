//! Console mux: serial + FB out, PS/2 + serial RX in. ROADMAP §5.3.
//!
//! Backends never call `log!`. Keyboard ISR does not take this path;
//! input is popped with IRQs off.

use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::console::BackendId;
use vibeos::kbd::DecodedKey;
use vibeos::marker;

use crate::fb_init;
use crate::kbd_init;
use crate::log_init;
use crate::serial::{self, Serial};

static SERIAL_ON: AtomicBool = AtomicBool::new(false);
static FB_ON: AtomicBool = AtomicBool::new(false);
static LIVE: AtomicBool = AtomicBool::new(false);

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn live() -> bool {
    LIVE.load(Ordering::Acquire)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn set_enabled(id: BackendId, on: bool) {
    match id {
        BackendId::Serial => SERIAL_ON.store(on, Ordering::Release),
        BackendId::Framebuffer => {
            if on && !fb_init::ready() {
                return;
            }
            FB_ON.store(on, Ordering::Release);
        }
    }
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn enabled(id: BackendId) -> bool {
    match id {
        BackendId::Serial => SERIAL_ON.load(Ordering::Acquire),
        BackendId::Framebuffer => FB_ON.load(Ordering::Acquire),
    }
}

/// Fan-out. Silent backends.
#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn write(bytes: &[u8]) {
    if SERIAL_ON.load(Ordering::Acquire) {
        Serial::write_bytes_plain(bytes);
    }
    if FB_ON.load(Ordering::Acquire) {
        fb_init::write(bytes);
    }
}

/// PS/2 first, then serial RX. IRQs off in the keyboard pop.
#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn read() -> Option<DecodedKey> {
    if let Some(k) = kbd_init::pop() {
        return Some(k);
    }
    if SERIAL_ON.load(Ordering::Acquire) {
        if let Some(b) = Serial::try_read_byte() {
            return Some(DecodedKey::Char(b));
        }
    }
    None
}

/// FB text, replay the pre-FB ring, PS/2, then `console ok`.
pub fn init() {
    let fb = fb_init::init();
    SERIAL_ON.store(true, Ordering::Release);
    FB_ON.store(fb, Ordering::Release);
    if fb {
        replay_log();
    }
    let _kbd = kbd_init::init();
    LIVE.store(true, Ordering::Release);
    serial::line(marker::CONSOLE_OK);
    if fb {
        fb_init::write(marker::CONSOLE_OK.as_bytes());
        fb_init::write(b"\n");
    }
}

fn replay_log() {
    log_init::for_each_msg(|msg| {
        fb_init::write(msg);
        fb_init::write(b"\n");
    });
}
