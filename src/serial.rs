//! COM1 serial writer. Uses the register constants from `vibeos::uart`.
//!
//! Polled TX with a bounded THRE wait; a dead UART drops the byte rather
//! than wedging the panic handler (DESIGN §9.6). Byte-granularity TX lock
//! so SMP CPUs do not interleave bytes (DESIGN §7.7). Panic/halt skips
//! the lock so a holder cannot stall the dump.

use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::lock::RANK_SERIAL;
use vibeos::uart::*;

use crate::sync_init::SpinMutex;
use crate::x86;

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

    fn write_byte(b: u8) {
        if crate::ipi_init::is_halting() {
            Self::write_byte_raw(b);
            return;
        }
        let _g = TX.lock();
        Self::write_byte_raw(b);
    }

    pub fn write_bytes(bytes: &[u8]) {
        for &b in bytes {
            if b == b'\n' {
                Self::write_byte(b'\r');
            }
            Self::write_byte(b);
        }
    }
}

impl fmt::Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        Self::write_bytes(s.as_bytes());
        Ok(())
    }
}

/// Write a marker line: `<msg>\n`.
pub fn line(msg: &str) {
    Serial::write_bytes(msg.as_bytes());
    Serial::write_bytes(b"\n");
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let _ = write!($crate::serial::Serial, $($arg)*);
    }};
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => {{
        $crate::print!($($arg)*);
        $crate::print!("\n");
    }};
}
