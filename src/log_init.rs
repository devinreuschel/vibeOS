#![allow(dead_code)] // printer thread stays parked
//! Kernel wiring for the log ring. ROADMAP §5.5.
//!
//! Global IRQ-safe ring + serial sink. Per-CPU printer thread is a
//! parked stub (Design ACK): line atomicity on serial is still only
//! as good as the TX lock. The ring itself is line-atomic because
//! serial capture assembles per-CPU until `\n`, and `klog!` pushes
//! a whole record.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use vibeos::log::{
    COMPILE_MAX, DEFAULT_RUNTIME_MAX, DUMP_LAST, Level, Logger, MSG_CAP, RING_CAP, Record, allowed,
};

use crate::cell::IrqCell;
use crate::per_cpu_init;
use crate::serial::{PlainSerial, Serial};
use crate::time_init;
use crate::x86::InterruptGuard;

struct Stage {
    buf: [u8; MSG_CAP],
    len: usize,
}

impl Stage {
    const fn empty() -> Self {
        Self {
            buf: [0; MSG_CAP],
            len: 0,
        }
    }
}

static LOG: IrqCell<Logger<RING_CAP, MSG_CAP>> = IrqCell::new(Logger::new());
/// Per-CPU: set while this CPU is inside `emit` so serial capture does
/// not store a duplicate.
static EMITTING: [AtomicBool; 64] = [const { AtomicBool::new(false) }; 64];
static STAGE: [IrqCell<Stage>; 64] = [const { IrqCell::new(Stage::empty()) }; 64];
/// Extra runtime copy so `allows` can be checked without the ring lock
/// on the serial capture path. Kept in sync with `Logger.filter`.
static RUNTIME: AtomicU8 = AtomicU8::new(DEFAULT_RUNTIME_MAX as u8);

fn cpu_index() -> usize {
    per_cpu_init::try_current()
        .map(|c| c.cpu_id as usize)
        .unwrap_or(0)
        .min(63)
}

fn runtime() -> Level {
    Level::from_u8(RUNTIME.load(Ordering::Acquire)).unwrap_or(DEFAULT_RUNTIME_MAX)
}

fn timestamp() -> u64 {
    // Seqlock tick once time is live; raw TSC before that. Same clock
    // paths as the rest of the kernel (DESIGN §9.4).
    if time_init::tsc_per_ms() != 0 {
        time_init::uptime_ms()
    } else {
        time_init::read_tsc()
    }
}

fn cpu_id() -> u8 {
    per_cpu_init::try_current()
        .map(|c| c.cpu_id as u8)
        .unwrap_or(0)
}

fn with_logger<R>(f: impl FnOnce(&mut Logger<RING_CAP, MSG_CAP>) -> R) -> R {
    LOG.with(f)
}

/// Panic path: other CPUs are halted. Drop a held TAS and read.
pub fn force_unlock() {
    LOG.force_unlock();
}

pub fn with_logger_unlocked<R>(f: impl FnOnce(&Logger<RING_CAP, MSG_CAP>) -> R) -> R {
    f(unsafe { &*LOG.as_ptr() })
}

pub fn is_emitting() -> bool {
    EMITTING[cpu_index()].load(Ordering::Relaxed)
}

pub fn set_max_level(max: Level) {
    RUNTIME.store(max as u8, Ordering::Release);
    with_logger(|l| l.filter.set(max));
}

pub fn max_level() -> Level {
    runtime()
}

pub fn compile_max() -> Level {
    COMPILE_MAX
}

fn push_record(level: Level, msg: &[u8]) -> bool {
    if !allowed(level, runtime(), COMPILE_MAX) {
        return false;
    }
    let rec = Record::from_msg(timestamp(), cpu_id(), level, msg);
    with_logger(|l| l.emit(rec))
}

/// `klog!` / formatted emit. Serial is try-lock + drop (DESIGN §5.5).
/// IRQ-off for the whole emit so `EMITTING` / try-write cannot race a
/// preempting thread on this CPU.
pub fn log_fmt(level: Level, args: fmt::Arguments<'_>) {
    if !allowed(level, runtime(), COMPILE_MAX) {
        return;
    }
    let _irq = InterruptGuard::enter();
    let i = cpu_index();
    if EMITTING[i]
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    let mut buf = [0u8; MSG_CAP];
    let n = {
        let mut w = StackBuf {
            buf: &mut buf,
            pos: 0,
        };
        let _ = w.write_fmt(args);
        w.pos
    };
    let msg = &buf[..n];
    let _ = push_record(level, msg);
    let _ = crate::serial::Serial::try_write_bytes(msg);
    if !msg.ends_with(b"\n") {
        let _ = crate::serial::Serial::try_write_bytes(b"\n");
    }
    EMITTING[i].store(false, Ordering::Release);
}

struct StackBuf<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl fmt::Write for StackBuf<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let src = s.as_bytes();
        let space = self.buf.len().saturating_sub(self.pos);
        let n = src.len().min(space);
        self.buf[self.pos..self.pos + n].copy_from_slice(&src[..n]);
        self.pos += n;
        Ok(())
    }
}

