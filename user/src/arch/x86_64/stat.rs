//! x86_64's `struct stat`, the 144 bytes `fstat` writes (ROADMAP §10.5).
//! The layout is a fact taken from musl's MIT-licensed
//! `arch/x86_64/bits/stat.h`; the kernel's copy is
//! `vibeos::arch::x86_64::stat::Stat`.

use core::mem::{offset_of, size_of};

/// x86_64's `struct stat`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_nlink: u64,
    pub st_mode: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub __pad0: u32,
    pub st_rdev: u64,
    pub st_size: i64,
    pub st_blksize: i64,
    pub st_blocks: i64,
    pub st_atime: i64,
    pub st_atime_nsec: i64,
    pub st_mtime: i64,
    pub st_mtime_nsec: i64,
    pub st_ctime: i64,
    pub st_ctime_nsec: i64,
    pub __unused: [i64; 3],
}

const _: () = {
    assert!(size_of::<Stat>() == 144);
    assert!(offset_of!(Stat, st_ino) == 8);
    assert!(offset_of!(Stat, st_nlink) == 16);
    assert!(offset_of!(Stat, st_mode) == 24);
    assert!(offset_of!(Stat, st_uid) == 28);
    assert!(offset_of!(Stat, st_gid) == 32);
    assert!(offset_of!(Stat, st_rdev) == 40);
    assert!(offset_of!(Stat, st_size) == 48);
    assert!(offset_of!(Stat, st_blksize) == 56);
    assert!(offset_of!(Stat, st_blocks) == 64);
    assert!(offset_of!(Stat, st_atime) == 72);
    assert!(offset_of!(Stat, st_mtime) == 88);
    assert!(offset_of!(Stat, st_ctime) == 104);
    assert!(offset_of!(Stat, __unused) == 120);
};
