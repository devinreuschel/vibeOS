//! Small no-alloc formatting helpers used by the panic path and the
//! serial line writer.

use core::fmt;

/// A `fmt::Write` over a caller's byte buffer, usually on the stack. It
/// copies what fits and drops the rest, so it never returns `Err`;
/// `is_cut` says whether anything was dropped.
pub struct StackBuf<'a> {
    buf: &'a mut [u8],
    pos: usize,
    cut: bool,
}

impl<'a> StackBuf<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            cut: false,
        }
    }

    /// The bytes written so far.
    pub fn as_bytes(&self) -> &[u8] {
        self.buf.get(..self.pos).unwrap_or(&[])
    }

    pub fn len(&self) -> usize {
        self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.pos == 0
    }

    /// True once a write did not fit.
    pub fn is_cut(&self) -> bool {
        self.cut
    }

    /// On a cut line, make its last three bytes `...` so a reader sees the
    /// cut. A line that was not cut is unchanged.
    pub fn mark_cut(&mut self) {
        if !self.cut {
            return;
        }
        let start = self.pos.saturating_sub(3);
        if let Some(tail) = self.buf.get_mut(start..self.pos) {
            tail.fill(b'.');
        }
    }

    /// Append `src`, or as much of it as fits.
    pub fn push_bytes(&mut self, src: &[u8]) {
        let space = self.buf.len().saturating_sub(self.pos);
        let n = src.len().min(space);
        if n < src.len() {
            self.cut = true;
        }
        let end = self.pos.saturating_add(n);
        if let (Some(dst), Some(src)) = (self.buf.get_mut(self.pos..end), src.get(..n)) {
            dst.copy_from_slice(src);
            self.pos = end;
        }
    }
}

impl fmt::Write for StackBuf<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.push_bytes(s.as_bytes());
        Ok(())
    }
}

/// Write a decimal `u64` into `buf`, returning the slice actually filled.
/// Never allocates; safe from the panic handler.
pub fn write_dec(mut n: u64, buf: &mut [u8]) -> &[u8] {
    if n == 0 {
        if buf.is_empty() {
            return &buf[..0];
        }
        buf[0] = b'0';
        return &buf[..1];
    }

    // Fill from the tail then shift down. Twenty digits fits u64::MAX.
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    while n > 0 && i > 0 {
        i -= 1;
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    let src = &tmp[i..];
    let len = src.len().min(buf.len());
    buf[..len].copy_from_slice(&src[..len]);
    &buf[..len]
}

/// Write `n` as 16 lowercase hex digits (no `0x` prefix) into `buf`.
/// Truncates from the left if `buf` is short. Never allocates; used by
/// exception dumps on IST stacks (DESIGN §5.2).
pub fn write_hex(n: u64, buf: &mut [u8]) -> &[u8] {
    if buf.is_empty() {
        return &buf[..0];
    }
    let mut tmp = [b'0'; 16];
    let mut x = n;
    let mut i = 16;
    while i > 0 {
        i -= 1;
        let d = (x & 0xF) as u8;
        tmp[i] = if d < 10 { b'0' + d } else { b'a' + (d - 10) };
        x >>= 4;
    }
    let len = 16.min(buf.len());
    buf[..len].copy_from_slice(&tmp[..len]);
    &buf[..len]
}

#[cfg(test)]
mod tests {
    use core::fmt::Write;

    use super::*;
    use crate::log::MSG_CAP;

    #[test]
    fn stackbuf_cuts_and_marks() {
        let mut b = [0u8; 8];
        let mut w = StackBuf::new(&mut b);
        assert!(write!(w, "abc").is_ok());
        assert!(!w.is_cut());
        w.mark_cut();
        assert_eq!(w.as_bytes(), b"abc");
        assert!(write!(w, "defghijk").is_ok());
        assert!(w.is_cut());
        assert_eq!(w.len(), 8);
        assert_eq!(w.as_bytes(), b"abcdefgh");
        w.mark_cut();
        assert_eq!(w.as_bytes(), b"abcde...");
    }

    #[test]
    fn stackbuf_room_for_newline_at_msg_cap() {
        // `log_fmt` formats into the first `MSG_CAP` bytes of a
        // `MSG_CAP + 1` buffer, so a record at the cap still has a byte
        // for its newline.
        let mut b = [0u8; MSG_CAP + 1];
        let n = {
            let mut w = StackBuf::new(&mut b[..MSG_CAP]);
            for _ in 0..MSG_CAP {
                assert!(w.write_str("xy").is_ok());
            }
            assert!(w.is_cut());
            w.len()
        };
        assert_eq!(n, MSG_CAP);
        b[n] = b'\n';
        assert_eq!(b[MSG_CAP], b'\n');
        assert!(b[..MSG_CAP].iter().all(|&c| c == b'x' || c == b'y'));
    }

    #[test]
    fn zero() {
        let mut b = [0u8; 4];
        assert_eq!(write_dec(0, &mut b), b"0");
    }

    #[test]
    fn small() {
        let mut b = [0u8; 4];
        assert_eq!(write_dec(42, &mut b), b"42");
    }

    #[test]
    fn max() {
        let mut b = [0u8; 20];
        assert_eq!(write_dec(u64::MAX, &mut b), b"18446744073709551615");
    }

    #[test]
    fn truncates_when_buffer_short() {
        let mut b = [0u8; 2];
        // 12345 -> first two digits fit
        assert_eq!(write_dec(12345, &mut b), b"12");
    }

    #[test]
    fn hex_zero_padded() {
        let mut b = [0u8; 16];
        assert_eq!(write_hex(0, &mut b), b"0000000000000000");
        assert_eq!(write_hex(0xAB, &mut b), b"00000000000000ab");
    }

    #[test]
    fn hex_max() {
        let mut b = [0u8; 16];
        assert_eq!(write_hex(u64::MAX, &mut b), b"ffffffffffffffff");
    }

    #[test]
    fn hex_truncates_high_digits() {
        let mut b = [0u8; 4];
        assert_eq!(write_hex(0x1234_5678_9ABC_DEF0, &mut b), b"1234");
    }
}
