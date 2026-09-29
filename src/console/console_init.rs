//! Console mux: serial + FB out, PS/2 + serial RX in. ROADMAP §5.3.
//!
//! Backends never call `log!`. Keyboard ISR does not take this path;
//! input is popped with IRQs off.

use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::fb::CHUNK;
use vibeos::kbd::DecodedKey;
use vibeos::marker;

use crate::fb_init;
use crate::kbd_init;
use crate::log_init;
use crate::per_cpu_init;
use crate::serial::Serial;
use crate::thread_init;
use crate::x86::{self, InterruptGuard};

pub(super) static SERIAL_ON: AtomicBool = AtomicBool::new(false);
pub(super) static FB_ON: AtomicBool = AtomicBool::new(false);
pub(super) static LIVE: AtomicBool = AtomicBool::new(false);

/// Fan-out, one [`CHUNK`] at a time: serial under its TX lock, then the
/// framebuffer's grid under the console lock, never nested (ranks SERIAL
/// then DEVICE). Silent backends. IF is on between chunks whenever the
/// caller runs with IF=1 (DESIGN §2.9 rule 2). An empty write only lets
/// the framebuffer redraw.
pub fn write(bytes: &[u8]) {
    let fb = FB_ON.load(Ordering::Acquire);
    if bytes.is_empty() {
        if fb {
            fb_init::write(bytes);
        }
        return;
    }
    for chunk in bytes.chunks(CHUNK) {
        if SERIAL_ON.load(Ordering::Acquire) {
            Serial::write_user(chunk);
        }
        if fb {
            fb_init::write(chunk);
        }
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

/// Block for one key, and return with IF as it was on entry: a syscall
/// body that calls this returns to an exit that must run with IF=0
/// (AGENTS.md rule 2).
pub fn wait_key() -> DecodedKey {
    let if_on = x86::interrupts_enabled();
    let k = wait_key_loop();
    if if_on {
        x86::sti();
    } else {
        x86::cli();
    }
    k
}

/// Drain with IRQs off; never hold the FB/serial lock across the wait.
/// `sti; hlt` is one instruction so a keyboard IRQ cannot slip between
/// enable and halt. May return with IF=1.
fn wait_key_loop() -> DecodedKey {
    loop {
        if let Some(k) = read() {
            return k;
        }
        if !per_cpu_init::current().runq.is_empty() {
            thread_init::yield_now();
            continue;
        }
        // SAFETY: `cli` only changes IF, which this wait loop owns: it holds no
        // lock and no `InterruptGuard` here; established here.
        unsafe {
            core::arch::asm!("cli", options(nostack, preserves_flags));
        }
        if let Some(k) = read() {
            // SAFETY: `sti` only changes IF, which this wait loop owns: it holds no
            // lock and no `InterruptGuard` here; established here.
            unsafe {
                core::arch::asm!("sti", options(nostack, preserves_flags));
            }
            return k;
        }
        if !per_cpu_init::current().runq.is_empty() {
            // SAFETY: `sti` only changes IF, which this wait loop owns: it holds no
            // lock and no `InterruptGuard` here; established here.
            unsafe {
                core::arch::asm!("sti", options(nostack, preserves_flags));
            }
            thread_init::yield_now();
            continue;
        }
        #[cfg(feature = "kernel_tests")]
        testing::HALTS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `sti; hlt` only enables interrupts and halts until one
        // arrives, and `sti`'s one-instruction shadow keeps a wake-up IRQ from
        // landing before the `hlt`; this loop holds no lock; established here.
        unsafe {
            core::arch::asm!("sti; hlt", options(nomem, nostack));
        }
    }
}

/// In-guest test hooks. `kernel_tests` only (AGENTS.md rule 9).
#[cfg(feature = "kernel_tests")]
pub(crate) mod testing {
    use core::sync::atomic::{AtomicU64, Ordering};

    pub(super) static HALTS: AtomicU64 = AtomicU64::new(0);

    /// Calls of `wait_key` that reached its `sti; hlt`, since the last
    /// [`reset_halts`].
    pub(crate) fn halts() -> u64 {
        HALTS.load(Ordering::Relaxed)
    }

    pub(crate) fn reset_halts() {
        HALTS.store(0, Ordering::Relaxed);
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
    crate::marker!(marker::CONSOLE_OK);
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
