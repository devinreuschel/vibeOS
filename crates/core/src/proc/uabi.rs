//! Linux's user ABI for the §10.5 floor calls, portable half (ROADMAP
//! §10.5): the `linux_dirent64` records `getdents64` writes, the values
//! `fstat` copies out, `nanosleep`'s deadline, and `reboot`'s argument
//! checks. The kernel half (`proc_init::floor`) does the descriptor lookups
//! and the user copies; the architecture's `struct stat` is
//! `crate::arch::<port>::stat`.
//!
//! Sources, as facts (DESIGN §1.5): getdents(2) for the record layout,
//! inode(7) for the `DT_*` and mode values, nanosleep(2) for the timespec
//! checks, and reboot(2) for the magic numbers and commands.

use crate::fs::{InodeKind, S_IFBLK, S_IFCHR, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG, Stat};
use crate::kerror::KError;
use crate::sched::FAR_DEADLINE;

// `d_type` values, from inode(7) (not errnos).
/// A character device.
pub const DT_CHR: u8 = 2;
/// A directory.
pub const DT_DIR: u8 = 4;
/// A block device.
pub const DT_BLK: u8 = 6;
/// A regular file.
pub const DT_REG: u8 = 8;
/// A symbolic link.
pub const DT_LNK: u8 = 10;

/// `d_ino`'s offset in a `linux_dirent64`.
const D_INO: usize = 0;
/// `d_off`'s offset.
const D_OFF: usize = 8;
/// `d_reclen`'s offset.
const D_RECLEN: usize = 16;
/// `d_type`'s offset.
const D_TYPE: usize = 18;
/// `d_name`'s offset.
pub const D_NAME: usize = 19;

/// `d_type` for an inode of `kind`.
pub const fn d_type(kind: InodeKind) -> u8 {
    match kind {
        InodeKind::Reg => DT_REG,
        InodeKind::Dir => DT_DIR,
        InodeKind::Lnk => DT_LNK,
        InodeKind::Chr => DT_CHR,
        InodeKind::Blk => DT_BLK,
    }
}

/// The length of the record for a `name_len`-byte name: the header, the
/// name and its NUL, rounded up to 8.
pub const fn dirent64_reclen(name_len: usize) -> Option<usize> {
    match D_NAME.checked_add(name_len) {
        Some(n) => match n.checked_add(1 + 7) {
            Some(m) => Some(m & !7),
            None => None,
        },
        None => None,
    }
}

/// Writes whole `linux_dirent64` records into a buffer, one after the
/// other, refusing any record that does not fit.
pub struct Dirent64Writer<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> Dirent64Writer<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, len: 0 }
    }

    /// Append the record for `name`: inode `ino`, `next` the cookie of the
    /// entry after it (`d_off`), and `kind`'s `d_type`. The padding after
    /// the name's NUL is zero. Returns false, writing nothing, when the
    /// record does not fit in what is left of the buffer.
    pub fn push(&mut self, ino: u64, next: u64, kind: InodeKind, name: &[u8]) -> bool {
        let Some(reclen) = dirent64_reclen(name.len()) else {
            return false;
        };
        let Ok(reclen16) = u16::try_from(reclen) else {
            return false;
        };
        let Some(end) = self.len.checked_add(reclen) else {
            return false;
        };
        let Some(rec) = self.buf.get_mut(self.len..end) else {
            return false;
        };
        rec.fill(0);
        let fields: [(usize, &[u8]); 5] = [
            (D_INO, &ino.to_le_bytes()),
            (D_OFF, &next.to_le_bytes()),
            (D_RECLEN, &reclen16.to_le_bytes()),
            (D_TYPE, &[d_type(kind)]),
            (D_NAME, name),
        ];
        for (at, bytes) in fields {
            // `reclen` covers the header and the name, so every range fits.
            if let Some(dst) = rec.get_mut(at..at.saturating_add(bytes.len())) {
                dst.copy_from_slice(bytes);
            }
        }
        self.len = end;
        true
    }

    /// The bytes written so far.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when no record has been written.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// The `st_blksize` vibeOS reports (SYSCALL.md §3.1).
pub const STAT_BLKSIZE: u64 = 4096;

