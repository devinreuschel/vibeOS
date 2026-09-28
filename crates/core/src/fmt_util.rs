//! Small no-alloc formatting helpers used by the panic path.

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
    use super::*;

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
