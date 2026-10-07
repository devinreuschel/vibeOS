//! x86_64 `open` flags.
//!
//! The values are Linux's `include/uapi/asm-generic/fcntl.h`, which this
//! architecture uses. They are also the VFS's bits (`vibeos::fs::O_*`), so
//! [`from_user`] is the identity.

/// `O_DIRECT` (`1 << 14`).
pub const O_DIRECT: u32 = 0x4000;
/// `O_LARGEFILE` (`1 << 15`).
pub const O_LARGEFILE: u32 = 0x8000;
/// `O_DIRECTORY` (`1 << 16`).
pub const O_DIRECTORY: u32 = 0x10000;
/// `O_NOFOLLOW` (`1 << 17`).
pub const O_NOFOLLOW: u32 = 0x20000;

/// x86_64 user bits are the VFS bits.
pub const fn from_user(bits: u32) -> u32 {
    bits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x86_64_open_flags_match_uapi() {
        // `include/uapi/asm-generic/fcntl.h`.
        assert_eq!(O_DIRECT, 1 << 14);
        assert_eq!(O_LARGEFILE, 1 << 15);
        assert_eq!(O_DIRECTORY, 1 << 16);
        assert_eq!(O_NOFOLLOW, 1 << 17);
        assert_eq!(O_DIRECT, crate::fs::O_DIRECT);
        assert_eq!(O_LARGEFILE, crate::fs::O_LARGEFILE);
        assert_eq!(O_DIRECTORY, crate::fs::O_DIRECTORY);
        assert_eq!(O_NOFOLLOW, crate::fs::O_NOFOLLOW);
        let word = O_DIRECTORY | O_LARGEFILE | crate::fs::O_CLOEXEC | crate::fs::O_CREAT;
        assert_eq!(from_user(word), word);
    }
}
