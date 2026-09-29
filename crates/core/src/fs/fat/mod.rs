//! FAT32 read/write. ROADMAP §8.2–8.3.
//!
//! Library half: BPB, FAT chain (cached), 8.3 + LFN, r/w, mkdir/rmdir.
//! Dual FAT + FSInfo stay in sync. Flush order: allocate and commit FAT
//! copies before a dirent may point at the clusters. `sync` uses disk
//! [`Disk::flush`], not a barrier.
//!
//! FAT has no POSIX perms/symlinks/hard links: those ops return
//! [`FatError::NotSupp`]. Do not fake success.

use crate::fs::{FsError, InodeKind};

#[deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
mod chain;
#[deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
mod dirent;
#[deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
mod mkfs;
#[deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
mod rw;
#[deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]
mod vol;

pub use dirent::lfn_checksum;
pub use mkfs::{mkfs, mkinitrd};

use chain::{fat_loc, is_eoc};
use dirent::{decode_short, eq_ci, fat_datetime, fat_to_unix, fill_lfn, utf16_len};

pub const SEC: usize = 512;
pub const INITRD_BYTES: usize = 64 * 1024;
pub const MAX_CLUS_BYTES: usize = 4096;
pub use crate::limits::MAX_NAME;
pub const FAT_CACHE: usize = 8;
pub const EOC_MIN: u32 = 0x0FFFFFF8;
pub const BAD_CLUS: u32 = 0x0FFFFFF7;
pub const ROOT_INO: u32 = 1;

#[allow(dead_code)]
const ATTR_RO: u8 = 0x01;
#[allow(dead_code)]
const ATTR_HIDDEN: u8 = 0x02;
#[allow(dead_code)]
const ATTR_SYS: u8 = 0x04;
const ATTR_VOL: u8 = 0x08;
const ATTR_DIR: u8 = 0x10;
const ATTR_ARCH: u8 = 0x20;
const ATTR_LFN: u8 = 0x0F;
const LFN_LAST: u8 = 0x40;
const ENT_DEL: u8 = 0xE5;
const ENT_FREE: u8 = 0x00;
const ENT: usize = 32;
const LFN_CHARS: usize = 13;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FatError {
    Inval,
    Io,
    Corrupt,
    NoSpace,
    NotFound,
    Exists,
    NotDir,
    IsDir,
    NotEmpty,
    NameTooLong,
    NotSupp,
    /// A kernel heap allocation below the volume failed.
    NoMem,
}

impl FatError {
    pub fn as_str(self) -> &'static str {
        match self {
            FatError::Inval => "inval",
            FatError::Io => "io",
            FatError::Corrupt => "corrupt",
            FatError::NoSpace => "no space",
            FatError::NotFound => "not found",
            FatError::Exists => "exists",
            FatError::NotDir => "not dir",
            FatError::IsDir => "is dir",
            FatError::NotEmpty => "not empty",
            FatError::NameTooLong => "name too long",
            FatError::NotSupp => "not supp",
            FatError::NoMem => "no memory",
        }
    }

    pub fn to_fs(self) -> FsError {
        match self {
            FatError::Inval | FatError::Corrupt => FsError::Inval,
            FatError::Io => FsError::Io,
            FatError::NoSpace => FsError::NoSpace,
            FatError::NotFound => FsError::NotFound,
            FatError::Exists => FsError::Exists,
            FatError::NotDir => FsError::NotDir,
            FatError::IsDir => FsError::IsDir,
            FatError::NotEmpty => FsError::NotEmpty,
            FatError::NameTooLong => FsError::NameTooLong,
            FatError::NotSupp => FsError::NotSupp,
            FatError::NoMem => FsError::NoMem,
        }
    }
}

impl From<FatError> for FsError {
    fn from(e: FatError) -> Self {
        e.to_fs()
    }
}

/// Byte-oriented FAT sectors. `flush` is a durable write (DESIGN §10.2).
pub trait Disk {
    fn sector_size(&self) -> u32;
    fn nsectors(&self) -> u32;
    fn read(&mut self, lba: u32, buf: &mut [u8]) -> Result<(), FatError>;
    fn write(&mut self, lba: u32, buf: &[u8]) -> Result<(), FatError>;
    fn flush(&mut self) -> Result<(), FatError>;
}

pub struct MemDisk<'a> {
    data: &'a mut [u8],
    sec: u32,
}

impl<'a> MemDisk<'a> {
    pub fn new(data: &'a mut [u8], sec: u32) -> Result<Self, FatError> {
        if sec == 0 || data.len() < sec as usize || !data.len().is_multiple_of(sec as usize) {
            return Err(FatError::Inval);
        }
        Ok(Self { data, sec })
    }

    pub fn bytes(&self) -> &[u8] {
        self.data
    }
}

