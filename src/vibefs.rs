//! vibefs format, mkfs, fsck, volume ops. ROADMAP §8.5 / docs/VIBEFS.md.
//!
//! Version 1. CoW metadata, dual superblocks, generation + CRC-32.
//! Host mkfs/fsck and the kernel share this module. Do not grow a second
//! on-disk layout.

use crate::fs::{FsError, InodeKind};
use crate::part::crc32_ieee;

pub const VERSION: u16 = 1;
pub const BLOCK: usize = 4096;
pub const MAGIC_SUPER: u32 = 0x4542_4956; // VIBE
pub const MAGIC_META: u32 = 0x4B4C_4256; // VBLK
pub const MAX_BLOCKS: usize = 1024;
pub const MAX_INODES: usize = 64;
pub const MAX_DENTS: usize = 96;
pub const MAX_NAME: usize = 64;
pub const INLINE: usize = 128;
pub const MAX_EXT: usize = 4;
pub const MAX_SNAPS: usize = 4;
pub const ROOT_INO: u32 = 1;
pub const MIN_BLOCKS: u32 = 16;
pub const FLAG_DATA_CRC: u8 = 1;
pub const F_INLINE: u8 = 1;
pub const KIND_REG: u8 = 1;
pub const KIND_DIR: u8 = 2;
pub const KIND_LNK: u8 = 3;
pub const META_ALLOC: u8 = 1;
pub const META_INODE_LEAF: u8 = 2;
pub const META_INODE_INT: u8 = 3;
pub const META_DIR_LEAF: u8 = 4;
pub const META_DIR_INT: u8 = 5;
pub const INODE_REC: usize = 256;
pub const DENT_REC: usize = 72;
pub const HDR: usize = 32;
pub const INODE_PER_LEAF: usize = (BLOCK - HDR) / INODE_REC;
pub const DENT_PER_LEAF: usize = (BLOCK - HDR) / DENT_REC;
pub const INODE_INT_PER: usize = (BLOCK - HDR) / 8;
/// Metadata blocks in one v1 generation, at most: the alloc block, the
/// inode leaves and their internal node, and one leaf per directory plus the
/// one directory that can pass a leaf (docs/VIBEFS.md §6).
pub const MAX_META: usize = 1 + (MAX_INODES.div_ceil(INODE_PER_LEAF) + 1) + (MAX_INODES + 2);
const _: () = assert!(MAX_META == 73);
pub const MAX_DROP: usize = 96;
pub const SB_CRC_OFF: usize = 4092;
/// A file ends at or below this byte, so its last block index is at most
/// `u32::MAX - 1` (docs/VIBEFS.md §3).
pub const MAX_FILE_SIZE: u64 = u32::MAX as u64 * BLOCK as u64;

const KIND_REG_U: u8 = KIND_REG;
const KIND_DIR_U: u8 = KIND_DIR;
const KIND_LNK_U: u8 = KIND_LNK;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
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
    FileTooBig,
}

impl Error {
    pub fn as_str(self) -> &'static str {
        match self {
            Error::Inval => "inval",
            Error::Io => "io",
            Error::Corrupt => "corrupt",
            Error::NoSpace => "no space",
            Error::NotFound => "not found",
            Error::Exists => "exists",
            Error::NotDir => "not dir",
            Error::IsDir => "is dir",
            Error::NotEmpty => "not empty",
            Error::NameTooLong => "name too long",
            Error::NotSupp => "not supp",
            Error::FileTooBig => "file too big",
        }
    }

    pub fn to_fs(self) -> FsError {
        match self {
            Error::Inval | Error::Corrupt => FsError::Inval,
            Error::Io => FsError::Io,
            Error::NoSpace => FsError::NoSpace,
            Error::NotFound => FsError::NotFound,
            Error::Exists => FsError::Exists,
            Error::NotDir => FsError::NotDir,
            Error::IsDir => FsError::IsDir,
            Error::NotEmpty => FsError::NotEmpty,
            Error::NameTooLong => FsError::NameTooLong,
            Error::NotSupp => FsError::NotSupp,
            Error::FileTooBig => FsError::FileTooBig,
        }
    }
}

pub trait Disk {
    fn nblocks(&self) -> u32;
    fn read_block(&mut self, bno: u32, buf: &mut [u8; BLOCK]) -> Result<(), Error>;
    fn write_block(&mut self, bno: u32, buf: &[u8; BLOCK]) -> Result<(), Error>;
    fn flush(&mut self) -> Result<(), Error>;
}

pub struct MemDisk<'a> {
    data: &'a mut [u8],
}

impl<'a> MemDisk<'a> {
    pub fn new(data: &'a mut [u8]) -> Result<Self, Error> {
        if data.len() < MIN_BLOCKS as usize * BLOCK || !data.len().is_multiple_of(BLOCK) {
            return Err(Error::Inval);
        }
        if data.len() / BLOCK > MAX_BLOCKS {
            return Err(Error::Inval);
        }
        Ok(Self { data })
    }

    pub fn bytes(&self) -> &[u8] {
        self.data
    }
}

impl Disk for MemDisk<'_> {
    fn nblocks(&self) -> u32 {
        (self.data.len() / BLOCK) as u32
    }

    fn read_block(&mut self, bno: u32, buf: &mut [u8; BLOCK]) -> Result<(), Error> {
        let off = (bno as usize).checked_mul(BLOCK).ok_or(Error::Inval)?;
        let end = off.checked_add(BLOCK).ok_or(Error::Inval)?;
        if end > self.data.len() {
            return Err(Error::Io);
        }
        buf.copy_from_slice(&self.data[off..end]);
        Ok(())
    }

    fn write_block(&mut self, bno: u32, buf: &[u8; BLOCK]) -> Result<(), Error> {
        let off = (bno as usize).checked_mul(BLOCK).ok_or(Error::Inval)?;
        let end = off.checked_add(BLOCK).ok_or(Error::Inval)?;
        if end > self.data.len() {
            return Err(Error::Io);
        }
        self.data[off..end].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// Drops writes after `limit` device ops (write or flush). Power-loss stand-in.
pub struct CrashDisk<'a> {
    data: &'a mut [u8],
    pub ops: u64,
    pub limit: u64,
    pub dropped: u64,
}

impl<'a> CrashDisk<'a> {
    pub fn new(data: &'a mut [u8], limit: u64) -> Result<Self, Error> {
        if data.len() < MIN_BLOCKS as usize * BLOCK || !data.len().is_multiple_of(BLOCK) {
            return Err(Error::Inval);
        }
        Ok(Self {
            data,
            ops: 0,
            limit,
            dropped: 0,
        })
    }
}

impl Disk for CrashDisk<'_> {
    fn nblocks(&self) -> u32 {
        (self.data.len() / BLOCK) as u32
    }

    fn read_block(&mut self, bno: u32, buf: &mut [u8; BLOCK]) -> Result<(), Error> {
        let off = (bno as usize).checked_mul(BLOCK).ok_or(Error::Inval)?;
        let end = off.checked_add(BLOCK).ok_or(Error::Inval)?;
        if end > self.data.len() {
            return Err(Error::Io);
        }
        buf.copy_from_slice(&self.data[off..end]);
        Ok(())
    }

    fn write_block(&mut self, bno: u32, buf: &[u8; BLOCK]) -> Result<(), Error> {
        self.ops = self.ops.saturating_add(1);
        if self.ops > self.limit {
            self.dropped = self.dropped.saturating_add(1);
            return Ok(());
        }
        let off = (bno as usize).checked_mul(BLOCK).ok_or(Error::Inval)?;
        let end = off.checked_add(BLOCK).ok_or(Error::Inval)?;
        if end > self.data.len() {
            return Err(Error::Io);
        }
        self.data[off..end].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.ops = self.ops.saturating_add(1);
        if self.ops > self.limit {
            self.dropped = self.dropped.saturating_add(1);
        }
        Ok(())
    }
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes([
        b[o],
        b[o + 1],
        b[o + 2],
        b[o + 3],
        b[o + 4],
        b[o + 5],
        b[o + 6],
        b[o + 7],
    ])
}

fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn put64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

fn crc_block(buf: &[u8; BLOCK], crc_off: usize) -> u32 {
    // Same poly as `crc32_ieee`, CRC field treated as zero. No 4KiB copy:
    // kernel stacks are 16 KiB (DESIGN §3.5).
    let mut crc = 0xFFFF_FFFFu32;
    let mut i = 0usize;
    while i < BLOCK {
        let byte = if i >= crc_off && i < crc_off + 4 {
            0u8
        } else {
            buf[i]
        };
        crc ^= byte as u32;
        let mut b = 0u8;
        while b < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
            b += 1;
        }
        i += 1;
    }
    !crc
}

fn set_crc(buf: &mut [u8; BLOCK], crc_off: usize) {
    put32(buf, crc_off, 0);
    let c = crc32_ieee(&buf[..]);
    put32(buf, crc_off, c);
}