/// The portable values of a `struct stat`; the port's `Stat::from_fields`
/// lays them out. Times are whole seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatFields {
    pub ino: u64,
    /// The file type bits and the permission bits.
    pub mode: u32,
    pub nlink: u64,
    pub size: u64,
    pub blksize: u64,
    /// 512-byte blocks: ⌈size/512⌉.
    pub blocks: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
}

/// The `S_IF*` type bits for `kind`.
pub const fn mode_type(kind: InodeKind) -> u16 {
    match kind {
        InodeKind::Reg => S_IFREG,
        InodeKind::Dir => S_IFDIR,
        InodeKind::Lnk => S_IFLNK,
        InodeKind::Chr => S_IFCHR,
        InodeKind::Blk => S_IFBLK,
    }
}

/// ⌈size/512⌉.
const fn blocks_of(size: u64) -> u64 {
    size.div_ceil(512)
}

/// The values for an inode's [`Stat`]: the type bits come from its kind,
/// whatever its backend keeps in `mode`.
pub fn stat_fields(st: &Stat) -> StatFields {
    let mode = mode_type(st.kind) | (st.mode & !S_IFMT);
    StatFields {
        ino: u64::from(st.ino),
        mode: u32::from(mode),
        nlink: u64::from(st.nlink),
        size: st.size,
        blksize: STAT_BLKSIZE,
        blocks: blocks_of(st.size),
        atime: st.atime,
        mtime: st.mtime,
        ctime: st.ctime,
    }
}

/// The values for the console descriptor: a character device, mode
/// `S_IFCHR | 0o620`, as a terminal reads on Linux.
pub const fn console_stat_fields() -> StatFields {
    StatFields {
        ino: 0,
        mode: (S_IFCHR | 0o620) as u32,
        nlink: 1,
        size: 0,
        blksize: STAT_BLKSIZE,
        blocks: 0,
        atime: 0,
        mtime: 0,
        ctime: 0,
    }
}

/// The largest `tv_nsec` nanosleep(2) accepts.
const NSEC_MAX: i64 = 999_999_999;

/// `nanosleep`'s `CLOCK_MONOTONIC` deadline in nanoseconds: `now_ns` plus
/// `sec` seconds and `nsec` nanoseconds, or `EINVAL` when `nsec` is outside
/// 0..=999,999,999 or `sec` is negative. A deadline past what a `u64` holds
/// is [`FAR_DEADLINE`].
pub fn timespec_deadline(now_ns: u64, sec: i64, nsec: i64) -> Result<u64, KError> {
    if sec < 0 || !(0..=NSEC_MAX).contains(&nsec) {
        return Err(KError::Inval);
    }
    // Both are non-negative after the check above.
    let (sec, nsec) = (sec as u64, nsec as u64);
    let far = FAR_DEADLINE.ns;
    Ok(sec
        .checked_mul(1_000_000_000)
        .and_then(|ns| ns.checked_add(nsec))
        .and_then(|ns| ns.checked_add(now_ns))
        .map_or(far, |d| d.min(far)))
}

/// `LINUX_REBOOT_MAGIC1`, reboot(2).
pub const REBOOT_MAGIC1: u32 = 0xfee1_dead;
/// The four `magic2` values reboot(2) accepts.
pub const REBOOT_MAGIC2: [u32; 4] = [672_274_793, 85_072_278, 369_367_448, 537_993_216];

/// reboot(2)'s command values.
pub const REBOOT_CMD_RESTART: u32 = 0x0123_4567;
pub const REBOOT_CMD_HALT: u32 = 0xcdef_0123;
pub const REBOOT_CMD_CAD_ON: u32 = 0x89ab_cdef;
pub const REBOOT_CMD_CAD_OFF: u32 = 0;
pub const REBOOT_CMD_POWER_OFF: u32 = 0x4321_fedc;
pub const REBOOT_CMD_RESTART2: u32 = 0xa1b2_c3d4;
pub const REBOOT_CMD_SW_SUSPEND: u32 = 0xd000_fce1;
pub const REBOOT_CMD_KEXEC: u32 = 0x4558_4543;

/// A `reboot` command vibeOS carries out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RebootCmd {
    PowerOff,
    Restart,
    /// Restart with a command string, which x86_64 ignores.
    Restart2,
    CadOn,
    CadOff,
}

