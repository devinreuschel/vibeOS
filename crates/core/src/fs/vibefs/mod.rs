//! vibefs format, mkfs, fsck, volume ops. ROADMAP §8.5 / docs/VIBEFS.md.
//!
//! Version 1. CoW metadata, dual superblocks, generation + CRC-32.
//! Host mkfs/fsck and the kernel share this module. Do not grow a second
//! on-disk layout.

#![allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "vibefs v1 predates the parser deny; ROADMAP §14.8's v2 is written under it and retires this allow"
)]

use crate::fs::InodeKind;
use crate::part::crc32_ieee;

mod commit;
mod disk;
mod fsck;
mod layout;
mod mkfs;
mod ops;
mod vol;

pub use commit::mount;
#[cfg(test)]
pub use disk::CrashDisk;
pub use disk::MemDisk;
pub use fsck::{Defect, FsckReport, fsck};
pub use layout::{Extent, Node, Snap, probe};
pub use mkfs::mkfs;

use layout::{
    finish_meta, load_alloc, meta_hdr, pack_inode, pack_super, parse_meta, pick_super,
    unpack_inode, write_alloc_into,
};

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

/// vibefs's errors are the filesystem's (E2, F083): one type, one `From`
/// into `KError`.
pub type Error = super::FsError;

pub trait Disk {
    fn nblocks(&self) -> u32;
    fn read_block(&mut self, bno: u32, buf: &mut [u8; BLOCK]) -> Result<(), Error>;
    fn write_block(&mut self, bno: u32, buf: &[u8; BLOCK]) -> Result<(), Error>;
    fn flush(&mut self) -> Result<(), Error>;
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

/// A commit defect a `vibefs_crash` kernel plants, so the crash test shows
/// that it can fail (docs/VIBEFS.md §12). Test-only: the `crash_plant`
/// feature, which only that build enables.
#[cfg(any(test, feature = "crash_plant"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plant {
    None,
    /// Each commit leaves its first directory block out of the metadata
    /// table, so the next commit never frees it: a leak `fsck` warns of.
    Leak,
    /// Each commit writes its superblock before the flush ahead of it, so
    /// the super can reach the medium before the blocks it names.
    EarlySuper,
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
    /// Unix seconds the next change stamps, which the kernel half sets
    /// from the wall clock before each operation; 0 when there is none.
    pub now: u64,
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
    #[cfg(any(test, feature = "crash_plant"))]
    plant: Plant,
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

#[cfg(test)]
mod tests;