impl Disk for MemDisk<'_> {
    fn sector_size(&self) -> u32 {
        self.sec
    }

    fn nsectors(&self) -> u32 {
        (self.data.len() / self.sec as usize) as u32
    }

    fn read(&mut self, lba: u32, buf: &mut [u8]) -> Result<(), FatError> {
        let ss = self.sec as usize;
        if buf.len() != ss {
            return Err(FatError::Inval);
        }
        let off = (lba as usize).checked_mul(ss).ok_or(FatError::Inval)?;
        let end = off.checked_add(ss).ok_or(FatError::Inval)?;
        if end > self.data.len() {
            return Err(FatError::Io);
        }
        buf.copy_from_slice(&self.data[off..end]);
        Ok(())
    }

    fn write(&mut self, lba: u32, buf: &[u8]) -> Result<(), FatError> {
        let ss = self.sec as usize;
        if buf.len() != ss {
            return Err(FatError::Inval);
        }
        let off = (lba as usize).checked_mul(ss).ok_or(FatError::Inval)?;
        let end = off.checked_add(ss).ok_or(FatError::Inval)?;
        if end > self.data.len() {
            return Err(FatError::Io);
        }
        self.data[off..end].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), FatError> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FatInfo {
    pub bps: u32,
    pub spc: u8,
    pub rsvd: u32,
    pub num_fats: u8,
    pub fatsz: u32,
    pub totsec: u32,
    pub root_clus: u32,
    pub fsinfo: u32,
    pub backup: u32,
    pub data_lba: u32,
    pub nclus: u32,
    pub media: u8,
}

impl FatInfo {
    pub fn clus_bytes(self) -> usize {
        self.spc as usize * self.bps as usize
    }

    pub fn clus_lba(self, clu: u32) -> Result<u32, FatError> {
        if clu < 2 || clu >= self.nclus + 2 {
            return Err(FatError::Corrupt);
        }
        Ok(self.data_lba + (clu - 2) * self.spc as u32)
    }

    pub fn fat_lba(self, copy: u8, fat_sec: u32) -> Result<u32, FatError> {
        if copy >= self.num_fats || fat_sec >= self.fatsz {
            return Err(FatError::Inval);
        }
        Ok(self.rsvd + copy as u32 * self.fatsz + fat_sec)
    }

    /// Whether `clu` lies past the last cluster, `nclus + 1`; FAT entries
    /// 0 and 1 are in range.
    pub(super) fn past_end(self, clu: u32) -> bool {
        clu.checked_sub(2).is_some_and(|c| c >= self.nclus)
    }

    pub fn data_bytes(self) -> u64 {
        self.nclus as u64 * self.clus_bytes() as u64
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Node {
    pub ino: u32,
    pub kind: InodeKind,
    pub clu: u32,
    pub size: u32,
    pub dir_clu: u32,
    pub dir_off: u32,
    pub attr: u8,
    pub mtime: u64,
    pub name_len: u8,
    pub name: [u8; MAX_NAME],
}

impl Node {
    pub const EMPTY: Self = Self {
        ino: 0,
        kind: InodeKind::Reg,
        clu: 0,
        size: 0,
        dir_clu: 0,
        dir_off: 0,
        attr: 0,
        mtime: 0,
        name_len: 0,
        name: [0; MAX_NAME],
    };

    pub fn name(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }

    pub fn is_dir(self) -> bool {
        self.kind == InodeKind::Dir
    }
}

#[derive(Clone, Copy)]
struct FatSec {
    used: bool,
    dirty: bool,
    idx: u32,
    data: [u8; SEC],
}

impl FatSec {
    const EMPTY: Self = Self {
        used: false,
        dirty: false,
        idx: 0,
        data: [0; SEC],
    };
}

pub struct FatVol {
    pub info: FatInfo,
    cache: [FatSec; FAT_CACHE],
    pub hint: u32,
    pub free: u32,
    fsinfo_dirty: bool,
    pub now: u32,
}

/// A FAT file's inode words, owned by the caller (the `Vfs` inode, from
/// ROADMAP §10.4's `InodeOps` box). Its identity is its dirent location,
/// `(dir_clu, dir_off)` (the root directory is `(0, 0)`), never the first
/// cluster, which changes on an empty file's first write and on truncate
/// to 0. While the file is open these words, not the dirent, hold the
/// first cluster and the size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FatInode {
    pub dir_clu: u32,
    pub dir_off: u32,
    pub first_clu: u32,
    pub size: u64,
    pub kind: InodeKind,
}

impl FatInode {
    /// The words of the file `n` names, as its dirent holds them.
    pub fn of_node(n: &Node) -> Self {
        Self {
            dir_clu: n.dir_clu,
            dir_off: n.dir_off,
            first_clu: n.clu,
            size: u64::from(n.size),
            kind: n.kind,
        }
    }
}

/// What [`FatVol::rename`] moved: the source dirent `from` and where its
/// entry now is, `to`, each `(dir_clu, dir_off)`, and the entry the rename
/// replaced, whose clusters the caller frees once nothing holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenameMoved {
    pub from: (u32, u32),
    pub to: (u32, u32),
    pub replaced: Option<FatInode>,
}

