//! COM1 serial writer. Uses the register constants from `vibeos::uart`.
//!
//! Polled TX with a bounded THRE wait; a dead UART drops the byte rather
//! than wedging the panic handler (DESIGN §9.6). Byte-granularity TX lock
//! so SMP CPUs do not interleave bytes (DESIGN §7.7). Once `raw::HALTING`
//! is set the writes skip the lock so a holder cannot stall the dump.
//! `log!` uses try-lock + drop. The port I/O, the halt flag and the dump
//! owner live in [`raw`], which takes no lock and calls nothing above arch.

pub mod raw;

use core::fmt::{self, Write};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use vibeos::lock::RANK_SERIAL;

use crate::sync_init::SpinMutex;
use crate::x86::InterruptGuard;

static INITIALIZED: AtomicBool = AtomicBool::new(false);
static TX: SpinMutex<()> = SpinMutex::with_rank((), RANK_SERIAL);

/// The log ring's serial capture, set by the log module's `init` before the first
/// marker (DESIGN §1.2). Unset, nothing is captured.
static CAPTURE: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Install the log capture `Serial::write_bytes` calls while `HALTING` is
/// clear.
pub fn set_capture_hook(f: fn(&[u8])) {
    // Release: pairs with the Acquire load in `capture`.
    CAPTURE.store(f as *mut (), Ordering::Release);
}

fn capture(bytes: &[u8]) {
    // Acquire: pairs with the Release store in `set_capture_hook`.
    let p = CAPTURE.load(Ordering::Acquire);
    if p.is_null() {
        return;
    }
    // SAFETY: invariant: a non-null `CAPTURE` holds a `fn(&[u8])`;
    // established by `serial::set_capture_hook`, its only store.
    let f = unsafe { core::mem::transmute::<*mut (), fn(&[u8])>(p) };
    f(bytes);
}

fn halting() -> bool {
    raw::HALTING.load(Ordering::Acquire)
}

pub struct Serial;

impl Serial {
    /// Bring COM1 up: DLAB dance, 115200 8N1, FIFO on.
    ///
    /// Safe to call more than once; the panic handler re-runs it since the
    /// panic may itself be *in* the serial path (DESIGN §2.5).
    pub fn init() {
        raw::init();
        INITIALIZED.store(true, Ordering::Release);
    }

    pub fn write_bytes(bytes: &[u8]) {
        let _irq = InterruptGuard::enter();
        if !halting() {
            capture(bytes);
        }
        Self::write_bytes_plain(bytes);
    }

    /// TX without ring capture. `dmesg` uses this so a dump cannot wrap
    /// the ring in copies of itself.
    pub fn write_bytes_plain(bytes: &[u8]) {
        if halting() {
            // IF=0 for the CPU-index read in `write_after_halt`.
            let _irq = InterruptGuard::enter();
            raw::write_after_halt(bytes);
            return;
        }
        let _g = TX.lock();
        raw::write_bytes(bytes);
    }

    /// Poll COM1 RX. No lock; caller holds IRQs off if racing a consumer.
    pub fn try_read_byte() -> Option<u8> {
        raw::try_read_byte()
    }

    /// ISR / log sink: one lock for the whole buffer, drop the line if busy.
    pub fn try_write_bytes(bytes: &[u8]) -> bool {
        if halting() {
            let _irq = InterruptGuard::enter();
            raw::write_after_halt(bytes);
            return true;
        }
        let Some(_g) = TX.try_lock() else {
            return false;
        };
        raw::write_bytes(bytes);
        true
    }
}

impl fmt::Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        Self::write_bytes(s.as_bytes());
        Ok(())
    }

    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> fmt::Result {
        // One IRQ-off region for the whole formatted write so per-CPU
        // capture stage cannot mix with a preempting thread (or ISR).
        let _irq = InterruptGuard::enter();
        fmt::write(self, args)
    }
}

/// Serial TX that does not land in the log ring.
pub struct PlainSerial;

impl fmt::Write for PlainSerial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        Serial::write_bytes_plain(s.as_bytes());
        Ok(())
    }

    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> fmt::Result {
        let _irq = InterruptGuard::enter();
        fmt::write(self, args)
    }
}

/// Write a marker line: `<msg>\n`. Captured into the log ring.
/// Prefer `marker!` at call sites (DESIGN §2.6).
pub fn line(msg: &str) {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "a write to Serial cannot fail (DESIGN §2.5)"
    )]
    let _ = writeln!(Serial, "{msg}");
}

/// Contract serial line. Never filtered; always captured into the log ring.
///
/// `marker!(marker::X)` / `marker!("vibeOS: …")` for a full line;
/// `marker!("vibeOS: … {}", x)` for formatted contract lines.
/// `klog!` is filtered. `PlainSerial` is only for `dmesg` and panic dumps.
#[macro_export]
macro_rules! marker {
    ($fmt:literal $(, $($arg:tt)*)?) => {{
        use core::fmt::Write;
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a write to Serial cannot fail (DESIGN §2.5)"
        )]
        let _ = writeln!($crate::serial::Serial, $fmt $(, $($arg)*)?);
    }};
    ($msg:expr) => {
        $crate::serial::line($msg)
    };
}
