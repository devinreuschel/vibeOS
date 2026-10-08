//! The report's signature (ROADMAP §10.7): the panic line the dump owner
//! records, the message normalization, the panic-machinery skip list and
//! the CPU and frames the `sig:` line names.

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use core::fmt::{self, Write};
use core::mem::{offset_of, size_of};

use super::{PANIC_LINE_CAP, SIG_FRAMES, le32, offset};
use crate::atomic::Ordering;
use crate::atomic::statics::{AtomicU8, AtomicU32};

// --------------------------------------------------------------- the panic line

/// Set in [`PanicLine`]'s `len` word once a line is recorded, so an empty
/// message still reads as a panic.
const LINE_SET: u32 = 1 << 31;

/// The first line of the dump owner's panic message, which the core tool's
/// signature reads (the log ring gets no panic text: serial capture stops
/// while halting). The owner stores the bytes and its CPU, then `len` with
/// Release, last.
#[repr(C)]
pub struct PanicLine {
    len: AtomicU32,
    cpu: AtomicU32,
    msg: [AtomicU8; PANIC_LINE_CAP],
}

const _: () = {
    assert!(offset_of!(PanicLine, len) == 0);
    assert!(offset_of!(PanicLine, cpu) == 4);
    assert!(offset_of!(PanicLine, msg) == 8);
    assert!(size_of::<PanicLine>() == 8 + PANIC_LINE_CAP);
};

/// A recorded panic line's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanicText {
    pub bytes: [u8; PANIC_LINE_CAP],
    pub len: usize,
}

impl PanicText {
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

/// `line` cut at its first `\n` and at [`PANIC_LINE_CAP`] bytes, back to a
/// UTF-8 boundary, without trailing ASCII whitespace (a cut can end on the
/// space before a field).
pub fn first_line(line: &[u8]) -> &[u8] {
    let end = line.iter().position(|&b| b == b'\n').unwrap_or(line.len());
    let mut end = end.min(PANIC_LINE_CAP);
    // A byte `10xxxxxx` continues a character: back up to its start.
    while end > 0 && end < line.len() && line.get(end).is_some_and(|b| b & 0xC0 == 0x80) {
        end = end.saturating_sub(1);
    }
    let cut = line.get(..end).unwrap_or(&[]);
    let keep = cut
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(0, |i| i.saturating_add(1));
    cut.get(..keep).unwrap_or(&[])
}

impl PanicLine {
    /// An empty line, before any panic: `const` in every configuration,
    /// since the fields are `core`'s atomics (the seam's statics, C-ATOMICS).
    #[allow(
        clippy::new_without_default,
        reason = "a const constructor for statics"
    )]
    pub const fn new() -> Self {
        Self {
            len: AtomicU32::new(0),
            cpu: AtomicU32::new(0),
            msg: [const { AtomicU8::new(0) }; PANIC_LINE_CAP],
        }
    }

    /// Store `line`'s first line ([`first_line`]) for CPU `cpu`. The dump's
    /// owner calls it once, with IF=0: no lock, no allocation.
    pub fn record(&self, cpu: u32, line: &[u8]) {
        let line = first_line(line);
        for (d, s) in self.msg.iter().zip(line) {
            // Relaxed: the Release store of `len` below publishes it; pairs with nothing.
            d.store(*s, Ordering::Relaxed);
        }
        // Relaxed: as the bytes; pairs with nothing.
        self.cpu.store(cpu, Ordering::Relaxed);
        // Release: pairs with the Acquire load in `read`; the bytes and the
        // CPU above are whole before a reader that sees `len` reads them
        // (AGENTS.md rule 5).
        self.len
            .store(line.len() as u32 | LINE_SET, Ordering::Release);
    }

    /// The recorded CPU and line, or `None` before [`PanicLine::record`].
    pub fn read(&self) -> Option<(u32, PanicText)> {
        // Acquire: pairs with the Release store in `record`.
        let len = self.len.load(Ordering::Acquire);
        if len & LINE_SET == 0 {
            return None;
        }
        let mut bytes = [0u8; PANIC_LINE_CAP];
        for (d, s) in bytes.iter_mut().zip(&self.msg) {
            // Relaxed: ordered by the Acquire load of `len` above; pairs with nothing.
            *d = s.load(Ordering::Relaxed);
        }
        let n = ((len & !LINE_SET) as usize).min(PANIC_LINE_CAP);
        // Relaxed: as the bytes; pairs with nothing.
        Some((
            self.cpu.load(Ordering::Relaxed),
            PanicText { bytes, len: n },
        ))
    }

    /// A `PanicLine`'s bytes as a core holds them.
    pub fn decode(raw: &[u8; size_of::<PanicLine>()]) -> Option<(u32, PanicText)> {
        let len = le32(raw, offset(offset_of!(PanicLine, len))).ok()?;
        if len & LINE_SET == 0 {
            return None;
        }
        let cpu = le32(raw, offset(offset_of!(PanicLine, cpu))).ok()?;
        let mut bytes = [0u8; PANIC_LINE_CAP];
        let src = raw.get(offset_of!(PanicLine, msg)..).unwrap_or(&[]);
        for (d, s) in bytes.iter_mut().zip(src) {
            *d = *s;
        }
        let n = ((len & !LINE_SET) as usize).min(PANIC_LINE_CAP);
        Some((cpu, PanicText { bytes, len: n }))
    }
}
// --------------------------------------------------------------- signature

