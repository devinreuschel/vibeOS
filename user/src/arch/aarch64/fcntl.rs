//! aarch64 `open` flags, from `arch/arm64/include/uapi/asm/fcntl.h`.

/// Fail unless the path names a directory.
pub const O_DIRECTORY: i32 = 0o40000;
/// Fail on a symbolic link in the last component.
pub const O_NOFOLLOW: i32 = 0o100000;
/// Direct I/O hint. The kernel ignores it.
pub const O_DIRECT: i32 = 0o200000;
/// Ignored. musl sets it on every `open`.
pub const O_LARGEFILE: i32 = 0o400000;
