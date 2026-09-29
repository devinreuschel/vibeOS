//! Kernel console lines: the frame and the escapes (DESIGN §2.6).
//!
//! Every line the kernel writes to its console UART starts with [`FRAME`],
//! and a `\r`, `\n` or [`FRAME`] inside it prints as [`SUBST`], so a
//! reader of the UART takes a line as the kernel's only when its first byte
//! is the frame. Bytes a process writes to the console carry no frame, and
//! a [`FRAME`] among them prints as [`SUBST`], so ring 3 cannot forge a
//! kernel line. The framebuffer console never sees any of this: only the
//! raw UART writer calls these functions.

/// ASCII RS. The first byte of every kernel line on the console UART.
pub const FRAME: u8 = 0x1E;
/// What a `\r`, `\n` or [`FRAME`] inside a kernel line, or a [`FRAME`] in
/// user bytes, prints as.
pub const SUBST: u8 = b'?';
/// The longest kernel line, newline excluded. A longer one is cut and ends
/// in `...` (`fmt_util::StackBuf::mark_cut`).
pub const LINE_CAP: usize = 256;

/// Emit one kernel line through `put`: `\r\n` first when a user line is
/// open, then [`FRAME`], then `content` with each `\r`, `\n` and [`FRAME`]
/// as [`SUBST`], then `\r\n`. One trailing `\n` in `content` is the
/// terminator and is dropped.
pub fn kernel_line(user_open: bool, content: &[u8], mut put: impl FnMut(u8)) {
    if user_open {
        put(b'\r');
        put(b'\n');
    }
    put(FRAME);
    let body = content.strip_suffix(b"\n").unwrap_or(content);
    for &b in body {
        put(if b == b'\r' || b == b'\n' || b == FRAME {
            SUBST
        } else {
            b
        });
    }
    put(b'\r');
    put(b'\n');
}

/// Emit user console bytes through `put`: each [`FRAME`] as [`SUBST`],
/// `\n` as `\r\n`, every other byte unchanged. `was_open` says whether a
/// user line was open before them; the result says whether one is open
/// after them, that is, whether bytes follow the last `\n`.
pub fn user_bytes(was_open: bool, bytes: &[u8], mut put: impl FnMut(u8)) -> bool {
    let mut open = was_open;
    for &b in bytes {
        match b {
            b'\n' => {
                put(b'\r');
                put(b'\n');
                open = false;
            }
            FRAME => {
                put(SUBST);
                open = true;
            }
            _ => {
                put(b);
                open = true;
            }
        }
    }
    open
}

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use super::*;

    fn kline(open: bool, content: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        kernel_line(open, content, |b| out.push(b));
        out
    }

    fn ubytes(open: bool, bytes: &[u8]) -> (Vec<u8>, bool) {
        let mut out = Vec::new();
        let still = user_bytes(open, bytes, |b| out.push(b));
        (out, still)
    }

    #[test]
    fn kernel_line_frames_empty() {
        assert_eq!(kline(false, b""), b"\x1e\r\n");
        assert_eq!(kline(false, b"\n"), b"\x1e\r\n");
    }

    #[test]
    fn kernel_line_escapes_cr_lf_rs() {
        assert_eq!(kline(false, b"a\nb\rc\x1ed"), b"\x1ea?b?c?d\r\n");
    }

    #[test]
    fn kernel_line_drops_one_trailing_lf() {
        assert_eq!(kline(false, b"vibeOS: x\n"), b"\x1evibeOS: x\r\n");
        assert_eq!(kline(false, b"vibeOS: x\n\n"), b"\x1evibeOS: x?\r\n");
    }

    #[test]
    fn kernel_line_breaks_open_user_line() {
        assert_eq!(kline(true, b"k\n"), b"\r\n\x1ek\r\n");
    }

    #[test]
    fn kernel_line_after_closed_user_line() {
        let (_, open) = ubytes(true, b"user line\n");
        assert!(!open);
        assert_eq!(kline(open, b"k"), b"\x1ek\r\n");
    }

    #[test]
    fn user_bytes_escape_rs_only() {
        let (out, _) = ubytes(false, b"\x1evibeOS: x\r\x07?");
        assert_eq!(out, b"?vibeOS: x\r\x07?");
    }

    #[test]
    fn user_bytes_crlf() {
        let (out, _) = ubytes(false, b"a\nb\n");
        assert_eq!(out, b"a\r\nb\r\n");
    }

    #[test]
    fn user_bytes_open_state() {
        assert!(ubytes(false, b"prompt> ").1);
        assert!(!ubytes(true, b"done\n").1);
        assert!(ubytes(false, b"a\nb").1);
        assert!(ubytes(true, b"").1);
        assert!(!ubytes(false, b"").1);
        assert!(ubytes(false, b"\x1e").1);
    }
}