/// `reboot`'s argument checks after the permission check, in Linux's
/// order: `EINVAL` unless the two magic numbers are right, then `EINVAL`
/// for a command vibeOS does not carry out (`HALT`, a documented
/// divergence; `KEXEC` and `SW_SUSPEND`, as on a Linux built without them;
/// and any unknown value).
pub fn reboot_decode(magic1: i32, magic2: i32, cmd: u32) -> Result<RebootCmd, KError> {
    let einval = KError::Inval;
    if magic1 as u32 != REBOOT_MAGIC1 || !REBOOT_MAGIC2.contains(&(magic2 as u32)) {
        return Err(einval);
    }
    match cmd {
        REBOOT_CMD_POWER_OFF => Ok(RebootCmd::PowerOff),
        REBOOT_CMD_RESTART => Ok(RebootCmd::Restart),
        REBOOT_CMD_RESTART2 => Ok(RebootCmd::Restart2),
        REBOOT_CMD_CAD_ON => Ok(RebootCmd::CadOn),
        REBOOT_CMD_CAD_OFF => Ok(RebootCmd::CadOff),
        _ => Err(einval),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::S_IFMT;

    fn u64_at(b: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
    }

    #[test]
    fn dirent64_record_layout() {
        let mut buf = [0xAAu8; 64];
        let mut w = Dirent64Writer::new(&mut buf);
        assert!(w.push(7, 3, InodeKind::Reg, b"hello.txt"));
        assert!(w.push(9, 4, InodeKind::Dir, b"etc"));
        let (a, b) = (dirent64_reclen(9).unwrap(), dirent64_reclen(3).unwrap());
        assert_eq!(w.len(), a + b);
        assert_eq!((a, b), (32, 24));
        assert_eq!(u64_at(&buf, D_INO), 7);
        assert_eq!(u64_at(&buf, D_OFF), 3);
        assert_eq!(u16::from_le_bytes([buf[16], buf[17]]) as usize, a);
        assert_eq!(buf[D_TYPE], DT_REG);
        assert_eq!(&buf[D_NAME..D_NAME + 9], b"hello.txt");
        assert!(
            buf[D_NAME + 9..a].iter().all(|&x| x == 0),
            "NUL and padding"
        );
        let r2 = &buf[a..];
        assert_eq!(u64_at(r2, 0), 9);
        assert_eq!(u64_at(r2, 8), 4);
        assert_eq!(u16::from_le_bytes([r2[16], r2[17]]) as usize, b);
        assert_eq!(r2[18], DT_DIR);
        assert_eq!(&r2[19..22], b"etc");
        assert!(r2[22..b].iter().all(|&x| x == 0));
        assert_eq!(buf[a + b], 0xAA, "nothing past the last record");
        for n in 0..=64 {
            let l = dirent64_reclen(n).unwrap();
            assert_eq!(l % 8, 0);
            assert!(l > D_NAME + n);
        }
        assert_eq!(dirent64_reclen(64), Some(88));
    }

    #[test]
    fn dirent64_refuses_what_does_not_fit() {
        let mut buf = [0u8; 40];
        let mut w = Dirent64Writer::new(&mut buf);
        assert!(w.is_empty());
        assert!(w.push(1, 1, InodeKind::Dir, b"."));
        assert_eq!(w.len(), 24);
        assert!(!w.push(2, 2, InodeKind::Reg, b"too-long-to-fit"));
        assert_eq!(w.len(), 24, "a refused record writes nothing");
        let mut one = [0u8; 1];
        assert!(!Dirent64Writer::new(&mut one).push(1, 1, InodeKind::Dir, b"."));
        let mut exact = [0u8; 24];
        assert!(Dirent64Writer::new(&mut exact).push(1, 1, InodeKind::Dir, b"."));
    }

    #[test]
    fn dirent64_type_per_inode_kind() {
        assert_eq!(d_type(InodeKind::Reg), 8);
        assert_eq!(d_type(InodeKind::Dir), 4);
        assert_eq!(d_type(InodeKind::Lnk), 10);
        assert_eq!(d_type(InodeKind::Chr), 2);
        assert_eq!(d_type(InodeKind::Blk), 6);
    }

    #[test]
    fn stat_fields_per_kind() {
        let kinds = [
            (InodeKind::Reg, 0o100000),
            (InodeKind::Dir, 0o040000),
            (InodeKind::Lnk, 0o120000),
            (InodeKind::Chr, 0o020000),
            (InodeKind::Blk, 0o060000),
        ];
        for (kind, bits) in kinds {
            for mode in [0o644u16, 0o755, bits as u16 | 0o600, S_IFDIR | 0o700] {
                let st = Stat {
                    ino: 42,
                    kind,
                    mode,
                    nlink: 2,
                    size: 1025,
                    atime: 1,
                    mtime: 2,
                    ctime: 3,
                };
                let f = stat_fields(&st);
                assert_eq!(f.mode & u32::from(S_IFMT), bits, "{kind:?} {mode:o}");
                assert_eq!(f.mode & 0o7777, u32::from(mode & 0o7777));
                assert_eq!(f.ino, 42);
                assert_eq!(f.nlink, 2);
                assert_eq!(f.size, 1025);
                assert_eq!(f.blocks, 3);
                assert_eq!(f.blksize, 4096);
                assert_eq!((f.atime, f.mtime, f.ctime), (1, 2, 3));
            }
        }
        assert_eq!(blocks_of(0), 0);
        assert_eq!(blocks_of(512), 1);
        assert_eq!(blocks_of(513), 2);
        let c = console_stat_fields();
        assert_eq!(c.mode, 0o020620);
        assert_eq!(c.mode & u32::from(S_IFMT), u32::from(S_IFCHR));
    }

    #[test]
    fn timespec_rejects_what_linux_rejects() {
        let e = Err(KError::Inval);
        assert_eq!(timespec_deadline(0, 0, -1), e);
        assert_eq!(timespec_deadline(0, 0, 1_000_000_000), e);
        assert_eq!(timespec_deadline(0, -1, 0), e);
        assert_eq!(timespec_deadline(0, i64::MIN, 0), e);
        assert_eq!(timespec_deadline(5, 0, 0), Ok(5));
        assert_eq!(timespec_deadline(5, 0, 999_999_999), Ok(1_000_000_004));
        assert_eq!(timespec_deadline(100, 2, 3), Ok(2_000_000_103));
    }

    #[test]
    fn timespec_deadline_saturates() {
        let far = FAR_DEADLINE.ns;
        assert_eq!(timespec_deadline(0, i64::MAX, 0), Ok(far));
        assert_eq!(timespec_deadline(0, i64::MAX, 999_999_999), Ok(far));
        assert_eq!(timespec_deadline(u64::MAX - 1, 1, 0), Ok(far));
        assert_eq!(timespec_deadline(u64::MAX, 0, 1), Ok(far));
        assert_eq!(timespec_deadline(u64::MAX, 0, 0), Ok(far));
    }

    #[test]
    fn reboot_decode_matches_linux() {
        let m1 = REBOOT_MAGIC1 as i32;
        let einval = Err(KError::Inval);
        for m2 in [672274793, 85072278, 369367448, 537993216] {
            assert_eq!(reboot_decode(m1, m2, 0x4321fedc), Ok(RebootCmd::PowerOff));
            assert_eq!(reboot_decode(m1, m2, 0x01234567), Ok(RebootCmd::Restart));
            assert_eq!(reboot_decode(m1, m2, 0xa1b2c3d4), Ok(RebootCmd::Restart2));
            assert_eq!(reboot_decode(m1, m2, 0x89abcdef), Ok(RebootCmd::CadOn));
            assert_eq!(reboot_decode(m1, m2, 0), Ok(RebootCmd::CadOff));
            for bad in [0xcdef0123, 0x45584543, 0xd000fce1, 0x12345678, u32::MAX] {
                assert_eq!(reboot_decode(m1, m2, bad), einval, "{bad:#x}");
            }
        }
        assert_eq!(reboot_decode(0x0fee1dea, 672274793, 0), einval);
        assert_eq!(reboot_decode(0, 672274793, 0x01234567), einval);
        assert_eq!(reboot_decode(m1, 1, 0x01234567), einval);
        assert_eq!(reboot_decode(m1, 0x28121968, 0), einval);
    }
}
