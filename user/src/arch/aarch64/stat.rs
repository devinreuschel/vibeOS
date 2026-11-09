//! aarch64's `struct stat`, the 128 bytes `fstat` writes (ROADMAP §11.6).
//! The layout is a fact taken from Linux's `include/uapi/asm-generic/stat.h`
//! (DESIGN §1.5); the kernel's copy is `vibeos::arch::aarch64::stat::Stat`.

use core::mem::{offset_of, size_of};

/// asm-generic `struct stat` as Linux arm64 uses it.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_mode: u32,
    pub st_nlink: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub st_rdev: u64,
    pub __pad1: u64,
    pub st_size: i64,
    pub st_blksize: i32,
    pub __pad2: i32,
    pub st_blocks: i64,
    pub st_atime: i64,
    pub st_atime_nsec: u64,
    pub st_mtime: i64,
    pub st_mtime_nsec: u64,
    pub st_ctime: i64,
    pub st_ctime_nsec: u64,
    pub __unused4: u32,
    pub __unused5: u32,
}

const _: () = {
    assert!(size_of::<Stat>() == 128);
    assert!(offset_of!(Stat, st_ino) == 8);
    assert!(offset_of!(Stat, st_mode) == 16);
    assert!(offset_of!(Stat, st_nlink) == 20);
    assert!(offset_of!(Stat, st_uid) == 24);
    assert!(offset_of!(Stat, st_gid) == 28);
    assert!(offset_of!(Stat, st_rdev) == 32);
    assert!(offset_of!(Stat, st_size) == 48);
    assert!(offset_of!(Stat, st_blksize) == 56);
    assert!(offset_of!(Stat, st_blocks) == 64);
    assert!(offset_of!(Stat, st_atime) == 72);
    assert!(offset_of!(Stat, st_mtime) == 88);
    assert!(offset_of!(Stat, st_ctime) == 104);
};
