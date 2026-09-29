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
    #[cfg(feature = "kernel_tests")]
    testing::record(bytes);
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
    use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};

    pub(super) static HALTS: AtomicU64 = AtomicU64::new(0);

    /// How many bytes of [`write`](super::write) a capture keeps.
    pub(crate) const CAPTURE_CAP: usize = 256;

    static CAPTURING: AtomicBool = AtomicBool::new(false);
    /// Bytes recorded since [`start_capture`], past `CAPTURE_CAP` included.
    static CAPTURED: AtomicUsize = AtomicUsize::new(0);
    static BYTES: [AtomicU8; CAPTURE_CAP] = [const { AtomicU8::new(0) }; CAPTURE_CAP];

    /// Keep `bytes`, the console output of a process's fd 1 or fd 2 or of
    /// the kernel, while a capture is on.
    pub(super) fn record(bytes: &[u8]) {
        if !CAPTURING.load(Ordering::SeqCst) {
            return;
        }
        for &b in bytes {
            let i = CAPTURED.fetch_add(1, Ordering::SeqCst);
            let Some(slot) = BYTES.get(i) else {
                return;
            };
            slot.store(b, Ordering::SeqCst);
        }
    }

    /// Start keeping what [`write`](super::write) sends, from empty.
    pub(crate) fn start_capture() {
        CAPTURING.store(false, Ordering::SeqCst);
        for b in &BYTES {
            b.store(0, Ordering::SeqCst);
        }
        CAPTURED.store(0, Ordering::SeqCst);
        CAPTURING.store(true, Ordering::SeqCst);
    }

    pub(crate) fn stop_capture() {
        CAPTURING.store(false, Ordering::SeqCst);
    }

    /// Copy the bytes kept so far into `out`; how many.
    pub(crate) fn captured(out: &mut [u8]) -> usize {
        let n = CAPTURED
            .load(Ordering::SeqCst)
            .min(CAPTURE_CAP)
            .min(out.len());
        for (o, b) in out.iter_mut().zip(BYTES.iter()).take(n) {
            *o = b.load(Ordering::SeqCst);
        }
        n
    }

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