/// Assemble COM1 bytes into records so boot `marker!` / `writeln!(Serial)` is captured
/// before a framebuffer exists. Caller holds IRQs off (`Serial::write_bytes`
/// / `write_fmt`); `STAGE` is CPU-local and must not outlive that.
pub fn capture_serial(bytes: &[u8]) {
    if crate::ipi_init::is_halting() || is_emitting() {
        return;
    }
    if !allowed(Level::Info, runtime(), COMPILE_MAX) {
        return;
    }
    let _irq = InterruptGuard::enter();
    let i = cpu_index();
    STAGE[i].with(|st| {
        for &b in bytes {
            if b == b'\r' {
                continue;
            }
            if b == b'\n' {
                if st.len > 0 {
                    let rec =
                        Record::from_msg(timestamp(), cpu_id(), Level::Info, &st.buf[..st.len]);
                    st.len = 0;
                    let _ = LOG.with(|l| l.emit(rec));
                }
                continue;
            }
            if st.len < MSG_CAP {
                st.buf[st.len] = b;
                st.len += 1;
            }
        }
    });
}

pub fn contains_msg(needle: &str) -> bool {
    let n = needle.as_bytes();
    if n.is_empty() {
        return true;
    }
    with_logger(|l| {
        l.ring
            .iter()
            .any(|r| r.msg().windows(n.len()).any(|w| w == n))
    })
}

pub fn ring_len() -> usize {
    with_logger(|l| l.ring.len())
}

pub fn written() -> u64 {
    with_logger(|l| l.ring.written())
}

/// Copy record `i` (oldest-first). Lock is not held after return.
pub fn record_at(i: usize) -> Option<vibeos::log::Record<MSG_CAP>> {
    with_logger(|l| l.ring.get(i).copied())
}

/// Copy each message out, then call `f`. Lock is not held across `f`.
pub fn for_each_msg(mut f: impl FnMut(&[u8])) {
    let len = with_logger(|l| l.ring.len());
    let mut i = 0usize;
    while i < len {
        let rec = with_logger(|l| l.ring.get(i).copied());
        i += 1;
        let Some(r) = rec else {
            break;
        };
        f(r.msg());
    }
}

pub fn dropped() -> u64 {
    with_logger(|l| l.ring.dropped())
}

/// `dmesg` dump. `view` None → current runtime max.
/// Does not hold the ring lock across TX (capture would deadlock).
pub fn dmesg(view: Option<Level>) {
    dmesg_write(&mut PlainSerial, view);
}

pub fn dmesg_write(w: &mut impl Write, view: Option<Level>) {
    let view = view.unwrap_or_else(max_level);
    let len = with_logger(|l| l.ring.len());
    let mut i = 0usize;
    while i < len {
        let rec = with_logger(|l| l.ring.get(i).copied());
        i += 1;
        let Some(r) = rec else {
            break;
        };
        if !allowed(r.level, view, COMPILE_MAX) {
            continue;
        }
        write_record(w, &r);
    }
}

pub fn write_record(w: &mut impl Write, r: &vibeos::log::Record<MSG_CAP>) {
    let unit = if time_init::tsc_per_ms() != 0 {
        "ms"
    } else {
        "tsc"
    };
    let _ = writeln!(
        w,
        "vibeOS: dmesg: {}{} cpu{} {} {}",
        r.timestamp,
        unit,
        r.cpu_id,
        r.level.as_str(),
        r.msg_str()
    );
}

/// Last N records for the panic dump. Caller holds no log lock.
pub fn dump_tail(n: usize) {
    force_unlock();
    let n = if n == 0 { DUMP_LAST } else { n };
    with_logger_unlocked(|l| {
        let _ = writeln!(
            Serial,
            "vibeOS: log: last {} ({} dropped)",
            n.min(l.ring.len()),
            l.ring.dropped()
        );
        let unit = if time_init::tsc_per_ms() != 0 {
            "ms"
        } else {
            "tsc"
        };
        for r in l.ring.last_n(n) {
            let _ = writeln!(
                Serial,
                "vibeOS: logrec: {}{} cpu{} {} {}",
                r.timestamp,
                unit,
                r.cpu_id,
                r.level.as_str(),
                r.msg_str()
            );
        }
    });
}

/// Parked. DESIGN §7.7 line atomicity wants per-CPU staging + a printer
/// thread. Slice A keeps the global ring + serial try-lock sink.
pub fn start_printer_thread() {}

/// `fmt::Write` that emits one Info record per newline, plus serial.
pub struct Log;

impl fmt::Write for Log {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        log_fmt(Level::Info, format_args!("{s}"));
        Ok(())
    }
}

#[macro_export]
macro_rules! klog {
    ($lvl:expr, $($arg:tt)*) => {{
        $crate::log_init::log_fmt($lvl, format_args!($($arg)*));
    }};
}
