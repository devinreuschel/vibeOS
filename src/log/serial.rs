//! COM1 serial writer. Uses the register constants from `vibeos::uart`.
//!
//! Polled TX with a bounded THRE wait; a dead UART drops the byte rather
//! than wedging the panic handler (DESIGN §9.6). Byte-granularity TX lock
//! so SMP CPUs do not interleave bytes (DESIGN §7.7). Panic/halt skips
//! the lock so a holder cannot stall the dump. `log!` uses try-lock + drop.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::lock::RANK_SERIAL;
use vibeos::uart::*;

use crate::sync_init::SpinMutex;
use crate::x86::{self, InterruptGuard};

static INITIALIZED: AtomicBool = AtomicBool::new(false);
static TX: SpinMutex<()> = SpinMutex::with_rank((), RANK_SERIAL);

pub struct Serial;

impl Serial {
    /// Bring COM1 up: DLAB dance, 115200 8N1, FIFO on.
    ///
    /// Safe to call more than once; the panic handler re-runs it since the
    /// panic may itself be *in* the serial path (DESIGN §2.5).
    pub fn init() {
        unsafe {
            x86::outb(COM1_BASE + REG_IER, 0x00); // mask all interrupts
            x86::outb(COM1_BASE + REG_LCR, LCR_DLAB);
            x86::outb(COM1_BASE + REG_DLL, (BAUD_115200_DIVISOR & 0xFF) as u8);
            x86::outb(COM1_BASE + REG_DLM, (BAUD_115200_DIVISOR >> 8) as u8);
            x86::outb(COM1_BASE + REG_LCR, LCR_8N1);
            x86::outb(COM1_BASE + REG_FCR, FCR_ENABLE);
            x86::outb(COM1_BASE + REG_MCR, MCR_READY);
        }
        INITIALIZED.store(true, Ordering::Release);
    }

    fn write_byte_raw(b: u8) {
        // Bounded THRE poll; drop on cap rather than spin forever.
        let mut spin = TX_POLL_CAP;
        while spin > 0 {
            let lsr = unsafe { x86::inb(COM1_BASE + REG_LSR) };
            if lsr & LSR_THRE != 0 {
                unsafe { x86::outb(COM1_BASE + REG_DATA, b) };
                return;
            }
            spin -= 1;
        }
    }

    fn write_bytes_raw(bytes: &[u8]) {
        for &b in bytes {
            if b == b'\n' {
                Self::write_byte_raw(b'\r');
            }
            Self::write_byte_raw(b);
        }
    }

    pub fn write_bytes(bytes: &[u8]) {
        let _irq = InterruptGuard::enter();
        if !crate::ipi_init::is_halting() && !crate::log_init::is_emitting() {
            crate::log_init::capture_serial(bytes);
        }
        Self::write_bytes_plain(bytes);
    }

    /// TX without ring capture. `dmesg` uses this so a dump cannot wrap
    /// the ring in copies of itself.
    pub fn write_bytes_plain(bytes: &[u8]) {
        if crate::ipi_init::is_halting() {
            Self::write_bytes_raw(bytes);
            return;
        }
        let _g = TX.lock();
        Self::write_bytes_raw(bytes);
    }

    /// Poll COM1 RX. No lock; caller holds IRQs off if racing a consumer.
    pub fn try_read_byte() -> Option<u8> {
        let lsr = unsafe { x86::inb(COM1_BASE + REG_LSR) };
        if lsr & LSR_DR == 0 {
            return None;
        }
        Some(unsafe { x86::inb(COM1_BASE + REG_DATA) })
    }

    /// ISR / log sink: one lock for the whole buffer, drop the line if busy.
    pub fn try_write_bytes(bytes: &[u8]) -> bool {
        if crate::ipi_init::is_halting() {
            Self::write_bytes_raw(bytes);
            return true;
        }
        let Some(_g) = TX.try_lock() else {
            return false;
        };
        Self::write_bytes_raw(bytes);
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
        let _ = writeln!($crate::serial::Serial, $fmt $(, $($arg)*)?);
    }};
    ($msg:expr) => {
        $crate::serial::line($msg)
    };
}