/// The `st_ino` of the dirent at `(dir_clu, dir_off)`: [`ROOT_INO`] for
/// the root, `(dir_clu << 16) | (dir_off / 32)` when both halves fit in
/// 16 bits, and otherwise the fold `dir_clu * 0x9E3779B9 ^ (dir_off / 32)`,
/// moved past 0 and [`ROOT_INO`]. A pure function of the dirent, so both
/// the File API and `Vfs` report the same number.
pub fn stat_ino(dir_clu: u32, dir_off: u32) -> u32 {
    if dir_clu == 0 && dir_off == 0 {
        return ROOT_INO;
    }
    let idx = dir_off / ENT as u32;
    let n = if dir_clu < 0x1_0000 && idx < 0x1_0000 {
        (dir_clu << 16) | idx
    } else {
        dir_clu.wrapping_mul(0x9E37_79B9) ^ idx
    };
    if n <= ROOT_INO { n + 2 } else { n }
}

impl FatVol {
    fn nth_clu<D: Disk>(&mut self, d: &mut D, first: u32, n: u32) -> Result<Option<u32>, FatError> {
        let mut clu = first;
        let mut i = 0u32;
        while i < n {
            let next = self.fat_get(d, clu)?;
            if next < 2 || is_eoc(next) {
                return Ok(None);
            }
            clu = next;
            i += 1;
            if i > self.info.nclus {
                return Err(FatError::Corrupt);
            }
        }
        Ok(Some(clu))
    }

    fn write_cluster<D: Disk>(&mut self, d: &mut D, clu: u32, buf: &[u8]) -> Result<(), FatError> {
        let n = self.info.clus_bytes();
        if buf.len() < n {
            return Err(FatError::Inval);
        }
        let lba = self.info.clus_lba(clu)?;
        let mut i = 0u32;
        while i < self.info.spc as u32 {
            let off = i as usize * self.info.bps as usize;
            d.write(lba + i, &buf[off..off + self.info.bps as usize])?;
            i += 1;
        }
        Ok(())
    }

    fn fat_set<D: Disk>(&mut self, d: &mut D, clu: u32, val: u32) -> Result<(), FatError> {
        if self.info.past_end(clu) {
            return Err(FatError::Corrupt);
        }
        let (sec, ent_off) = fat_loc(clu)?;
        let c = self.fat_sec(d, sec)?;
        let old = le32(&c.data, ent_off)?;
        let packed = (old & 0xF000_0000) | (val & 0x0FFF_FFFF);
        put_le32(&mut c.data, ent_off, packed)?;
        c.dirty = true;
        if val == 0 && old & 0x0FFF_FFFF != 0 {
            self.free = self.free.saturating_add(1);
            self.fsinfo_dirty = true;
        } else if val != 0 && old & 0x0FFF_FFFF == 0 {
            self.free = self.free.saturating_sub(1);
            self.fsinfo_dirty = true;
        }
        Ok(())
    }

    fn alloc_clu<D: Disk>(&mut self, d: &mut D, _hint_prev: u32) -> Result<u32, FatError> {
        let start = if self.hint >= 2 { self.hint } else { 2 };
        let end = self.info.nclus + 2;
        let mut clu = start;
        let mut scanned = 0u32;
        while scanned < self.info.nclus {
            if clu >= end {
                clu = 2;
            }
            let v = self.fat_get(d, clu)?;
            if v == 0 {
                self.fat_set(d, clu, EOC_MIN)?;
                self.hint = clu.saturating_add(1);
                if self.hint >= end {
                    self.hint = 2;
                }
                self.fsinfo_dirty = true;
                return Ok(clu);
            }
            clu += 1;
            scanned += 1;
        }
        Err(FatError::NoSpace)
    }
}

/// The `N` bytes of `b` at `o`; `Corrupt` when they run past its end.
fn bytes_at<const N: usize>(b: &[u8], o: usize) -> Result<[u8; N], FatError> {
    let end = o.checked_add(N).ok_or(FatError::Corrupt)?;
    let s = b.get(o..end).ok_or(FatError::Corrupt)?;
    <[u8; N]>::try_from(s).map_err(|_| FatError::Corrupt)
}

/// Copy `v` into `b` at `o`; `Corrupt` when it runs past `b`'s end.
fn put_at(b: &mut [u8], o: usize, v: &[u8]) -> Result<(), FatError> {
    let end = o.checked_add(v.len()).ok_or(FatError::Corrupt)?;
    b.get_mut(o..end)
        .ok_or(FatError::Corrupt)?
        .copy_from_slice(v);
    Ok(())
}

fn le16(b: &[u8], o: usize) -> Result<u16, FatError> {
    bytes_at(b, o).map(u16::from_le_bytes)
}

fn le32(b: &[u8], o: usize) -> Result<u32, FatError> {
    bytes_at(b, o).map(u32::from_le_bytes)
}

fn put_le16(b: &mut [u8], o: usize, v: u16) -> Result<(), FatError> {
    put_at(b, o, &v.to_le_bytes())
}

fn put_le32(b: &mut [u8], o: usize, v: u32) -> Result<(), FatError> {
    put_at(b, o, &v.to_le_bytes())
}

fn name_is_dot(n: &[u8]) -> bool {
    n.len() == 1 && n[0] == b'.'
}

fn name_is_dotdot(n: &[u8]) -> bool {
    n.len() == 2 && n[0] == b'.' && n[1] == b'.'
}

#[cfg(test)]
mod tests;