/// The frames the signature skips at the top of the panicking CPU's
/// backtrace: the panic machinery between the kernel's `panic!` site and
/// the dump. Each entry is a prefix of the demangled names it matches;
/// `rust_begin_unwind` also matches as a last path segment
/// (`__rustc::rust_begin_unwind`).
pub const PANIC_FRAMES: &[&str] = &[
    "rust_begin_unwind",
    "core::panicking::",
    "core::option::unwrap_failed",
    "core::option::expect_failed",
    "core::result::unwrap_failed",
    "core::slice::index::",
    "core::str::slice_error_fail",
    "vibeos::log::panic::",
];

/// Whether `name` (demangled, no hash) is panic machinery.
pub fn is_panic_frame(name: &str) -> bool {
    PANIC_FRAMES.iter().any(|p| name.starts_with(p)) || name.ends_with("::rust_begin_unwind")
}

/// Write `msg` with each maximal `0x[0-9A-Fa-f]+` or `[0-9]+` as `N`.
pub fn normalize_message<W: Write>(msg: &str, out: &mut W) -> fmt::Result {
    let b = msg.as_bytes();
    let mut i = 0usize;
    let mut plain = 0usize;
    while let Some(&c) = b.get(i) {
        let hex = c == b'0'
            && b.get(i.saturating_add(1)) == Some(&b'x')
            && b.get(i.saturating_add(2))
                .is_some_and(u8::is_ascii_hexdigit);
        if !(hex || c.is_ascii_digit()) {
            i = i.saturating_add(1);
            continue;
        }
        out.write_str(msg.get(plain..i).unwrap_or(""))?;
        out.write_char('N')?;
        i = i.saturating_add(if hex { 2 } else { 0 });
        let digit = |c: &u8| {
            if hex {
                c.is_ascii_hexdigit()
            } else {
                c.is_ascii_digit()
            }
        };
        while b.get(i).is_some_and(digit) {
            i = i.saturating_add(1);
        }
        plain = i;
    }
    out.write_str(msg.get(plain..).unwrap_or(""))
}

/// The CPU the signature reads: the one the panic line names, else the
/// lowest `cpu_id` whose current thread is not its idle thread, else CPU 0.
/// `cpus` holds `(cpu_id, current, idle)` per CPU.
pub fn pick_cpu(panic_cpu: Option<u32>, cpus: &[(u32, u64, u64)]) -> u32 {
    if let Some(c) = panic_cpu {
        return c;
    }
    cpus.iter()
        .filter(|(_, cur, idle)| cur != idle)
        .map(|(id, _, _)| *id)
        .min()
        .unwrap_or(0)
}

/// The report's first line: `sig: <message> @ <f0> < <f1> < <f2>`.
/// `message` is the panic line (`None` for a timeout, written `timeout`);
/// `frames` are the picked CPU's frame names in order, `None` for an
/// address with no symbol. On a panic, leading [`is_panic_frame`] names
/// are skipped; a missing frame is `?`.
pub fn write_signature<'n, W: Write>(
    out: &mut W,
    message: Option<&str>,
    frames: impl IntoIterator<Item = Option<&'n str>>,
) -> fmt::Result {
    out.write_str("sig: ")?;
    match message {
        Some(m) => normalize_message(m, out)?,
        None => out.write_str("timeout")?,
    }
    out.write_str(" @ ")?;
    let mut it = frames.into_iter().peekable();
    if message.is_some() {
        while it.peek().is_some_and(|f| f.is_some_and(is_panic_frame)) {
            it.next();
        }
    }
    for k in 0..SIG_FRAMES {
        if k > 0 {
            out.write_str(" < ")?;
        }
        out.write_str(it.next().flatten().unwrap_or("?"))?;
    }
    Ok(())
}
