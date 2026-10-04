//! COM1 serial writer. Uses the register constants from `vibeos::uart`.
//!
//! One call writes one line, framed (DESIGN §2.6, [`raw::put_line`]). `Serial::write_fmt` formats the whole line,
//! newline included, into a stack buffer of `LINE_CAP` bytes and writes it
//! under one TX hold, so another CPU cannot split it (DESIGN §7.7, ROADMAP
//! §10.2 F138); a longer line is cut and ends in `...`. `write_str` writes
//! its argument as one line, so a caller that builds a line from pieces
//! uses [`write_line_with`]. Polled TX with a bounded THRE wait; a dead
//! UART drops the byte rather than wedging the panic handler (DESIGN
//! §9.6). Once `raw::HALTING` is set a write on any CPU but the dump's
//! owner stops that CPU (`raw::stop_if_halting`), and the owner writes
//! through `raw::write_owner`, with no TX lock and no `InterruptGuard`, so
//! a holder cannot stall the dump. `klog!` uses try-lock + drop. The port I/O, the
//! halt flag and the dump owner live in [`raw`], which takes no lock and
//! calls nothing above arch.

pub mod raw;

use core::fmt::{self, Write};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use vibeos::fmt_util::StackBuf;
use vibeos::lock::RANK_SERIAL;
use vibeos::log::line::LINE_CAP;

use crate::arch::current::InterruptGuard;
use crate::sync_init::SpinMutex;

static INITIALIZED: AtomicBool = AtomicBool::new(false);
static TX: SpinMutex<()> = SpinMutex::with_rank((), RANK_SERIAL);

/// The log ring's serial capture, set by the log module's `init` before the first
/// marker (DESIGN §1.2). Unset, nothing is captured.
static CAPTURE: AtomicPtr<()> = AtomicPtr::new(ptr::null_mut());

/// Install the log capture `Serial::write_line` calls while `HALTING` is
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
    // Acquire: pairs with the Release store in `ipi_init::stop_others`.
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
        // Release: pairs with nothing; nothing reads it.
        INITIALIZED.store(true, Ordering::Release);
    }

    /// Write `content` as one kernel line, captured into the log ring. One
    /// trailing `\n` is the terminator; without one, the line gets one. A
    /// line over `LINE_CAP` bytes is cut and ends in `...`.
    pub fn write_line(content: &[u8]) {
        emit_bytes(content, true);
    }

    /// Console bytes a process wrote: not captured, and not a kernel line,
    /// so unframed, with each frame byte escaped (DESIGN §2.6).
    /// `console_init::write` is the only caller.
    pub fn write_user(bytes: &[u8]) {
        if halting() {
            raw::stop_if_halting();
            raw::put_user(bytes);
            return;
        }
        let _g = TX.lock();
        raw::put_user(bytes);
    }

    /// Poll COM1 RX. No lock; caller holds IRQs off if racing a consumer.
    pub fn try_read_byte() -> Option<u8> {
        raw::try_read_byte()
    }

    /// ISR / log sink: `line`, newline included, as one kernel line under
    /// one try-lock; the line is dropped if TX is busy. Not captured.
    pub fn try_write_bytes(bytes: &[u8]) -> bool {
        if halting() {
            raw::stop_if_halting();
            raw::write_owner(bytes);
            return true;
        }
        let Some(_g) = TX.try_lock() else {
            return false;
        };
        raw::put_line(bytes);
        true
    }
}

/// Write one whole line, `\n` included, under one TX hold, after one
/// capture into the log ring when `capture` is set.
fn emit(line: &[u8], capture: bool) {
    if halting() {
        raw::stop_if_halting();
        raw::write_owner(line);
        return;
    }
    let _irq = InterruptGuard::enter();
    if capture {
        self::capture(line);
    }
    let _g = TX.lock();
    raw::put_line(line);
}

/// `content` as one line: sent as it is when it already ends in its `\n`
/// and fits, otherwise copied, cut to `LINE_CAP`, and given its `\n`.
fn emit_bytes(content: &[u8], capture: bool) {
    if content.ends_with(b"\n") && content.len() <= LINE_CAP + 1 {
        emit(content, capture);
        return;
    }
    emit_built(capture, |w| w.push_bytes(content));
}

/// Build one line in a `LINE_CAP` stack buffer with `f`, then emit it.
fn emit_built(capture: bool, f: impl FnOnce(&mut StackBuf<'_>)) {
    let mut buf = [0u8; LINE_CAP + 1];
    let n = {
        let mut w = StackBuf::new(&mut buf[..LINE_CAP]);
        f(&mut w);
        w.mark_cut();
        let n = w.len();
        if w.as_bytes().ends_with(b"\n") {
            n - 1
        } else {
            n
        }
    };
    // `n <= LINE_CAP`, so the newline has its byte.
    if let Some(b) = buf.get_mut(n) {
        *b = b'\n';
    }
    emit(buf.get(..=n).unwrap_or(&[]), capture);
}

/// [`emit_built`] with a fallible builder. The line is written even when
/// `f` fails; `f`'s result is returned.
fn emit_with(capture: bool, f: impl FnOnce(&mut StackBuf<'_>) -> fmt::Result) -> fmt::Result {
    let mut res = Ok(());
    emit_built(capture, |w| res = f(w));
    res
}

/// Write one kernel line that `f` builds from pieces, captured into the
/// log ring: the caller of a line assembled by helpers (`write_hex`, a
/// shared `write_marker`) instead of several `Serial` writes, each of
/// which would be a line of its own.
pub fn write_line_with(f: impl FnOnce(&mut StackBuf<'_>) -> fmt::Result) -> fmt::Result {
    emit_with(true, f)
}

impl fmt::Write for Serial {
    /// `s` as one line.
    fn write_str(&mut self, s: &str) -> fmt::Result {
        Self::write_line(s.as_bytes());
        Ok(())
    }

    /// The whole formatted line, newline included, under one TX hold.
    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> fmt::Result {
        emit_with(true, |w| w.write_fmt(args))
    }
}

/// Serial TX that does not land in the log ring. The in-guest `dmesg`
/// test's dump uses it so a dump cannot wrap the ring in copies of itself,
/// and the IF-off tracer's report (`sched::irqoff`) so its lines every
/// 100 ms cannot wrap it; the shell's `dmesg` writes to its console. Only
/// those builds have it.
#[cfg(any(feature = "kernel_tests", feature = "irqoff"))]
#[cfg_attr(
    target_arch = "aarch64",
    expect(dead_code, reason = "boot-CPU S7; unused on this path")
)]
pub struct PlainSerial;

#[cfg(any(feature = "kernel_tests", feature = "irqoff"))]
impl fmt::Write for PlainSerial {
    /// `s` as one line.
    fn write_str(&mut self, s: &str) -> fmt::Result {
        emit_bytes(s.as_bytes(), false);
        Ok(())
    }

    /// The whole formatted line, newline included, under one TX hold.
    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> fmt::Result {
        emit_with(false, |w| w.write_fmt(args))
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
/// `klog!` is filtered. `PlainSerial` is only for `dmesg` and the IF-off
/// tracer's report; the panic dump writes through `raw::write_owner`.
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
