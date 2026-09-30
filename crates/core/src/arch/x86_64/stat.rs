//! x86_64's `struct stat`, the 144 bytes `fstat` writes (ROADMAP §10.5).
//!
//! The layout is a fact taken from musl's MIT-licensed
//! `arch/x86_64/bits/stat.h` (DESIGN §1.5); aarch64's differs and gets its
//! own file with its port (ROADMAP §11.4). Every byte is a named field, so
//! the type has no padding (DESIGN §2.4).

use zerocopy::{Immutable, IntoBytes};

use crate::proc::uabi::StatFields;

/// x86_64's `struct stat`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, IntoBytes, Immutable)]
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

const _: () = assert!(core::mem::size_of::<Stat>() == 144);

/// A `u64` as the `i64` a field holds, saturating: no inode is that large.
fn sat(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

impl Stat {
    /// The record for `f`: `st_blksize` 4096 and `st_blocks` ⌈size/512⌉ as
    /// `f` carries them, the nanosecond fields 0, and `st_dev`, `st_rdev`,
    /// `st_uid` and `st_gid` 0 (SYSCALL.md §3.1).
    pub fn from_fields(f: &StatFields) -> Stat {
        Stat {
            st_ino: f.ino,
            st_nlink: f.nlink,
            st_mode: f.mode,
            st_size: sat(f.size),
            st_blksize: sat(f.blksize),
            st_blocks: sat(f.blocks),
            st_atime: sat(f.atime),
            st_mtime: sat(f.mtime),
            st_ctime: sat(f.ctime),
            ..Stat::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use core::mem::{offset_of, size_of};

    use super::*;
    use crate::fs::{InodeKind, Stat as FsStat};
    use crate::proc::uabi::stat_fields;

    #[test]
    fn stat_x86_64_layout_is_linux() {
        assert_eq!(size_of::<Stat>(), 144);
        let offs = [
            (offset_of!(Stat, st_dev), 0),
            (offset_of!(Stat, st_ino), 8),
            (offset_of!(Stat, st_nlink), 16),
            (offset_of!(Stat, st_mode), 24),
            (offset_of!(Stat, st_uid), 28),
            (offset_of!(Stat, st_gid), 32),
            (offset_of!(Stat, __pad0), 36),
            (offset_of!(Stat, st_rdev), 40),
            (offset_of!(Stat, st_size), 48),
            (offset_of!(Stat, st_blksize), 56),
            (offset_of!(Stat, st_blocks), 64),
            (offset_of!(Stat, st_atime), 72),
            (offset_of!(Stat, st_atime_nsec), 80),
            (offset_of!(Stat, st_mtime), 88),
            (offset_of!(Stat, st_mtime_nsec), 96),
            (offset_of!(Stat, st_ctime), 104),
            (offset_of!(Stat, st_ctime_nsec), 112),
            (offset_of!(Stat, __unused), 120),
        ];
        for (i, (got, want)) in offs.into_iter().enumerate() {
            assert_eq!(got, want, "field {i}");
        }
        let st = FsStat {
            ino: 5,
            kind: InodeKind::Reg,
            mode: 0o644,
            nlink: 1,
            size: 1000,
            atime: 10,
            mtime: 20,
            ctime: 30,
        };
        let s = Stat::from_fields(&stat_fields(&st));
        assert_eq!(s.st_ino, 5);
        assert_eq!(s.st_mode, 0o100644);
        assert_eq!(s.st_size, 1000);
        assert_eq!(s.st_blksize, 4096);
        assert_eq!(s.st_blocks, 2);
        assert_eq!((s.st_atime, s.st_mtime, s.st_ctime), (10, 20, 30));
        assert_eq!((s.st_dev, s.st_rdev, s.st_uid, s.st_gid), (0, 0, 0, 0));
        assert_eq!(s.st_mtime_nsec, 0);
        let bytes = s.as_bytes();
        assert_eq!(bytes.len(), 144);
        assert_eq!(&bytes[8..16], &5u64.to_le_bytes());
    }
}