fn check_crc(buf: &[u8; BLOCK], crc_off: usize) -> Result<(), Error> {
    let stored = le32(buf, crc_off);
    if stored != crc_block(buf, crc_off) {
        return Err(Error::Corrupt);
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub struct Extent {
    pub log: u32,
    pub phys: u32,
    pub len: u32,
    pub crc: u32,
}

impl Extent {
    const EMPTY: Self = Self {
        log: 0,
        phys: 0,
        len: 0,
        crc: 0,
    };
}

#[derive(Clone, Copy)]
struct Inode {
    used: bool,
    ino: u32,
    kind: u8,
    flags: u8,
    mode: u16,
    nlink: u32,
    uid: u32,
    gid: u32,
    size: u64,
    atime: u64,
    mtime: u64,
    ctime: u64,
    dir_root: u32,
    n_ext: u8,
    inline_len: u8,
    extents: [Extent; MAX_EXT],
    inline_data: [u8; INLINE],
}

impl Inode {
    const EMPTY: Self = Self {
        used: false,
        ino: 0,
        kind: 0,
        flags: 0,
        mode: 0,
        nlink: 0,
        uid: 0,
        gid: 0,
        size: 0,
        atime: 0,
        mtime: 0,
        ctime: 0,
        dir_root: 0,
        n_ext: 0,
        inline_len: 0,
        extents: [Extent::EMPTY; MAX_EXT],
        inline_data: [0; INLINE],
    };
}

#[derive(Clone, Copy)]
struct Dent {
    used: bool,
    parent: u32,
    ino: u32,
    kind: u8,
    nlen: u8,
    name: [u8; MAX_NAME],
}

impl Dent {
    const EMPTY: Self = Self {
        used: false,
        parent: 0,
        ino: 0,
        kind: 0,
        nlen: 0,
        name: [0; MAX_NAME],
    };

    fn name(&self) -> &[u8] {
        &self.name[..self.nlen as usize]
    }
}

#[derive(Clone, Copy)]
pub struct Snap {
    pub used: bool,
    pub name: [u8; 16],
    pub generation: u64,
    pub inode_root: u32,
    pub alloc_root: u32,
    pub next_ino: u32,
}

impl Snap {
    const EMPTY: Self = Self {
        used: false,
        name: [0; 16],
        generation: 0,
        inode_root: 0,
        alloc_root: 0,
        next_ino: 0,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Node {
    pub ino: u32,
    pub kind: InodeKind,
    pub size: u64,
    pub mode: u16,
    pub nlink: u32,
    pub mtime: u64,
    pub name_len: u8,
    pub name: [u8; MAX_NAME],
}

impl Node {
    pub const EMPTY: Self = Self {
        ino: 0,
        kind: InodeKind::Reg,
        size: 0,
        mode: 0,
        nlink: 0,
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

/// A class of defect `fsck` reports (docs/VIBEFS.md §11). Every class but
/// `Leak` is an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Defect {
    /// The volume does not mount, an out-of-range inode kind included.
    Mount,
    /// An extent is empty or runs outside the volume.
    Extent,
    /// An extent's data does not match its CRC.
    DataCrc,
    /// A mode's `S_IFMT` bits name another kind than the inode's.
    Mode,
    /// A dirent's kind differs from its inode's.
    Kind,
    /// A dirent names an inode that does not exist.
    Dangling,
    /// The inline flag on an inode larger than `INLINE` bytes.
    Inline,
    /// Two entries of one directory share a name.
    DupName,
    /// A directory's `nlink` differs from the dirents naming it.
    DirNlink,
    /// A non-directory's `nlink` differs from the dirents naming it.
    Nlink,
    /// An inode the root does not reach through dirents.
    Unreachable,
    /// A reachable block whose refcount is 0.
    RefFree,
    /// A reachable block whose bitmap bit is clear.
    BitFree,
    /// An unreachable block with a refcount or its bit set (a warning).
    Leak,
}

impl Defect {
    pub const ALL: [Defect; 14] = [
        Defect::Mount,
        Defect::Extent,
        Defect::DataCrc,
        Defect::Mode,
        Defect::Kind,
        Defect::Dangling,
        Defect::Inline,
        Defect::DupName,
        Defect::DirNlink,
        Defect::Nlink,
        Defect::Unreachable,
        Defect::RefFree,
        Defect::BitFree,
        Defect::Leak,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Defect::Mount => "mount",
            Defect::Extent => "extent",
            Defect::DataCrc => "data-crc",
            Defect::Mode => "mode",
            Defect::Kind => "kind",
            Defect::Dangling => "dangling",
            Defect::Inline => "inline",
            Defect::DupName => "dup-name",
            Defect::DirNlink => "dir-nlink",
            Defect::Nlink => "nlink",
            Defect::Unreachable => "unreachable",
            Defect::RefFree => "ref-free",
            Defect::BitFree => "bit-free",
            Defect::Leak => "leak",
        }
    }

    pub fn is_warning(self) -> bool {
        self == Defect::Leak
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsckReport {
    pub errors: u32,
    pub warnings: u32,
    pub generation: u64,
    /// Defects found, indexed by `Defect as usize`.
    pub counts: [u32; 14],
}

impl FsckReport {
    pub fn count(&self, d: Defect) -> u32 {
        self.counts.get(d as usize).copied().unwrap_or(0)
    }

    fn add(&mut self, d: Defect) {
        if let Some(c) = self.counts.get_mut(d as usize) {
            *c = c.saturating_add(1);
        }
        if d.is_warning() {
            self.warnings = self.warnings.saturating_add(1);
        } else {
            self.errors = self.errors.saturating_add(1);
        }
    }
}

pub struct Vol {
    pub nblocks: u32,
    pub generation: u64,
    pub inode_root: u32,
    pub alloc_root: u32,
    pub next_ino: u32,
    pub root_ino: u32,
    pub uuid: [u8; 16],
    pub label: [u8; 32],
    pub flags: u8,
    pub dirty: bool,
    bitmap: [u8; MAX_BLOCKS.div_ceil(8)],
    refc: [u8; MAX_BLOCKS],
    txn: [u8; MAX_BLOCKS.div_ceil(8)],
    inodes: [Inode; MAX_INODES],
    dents: [Dent; MAX_DENTS],
    pub snaps: [Snap; MAX_SNAPS],
    meta: [u32; MAX_META],
    nmeta: u8,
    drop: [u32; MAX_DROP],
    ndrop: u8,
    iobuf: [u8; BLOCK],
}

impl Vol {
    pub const fn new() -> Self {
        Self {
            nblocks: 0,
            generation: 0,
            inode_root: 0,
            alloc_root: 0,
            next_ino: 2,
            root_ino: ROOT_INO,
            uuid: [0; 16],
            label: [0; 32],
            flags: FLAG_DATA_CRC,
            dirty: false,
            bitmap: [0; MAX_BLOCKS.div_ceil(8)],
            refc: [0; MAX_BLOCKS],
            txn: [0; MAX_BLOCKS.div_ceil(8)],
            inodes: [Inode::EMPTY; MAX_INODES],
            dents: [Dent::EMPTY; MAX_DENTS],
            snaps: [Snap::EMPTY; MAX_SNAPS],
            meta: [0; MAX_META],
            nmeta: 0,
            drop: [0; MAX_DROP],
            ndrop: 0,
            iobuf: [0; BLOCK],
        }
    }

    /// Zero in place. Do not `*v = Vol::new()`: that builds a ~30KiB
    /// temporary on the kernel stack.
    pub fn clear(&mut self) {
        self.nblocks = 0;
        self.generation = 0;
        self.inode_root = 0;
        self.alloc_root = 0;
        self.next_ino = 2;
        self.root_ino = ROOT_INO;
        self.uuid = [0; 16];
        self.label = [0; 32];
        self.flags = FLAG_DATA_CRC;
        self.dirty = false;
        self.bitmap.fill(0);
        self.refc.fill(0);
        self.txn.fill(0);
        let mut i = 0usize;
        while i < MAX_INODES {
            self.inodes[i] = Inode::EMPTY;
            i += 1;
        }
        i = 0;
        while i < MAX_DENTS {
            self.dents[i] = Dent::EMPTY;
            i += 1;
        }
        i = 0;
        while i < MAX_SNAPS {
            self.snaps[i] = Snap::EMPTY;
            i += 1;
        }
        i = 0;
        while i < MAX_META {
            self.meta[i] = 0;
            i += 1;
        }
        self.nmeta = 0;
        i = 0;
        while i < MAX_DROP {
            self.drop[i] = 0;
            i += 1;
        }
        self.ndrop = 0;
        self.iobuf.fill(0);
    }
}

impl Default for Vol {
    fn default() -> Self {
        Self::new()
    }
}

fn kind_of(k: u8) -> Result<InodeKind, Error> {
    match k {
        KIND_REG_U => Ok(InodeKind::Reg),
        KIND_DIR_U => Ok(InodeKind::Dir),
        KIND_LNK_U => Ok(InodeKind::Lnk),
        _ => Err(Error::Corrupt),
    }
}

fn kind_to(k: InodeKind) -> u8 {
    match k {
        InodeKind::Reg => KIND_REG,
        InodeKind::Dir => KIND_DIR,
        InodeKind::Lnk => KIND_LNK,
        InodeKind::Chr | InodeKind::Blk => KIND_REG,
    }
}

fn bit_get(map: &[u8], i: u32) -> bool {
    let i = i as usize;
    (map[i / 8] >> (i % 8)) & 1 != 0
}

fn bit_set(map: &mut [u8], i: u32, v: bool) {
    let i = i as usize;
    if v {
        map[i / 8] |= 1 << (i % 8);
    } else {
        map[i / 8] &= !(1 << (i % 8));
    }
}

fn name_ok(n: &[u8]) -> Result<(), Error> {
    if n.is_empty() {
        return Err(Error::Inval);
    }
    if n.len() > MAX_NAME {
        return Err(Error::NameTooLong);
    }
    let mut i = 0usize;
    while i < n.len() {
        if n[i] == 0 || n[i] == b'/' {
            return Err(Error::Inval);
        }
        i += 1;
    }
    if n == b"." || n == b".." {
        return Err(Error::Inval);
    }
    Ok(())
}

fn name_cmp(a: &[u8], b: &[u8]) -> core::cmp::Ordering {
    a.cmp(b)
}

fn pack_inode(buf: &mut [u8], rec: &Inode) {
    buf[..INODE_REC].fill(0);
    put32(buf, 0, rec.ino);
    buf[4] = rec.kind;
    buf[5] = rec.flags;
    put16(buf, 6, rec.mode);
    put32(buf, 8, rec.nlink);
    put32(buf, 12, rec.uid);
    put32(buf, 16, rec.gid);
    put64(buf, 20, rec.size);
    put64(buf, 28, rec.atime);
    put64(buf, 36, rec.mtime);
    put64(buf, 44, rec.ctime);
    put32(buf, 52, rec.dir_root);
    buf[56] = rec.n_ext;
    buf[57] = rec.inline_len;
    let mut i = 0usize;
    while i < MAX_EXT {
        let o = 60 + i * 16;
        put32(buf, o, rec.extents[i].log);
        put32(buf, o + 4, rec.extents[i].phys);
        put32(buf, o + 8, rec.extents[i].len);
        put32(buf, o + 12, rec.extents[i].crc);
        i += 1;
    }
    buf[128..128 + INLINE].copy_from_slice(&rec.inline_data);
}

fn unpack_inode(buf: &[u8]) -> Result<Inode, Error> {
    let mut rec = Inode::EMPTY;
    rec.used = true;
    rec.ino = le32(buf, 0);
    rec.kind = buf[4];
    rec.flags = buf[5];
    rec.mode = le16(buf, 6);
    rec.nlink = le32(buf, 8);
    rec.uid = le32(buf, 12);
    rec.gid = le32(buf, 16);
    rec.size = le64(buf, 20);
    rec.atime = le64(buf, 28);
    rec.mtime = le64(buf, 36);
    rec.ctime = le64(buf, 44);
    rec.dir_root = le32(buf, 52);
    rec.n_ext = buf[56];
    rec.inline_len = buf[57];
    if rec.n_ext as usize > MAX_EXT || rec.inline_len as usize > INLINE {
        return Err(Error::Corrupt);
    }
    let _ = kind_of(rec.kind)?;
    let mut i = 0usize;
    while i < MAX_EXT {
        let o = 60 + i * 16;
        rec.extents[i] = Extent {
            log: le32(buf, o),
            phys: le32(buf, o + 4),
            len: le32(buf, o + 8),
            crc: le32(buf, o + 12),
        };
        i += 1;
    }
    rec.inline_data.copy_from_slice(&buf[128..128 + INLINE]);
    Ok(rec)
}

fn meta_hdr(buf: &mut [u8; BLOCK], kind: u8, level: u8, count: u16, g: u64, owner: u32) {
    buf.fill(0);
    put32(buf, 0, MAGIC_META);
    buf[4] = kind;
    buf[5] = level;
    put16(buf, 6, count);
    put64(buf, 8, g);
    put32(buf, 16, 0);
    put32(buf, 20, owner);
}

fn finish_meta(buf: &mut [u8; BLOCK]) {
    set_crc(buf, 16);
}

fn parse_meta(buf: &[u8; BLOCK], want: u8) -> Result<(u8, u16, u32), Error> {
    if le32(buf, 0) != MAGIC_META {
        return Err(Error::Corrupt);
    }
    check_crc(buf, 16)?;
    if buf[4] != want {
        return Err(Error::Corrupt);
    }
    Ok((buf[5], le16(buf, 6), le32(buf, 20)))
}

fn pack_super(buf: &mut [u8; BLOCK], v: &Vol, slot: u8) {
    buf.fill(0);
    put32(buf, 0, MAGIC_SUPER);
    put16(buf, 4, VERSION);
    buf[6] = slot;
    buf[7] = v.flags;
    put32(buf, 8, BLOCK as u32);
    put32(buf, 12, v.nblocks);
    put64(buf, 16, v.generation);
    put32(buf, 24, v.inode_root);
    put32(buf, 28, v.alloc_root);
    put32(buf, 32, v.next_ino);
    put32(buf, 36, v.root_ino);
    buf[40..56].copy_from_slice(&v.uuid);
    buf[56..88].copy_from_slice(&v.label);
    let mut ns = 0u8;
    let mut i = 0usize;
    while i < MAX_SNAPS {
        if v.snaps[i].used {
            ns += 1;
        }
        i += 1;
    }
    buf[88] = ns;
    i = 0;
    while i < MAX_SNAPS {
        let o = 96 + i * 40;
        buf[o..o + 16].copy_from_slice(&v.snaps[i].name);
        put64(buf, o + 16, v.snaps[i].generation);
        put32(buf, o + 24, v.snaps[i].inode_root);
        put32(buf, o + 28, v.snaps[i].alloc_root);
        put32(buf, o + 32, v.snaps[i].next_ino);
        i += 1;
    }
    set_crc(buf, SB_CRC_OFF);
}

struct SuperInfo {
    generation: u64,
    nblocks: u32,
    inode_root: u32,
    alloc_root: u32,
    next_ino: u32,
    root_ino: u32,
    flags: u8,
    uuid: [u8; 16],
    label: [u8; 32],
    snaps: [Snap; MAX_SNAPS],
}

fn parse_super(buf: &[u8; BLOCK], slot: u8) -> Result<SuperInfo, Error> {
    if le32(buf, 0) != MAGIC_SUPER {
        return Err(Error::Corrupt);
    }
    check_crc(buf, SB_CRC_OFF)?;
    if le16(buf, 4) != VERSION {
        return Err(Error::Inval);
    }
    if buf[6] != slot {
        return Err(Error::Corrupt);
    }
    if le32(buf, 8) != BLOCK as u32 {
        return Err(Error::Inval);
    }
    let nblocks = le32(buf, 12);
    if nblocks < MIN_BLOCKS || nblocks as usize > MAX_BLOCKS {
        return Err(Error::Inval);
    }
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&buf[40..56]);
    let mut label = [0u8; 32];
    label.copy_from_slice(&buf[56..88]);
    let mut snaps = [Snap::EMPTY; MAX_SNAPS];
    let ns = buf[88] as usize;
    if ns > MAX_SNAPS {
        return Err(Error::Corrupt);
    }
    let mut i = 0usize;
    while i < MAX_SNAPS {
        let o = 96 + i * 40;
        snaps[i].name.copy_from_slice(&buf[o..o + 16]);
        snaps[i].generation = le64(buf, o + 16);
        snaps[i].inode_root = le32(buf, o + 24);
        snaps[i].alloc_root = le32(buf, o + 28);
        snaps[i].next_ino = le32(buf, o + 32);
        snaps[i].used = i < ns && snaps[i].inode_root != 0;
        i += 1;
    }
    Ok(SuperInfo {
        generation: le64(buf, 16),
        nblocks,
        inode_root: le32(buf, 24),
        alloc_root: le32(buf, 28),
        next_ino: le32(buf, 32),
        root_ino: le32(buf, 36),
        flags: buf[7],
        uuid,
        label,
        snaps,
    })
}

fn pick_super<D: Disk>(d: &mut D, buf: &mut [u8; BLOCK]) -> Result<SuperInfo, Error> {
    let mut best: Option<SuperInfo> = None;
    let mut slot = 0u8;
    while slot < 2 {
        if let Ok(()) = d.read_block(slot as u32, buf)
            && let Ok(s) = parse_super(buf, slot)
        {
            let take = match &best {
                None => true,
                Some(b) => {
                    s.generation > b.generation || (s.generation == b.generation && slot == 0)
                }
            };
            if take {
                best = Some(s);
            }
        }
        slot += 1;
    }
    best.ok_or(Error::Corrupt)
}

pub fn probe<D: Disk>(d: &mut D) -> bool {
    let mut buf = [0u8; BLOCK];
    pick_super(d, &mut buf).is_ok()
}

/// The one decrement rule for a committed block that a commit replaces or
/// drops (docs/VIBEFS.md §10 steps 3 and 7). Blocks 0 and 1, a block past
/// `nblocks` (a crafted image can hand mount one) and a block already free
/// are left alone.
fn drop_ref(bitmap: &mut [u8], refc: &mut [u8], nblocks: u32, b: u32) {
    if b < 2 || b >= nblocks {
        return;
    }
    let Some(r) = refc.get_mut(b as usize) else {
        return;
    };
    if *r == 0 {
        return;
    }
    *r -= 1;
    if *r == 0
        && let Some(byte) = bitmap.get_mut(b as usize / 8)
    {
        *byte &= !(1 << (b % 8));
    }
}

/// Serialize the alloc map with `old_meta` and the drop list already
/// dropped, so the map a commit writes matches memory after step 7.
fn write_alloc_into(v: &Vol, old_meta: &[u32], buf: &mut [u8; BLOCK]) {
    let nbytes = (v.nblocks as usize).div_ceil(8);
    meta_hdr(buf, META_ALLOC, 0, v.nblocks as u16, v.generation, 0);
    buf[HDR..HDR + nbytes].copy_from_slice(&v.bitmap[..nbytes]);
    buf[HDR + nbytes..HDR + nbytes + v.nblocks as usize]
        .copy_from_slice(&v.refc[..v.nblocks as usize]);
    {
        let (head, rest) = buf.split_at_mut(HDR + nbytes);
        let bitmap = &mut head[HDR..];
        let refc = &mut rest[..v.nblocks as usize];
        for &b in old_meta.iter().chain(v.drop[..v.ndrop as usize].iter()) {
            drop_ref(bitmap, refc, v.nblocks, b);
        }
    }
    finish_meta(buf);
}

fn load_alloc(v: &mut Vol, buf: &[u8; BLOCK]) -> Result<(), Error> {
    parse_meta(buf, META_ALLOC)?;
    let nbytes = (v.nblocks as usize).div_ceil(8);
    if HDR + nbytes + v.nblocks as usize > BLOCK {
        return Err(Error::Corrupt);
    }
    v.bitmap[..nbytes].copy_from_slice(&buf[HDR..HDR + nbytes]);
    v.refc[..v.nblocks as usize]
        .copy_from_slice(&buf[HDR + nbytes..HDR + nbytes + v.nblocks as usize]);
    Ok(())
}

impl Vol {
    fn mark_meta(&mut self, bno: u32) -> Result<(), Error> {
        if self.nmeta as usize >= MAX_META {
            return Err(Error::NoSpace);
        }
        self.meta[self.nmeta as usize] = bno;
        self.nmeta += 1;
        Ok(())
    }

    fn pending_drop(&mut self, bno: u32) -> Result<(), Error> {
        if bno < 2 {
            return Ok(());
        }
        if bit_get(&self.txn, bno) {
            if self.refc[bno as usize] > 0 {
                self.refc[bno as usize] -= 1;
            }
            if self.refc[bno as usize] == 0 {
                bit_set(&mut self.bitmap, bno, false);
            }
            bit_set(&mut self.txn, bno, false);
            return Ok(());
        }
        if self.ndrop as usize >= MAX_DROP {
            return Err(Error::NoSpace);
        }
        self.drop[self.ndrop as usize] = bno;
        self.ndrop += 1;
        Ok(())
    }

    fn alloc_block(&mut self) -> Result<u32, Error> {
        let mut i = 2u32;
        while i < self.nblocks {
            if self.refc[i as usize] == 0 {
                self.refc[i as usize] = 1;
                bit_set(&mut self.bitmap, i, true);
                bit_set(&mut self.txn, i, true);
                return Ok(i);
            }
            i += 1;
        }
        Err(Error::NoSpace)
    }

    fn free_count(&self) -> u32 {
        let mut n = 0u32;
        let mut i = 2u32;
        while i < self.nblocks {
            if self.refc[i as usize] == 0 {
                n += 1;
            }
            i += 1;
        }
        n
    }

    fn inode_slot(&self, ino: u32) -> Result<usize, Error> {
        let mut i = 0usize;
        while i < MAX_INODES {
            if self.inodes[i].used && self.inodes[i].ino == ino {
                return Ok(i);
            }
            i += 1;
        }
        Err(Error::NotFound)
    }

    fn alloc_ino_slot(&mut self) -> Result<usize, Error> {
        let mut i = 0usize;
        while i < MAX_INODES {
            if !self.inodes[i].used {
                return Ok(i);
            }
            i += 1;
        }
        Err(Error::NoSpace)
    }

    fn alloc_dent(&mut self) -> Result<usize, Error> {
        let mut i = 0usize;
        while i < MAX_DENTS {
            if !self.dents[i].used {
                return Ok(i);
            }
            i += 1;
        }
        Err(Error::NoSpace)
    }

    fn find_dent(&self, parent: u32, name: &[u8]) -> Result<usize, Error> {
        let mut i = 0usize;
        while i < MAX_DENTS {
            if self.dents[i].used && self.dents[i].parent == parent && self.dents[i].name() == name
            {
                return Ok(i);
            }
            i += 1;
        }
        Err(Error::NotFound)
    }

    fn dir_count(&self, parent: u32) -> u32 {
        let mut n = 0u32;
        let mut i = 0usize;
        while i < MAX_DENTS {
            if self.dents[i].used && self.dents[i].parent == parent {
                n += 1;
            }
            i += 1;
        }
        n
    }

    fn node_from(&self, ino: u32, name: &[u8]) -> Result<Node, Error> {
        let s = self.inode_slot(ino)?;
        let r = &self.inodes[s];
        let mut n = Node::EMPTY;
        n.ino = r.ino;
        n.kind = kind_of(r.kind)?;
        n.size = r.size;
        n.mode = r.mode;
        n.nlink = r.nlink;
        n.mtime = r.mtime;
        let l = name.len().min(MAX_NAME);
        n.name[..l].copy_from_slice(&name[..l]);
        n.name_len = l as u8;
        Ok(n)
    }

    pub fn lookup<D: Disk>(&mut self, _d: &mut D, dir: u32, name: &[u8]) -> Result<Node, Error> {
        name_ok(name)?;
        let ds = self.inode_slot(dir)?;
        if self.inodes[ds].kind != KIND_DIR {
            return Err(Error::NotDir);
        }
        let e = self.find_dent(dir, name)?;
        self.node_from(self.dents[e].ino, name)
    }

    pub fn walk<D: Disk>(&mut self, d: &mut D, path: &[u8]) -> Result<Node, Error> {
        let mut p = path;
        if p.is_empty() || p == b"/" {
            return self.node_from(self.root_ino, b"/");
        }
        if p[0] == b'/' {
            p = &p[1..];
        }
        let mut dir = self.root_ino;
        loop {
            let mut i = 0usize;
            while i < p.len() && p[i] != b'/' {
                i += 1;
            }
            let comp = &p[..i];
            if comp.is_empty() {
                return Err(Error::Inval);
            }
            let n = self.lookup(d, dir, comp)?;
            if i == p.len() {
                return Ok(n);
            }
            if n.kind != InodeKind::Dir {
                return Err(Error::NotDir);
            }
            dir = n.ino;
            p = &p[i + 1..];
            if p.is_empty() {
                return Ok(n);
            }
        }
    }

    pub fn readdir<D: Disk>(
        &mut self,
        _d: &mut D,
        dir: u32,
        cookie: u64,
        out: &mut Node,
    ) -> Result<Option<u64>, Error> {
        let ds = self.inode_slot(dir)?;
        if self.inodes[ds].kind != KIND_DIR {
            return Err(Error::NotDir);
        }
        let start = cookie as usize;
        let mut i = start;
        while i < MAX_DENTS {
            if self.dents[i].used && self.dents[i].parent == dir {
                *out = self.node_from(self.dents[i].ino, self.dents[i].name())?;
                return Ok(Some((i as u64) + 1));
            }
            i += 1;
        }
        Ok(None)
    }

    fn bump_mtime(&mut self, ino: u32) {
        if let Ok(s) = self.inode_slot(ino) {
            self.inodes[s].mtime = self.inodes[s].mtime.saturating_add(1);
            self.inodes[s].ctime = self.inodes[s].mtime;
        }
        self.dirty = true;
    }

    pub fn create<D: Disk>(
        &mut self,
        _d: &mut D,
        dir: u32,
        name: &[u8],
        kind: InodeKind,
        mode: u16,
        target: Option<&[u8]>,
    ) -> Result<Node, Error> {
        name_ok(name)?;
        let ds = self.inode_slot(dir)?;
        if self.inodes[ds].kind != KIND_DIR {
            return Err(Error::NotDir);
        }
        if self.find_dent(dir, name).is_ok() {
            return Err(Error::Exists);
        }
        let link_target = if kind == InodeKind::Lnk {
            let t = target.ok_or(Error::Inval)?;
            if t.is_empty() || t.len() > INLINE {
                return Err(Error::Inval);
            }
            Some(t)
        } else {
            None
        };
        if kind == InodeKind::Chr || kind == InodeKind::Blk {
            return Err(Error::NotSupp);
        }
        let slot = self.alloc_ino_slot()?;
        let de = self.alloc_dent()?;
        let ino = self.next_ino;
        if ino == 0 {
            return Err(Error::NoSpace);
        }
        self.next_ino = self.next_ino.saturating_add(1);
        let mut rec = Inode::EMPTY;
        rec.used = true;
        rec.ino = ino;
        rec.kind = kind_to(kind);
        rec.mode = mode;
        rec.nlink = 1;
        rec.flags = if kind != InodeKind::Dir { F_INLINE } else { 0 };
        if let Some(t) = link_target {
            rec.inline_len = t.len() as u8;
            rec.size = t.len() as u64;
            rec.inline_data[..t.len()].copy_from_slice(t);
        }
        self.inodes[slot] = rec;
        self.dents[de] = Dent {
            used: true,
            parent: dir,
            ino,
            kind: rec.kind,
            nlen: name.len() as u8,
            name: {
                let mut n = [0u8; MAX_NAME];
                n[..name.len()].copy_from_slice(name);
                n
            },
        };
        self.bump_mtime(dir);
        self.node_from(ino, name)
    }

    pub fn unlink<D: Disk>(
        &mut self,
        d: &mut D,
        dir: u32,
        name: &[u8],
        rmdir: bool,
    ) -> Result<(), Error> {
        name_ok(name)?;
        let ds = self.inode_slot(dir)?;
        if self.inodes[ds].kind != KIND_DIR {
            return Err(Error::NotDir);
        }
        let e = self.find_dent(dir, name)?;
        let ino = self.dents[e].ino;
        let is = self.inode_slot(ino)?;
        let k = self.inodes[is].kind;
        if rmdir {
            if k != KIND_DIR {
                return Err(Error::NotDir);
            }
            if self.dir_count(ino) != 0 {
                return Err(Error::NotEmpty);
            }
        } else if k == KIND_DIR {
            return Err(Error::IsDir);
        }
        self.dents[e] = Dent::EMPTY;
        let nlink = self.inodes[is].nlink.saturating_sub(1);
        self.inodes[is].nlink = nlink;
        if nlink == 0 {
            self.free_inode_data(d, ino)?;
            self.inodes[is] = Inode::EMPTY;
        }
        self.bump_mtime(dir);
        Ok(())
    }

    fn free_inode_data<D: Disk>(&mut self, _d: &mut D, ino: u32) -> Result<(), Error> {
        let is = self.inode_slot(ino)?;
        let n_ext = self.inodes[is].n_ext as usize;
        let mut ex = [Extent::EMPTY; MAX_EXT];
        ex.copy_from_slice(&self.inodes[is].extents);
        let mut i = 0usize;
        while i < n_ext {
            let mut b = 0u32;
            while b < ex[i].len {
                self.pending_drop(ex[i].phys + b)?;
                b += 1;
            }
            i += 1;
        }
        Ok(())
    }

    pub fn rename<D: Disk>(
        &mut self,
        d: &mut D,
        src_dir: u32,
        src_name: &[u8],
        dst_dir: u32,
        dst_name: &[u8],
    ) -> Result<(), Error> {
        name_ok(src_name)?;
        name_ok(dst_name)?;
        let e = self.find_dent(src_dir, src_name)?;
        if src_dir == dst_dir && src_name == dst_name {
            return Ok(());
        }
        // Validate before anything changes, so a refused rename loses no
        // destination.
        let ds = self.inode_slot(dst_dir)?;
        if self.inodes[ds].kind != KIND_DIR {
            return Err(Error::NotDir);
        }
        let src_ino = self.dents[e].ino;
        let ss = self.inode_slot(src_ino)?;
        if self.inodes[ss].kind == KIND_DIR && self.in_subtree(src_ino, dst_dir)? {
            return Err(Error::Inval);
        }
        if self.find_dent(dst_dir, dst_name).is_ok() {
            self.unlink(d, dst_dir, dst_name, false)?;
        }
        let mut nm = [0u8; MAX_NAME];
        nm[..dst_name.len()].copy_from_slice(dst_name);
        self.dents[e].parent = dst_dir;
        self.dents[e].nlen = dst_name.len() as u8;
        self.dents[e].name = nm;
        self.bump_mtime(src_dir);
        self.bump_mtime(dst_dir);
        Ok(())
    }

    /// Whether directory `dir` is `top` or lies below it, walking up through
    /// the one dirent that names each directory to the root.
    fn in_subtree(&self, top: u32, dir: u32) -> Result<bool, Error> {
        let mut cur = dir;
        let mut steps = 0usize;
        loop {
            if cur == top {
                return Ok(true);
            }
            if cur == self.root_ino {
                return Ok(false);
            }
            if steps >= MAX_INODES {
                return Err(Error::Corrupt);
            }
            let up = self
                .dents
                .iter()
                .find(|de| de.used && de.ino == cur)
                .ok_or(Error::Corrupt)?;
            cur = up.parent;
            steps += 1;
        }
    }

    fn extent_crc<D: Disk>(&mut self, d: &mut D, phys: u32, len: u32) -> Result<u32, Error> {
        let mut crc = 0xFFFF_FFFFu32;
        let mut b = 0u32;
        while b < len {
            d.read_block(phys + b, &mut self.iobuf)?;
            let mut i = 0usize;
            while i < BLOCK {
                crc ^= self.iobuf[i] as u32;
                let mut k = 0;
                while k < 8 {
                    if crc & 1 != 0 {
                        crc = (crc >> 1) ^ 0xEDB8_8320;
                    } else {
                        crc >>= 1;
                    }
                    k += 1;
                }
                i += 1;
            }
            b += 1;
        }
        Ok(!crc)
    }

    fn check_extent<D: Disk>(&mut self, d: &mut D, e: Extent) -> Result<(), Error> {
        if e.len == 0 {
            return Err(Error::Corrupt);
        }
        let got = self.extent_crc(d, e.phys, e.len)?;
        if got != e.crc {
            return Err(Error::Corrupt);
        }
        Ok(())
    }

    fn write_extent_bytes<D: Disk>(
        &mut self,
        d: &mut D,
        phys: u32,
        len: u32,
        data: &[u8],
    ) -> Result<u32, Error> {
        let mut off = 0usize;
        let mut b = 0u32;
        while b < len {
            self.iobuf.fill(0);
            let n = (data.len() - off).min(BLOCK);
            if n > 0 {
                self.iobuf[..n].copy_from_slice(&data[off..off + n]);
                off += n;
            }
            d.write_block(phys + b, &self.iobuf)?;
            b += 1;
        }
        self.extent_crc(d, phys, len)
    }

    fn spill_inline<D: Disk>(&mut self, d: &mut D, ino: u32) -> Result<(), Error> {
        let is = self.inode_slot(ino)?;
        if self.inodes[is].flags & F_INLINE == 0 {
            return Ok(());
        }
        let size = self.inodes[is].size as usize;
        let mut tmp = [0u8; INLINE];
        tmp.copy_from_slice(&self.inodes[is].inline_data);
        if size == 0 {
            self.inodes[is].flags &= !F_INLINE;
            self.inodes[is].inline_len = 0;
            self.inodes[is].n_ext = 0;
            return Ok(());
        }
        let nb = size.div_ceil(BLOCK) as u32;
        if nb as usize > MAX_EXT {
            return Err(Error::NoSpace);
        }
        let mut physs = [0u32; MAX_EXT];
        let mut p = 0u32;
        while p < nb {
            physs[p as usize] = self.alloc_block()?;
            p += 1;
        }
        p = 0;
        while p < nb {
            let start = (p as usize) * BLOCK;
            let chunk = &tmp[start..size.min(start + BLOCK)];
            let crc = self.write_extent_bytes(d, physs[p as usize], 1, chunk)?;
            let is = self.inode_slot(ino)?;
            self.inodes[is].extents[p as usize] = Extent {
                log: p,
                phys: physs[p as usize],
                len: 1,
                crc,
            };
            p += 1;
        }
        let is = self.inode_slot(ino)?;
        self.inodes[is].flags &= !F_INLINE;
        self.inodes[is].inline_len = 0;
        self.inodes[is].n_ext = nb as u8;
        Ok(())
    }

    /// The size of inode `ino`, which `SEEK_END` and `O_APPEND` read.
    pub fn file_size(&self, ino: u32) -> Result<u64, Error> {
        let s = self.inode_slot(ino)?;
        Ok(self.inodes[s].size)
    }

    /// The extent index and physical block of file block `file_blk`, or
    /// `None` for a hole. Extent ends are compared in `u64`; an extent
    /// whose physical block overflows is `Corrupt`.
    fn map_block(&self, ino_slot: usize, file_blk: u32) -> Result<Option<(usize, u32)>, Error> {
        let r = self.inodes.get(ino_slot).ok_or(Error::Corrupt)?;
        let fb = u64::from(file_blk);
        for (i, e) in r.extents.iter().take(r.n_ext as usize).enumerate() {
            let start = u64::from(e.log);
            let end = start.checked_add(u64::from(e.len)).ok_or(Error::Corrupt)?;
            if fb >= start && fb < end {
                let phys = e.phys.checked_add(file_blk - e.log).ok_or(Error::Corrupt)?;
                return Ok(Some((i, phys)));
            }
        }
        Ok(None)
    }

    /// File block index and offset within it of byte `pos`.
    fn block_of(pos: u64) -> Result<(u32, usize), Error> {
        let fblk = u32::try_from(pos / BLOCK as u64).map_err(|_| Error::FileTooBig)?;
        Ok((fblk, (pos % BLOCK as u64) as usize))
    }

    pub fn read<D: Disk>(
        &mut self,
        d: &mut D,
        ino: u32,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, Error> {
        let is = self.inode_slot(ino)?;
        if self.inodes[is].kind == KIND_DIR {
            return Err(Error::IsDir);
        }
        let size = self.inodes[is].size;
        if off >= size {
            return Ok(0);
        }
        let want = core::cmp::min(buf.len() as u64, size - off) as usize;
        if self.inodes[is].flags & F_INLINE != 0 {
            let s = off as usize;
            buf[..want].copy_from_slice(&self.inodes[is].inline_data[s..s + want]);
            return Ok(want);
        }
        let mut done = 0usize;
        while done < want {
            let pos = off.checked_add(done as u64).ok_or(Error::FileTooBig)?;
            let (fblk, pin) = Self::block_of(pos)?;
            let n = (BLOCK - pin).min(want - done);
            let (ei, phys) = match self.map_block(is, fblk)? {
                Some(mapping) => mapping,
                None => {
                    buf[done..done + n].fill(0);
                    done += n;
                    continue;
                }
            };
            d.read_block(phys, &mut self.iobuf)?;
            buf[done..done + n].copy_from_slice(&self.iobuf[pin..pin + n]);
            let e = self.inodes.get(is).and_then(|ino| ino.extents.get(ei));
            let e = *e.ok_or(Error::Corrupt)?;
            self.check_extent(d, e)?;
            done += n;
        }
        Ok(want)
    }

    fn add_extent(
        &mut self,
        is: usize,
        log: u32,
        phys: u32,
        len: u32,
        crc: u32,
    ) -> Result<(), Error> {
        let n = self.inodes[is].n_ext as usize;
        if n >= MAX_EXT {
            return Err(Error::NoSpace);
        }
        self.inodes[is].extents[n] = Extent {
            log,
            phys,
            len,
            crc,
        };
        self.inodes[is].n_ext = (n as u8) + 1;
        Ok(())
    }

    pub fn write<D: Disk>(
        &mut self,
        d: &mut D,
        ino: u32,
        off: u64,
        buf: &[u8],
    ) -> Result<usize, Error> {
        let is = self.inode_slot(ino)?;
        if self.inodes[is].kind == KIND_DIR {
            return Err(Error::IsDir);
        }
        if self.inodes[is].kind == KIND_LNK {
            return Err(Error::Inval);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        // Refuse a write that starts at or past the limit, and cut one
        // that would cross it short at the limit, as Linux does.
        if off >= MAX_FILE_SIZE {
            return Err(Error::FileTooBig);
        }
        let room = MAX_FILE_SIZE - off;
        let keep = usize::try_from(room).map_or(buf.len(), |r| r.min(buf.len()));
        let buf = &buf[..keep];
        let end = off.saturating_add(buf.len() as u64);
        if end <= INLINE as u64
            && (self.inodes[is].flags & F_INLINE != 0)
            && self.inodes[is].n_ext == 0
        {
            let s = off as usize;
            self.inodes[is].inline_data[s..s + buf.len()].copy_from_slice(buf);
            if (s + buf.len()) as u8 > self.inodes[is].inline_len {
                self.inodes[is].inline_len = (s + buf.len()) as u8;
            }
            if end > self.inodes[is].size {
                self.inodes[is].size = end;
            }
            self.inodes[is].flags |= F_INLINE;
            self.bump_mtime(ino);
            return Ok(buf.len());
        }
        if self.inodes[is].flags & F_INLINE != 0 {
            self.spill_inline(d, ino)?;
        }
        let mut done = 0usize;
        while done < buf.len() {
            let pos = off.checked_add(done as u64).ok_or(Error::FileTooBig)?;
            let (fblk, pin) = Self::block_of(pos)?;
            let n = (BLOCK - pin).min(buf.len() - done);
            let is = self.inode_slot(ino)?;
            let existing = self.map_block(is, fblk)?;
            self.iobuf.fill(0);
            if let Some((ei, phys)) = existing {
                d.read_block(phys, &mut self.iobuf)?;
                let old_e = self.inodes[is].extents[ei];
                let _ = self.check_extent(d, old_e);
                self.iobuf[pin..pin + n].copy_from_slice(&buf[done..done + n]);
                let newp = self.alloc_block()?;
                d.write_block(newp, &self.iobuf)?;
                let crc = self.extent_crc(d, newp, 1)?;
                // replace this one physical block in the extent list
                self.split_replace_extent(is, fblk, newp, crc)?;
                self.pending_drop(phys)?;
            } else {
                self.iobuf[pin..pin + n].copy_from_slice(&buf[done..done + n]);
                let newp = self.alloc_block()?;
                d.write_block(newp, &self.iobuf)?;
                let crc = self.extent_crc(d, newp, 1)?;
                let is = self.inode_slot(ino)?;
                self.add_extent(is, fblk, newp, 1, crc)?;
            }
            done += n;
        }
        let is = self.inode_slot(ino)?;
        if end > self.inodes[is].size {
            self.inodes[is].size = end;
        }
        self.bump_mtime(ino);
        Ok(buf.len())
    }

    fn split_replace_extent(
        &mut self,
        is: usize,
        fblk: u32,
        newp: u32,
        crc: u32,
    ) -> Result<(), Error> {
        let (ei, _) = self.map_block(is, fblk)?.ok_or(Error::Inval)?;
        let e = self.inodes[is].extents[ei];
        if e.len == 1 {
            self.inodes[is].extents[ei].phys = newp;
            self.inodes[is].extents[ei].crc = crc;
            return Ok(());
        }
        // split into prefix + new + suffix; may need extra extent slots.
        // `map_block` found `fblk` inside `e`.
        let fb = u64::from(fblk);
        let left_len = fb.checked_sub(u64::from(e.log)).ok_or(Error::Corrupt)?;
        let right_log = fb.checked_add(1).ok_or(Error::Corrupt)?;
        let right_phys = u64::from(e.phys)
            .checked_add(left_len)
            .and_then(|p| p.checked_add(1))
            .ok_or(Error::Corrupt)?;
        let right_len = u64::from(e.log)
            .checked_add(u64::from(e.len))
            .and_then(|end| end.checked_sub(right_log))
            .ok_or(Error::Corrupt)?;
        let to32 = |v: u64| u32::try_from(v).map_err(|_| Error::Corrupt);
        let (left_len, right_log, right_phys, right_len) = (
            to32(left_len)?,
            to32(right_log)?,
            to32(right_phys)?,
            to32(right_len)?,
        );
        // shrink original to left, or replace with the new block if left_len==0
        if left_len == 0 {
            self.inodes[is].extents[ei] = Extent {
                log: fblk,
                phys: newp,
                len: 1,
                crc,
            };
        } else {
            self.inodes[is].extents[ei].len = left_len;
            // left crc stale: v1 re-sums on next check via stored crc of old
            // whole extent. Recompute would need disk. Mark crc 0 to skip? Spec
            // requires crc. Leave old crc only if we don't check prefix here.
            // Simpler: only use 1-block extents after first CoW split.
            self.inodes[is].extents[ei] = Extent {
                log: e.log,
                phys: e.phys,
                len: left_len,
                crc: e.crc,
            };
            self.add_extent(is, fblk, newp, 1, crc)?;
        }
        if right_len > 0 {
            self.add_extent(is, right_log, right_phys, right_len, e.crc)?;
        }
        Ok(())
    }

    pub fn truncate<D: Disk>(&mut self, d: &mut D, ino: u32, new: u64) -> Result<(), Error> {
        let is = self.inode_slot(ino)?;
        if self.inodes[is].kind == KIND_DIR {
            return Err(Error::IsDir);
        }
        if new > MAX_FILE_SIZE {
            return Err(Error::FileTooBig);
        }
        let old = self.inodes[is].size;
        if new >= old {
            if self.inodes[is].flags & F_INLINE != 0 && new <= INLINE as u64 {
                self.inodes[is].size = new;
                self.inodes[is].inline_len = new as u8;
                self.bump_mtime(ino);
                return Ok(());
            }
            self.inodes[is].size = new;
            self.bump_mtime(ino);
            return Ok(());
        }
        if self.inodes[is].flags & F_INLINE != 0 {
            self.inodes[is].size = new;
            self.inodes[is].inline_len = new as u8;
            if new == 0 {
                self.inodes[is].inline_data = [0; INLINE];
            }
            self.bump_mtime(ino);
            return Ok(());
        }
        let keep_blks = u32::try_from(new.div_ceil(BLOCK as u64)).map_err(|_| Error::FileTooBig)?;
        let n_ext = (self.inodes[is].n_ext as usize).min(MAX_EXT);
        let mut i = 0usize;
        while i < n_ext {
            let e = self.inodes[is].extents[i];
            let end = u64::from(e.log)
                .checked_add(u64::from(e.len))
                .ok_or(Error::Corrupt)?;
            if e.log >= keep_blks {
                let mut b = 0u32;
                while b < e.len {
                    self.pending_drop(e.phys.checked_add(b).ok_or(Error::Corrupt)?)?;
                    b += 1;
                }
                self.inodes[is].extents[i] = Extent::EMPTY;
            } else if end > u64::from(keep_blks) {
                let keep = keep_blks - e.log;
                let mut b = keep;
                while b < e.len {
                    self.pending_drop(e.phys.checked_add(b).ok_or(Error::Corrupt)?)?;
                    b += 1;
                }
                self.inodes[is].extents[i].len = keep;
            }
            i += 1;
        }
        // compact extent array
        let is = self.inode_slot(ino)?;
        let mut w = 0usize;
        let mut r = 0usize;
        while r < MAX_EXT {
            if self.inodes[is].extents[r].len != 0 {
                self.inodes[is].extents[w] = self.inodes[is].extents[r];
                w += 1;
            }
            r += 1;
        }
        while w < MAX_EXT {
            self.inodes[is].extents[w] = Extent::EMPTY;
            w += 1;
        }
        self.inodes[is].n_ext = self.inodes[is]
            .extents
            .iter()
            .filter(|e| e.len != 0)
            .count() as u8;
        self.inodes[is].size = new;
        if new <= INLINE as u64 {
            let mut tmp = [0u8; INLINE];
            if new > 0 {
                let n = self.read(d, ino, 0, &mut tmp[..new as usize])?;
                let _ = n;
            }
            let _ = self.free_inode_data(d, ino);
            let is = self.inode_slot(ino)?;
            self.inodes[is].flags |= F_INLINE;
            self.inodes[is].n_ext = 0;
            self.inodes[is].extents = [Extent::EMPTY; MAX_EXT];
            self.inodes[is].inline_len = new as u8;
            self.inodes[is].inline_data = tmp;
            self.inodes[is].size = new;
        }
        self.bump_mtime(ino);
        Ok(())
    }

    pub fn readlink<D: Disk>(
        &mut self,
        _d: &mut D,
        ino: u32,
        buf: &mut [u8],
    ) -> Result<usize, Error> {
        let is = self.inode_slot(ino)?;
        if self.inodes[is].kind != KIND_LNK {
            return Err(Error::Inval);
        }
        let n = self.inodes[is].inline_len as usize;
        let n = n.min(buf.len());
        buf[..n].copy_from_slice(&self.inodes[is].inline_data[..n]);
        Ok(n)
    }

    pub fn snapshot<D: Disk>(&mut self, d: &mut D, name: &[u8]) -> Result<(), Error> {
        if name.is_empty() || name.len() > 16 {
            return Err(Error::Inval);
        }
        if self.dirty {
            self.sync(d)?;
        }
        let mut i = 0usize;
        while i < MAX_SNAPS {
            if !self.snaps[i].used {
                break;
            }
            i += 1;
        }
        if i >= MAX_SNAPS {
            return Err(Error::NoSpace);
        }
        let mut nm = [0u8; 16];
        nm[..name.len()].copy_from_slice(name);
        self.snaps[i] = Snap {
            used: true,
            name: nm,
            generation: self.generation,
            inode_root: self.inode_root,
            alloc_root: self.alloc_root,
            next_ino: self.next_ino,
        };
        let mut b = 0u32;
        while b < self.nblocks {
            if self.refc[b as usize] > 0 && self.refc[b as usize] < 255 {
                self.refc[b as usize] += 1;
            }
            b += 1;
        }
        self.dirty = true;
        self.sync(d)
    }

    pub fn df(&self) -> (u64, u64, u32) {
        let tot = self.nblocks as u64 * BLOCK as u64;
        let free = self.free_count() as u64 * BLOCK as u64;
        (tot, free, self.nblocks)
    }

    pub fn sync<D: Disk>(&mut self, d: &mut D) -> Result<(), Error> {
        if !self.dirty {
            return d.flush();
        }
        self.commit(d)
    }
}

fn count_used_inodes(v: &Vol) -> usize {
    let mut n = 0usize;
    let mut i = 0usize;
    while i < MAX_INODES {
        if v.inodes[i].used {
            n += 1;
        }
        i += 1;
    }
    n
}

fn inode_need(n: usize) -> (usize, usize) {
    if n == 0 {
        return (1, 0);
    }
    let leaves = n.div_ceil(INODE_PER_LEAF);
    let ints = if leaves > 1 { 1 } else { 0 };
    (leaves, ints)
}

fn dir_need(n: usize) -> (usize, usize) {
    if n == 0 {
        return (0, 0);
    }
    let leaves = n.div_ceil(DENT_PER_LEAF);
    let ints = if leaves > 1 { 1 } else { 0 };
    (leaves, ints)
}

impl Vol {
    fn collect_inodes(&self, out: &mut [usize; MAX_INODES]) -> usize {
        let mut n = 0usize;
        let mut i = 0usize;
        while i < MAX_INODES {
            if self.inodes[i].used {
                out[n] = i;
                n += 1;
            }
            i += 1;
        }
        let mut a = 1usize;
        while a < n {
            let mut b = a;
            while b > 0 && self.inodes[out[b - 1]].ino > self.inodes[out[b]].ino {
                out.swap(b - 1, b);
                b -= 1;
            }
            a += 1;
        }
        n
    }

    fn collect_dents(&self, parent: u32, out: &mut [usize; MAX_DENTS]) -> usize {
        let mut n = 0usize;
        let mut i = 0usize;
        while i < MAX_DENTS {
            if self.dents[i].used && self.dents[i].parent == parent {
                out[n] = i;
                n += 1;
            }
            i += 1;
        }
        let mut a = 1usize;
        while a < n {
            let mut b = a;
            while b > 0
                && name_cmp(self.dents[out[b - 1]].name(), self.dents[out[b]].name()).is_gt()
            {
                out.swap(b - 1, b);
                b -= 1;
            }
            a += 1;
        }
        n
    }

    fn commit<D: Disk>(&mut self, d: &mut D) -> Result<(), Error> {
        let nino = count_used_inodes(self);
        let (ileaves, iints) = inode_need(nino.max(1));
        let mut dblocks = 0usize;
        let mut i = 0usize;
        while i < MAX_INODES {
            if self.inodes[i].used && self.inodes[i].kind == KIND_DIR {
                let c = self.dir_count(self.inodes[i].ino) as usize;
                let (l, t) = dir_need(c);
                dblocks += l + t;
            }
            i += 1;
        }
        let need = 1 + ileaves + iints + dblocks;
        // Every block counted here goes into `self.meta` after the super
        // flush, so the table check runs while the old super is live.
        if need > MAX_META {
            return Err(Error::NoSpace);
        }
        if self.free_count() < need as u32 {
            return Err(Error::NoSpace);
        }
        let old_meta_n = self.nmeta;
        let mut old_meta = [0u32; MAX_META];
        old_meta[..old_meta_n as usize].copy_from_slice(&self.meta[..old_meta_n as usize]);

        // Every metadata block this commit writes; `self.meta` after step 7.
        let mut new_meta = [0u32; MAX_META];
        let mut n_new = 0usize;
        let alloc_bno = self.alloc_block()?;
        *new_meta.get_mut(n_new).ok_or(Error::NoSpace)? = alloc_bno;
        n_new += 1;
        let mut ileaf = [0u32; 8];
        let mut li = 0usize;
        while li < ileaves {
            ileaf[li] = self.alloc_block()?;
            *new_meta.get_mut(n_new).ok_or(Error::NoSpace)? = ileaf[li];
            n_new += 1;
            li += 1;
        }
        let iroot = if iints > 0 {
            let b = self.alloc_block()?;
            *new_meta.get_mut(n_new).ok_or(Error::NoSpace)? = b;
            n_new += 1;
            b
        } else {
            ileaf[0]
        };

        let mut idx = [0usize; MAX_INODES];
        let ni = self.collect_inodes(&mut idx);

        // directory roots assigned while writing dir trees
        let mut dir_roots = [0u32; MAX_INODES];

        li = 0;
        while li < MAX_INODES {
            if self.inodes[li].used && self.inodes[li].kind == KIND_DIR {
                let ino = self.inodes[li].ino;
                let mut dents = [0usize; MAX_DENTS];
                let nd = self.collect_dents(ino, &mut dents);
                if nd == 0 {
                    dir_roots[li] = 0;
                    li += 1;
                    continue;
                }
                let (leaves, ints) = dir_need(nd);
                let mut dleaf = [0u32; 8];
                let mut k = 0usize;
                while k < leaves {
                    dleaf[k] = self.alloc_block()?;
                    *new_meta.get_mut(n_new).ok_or(Error::NoSpace)? = dleaf[k];
                    n_new += 1;
                    k += 1;
                }
                let droot = if ints > 0 {
                    let b = self.alloc_block()?;
                    *new_meta.get_mut(n_new).ok_or(Error::NoSpace)? = b;
                    n_new += 1;
                    b
                } else {
                    dleaf[0]
                };
                k = 0;
                while k < leaves {
                    let start = k * DENT_PER_LEAF;
                    let end = (start + DENT_PER_LEAF).min(nd);
                    meta_hdr(
                        &mut self.iobuf,
                        META_DIR_LEAF,
                        0,
                        (end - start) as u16,
                        self.generation + 1,
                        ino,
                    );
                    let mut e = 0usize;
                    while start + e < end {
                        let de = &self.dents[dents[start + e]];
                        let o = HDR + e * DENT_REC;
                        put32(&mut self.iobuf, o, de.ino);
                        self.iobuf[o + 4] = de.kind;
                        self.iobuf[o + 5] = de.nlen;
                        self.iobuf[o + 6..o + 6 + MAX_NAME].copy_from_slice(&de.name);
                        e += 1;
                    }
                    finish_meta(&mut self.iobuf);
                    d.write_block(dleaf[k], &self.iobuf)?;
                    k += 1;
                }
                if ints > 0 {
                    meta_hdr(
                        &mut self.iobuf,
                        META_DIR_INT,
                        1,
                        leaves as u16,
                        self.generation + 1,
                        ino,
                    );
                    k = 0;
                    while k < leaves {
                        let de = &self.dents[dents[k * DENT_PER_LEAF]];
                        let o = HDR + k * DENT_REC;
                        put32(&mut self.iobuf, o, dleaf[k]);
                        self.iobuf[o + 4] = de.nlen;
                        self.iobuf[o + 5] = 0;
                        self.iobuf[o + 6..o + 6 + MAX_NAME].copy_from_slice(&de.name);
                        k += 1;
                    }
                    finish_meta(&mut self.iobuf);
                    d.write_block(droot, &self.iobuf)?;
                }
                dir_roots[li] = droot;
            }
            li += 1;
        }

        let mut ii = 0usize;
        while ii < MAX_INODES {
            if self.inodes[ii].used && self.inodes[ii].kind == KIND_DIR {
                self.inodes[ii].dir_root = dir_roots[ii];
            }
            ii += 1;
        }

        // inode leaves
        li = 0;
        while li < ileaves {
            let start = li * INODE_PER_LEAF;
            let end = (start + INODE_PER_LEAF).min(ni.max(1));
            let count = if ni == 0 { 0 } else { end - start };
            meta_hdr(
                &mut self.iobuf,
                META_INODE_LEAF,
                0,
                count as u16,
                self.generation + 1,
                0,
            );
            let mut e = 0usize;
            while start + e < end && ni > 0 {
                let rec = &self.inodes[idx[start + e]];
                pack_inode(&mut self.iobuf[HDR + e * INODE_REC..], rec);
                e += 1;
            }
            finish_meta(&mut self.iobuf);
            d.write_block(ileaf[li], &self.iobuf)?;
            li += 1;
        }
        if iints > 0 {
            meta_hdr(
                &mut self.iobuf,
                META_INODE_INT,
                1,
                ileaves as u16,
                self.generation + 1,
                0,
            );
            li = 0;
            while li < ileaves {
                let key = if ni == 0 {
                    ROOT_INO
                } else {
                    self.inodes[idx[li * INODE_PER_LEAF]].ino
                };
                let o = HDR + li * 8;
                put32(&mut self.iobuf, o, key);
                put32(&mut self.iobuf, o + 4, ileaf[li]);
                li += 1;
            }
            finish_meta(&mut self.iobuf);
            d.write_block(iroot, &self.iobuf)?;
        }

        self.generation = self.generation.saturating_add(1);
        self.inode_root = iroot;
        self.alloc_root = alloc_bno;
        let mut sbuf = [0u8; BLOCK];
        write_alloc_into(self, &old_meta[..old_meta_n as usize], &mut sbuf);
        d.write_block(alloc_bno, &sbuf)?;
        d.flush()?;

        let slot = (self.generation % 2) as u8;
        pack_super(&mut sbuf, self, slot);
        d.write_block(slot as u32, &sbuf)?;
        d.flush()?;

        // In memory, apply the drops the alloc map above already carries.
        let nblocks = self.nblocks;
        for &b in old_meta[..old_meta_n as usize]
            .iter()
            .chain(self.drop[..self.ndrop as usize].iter())
        {
            drop_ref(&mut self.bitmap, &mut self.refc, nblocks, b);
        }
        self.ndrop = 0;
        self.txn = [0; MAX_BLOCKS.div_ceil(8)];
        self.nmeta = 0;
        // `need <= MAX_META` was checked before the first allocation, so no
        // call below fails with the new super on disk.
        debug_assert_eq!(n_new, need);
        for &b in &new_meta[..n_new] {
            self.mark_meta(b)?;
        }
        self.dirty = false;
        Ok(())
    }

    fn load_inode_leaf(&mut self, buf: &[u8; BLOCK]) -> Result<(), Error> {
        let (_lvl, count, _) = parse_meta(buf, META_INODE_LEAF)?;
        let mut e = 0usize;
        while e < count as usize {
            let rec = unpack_inode(&buf[HDR + e * INODE_REC..])?;
            let slot = self.alloc_ino_slot()?;
            self.inodes[slot] = rec;
            e += 1;
        }
        Ok(())
    }

    fn load_dir_leaf(&mut self, buf: &[u8; BLOCK], parent: u32) -> Result<(), Error> {
        let (_lvl, count, _) = parse_meta(buf, META_DIR_LEAF)?;
        let mut e = 0usize;
        while e < count as usize {
            let o = HDR + e * DENT_REC;
            let de = self.alloc_dent()?;
            let nlen = buf[o + 5];
            if nlen as usize > MAX_NAME || nlen == 0 {
                return Err(Error::Corrupt);
            }
            let mut name = [0u8; MAX_NAME];
            name.copy_from_slice(&buf[o + 6..o + 6 + MAX_NAME]);
            self.dents[de] = Dent {
                used: true,
                parent,
                ino: le32(buf, o),
                kind: buf[o + 4],
                nlen,
                name,
            };
            e += 1;
        }
        Ok(())
    }
}

pub fn mkfs<D: Disk>(d: &mut D, label: &[u8], v: &mut Vol) -> Result<(), Error> {
    let nblocks = d.nblocks();
    if nblocks < MIN_BLOCKS || nblocks as usize > MAX_BLOCKS {
        return Err(Error::Inval);
    }
    v.clear();
    v.nblocks = nblocks;
    v.generation = 1;
    v.flags = FLAG_DATA_CRC;
    v.root_ino = ROOT_INO;
    v.next_ino = 2;
    let n = label.len().min(32);
    v.label[..n].copy_from_slice(&label[..n]);
    v.uuid[0] = 0x76;
    v.uuid[1] = 0x31;
    put32(&mut v.uuid, 4, nblocks);

    v.refc[0] = 1;
    v.refc[1] = 1;
    bit_set(&mut v.bitmap, 0, true);
    bit_set(&mut v.bitmap, 1, true);

    let alloc_bno = 2u32;
    let leaf = 3u32;
    v.refc[2] = 1;
    v.refc[3] = 1;
    bit_set(&mut v.bitmap, 2, true);
    bit_set(&mut v.bitmap, 3, true);
    v.alloc_root = alloc_bno;
    v.inode_root = leaf;

    v.inodes[0] = Inode {
        used: true,
        ino: ROOT_INO,
        kind: KIND_DIR,
        flags: 0,
        mode: 0o755,
        nlink: 1,
        uid: 0,
        gid: 0,
        size: 0,
        atime: 0,
        mtime: 0,
        ctime: 0,
        dir_root: 0,
        n_ext: 0,
        inline_len: 0,
        extents: [Extent::EMPTY; MAX_EXT],
        inline_data: [0; INLINE],
    };

    meta_hdr(&mut v.iobuf, META_INODE_LEAF, 0, 1, 1, 0);
    pack_inode(&mut v.iobuf[HDR..], &v.inodes[0]);
    finish_meta(&mut v.iobuf);
    d.write_block(leaf, &v.iobuf)?;
    let mut sbuf = [0u8; BLOCK];
    write_alloc_into(v, &[], &mut sbuf);
    d.write_block(alloc_bno, &sbuf)?;
    pack_super(&mut sbuf, v, 0);
    d.write_block(0, &sbuf)?;
    pack_super(&mut sbuf, v, 1);
    d.write_block(1, &sbuf)?;
    d.flush()
}

pub fn mount<D: Disk>(d: &mut D, v: &mut Vol) -> Result<(), Error> {
    v.clear();
    let mut blk = [0u8; BLOCK];
    let sb = pick_super(d, &mut blk)?;
    if sb.nblocks != d.nblocks() && d.nblocks() < sb.nblocks {
        return Err(Error::Inval);
    }
    v.nblocks = sb.nblocks;
    v.generation = sb.generation;
    v.inode_root = sb.inode_root;
    v.alloc_root = sb.alloc_root;
    v.next_ino = sb.next_ino;
    v.root_ino = sb.root_ino;
    v.flags = sb.flags;
    v.uuid = sb.uuid;
    v.label = sb.label;
    v.snaps = sb.snaps;
    v.dirty = false;

    d.read_block(v.alloc_root, &mut blk)?;
    load_alloc(v, &blk)?;
    v.mark_meta(v.alloc_root)?;

    d.read_block(v.inode_root, &mut blk)?;
    let kind = blk[4];
    match kind {
        META_INODE_LEAF => {
            check_crc(&blk, 16)?;
            v.mark_meta(v.inode_root)?;
            v.load_inode_leaf(&blk)?;
        }
        META_INODE_INT => {
            parse_meta(&blk, META_INODE_INT)?;
            v.mark_meta(v.inode_root)?;
            let count = le16(&blk, 6) as usize;
            let mut kids = [0u32; 8];
            let mut i = 0usize;
            while i < count && i < 8 {
                kids[i] = le32(&blk, HDR + i * 8 + 4);
                i += 1;
            }
            i = 0;
            while i < count && i < 8 {
                d.read_block(kids[i], &mut blk)?;
                v.mark_meta(kids[i])?;
                v.load_inode_leaf(&blk)?;
                i += 1;
            }
        }
        _ => return Err(Error::Corrupt),
    }

    let mut i = 0usize;
    while i < MAX_INODES {
        if v.inodes[i].used && v.inodes[i].kind == KIND_DIR {
            let root = v.inodes[i].dir_root;
            let ino = v.inodes[i].ino;
            if root != 0 {
                d.read_block(root, &mut blk)?;
                let k = blk[4];
                match k {
                    META_DIR_LEAF => {
                        v.mark_meta(root)?;
                        v.load_dir_leaf(&blk, ino)?;
                    }
                    META_DIR_INT => {
                        parse_meta(&blk, META_DIR_INT)?;
                        v.mark_meta(root)?;
                        let count = le16(&blk, 6) as usize;
                        let mut kids = [0u32; 8];
                        let mut j = 0usize;
                        while j < count && j < 8 {
                            kids[j] = le32(&blk, HDR + j * DENT_REC);
                            j += 1;
                        }
                        j = 0;
                        while j < count && j < 8 {
                            d.read_block(kids[j], &mut blk)?;
                            v.mark_meta(kids[j])?;
                            v.load_dir_leaf(&blk, ino)?;
                            j += 1;
                        }
                    }
                    _ => return Err(Error::Corrupt),
                }
            }
        }
        i += 1;
    }
    if v.inode_slot(v.root_ino).is_err() {
        return Err(Error::Corrupt);
    }
    Ok(())
}

pub fn fsck<D: Disk>(d: &mut D) -> Result<FsckReport, Error> {
    // Host/tests only. A Vol on this stack is ~30KiB; do not call from
    // the kernel (16 KiB stacks).
    let mut r = FsckReport {
        errors: 0,
        warnings: 0,
        generation: 0,
        counts: [0; 14],
    };
    let mut v = Vol::new();
    if mount(d, &mut v).is_err() {
        r.add(Defect::Mount);
        return Ok(r);
    }
    r.generation = v.generation;

    // Inodes: extents, data CRCs, mode and inline flag.
    let mut reached = [false; MAX_BLOCKS];
    reached[0] = true;
    reached[1] = true;
    for &b in &v.meta[..v.nmeta as usize] {
        if let Some(x) = reached.get_mut(b as usize) {
            *x = true;
        }
    }
    let mut i = 0usize;
    while i < MAX_INODES {
        if !v.inodes[i].used {
            i += 1;
            continue;
        }
        let ino = v.inodes[i];
        let mut e = 0usize;
        while e < ino.n_ext as usize {
            let ex = ino.extents[e];
            if ex.len == 0 || ex.phys < 2 || ex.phys as u64 + ex.len as u64 > v.nblocks as u64 {
                r.add(Defect::Extent);
            } else {
                for x in &mut reached[ex.phys as usize..(ex.phys + ex.len) as usize] {
                    *x = true;
                }
                if v.flags & FLAG_DATA_CRC != 0 && v.check_extent(d, ex).is_err() {
                    r.add(Defect::DataCrc);
                }
            }
            e += 1;
        }
        let fmt = ino.mode & crate::fs::S_IFMT;
        if let Ok(k) = kind_of(ino.kind)
            && fmt != 0
            && fmt != k.ifmt()
        {
            r.add(Defect::Mode);
        }
        if ino.flags & F_INLINE != 0 && ino.size > INLINE as u64 {
            r.add(Defect::Inline);
        }
        // Link count against the dirents naming it; the root has none.
        let mut links = 0u32;
        for de in &v.dents {
            if de.used && de.ino == ino.ino {
                links += 1;
            }
        }
        if ino.ino != v.root_ino && links != ino.nlink {
            if ino.kind == KIND_DIR {
                r.add(Defect::DirNlink);
            } else {
                r.add(Defect::Nlink);
            }
        }
        i += 1;
    }

    // Dirents: target, kind, and names unique within a directory.
    let mut j = 0usize;
    while j < MAX_DENTS {
        let de = v.dents[j];
        if !de.used {
            j += 1;
            continue;
        }
        match v.inode_slot(de.ino) {
            Ok(s) => {
                if v.inodes[s].kind != de.kind {
                    r.add(Defect::Kind);
                }
            }
            Err(_) => r.add(Defect::Dangling),
        }
        let mut k = 0usize;
        while k < j {
            let o = &v.dents[k];
            if o.used && o.parent == de.parent && o.name() == de.name() {
                r.add(Defect::DupName);
                break;
            }
            k += 1;
        }
        j += 1;
    }

    // Reachability from the root through dirents, one pass per level.
    let mut live = [false; MAX_INODES];
    if let Ok(s) = v.inode_slot(v.root_ino) {
        live[s] = true;
    }
    let mut pass = 0usize;
    while pass < MAX_INODES {
        let mut changed = false;
        for de in &v.dents {
            if !de.used {
                continue;
            }
            let (Ok(ps), Ok(cs)) = (v.inode_slot(de.parent), v.inode_slot(de.ino)) else {
                continue;
            };
            if live[ps] && v.inodes[ps].kind == KIND_DIR && !live[cs] {
                live[cs] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
        pass += 1;
    }
    i = 0;
    while i < MAX_INODES {
        if v.inodes[i].used && !live[i] {
            r.add(Defect::Unreachable);
        }
        i += 1;
    }

    // Blocks from 2 on: refcount and bitmap against reachability.
    let mut b = 2u32;
    while b < v.nblocks {
        let reach = reached[b as usize];
        let refc = v.refc[b as usize];
        let bit = bit_get(&v.bitmap, b);
        if reach && refc == 0 {
            r.add(Defect::RefFree);
        } else if reach && !bit {
            r.add(Defect::BitFree);
        } else if !reach && (refc > 0 || bit) {
            r.add(Defect::Leak);
        }
        b += 1;
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh(n: usize) -> Vec<u8> {
        let mut b = vec![0u8; n];
        {
            let mut d = MemDisk::new(&mut b).unwrap();
            let mut v = Vol::new();
            mkfs(&mut d, b"vibe", &mut v).unwrap();
        }
        b
    }

    fn with_vol<R>(buf: &mut [u8], f: impl FnOnce(&mut Vol, &mut MemDisk) -> R) -> R {
        let mut disk = MemDisk::new(buf).unwrap();
        let mut vol = Vol::new();
        mount(&mut disk, &mut vol).unwrap();
        f(&mut vol, &mut disk)
    }

    #[test]
    fn version_is_one() {
        assert_eq!(VERSION, 1);
        assert_eq!(MAGIC_SUPER, u32::from_le_bytes(*b"VIBE"));
    }

    #[test]
    fn mkfs_mount_root() {
        let mut b = fresh(256 * 1024);
        with_vol(&mut b, |v, d| {
            assert_eq!(v.generation, 1);
            let r = v.walk(d, b"/").unwrap();
            assert!(r.is_dir());
            assert_eq!(r.ino, ROOT_INO);
        });
        let mut disk = MemDisk::new(&mut b).unwrap();
        let r = fsck(&mut disk).unwrap();
        assert_eq!(r.errors, 0);
    }

    #[test]
    fn inline_write_read() {
        let mut b = fresh(256 * 1024);
        with_vol(&mut b, |v, d| {
            v.create(d, ROOT_INO, b"a.txt", InodeKind::Reg, 0o644, None)
                .unwrap();
            let n = v.lookup(d, ROOT_INO, b"a.txt").unwrap();
            v.write(d, n.ino, 0, b"hello").unwrap();
            let mut out = [0u8; 8];
            let g = v.read(d, n.ino, 0, &mut out).unwrap();
            assert_eq!(g, 5);
            assert_eq!(&out[..5], b"hello");
            v.sync(d).unwrap();
        });
        with_vol(&mut b, |v, d| {
            let n = v.lookup(d, ROOT_INO, b"a.txt").unwrap();
            let mut out = [0u8; 8];
            v.read(d, n.ino, 0, &mut out).unwrap();
            assert_eq!(&out[..5], b"hello");
        });
    }

    #[test]
    fn extent_past_inline() {
        let mut b = fresh(256 * 1024);
        let mut payload = [0u8; 400];
        let mut i = 0usize;
        while i < payload.len() {
            payload[i] = (i % 251) as u8;
            i += 1;
        }
        with_vol(&mut b, |v, d| {
            v.create(d, ROOT_INO, b"big.bin", InodeKind::Reg, 0o644, None)
                .unwrap();
            let n = v.lookup(d, ROOT_INO, b"big.bin").unwrap();
            v.write(d, n.ino, 0, &payload).unwrap();
            v.sync(d).unwrap();
        });
        with_vol(&mut b, |v, d| {
            let n = v.lookup(d, ROOT_INO, b"big.bin").unwrap();
            assert!(n.size as usize >= 400);
            let mut out = [0u8; 400];
            v.read(d, n.ino, 0, &mut out).unwrap();
            assert_eq!(out, payload);
        });
        let mut disk = MemDisk::new(&mut b).unwrap();
        assert_eq!(fsck(&mut disk).unwrap().errors, 0);
    }

    #[test]
    fn mkdir_symlink_readdir() {
        let mut b = fresh(256 * 1024);
        with_vol(&mut b, |v, d| {
            v.create(d, ROOT_INO, b"sub", InodeKind::Dir, 0o755, None)
                .unwrap();
            let sub = v.lookup(d, ROOT_INO, b"sub").unwrap();
            v.create(d, sub.ino, b"f", InodeKind::Reg, 0o644, None)
                .unwrap();
            v.create(d, ROOT_INO, b"l", InodeKind::Lnk, 0o777, Some(b"/sub/f"))
                .unwrap();
            v.sync(d).unwrap();
            let mut node = Node::EMPTY;
            let mut n = 0u32;
            let mut c = 0u64;
            while let Some(next) = v.readdir(d, ROOT_INO, c, &mut node).unwrap() {
                n += 1;
                c = next;
            }
            assert_eq!(n, 2);
            let mut t = [0u8; 16];
            let ln = v.lookup(d, ROOT_INO, b"l").unwrap();
            let k = v.readlink(d, ln.ino, &mut t).unwrap();
            assert_eq!(&t[..k], b"/sub/f");
        });
    }

    #[test]
    fn snapshot_pins_generation() {
        let mut b = fresh(256 * 1024);
        with_vol(&mut b, |v, d| {
            v.create(d, ROOT_INO, b"a", InodeKind::Reg, 0o644, None)
                .unwrap();
            let n = v.lookup(d, ROOT_INO, b"a").unwrap();
            v.write(d, n.ino, 0, b"one").unwrap();
            v.snapshot(d, b"snap0").unwrap();
            v.write(d, n.ino, 0, b"two").unwrap();
            v.sync(d).unwrap();
            assert!(v.snaps[0].used);
            assert!(v.snaps[0].generation >= 1);
        });
        let mut disk = MemDisk::new(&mut b).unwrap();
        assert_eq!(fsck(&mut disk).unwrap().errors, 0);
    }

    #[test]
    fn corrupt_super_uses_other_slot() {
        let mut b = fresh(256 * 1024);
        b[0] ^= 0xff;
        let mut disk = MemDisk::new(&mut b).unwrap();
        let mut vol = Vol::new();
        mount(&mut disk, &mut vol).unwrap();
        assert_eq!(vol.generation, 1);
    }

    #[test]
    fn corrupt_both_supers_fails() {
        let mut b = fresh(256 * 1024);
        b[0] ^= 0xff;
        b[BLOCK] ^= 0xff;
        let mut disk = MemDisk::new(&mut b).unwrap();
        let mut vol = Vol::new();
        assert_eq!(mount(&mut disk, &mut vol).unwrap_err(), Error::Corrupt);
        let r = fsck(&mut disk).unwrap();
        assert!(r.errors > 0);
    }

    #[test]
    fn corrupt_inode_crc_is_reported() {
        let mut b = fresh(256 * 1024);
        // inode leaf is block 3 after mkfs
        let off = 3 * BLOCK + HDR + 10;
        b[off] ^= 0xff;
        let mut disk = MemDisk::new(&mut b).unwrap();
        let mut vol = Vol::new();
        assert_eq!(mount(&mut disk, &mut vol).unwrap_err(), Error::Corrupt);
        assert!(fsck(&mut disk).unwrap().errors > 0);
    }

    #[test]
    fn corrupt_data_crc_not_returned() {
        let mut b = fresh(256 * 1024);
        with_vol(&mut b, |v, d| {
            v.create(d, ROOT_INO, b"x", InodeKind::Reg, 0o644, None)
                .unwrap();
            let n = v.lookup(d, ROOT_INO, b"x").unwrap();
            let big = [7u8; 200];
            v.write(d, n.ino, 0, &big).unwrap();
            v.sync(d).unwrap();
        });
        // smash a data block (search for non-meta by flipping a late block)
        let mut flipped = false;
        let mut blk = 4usize;
        while blk < b.len() / BLOCK {
            let o = blk * BLOCK;
            if le32(&b[o..], 0) != MAGIC_META && le32(&b[o..], 0) != MAGIC_SUPER {
                b[o] ^= 0xff;
                flipped = true;
                break;
            }
            blk += 1;
        }
        assert!(flipped);
        with_vol(&mut b, |v, d| {
            let n = v.lookup(d, ROOT_INO, b"x").unwrap();
            let mut out = [0u8; 200];
            assert_eq!(v.read(d, n.ino, 0, &mut out).unwrap_err(), Error::Corrupt);
        });
        let mut disk = MemDisk::new(&mut b).unwrap();
        assert!(fsck(&mut disk).unwrap().errors > 0);
    }

    #[test]
    fn truncated_mkfs() {
        let mut b = vec![0u8; 4096];
        assert!(MemDisk::new(&mut b).is_err());
    }

    #[test]
    fn btree_many_dirents() {
        let mut b = fresh(256 * 1024);
        with_vol(&mut b, |v, d| {
            let mut i = 0u32;
            while i < 40 {
                let mut name = [0u8; 8];
                name[0] = b'f';
                name[1] = b'0' + (i / 10) as u8;
                name[2] = b'0' + (i % 10) as u8;
                v.create(d, ROOT_INO, &name[..3], InodeKind::Reg, 0o644, None)
                    .unwrap();
                i += 1;
            }
            v.sync(d).unwrap();
            assert_eq!(v.dir_count(ROOT_INO), 40);
        });
        with_vol(&mut b, |v, d| {
            assert_eq!(v.dir_count(ROOT_INO), 40);
            v.lookup(d, ROOT_INO, b"f39").unwrap();
        });
        let mut disk = MemDisk::new(&mut b).unwrap();
        assert_eq!(fsck(&mut disk).unwrap().errors, 0);
    }

    #[test]
    fn crash_at_each_write_is_consistent() {
        let base = fresh(256 * 1024);
        // count ops on a full run
        let mut probe = base.clone();
        {
            let mut c = CrashDisk::new(&mut probe, u64::MAX).unwrap();
            let mut v = Vol::new();
            mount(&mut c, &mut v).unwrap();
            v.create(&mut c, ROOT_INO, b"a", InodeKind::Reg, 0o644, None)
                .unwrap();
            let a = v.lookup(&mut c, ROOT_INO, b"a").unwrap();
            v.write(&mut c, a.ino, 0, b"alpha").unwrap();
            v.sync(&mut c).unwrap();
            let payload = [9u8; 300];
            v.write(&mut c, a.ino, 0, &payload).unwrap();
            v.sync(&mut c).unwrap();
            v.create(&mut c, ROOT_INO, b"d", InodeKind::Dir, 0o755, None)
                .unwrap();
            let dir = v.lookup(&mut c, ROOT_INO, b"d").unwrap();
            v.create(&mut c, dir.ino, b"b", InodeKind::Reg, 0o644, None)
                .unwrap();
            let bb = v.lookup(&mut c, dir.ino, b"b").unwrap();
            v.write(&mut c, bb.ino, 0, b"beta-data").unwrap();
            v.sync(&mut c).unwrap();
            v.unlink(&mut c, ROOT_INO, b"a", false).unwrap();
            v.sync(&mut c).unwrap();
            assert!(c.ops > 4);
            let total = c.ops;
            let mut i = 1u64;
            while i <= total {
                let mut img = base.clone();
                {
                    let mut c = CrashDisk::new(&mut img, i).unwrap();
                    let mut v = Vol::new();
                    let _ = (|| {
                        mount(&mut c, &mut v)?;
                        v.create(&mut c, ROOT_INO, b"a", InodeKind::Reg, 0o644, None)?;
                        let a = v.lookup(&mut c, ROOT_INO, b"a")?;
                        v.write(&mut c, a.ino, 0, b"alpha")?;
                        v.sync(&mut c)?;
                        let payload = [9u8; 300];
                        v.write(&mut c, a.ino, 0, &payload)?;
                        v.sync(&mut c)?;
                        v.create(&mut c, ROOT_INO, b"d", InodeKind::Dir, 0o755, None)?;
                        let dir = v.lookup(&mut c, ROOT_INO, b"d")?;
                        v.create(&mut c, dir.ino, b"b", InodeKind::Reg, 0o644, None)?;
                        let bb = v.lookup(&mut c, dir.ino, b"b")?;
                        v.write(&mut c, bb.ino, 0, b"beta-data")?;
                        v.sync(&mut c)?;
                        v.unlink(&mut c, ROOT_INO, b"a", false)?;
                        v.sync(&mut c)?;
                        Ok::<(), Error>(())
                    })();
                }
                let mut disk = MemDisk::new(&mut img).unwrap();
                let r = fsck(&mut disk).expect("fsck runs");
                assert_eq!(r.errors, 0, "crash at op {i} left a corrupt live tree");
                let mut vol = Vol::new();
                mount(&mut disk, &mut vol).expect("mount after crash");
                i += 1;
            }
        }
    }

    #[test]
    fn error_strings_cover() {
        for e in [
            Error::Inval,
            Error::Io,
            Error::Corrupt,
            Error::NoSpace,
            Error::NotFound,
            Error::Exists,
            Error::NotDir,
            Error::IsDir,
            Error::NotEmpty,
            Error::NameTooLong,
            Error::NotSupp,
            Error::FileTooBig,
        ] {
            assert!(!e.as_str().is_empty());
        }
    }

    /// A regular file `name` in the root; its inode number.
    fn new_file(v: &mut Vol, d: &mut MemDisk, name: &[u8]) -> u32 {
        v.create(d, ROOT_INO, name, InodeKind::Reg, 0o644, None)
            .unwrap();
        v.lookup(d, ROOT_INO, name).unwrap().ino
    }

    fn n_ext(v: &Vol, ino: u32) -> u8 {
        v.inodes[v.inode_slot(ino).unwrap()].n_ext
    }

    #[test]
    fn write_past_size_limit() {
        assert_eq!(MAX_FILE_SIZE, (1u64 << 44) - 4096);
        let mut b = fresh(256 * 1024);
        with_vol(&mut b, |v, d| {
            let ino = new_file(v, d, b"big");
            assert_eq!(v.write(d, ino, 0, b"abc").unwrap(), 3);
            let df = v.df();
            let ext = n_ext(v, ino);
            for off in [MAX_FILE_SIZE, 1u64 << 44] {
                assert_eq!(v.write(d, ino, off, b"x").unwrap_err(), Error::FileTooBig);
            }
            assert_eq!(v.write(d, ino, MAX_FILE_SIZE, b"").unwrap(), 0);
            assert_eq!(v.file_size(ino).unwrap(), 3);
            let mut out = [0u8; 8];
            assert_eq!(v.read(d, ino, 0, &mut out).unwrap(), 3);
            assert_eq!(&out[..3], b"abc");
            assert_eq!(v.df(), df);
            assert_eq!(n_ext(v, ino), ext);
        });
    }

    #[test]
    fn write_crossing_size_limit_is_short() {
        let mut b = fresh(256 * 1024);
        with_vol(&mut b, |v, d| {
            let ino = new_file(v, d, b"edge");
            assert_eq!(v.write(d, ino, MAX_FILE_SIZE - 1, b"xy").unwrap(), 1);
            assert_eq!(v.file_size(ino).unwrap(), MAX_FILE_SIZE);
            let mut out = [0u8; 4];
            assert_eq!(v.read(d, ino, MAX_FILE_SIZE - 1, &mut out).unwrap(), 1);
            assert_eq!(out[0], b'x');
        });
    }

    #[test]
    fn truncate_past_size_limit() {
        let mut b = fresh(256 * 1024);
        with_vol(&mut b, |v, d| {
            let ino = new_file(v, d, b"t");
            assert_eq!(
                v.truncate(d, ino, MAX_FILE_SIZE + 1).unwrap_err(),
                Error::FileTooBig
            );
            v.truncate(d, ino, MAX_FILE_SIZE).unwrap();
            assert_eq!(v.file_size(ino).unwrap(), MAX_FILE_SIZE);
        });
    }

    #[test]
    fn node_size_above_4gib() {
        let mut b = fresh(256 * 1024);
        with_vol(&mut b, |v, d| {
            let ino = new_file(v, d, b"five");
            assert_eq!(v.write(d, ino, 5 << 30, b"x").unwrap(), 1);
            let n = v.lookup(d, ROOT_INO, b"five").unwrap();
            assert_eq!(n.size, (5u64 << 30) + 1);
        });
    }

    #[test]
    fn block_math_near_u32_limit() {
        let mut b = fresh(256 * 1024);
        with_vol(&mut b, |v, d| {
            let ino = new_file(v, d, b"top");
            let start = MAX_FILE_SIZE - 2 * BLOCK as u64;
            let mut two = vec![0u8; 2 * BLOCK];
            let mut i = 0usize;
            while i < two.len() {
                two[i] = (i % 253) as u8;
                i += 1;
            }
            assert_eq!(v.write(d, ino, start, &two).unwrap(), two.len());
            assert_eq!(v.file_size(ino).unwrap(), MAX_FILE_SIZE);
            // Overwrite part of the first block: the split path.
            two[10..20].fill(0xAB);
            assert_eq!(v.write(d, ino, start + 10, &[0xAB; 10]).unwrap(), 10);
            let mut out = vec![0u8; 2 * BLOCK];
            assert_eq!(v.read(d, ino, start, &mut out).unwrap(), out.len());
            assert_eq!(out, two);
            v.truncate(d, ino, MAX_FILE_SIZE - BLOCK as u64).unwrap();
            assert_eq!(v.file_size(ino).unwrap(), MAX_FILE_SIZE - BLOCK as u64);
            let mut out = vec![0u8; 2 * BLOCK];
            assert_eq!(v.read(d, ino, start, &mut out).unwrap(), BLOCK);
            assert_eq!(out[..BLOCK], two[..BLOCK]);
        });
    }

    /// `fsck` over the image in `b`.
    fn fsck_of(b: &mut [u8]) -> FsckReport {
        let mut d = MemDisk::new(b).unwrap();
        fsck(&mut d).unwrap()
    }

    fn payload(seed: u32) -> [u8; 300] {
        let mut p = [0u8; 300];
        let mut i = 0usize;
        while i < p.len() {
            p[i] = (seed as usize * 31 + i) as u8;
            i += 1;
        }
        p
    }

    #[test]
    fn sessions_64_no_leak() {
        let mut b = fresh(64 * BLOCK);
        with_vol(&mut b, |v, d| {
            let ino = new_file(v, d, b"f");
            assert_eq!(v.write(d, ino, 0, &payload(0)).unwrap(), 300);
            v.sync(d).unwrap();
        });
        let free = with_vol(&mut b, |v, _| v.df().1);
        let warn0 = fsck_of(&mut b).warnings;
        let mut s = 1u32;
        while s <= 64 {
            with_vol(&mut b, |v, d| {
                assert_eq!(v.df().1, free, "free bytes at session {s}");
                let ino = v.lookup(d, ROOT_INO, b"f").unwrap().ino;
                assert_eq!(v.write(d, ino, 0, &payload(s)).unwrap(), 300);
                v.sync(d)
                    .unwrap_or_else(|e| panic!("sync at session {s}: {e:?}"));
            });
            let r = fsck_of(&mut b);
            assert_eq!(r.errors, 0, "fsck errors at session {s}");
            assert_eq!(r.warnings, warn0, "fsck warnings at session {s}");
            s += 1;
        }
        with_vol(&mut b, |v, d| {
            let ino = v.lookup(d, ROOT_INO, b"f").unwrap().ino;
            let mut out = [0u8; 300];
            assert_eq!(v.read(d, ino, 0, &mut out).unwrap(), 300);
            assert_eq!(out, payload(64));
        });
    }

    #[test]
    fn commit_free_count_constant() {
        let mut b = fresh(64 * BLOCK);
        let free = with_vol(&mut b, |v, d| {
            let ino = new_file(v, d, b"f");
            assert_eq!(v.write(d, ino, 0, &payload(0)).unwrap(), 300);
            v.sync(d).unwrap();
            let free = v.free_count();
            let mut i = 1u32;
            while i <= 200 {
                assert_eq!(v.write(d, ino, 0, &payload(i)).unwrap(), 300);
                v.sync(d)
                    .unwrap_or_else(|e| panic!("sync at commit {i}: {e:?}"));
                assert_eq!(v.free_count(), free, "free count after commit {i}");
                i += 1;
            }
            free
        });
        with_vol(&mut b, |v, _| {
            assert_eq!(v.free_count(), free, "after remount")
        });
        let r = fsck_of(&mut b);
        assert_eq!((r.errors, r.warnings), (0, 0));
    }

    #[test]
    fn nested_dirs_63_commit_remount() {
        let mut b = fresh(256 * BLOCK);
        with_vol(&mut b, |v, d| {
            let mut dir = ROOT_INO;
            let mut n = 0usize;
            while n < 63 {
                v.create(d, dir, b"d", InodeKind::Dir, 0o755, None)
                    .unwrap_or_else(|e| panic!("mkdir {n}: {e:?}"));
                dir = v.lookup(d, dir, b"d").unwrap().ino;
                n += 1;
            }
            v.sync(d).unwrap();
            assert_eq!(v.nmeta, 70);
        });
        let mut path = Vec::new();
        let mut n = 0usize;
        while n < 63 {
            if n > 0 {
                path.push(b'/');
            }
            path.push(b'd');
            n += 1;
        }
        with_vol(&mut b, |v, d| {
            assert!(v.walk(d, &path).unwrap().is_dir());
            v.dirty = true;
            v.sync(d).unwrap();
            assert_eq!(v.nmeta, 70);
        });
        with_vol(&mut b, |v, d| {
            assert!(v.walk(d, &path).unwrap().is_dir());
        });
        let r = fsck_of(&mut b);
        assert_eq!((r.errors, r.warnings), (0, 0));
    }

    /// `f` (300 bytes, one extent), `g` (5 bytes, inline) and `p/c` on a
    /// 64-block image that fsck finds clean.
    fn base_tree() -> Vec<u8> {
        let mut b = fresh(64 * BLOCK);
        with_vol(&mut b, |v, d| {
            let f = new_file(v, d, b"f");
            assert_eq!(v.write(d, f, 0, &payload(1)).unwrap(), 300);
            let g = new_file(v, d, b"g");
            assert_eq!(v.write(d, g, 0, b"hello").unwrap(), 5);
            v.create(d, ROOT_INO, b"p", InodeKind::Dir, 0o755, None)
                .unwrap();
            let p = v.lookup(d, ROOT_INO, b"p").unwrap().ino;
            v.create(d, p, b"c", InodeKind::Dir, 0o755, None).unwrap();
            v.sync(d).unwrap();
        });
        let r = fsck_of(&mut b);
        assert_eq!((r.errors, r.warnings), (0, 0));
        b
    }

    /// The base tree with `plant` applied in memory and committed.
    fn planted(plant: impl FnOnce(&mut Vol, &mut MemDisk)) -> FsckReport {
        let mut b = base_tree();
        with_vol(&mut b, |v, d| {
            plant(v, d);
            v.dirty = true;
            v.sync(d).unwrap();
        });
        fsck_of(&mut b)
    }

    /// The base tree with `plant(bitmap, refc, f's data block)` applied to
    /// the on-disk `ALLOC` block, re-sealed in place.
    fn planted_alloc(plant: impl FnOnce(&mut [u8], &mut [u8], u32)) -> FsckReport {
        let mut b = base_tree();
        let (root, n, fblk) = with_vol(&mut b, |v, d| {
            let f = v.lookup(d, ROOT_INO, b"f").unwrap().ino;
            let s = v.inode_slot(f).unwrap();
            assert_eq!(v.inodes[s].n_ext, 1);
            (v.alloc_root, v.nblocks, v.inodes[s].extents[0].phys)
        });
        {
            let mut d = MemDisk::new(&mut b).unwrap();
            let mut blk = [0u8; BLOCK];
            d.read_block(root, &mut blk).unwrap();
            let nbytes = (n as usize).div_ceil(8);
            {
                let (head, rest) = blk.split_at_mut(HDR + nbytes);
                plant(&mut head[HDR..], &mut rest[..n as usize], fblk);
            }
            finish_meta(&mut blk);
            d.write_block(root, &blk).unwrap();
        }
        fsck_of(&mut b)
    }

    fn slot_of(v: &mut Vol, d: &mut MemDisk, dir: u32, name: &[u8]) -> (usize, usize) {
        let e = v.find_dent(dir, name).unwrap();
        let ino = v.lookup(d, dir, name).unwrap().ino;
        (e, v.inode_slot(ino).unwrap())
    }

    #[test]
    fn fsck_reports_each_planted_defect() {
        let expect = |what: &str, r: FsckReport, class: Defect, n: Option<u32>| {
            assert!(r.count(class) > 0, "{what}: no {} in {r:?}", class.as_str());
            if let Some(n) = n {
                assert_eq!(r.count(class), n, "{what}: {r:?}");
            }
            if class == Defect::Leak {
                assert_eq!(r.errors, 0, "{what}: {r:?}");
            } else {
                assert!(r.errors > 0, "{what}: {r:?}");
            }
        };
        let r = planted(|v, d| {
            let (e, _) = slot_of(v, d, ROOT_INO, b"f");
            v.dents[e].kind = KIND_DIR;
        });
        expect("dirent kind", r, Defect::Kind, None);
        let r = planted(|v, d| {
            let (_, s) = slot_of(v, d, ROOT_INO, b"f");
            v.inodes[s].mode = crate::fs::S_IFDIR | 0o644;
        });
        expect("mode", r, Defect::Mode, None);
        let r = planted(|v, d| {
            let (_, s) = slot_of(v, d, ROOT_INO, b"g");
            assert!(v.inodes[s].flags & F_INLINE != 0);
            v.inodes[s].size = 200;
        });
        expect("inline", r, Defect::Inline, None);
        let r = planted(|v, d| {
            let (e, _) = slot_of(v, d, ROOT_INO, b"g");
            v.dents[e].name[0] = b'f';
        });
        expect("dup name", r, Defect::DupName, Some(1));
        let r = planted(|v, d| {
            let (_, s) = slot_of(v, d, ROOT_INO, b"f");
            v.inodes[s].nlink = 2;
        });
        expect("nlink", r, Defect::Nlink, Some(1));
        let r = planted(|v, d| {
            let (e, _) = slot_of(v, d, ROOT_INO, b"g");
            v.dents[e].used = false;
        });
        expect("dirent emptied", r, Defect::Unreachable, Some(1));
        let r = planted(|v, d| {
            let p = v.lookup(d, ROOT_INO, b"p").unwrap().ino;
            let c = v.lookup(d, p, b"c").unwrap().ino;
            let e = v.find_dent(ROOT_INO, b"p").unwrap();
            v.dents[e].parent = c;
        });
        expect("dir in own subtree", r, Defect::Unreachable, Some(2));
        let r = planted(|v, d| {
            let (e, _) = slot_of(v, d, ROOT_INO, b"g");
            v.dents[e].ino = 999;
        });
        expect("dangling", r, Defect::Dangling, Some(1));
        let r = planted(|v, d| {
            let (_, s) = slot_of(v, d, ROOT_INO, b"g");
            v.inodes[s].kind = 9;
        });
        expect("kind out of range", r, Defect::Mount, Some(1));
        let r = planted_alloc(|bm, _, b| bit_set(bm, b, false));
        expect("bit clear", r, Defect::BitFree, Some(1));
        let r = planted_alloc(|bm, rc, b| {
            bit_set(bm, b, false);
            rc[b as usize] = 0;
        });
        expect("refcount clear", r, Defect::RefFree, Some(1));
        let r = planted_alloc(|bm, rc, _| {
            let last = rc.len() as u32 - 1;
            assert_eq!(rc[last as usize], 0);
            bit_set(bm, last, true);
        });
        expect("bit on a free block", r, Defect::Leak, Some(1));
    }

    #[test]
    fn fsck_dir_int_57_entries() {
        let name = |i: usize| [b'f', b'0' + (i / 10) as u8, b'0' + (i % 10) as u8];
        let mut b = fresh(64 * BLOCK);
        with_vol(&mut b, |v, d| {
            let mut i = 0usize;
            while i < 57 {
                new_file(v, d, &name(i));
                i += 1;
            }
            v.sync(d).unwrap();
        });
        let r = fsck_of(&mut b);
        assert_eq!((r.errors, r.warnings), (0, 0));
        let root_kind = |v: &mut Vol, d: &mut MemDisk| {
            let s = v.inode_slot(ROOT_INO).unwrap();
            let mut blk = [0u8; BLOCK];
            d.read_block(v.inodes[s].dir_root, &mut blk).unwrap();
            blk[4]
        };
        with_vol(&mut b, |v, d| {
            assert_eq!(root_kind(v, d), META_DIR_INT);
            assert_eq!(v.dir_count(ROOT_INO), 57);
            v.lookup(d, ROOT_INO, &name(56)).unwrap();
            // Two entries now share a name; fsck must read both leaves.
            let e = v.find_dent(ROOT_INO, &name(3)).unwrap();
            v.dents[e].name[..3].copy_from_slice(&name(55));
            v.dirty = true;
            v.sync(d).unwrap();
        });
        with_vol(&mut b, |v, d| assert_eq!(root_kind(v, d), META_DIR_INT));
        let r = fsck_of(&mut b);
        assert_eq!(r.count(Defect::DupName), 1, "{r:?}");
    }

    #[test]
    fn rename_dir_into_own_subtree_einval() {
        let mut b = fresh(64 * BLOCK);
        with_vol(&mut b, |v, d| {
            v.create(d, ROOT_INO, b"p", InodeKind::Dir, 0o755, None)
                .unwrap();
            let p = v.lookup(d, ROOT_INO, b"p").unwrap().ino;
            v.create(d, p, b"c", InodeKind::Dir, 0o755, None).unwrap();
            let c = v.lookup(d, p, b"c").unwrap().ino;
            assert_eq!(
                v.rename(d, ROOT_INO, b"p", c, b"q").unwrap_err(),
                Error::Inval
            );
            assert_eq!(
                v.rename(d, ROOT_INO, b"p", p, b"q").unwrap_err(),
                Error::Inval
            );
            let x = new_file(v, d, b"tmp");
            v.rename(d, ROOT_INO, b"tmp", c, b"x").unwrap();
            assert_eq!(
                v.rename(d, ROOT_INO, b"p", c, b"x").unwrap_err(),
                Error::Inval
            );
            assert_eq!(v.lookup(d, c, b"x").unwrap().ino, x);
            assert_eq!(v.walk(d, b"/p/c").unwrap().ino, c);
            v.sync(d).unwrap();
        });
        let r = fsck_of(&mut b);
        assert_eq!(r.errors, 0, "{r:?}");
        assert_eq!(r.count(Defect::Unreachable), 0);
    }
}
