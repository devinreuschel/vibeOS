//! Console mux: serial + FB out, PS/2 + serial RX in. ROADMAP §5.3.
//!
//! Backends never call `log!`. Keyboard ISR does not take this path;
//! input is popped with IRQs off.

use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::console::BackendId;
use vibeos::kbd::DecodedKey;
use vibeos::marker;

use crate::fb_init;
use crate::kbd_init;
use crate::log_init;
use crate::per_cpu_init;
use crate::serial::{self, Serial};
use crate::thread_init;
use crate::x86::InterruptGuard;

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

/// Fan-out. Silent backends. Serial TX lock is dropped before the FB lock
/// (ranks SERIAL then DEVICE are not nested).
pub fn write(bytes: &[u8]) {
    if SERIAL_ON.load(Ordering::Acquire) {
        Serial::write_bytes_plain(bytes);
    }
    if FB_ON.load(Ordering::Acquire) {
        fb_init::write(bytes);
    }
}

/// `fmt::Write` onto the mux. Does not capture into the log ring.
pub struct Console;

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write(s.as_bytes());
        Ok(())
    }
}

/// PS/2 first, then serial RX. Whole consumer critical section is IRQ-off
/// (DESIGN §9.4): `pop` already cli's, and serial RX must too — a nested
/// guard keeps IF off across both so we never poll COM1 with IF=1.
pub fn read() -> Option<DecodedKey> {
    let _irq = InterruptGuard::enter();
    if let Some(k) = kbd_init::pop() {
        return Some(k);
    }
    if SERIAL_ON.load(Ordering::Acquire)
        && let Some(b) = Serial::try_read_byte()
    {
        return Some(DecodedKey::Char(b));
    }
    None
}

/// Block for one key. Drain with IRQs off; never hold the FB/serial lock
/// across the wait. `sti; hlt` is one instruction so a keyboard IRQ
/// cannot slip between enable and halt.
pub fn wait_key() -> DecodedKey {
    loop {
        if let Some(k) = read() {
            return k;
        }
        if !per_cpu_init::current().runq.is_empty() {
            thread_init::yield_now();
            continue;
        }
        unsafe {
            core::arch::asm!("cli", options(nomem, nostack, preserves_flags));
        }
        if let Some(k) = read() {
            unsafe {
                core::arch::asm!("sti", options(nomem, nostack, preserves_flags));
            }
            return k;
        }
        if !per_cpu_init::current().runq.is_empty() {
            unsafe {
                core::arch::asm!("sti", options(nomem, nostack, preserves_flags));
            }
            thread_init::yield_now();
            continue;
        }
        unsafe {
            core::arch::asm!("sti; hlt", options(nomem, nostack));
        }
    }
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
