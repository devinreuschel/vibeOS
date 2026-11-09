//! aarch64 `open` flags.
//!
//! `arch/arm64/include/uapi/asm/fcntl.h` overrides four asm-generic bits.
//! [`from_user`] maps a user word onto the VFS bits in `vibeos::fs` before
//! `open` looks at it. The other bits are the same as asm-generic and pass
//! through.

/// `O_DIRECTORY` (`040000`).
pub const O_DIRECTORY: u32 = 0o40000;
/// `O_NOFOLLOW` (`0100000`).
pub const O_NOFOLLOW: u32 = 0o100000;
/// `O_DIRECT` (`0200000`).
pub const O_DIRECT: u32 = 0o200000;
/// `O_LARGEFILE` (`0400000`). musl sets this on every `open`.
pub const O_LARGEFILE: u32 = 0o400000;

const SWAPPED: u32 = O_DIRECTORY | O_NOFOLLOW | O_DIRECT | O_LARGEFILE;

/// Map an aarch64 user flag word onto the VFS's asm-generic bits.
pub const fn from_user(bits: u32) -> u32 {
    let rest = bits & !SWAPPED;
    rest | moved(bits, O_DIRECTORY, crate::fs::O_DIRECTORY)
        | moved(bits, O_NOFOLLOW, crate::fs::O_NOFOLLOW)
        | moved(bits, O_DIRECT, crate::fs::O_DIRECT)
        | moved(bits, O_LARGEFILE, crate::fs::O_LARGEFILE)
}

const fn moved(bits: u32, user: u32, vfs: u32) -> u32 {
    if bits & user != 0 { vfs } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aarch64_open_flags_match_uapi() {
        // `arch/arm64/include/uapi/asm/fcntl.h`.
        assert_eq!(O_DIRECTORY, 0o40000);
        assert_eq!(O_NOFOLLOW, 0o100000);
        assert_eq!(O_DIRECT, 0o200000);
        assert_eq!(O_LARGEFILE, 0o400000);
        assert_eq!(O_DIRECTORY, 0x4000);
        assert_eq!(O_NOFOLLOW, 0x8000);
        assert_eq!(O_DIRECT, 0x10000);
        assert_eq!(O_LARGEFILE, 0x20000);

        assert_eq!(from_user(O_DIRECTORY), crate::fs::O_DIRECTORY);
        assert_eq!(from_user(O_NOFOLLOW), crate::fs::O_NOFOLLOW);
        assert_eq!(from_user(O_DIRECT), crate::fs::O_DIRECT);
        assert_eq!(from_user(O_LARGEFILE), crate::fs::O_LARGEFILE);
        // musl's `O_LARGEFILE` is the bit the VFS used to treat as `O_NOFOLLOW`.
        assert_eq!(from_user(O_LARGEFILE) & crate::fs::O_NOFOLLOW, 0);
        // arm64 `O_DIRECT` is the bit the VFS used to treat as `O_DIRECTORY`.
        assert_eq!(from_user(O_DIRECT) & crate::fs::O_DIRECTORY, 0);

        let shared = crate::fs::O_RDWR
            | crate::fs::O_CREAT
            | crate::fs::O_EXCL
            | crate::fs::O_TRUNC
            | crate::fs::O_APPEND
            | crate::fs::O_CLOEXEC;
        let user = shared | O_DIRECTORY | O_NOFOLLOW | O_DIRECT | O_LARGEFILE;
        let want = shared
            | crate::fs::O_DIRECTORY
            | crate::fs::O_NOFOLLOW
            | crate::fs::O_DIRECT
            | crate::fs::O_LARGEFILE;
        assert_eq!(from_user(user), want);
        assert_eq!(from_user(0), 0);
        assert_eq!(from_user(1 << 21), 1 << 21);
    }
}
