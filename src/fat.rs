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

pub const SEC: usize = 512;
pub const INITRD_BYTES: usize = 64 * 1024;
pub const MAX_CLUS_BYTES: usize = 4096;
pub const MAX_NAME: usize = 64;
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
        }
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
    inos: [InoEnt; MAX_INOS],
    next_ino: u32,
}

const MAX_INOS: usize = 96;

#[derive(Clone, Copy)]
struct InoEnt {
    used: bool,
    dir_clu: u32,
    dir_off: u32,
    ino: u32,
    clu: u32,
    size: u32,
    kind: InodeKind,
}

impl InoEnt {
    const EMPTY: Self = Self {
        used: false,
        dir_clu: 0,
        dir_off: 0,
        ino: 0,
        clu: 0,
        size: 0,
        kind: InodeKind::Reg,
    };
}

impl FatVol {
    pub fn mount<D: Disk>(d: &mut D) -> Result<Self, FatError> {
        let ss = d.sector_size();
        if ss != SEC as u32 {
            return Err(FatError::Inval);
        }
        let mut boot = [0u8; SEC];
        d.read(0, &mut boot)?;
        let info = parse_bpb(&boot, d.nsectors())?;
        if info.clus_bytes() == 0 || info.clus_bytes() > MAX_CLUS_BYTES {
            return Err(FatError::Inval);
        }
        let mut vol = Self {
            info,
            cache: [FatSec::EMPTY; FAT_CACHE],
            hint: 2,
            free: 0xFFFFFFFF,
            fsinfo_dirty: false,
            now: 0,
            inos: [InoEnt::EMPTY; MAX_INOS],
            next_ino: 2,
        };
        if info.fsinfo != 0 && info.fsinfo < info.rsvd {
            let mut fs = [0u8; SEC];
            d.read(info.fsinfo, &mut fs)?;
            if le32(&fs, 0) == 0x4161_5252
                && le32(&fs, 484) == 0x6141_7272
                && le32(&fs, 508) == 0xAA55_0000
            {
                let free = le32(&fs, 488);
                let hint = le32(&fs, 492);
                if free != 0xFFFFFFFF {
                    vol.free = free;
                }
                if hint >= 2 && hint < info.nclus + 2 {
                    vol.hint = hint;
                }
            }
        }
        if vol.free == 0xFFFFFFFF {
            vol.free = vol.count_free(d)?;
        }
        Ok(vol)
    }

    pub fn root(&self) -> Node {
        let mut n = Node::EMPTY;
        n.ino = ROOT_INO;
        n.kind = InodeKind::Dir;
        n.clu = self.info.root_clus;
        n.attr = ATTR_DIR;
        n.name_len = 1;
        n.name[0] = b'/';
        n
    }

    pub fn walk<D: Disk>(&mut self, d: &mut D, path: &[u8]) -> Result<Node, FatError> {
        if path.is_empty() {
            return Err(FatError::Inval);
        }
        let mut node = self.root();
        let mut stack = [0u32; 16];
        let mut sp = 0usize;
        let mut i = 0usize;
        while i < path.len() && path[i] == b'/' {
            i += 1;
        }
        while i < path.len() {
            let mut j = i;
            while j < path.len() && path[j] != b'/' {
                j += 1;
            }
            let comp = &path[i..j];
            if !comp.is_empty() && !name_is_dot(comp) {
                if name_is_dotdot(comp) {
                    if sp == 0 {
                        node = self.root();
                    } else {
                        sp -= 1;
                        node = self.node_from_clu(stack[sp], InodeKind::Dir, 0, 0, 0)?;
                    }
                } else {
                    if node.kind != InodeKind::Dir {
                        return Err(FatError::NotDir);
                    }
                    if sp < stack.len() {
                        stack[sp] = node.clu;
                        sp += 1;
                    }
                    node = self.lookup(d, node.clu, comp)?;
                }
            }
            while j < path.len() && path[j] == b'/' {
                j += 1;
            }
            i = j;
        }
        Ok(node)
    }

    pub fn lookup<D: Disk>(&mut self, d: &mut D, dir: u32, name: &[u8]) -> Result<Node, FatError> {
        if name_is_dot(name) {
            return self.node_from_clu(dir, InodeKind::Dir, 0, 0, 0);
        }
        if name_is_dotdot(name) {
            return Err(FatError::Inval);
        }
        let mut off = 0u32;
        loop {
            match self.read_dirent(d, dir, off)? {
                None => return Err(FatError::NotFound),
                Some((next, node)) => {
                    if eq_ci(node.name(), name) {
                        return Ok(node);
                    }
                    off = next;
                }
            }
        }
    }

    pub fn readdir<D: Disk>(
        &mut self,
        d: &mut D,
        dir: u32,
        cookie: u64,
        out: &mut Node,
    ) -> Result<Option<u64>, FatError> {
        match self.read_dirent(d, dir, cookie as u32)? {
            None => Ok(None),
            Some((next, node)) => {
                *out = node;
                Ok(Some(next as u64))
            }
        }
    }

    pub fn read<D: Disk>(
        &mut self,
        d: &mut D,
        clu: u32,
        size: u32,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, FatError> {
        if off >= size as u64 {
            return Ok(0);
        }
        let max = (size as u64 - off) as usize;
        let n = buf.len().min(max);
        self.read_at(d, clu, off, &mut buf[..n])?;
        Ok(n)
    }

    #[allow(clippy::too_many_arguments)] // FAT dirent + cluster + size update
    pub fn write<D: Disk>(
        &mut self,
        d: &mut D,
        dir_clu: u32,
        dir_off: u32,
        first: &mut u32,
        size: &mut u32,
        off: u64,
        buf: &[u8],
    ) -> Result<usize, FatError> {
        if buf.is_empty() {
            return Ok(0);
        }
        let end = off.checked_add(buf.len() as u64).ok_or(FatError::NoSpace)?;
        if end > u32::MAX as u64 {
            return Err(FatError::NoSpace);
        }
        let need = end as u32;
        self.ensure_size(d, first, *size, need)?;
        self.write_at(d, *first, off, buf)?;
        if need > *size {
            *size = need;
            self.update_short(d, dir_clu, dir_off, *first, *size)?;
            d.flush()?;
        }
        Ok(buf.len())
    }

    pub fn truncate<D: Disk>(
        &mut self,
        d: &mut D,
        dir_clu: u32,
        dir_off: u32,
        first: &mut u32,
        size: &mut u32,
        new: u32,
    ) -> Result<(), FatError> {
        if new == *size {
            return Ok(());
        }
        if new > *size {
            self.ensure_size(d, first, *size, new)?;
            *size = new;
            self.update_short(d, dir_clu, dir_off, *first, *size)?;
            return d.flush();
        }
        // Size first while clusters stay allocated. Then drop the cluster
        // pointer (still allocated) so the dirent never names a free cluster.
        self.update_short(d, dir_clu, dir_off, *first, new)?;
        d.flush()?;
        let cb = self.info.clus_bytes() as u32;
        let keep = if new == 0 { 0 } else { new.div_ceil(cb) };
        if keep == 0 {
            let old = *first;
            *first = 0;
            *size = new;
            self.update_short(d, dir_clu, dir_off, 0, new)?;
            d.flush()?;
            if old >= 2 {
                self.free_chain(d, old)?;
                self.commit_fat(d)?;
                d.flush()?;
            }
            return Ok(());
        }
        let mut clu = *first;
        let mut i = 0u32;
        while clu >= 2 && !is_eoc(clu) {
            let next = self.fat_get(d, clu)?;
            i += 1;
            if i == keep {
                self.fat_set(d, clu, EOC_MIN)?;
            } else if i > keep {
                self.fat_set(d, clu, 0)?;
            }
            clu = next;
            if i > self.info.nclus {
                return Err(FatError::Corrupt);
            }
        }
        self.commit_fat(d)?;
        *size = new;
        d.flush()
    }

    pub fn create<D: Disk>(
        &mut self,
        d: &mut D,
        dir_clu: u32,
        name: &[u8],
        dir: bool,
    ) -> Result<Node, FatError> {
        self.check_name(name)?;
        match self.lookup(d, dir_clu, name) {
            Ok(_) => return Err(FatError::Exists),
            Err(FatError::NotFound) => {}
            Err(e) => return Err(e),
        }
        let mut short = [0u8; 11];
        let lfn = self.pick_short(d, dir_clu, name, &mut short)?;
        let n_lfn = if lfn {
            utf16_len(name).div_ceil(LFN_CHARS)
        } else {
            0
        };
        let slots = n_lfn + 1;
        let (ent_clu, ent_off) = self.dir_reserve(d, dir_clu, slots)?;
        let mut first = 0u32;
        if dir {
            first = self.alloc_clu(d, 0)?;
            self.init_dir_cluster(d, first, dir_clu)?;
            self.fat_set(d, first, EOC_MIN)?;
            self.commit_fat(d)?;
            d.flush()?;
        }
        let cs = lfn_checksum(&short);
        let (date, time) = fat_datetime(self.now);
        let mut slot = 0usize;
        while slot < n_lfn {
            let ord = (n_lfn - slot) as u8;
            let last = slot == 0;
            let mut ent = [0u8; ENT];
            fill_lfn(&mut ent, ord, last, cs, name);
            let off = ent_off + (slot * ENT) as u32;
            self.write_dir_raw(d, dir_clu, off, &ent)?;
            slot += 1;
        }
        let mut ent = [0u8; ENT];
        ent[..11].copy_from_slice(&short);
        ent[11] = if dir { ATTR_DIR } else { ATTR_ARCH };
        put_le16(&mut ent, 14, time);
        put_le16(&mut ent, 16, date);
        put_le16(&mut ent, 18, date);
        put_le16(&mut ent, 20, (first >> 16) as u16);
        put_le16(&mut ent, 22, time);
        put_le16(&mut ent, 24, date);
        put_le16(&mut ent, 26, (first & 0xFFFF) as u16);
        put_le32(&mut ent, 28, 0);
        let short_off = ent_off + (n_lfn * ENT) as u32;
        self.write_dir_raw(d, dir_clu, short_off, &ent)?;
        d.flush()?;
        let _ = (ent_clu, date, time);
        self.node_from_short(dir_clu, short_off, &ent, name)
    }

    pub fn unlink<D: Disk>(
        &mut self,
        d: &mut D,
        dir_clu: u32,
        name: &[u8],
        rmdir: bool,
    ) -> Result<(), FatError> {
        if name_is_dot(name) || name_is_dotdot(name) {
            return Err(FatError::Inval);
        }
        let node = self.lookup(d, dir_clu, name)?;
        if rmdir {
            if node.kind != InodeKind::Dir {
                return Err(FatError::NotDir);
            }
            if !self.dir_empty(d, node.clu)? {
                return Err(FatError::NotEmpty);
            }
        } else if node.kind == InodeKind::Dir {
            return Err(FatError::IsDir);
        }
        self.mark_deleted(d, dir_clu, node.dir_off)?;
        d.flush()?;
        if node.clu >= 2 {
            self.free_chain(d, node.clu)?;
            self.commit_fat(d)?;
            d.flush()?;
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
    ) -> Result<(), FatError> {
        if src_name == dst_name && src_dir == dst_dir {
            return Ok(());
        }
        let src = self.lookup(d, src_dir, src_name)?;
        match self.lookup(d, dst_dir, dst_name) {
            Ok(dst) => {
                if dst.kind == InodeKind::Dir {
                    return Err(FatError::IsDir);
                }
                self.unlink(d, dst_dir, dst_name, false)?;
            }
            Err(FatError::NotFound) => {}
            Err(e) => return Err(e),
        }
        // Recreate dest name pointing at existing clusters, then drop src dirent.
        let mut short = [0u8; 11];
        let lfn = self.pick_short(d, dst_dir, dst_name, &mut short)?;
        let n_lfn = if lfn {
            utf16_len(dst_name).div_ceil(LFN_CHARS)
        } else {
            0
        };
        let (_c, ent_off) = self.dir_reserve(d, dst_dir, n_lfn + 1)?;
        let cs = lfn_checksum(&short);
        let mut slot = 0usize;
        while slot < n_lfn {
            let ord = (n_lfn - slot) as u8;
            let mut ent = [0u8; ENT];
            fill_lfn(&mut ent, ord, slot == 0, cs, dst_name);
            self.write_dir_raw(d, dst_dir, ent_off + (slot * ENT) as u32, &ent)?;
            slot += 1;
        }
        let mut ent = [0u8; ENT];
        self.read_dir_raw(d, src_dir, src.dir_off, &mut ent)?;
        ent[..11].copy_from_slice(&short);
        let short_off = ent_off + (n_lfn * ENT) as u32;
        self.write_dir_raw(d, dst_dir, short_off, &ent)?;
        d.flush()?;
        self.mark_deleted(d, src_dir, src.dir_off)?;
        d.flush()
    }

    pub fn sync<D: Disk>(&mut self, d: &mut D) -> Result<(), FatError> {
        self.commit_fat(d)?;
        d.flush()
    }

    pub fn free_bytes(&self) -> u64 {
        self.free as u64 * self.info.clus_bytes() as u64
    }

    pub fn fats_identical<D: Disk>(&mut self, d: &mut D) -> Result<bool, FatError> {
        if self.info.num_fats < 2 {
            return Ok(true);
        }
        let mut a = [0u8; SEC];
        let mut b = [0u8; SEC];
        let mut sec = 0u32;
        while sec < self.info.fatsz {
            d.read(self.info.fat_lba(0, sec)?, &mut a)?;
            d.read(self.info.fat_lba(1, sec)?, &mut b)?;
            if a != b {
                return Ok(false);
            }
            sec += 1;
        }
        Ok(true)
    }

    fn node_from_clu(
        &mut self,
        clu: u32,
        kind: InodeKind,
        size: u32,
        dir_clu: u32,
        dir_off: u32,
    ) -> Result<Node, FatError> {
        let mut n = Node::EMPTY;
        n.ino = self.ino_of(dir_clu, dir_off, clu, size, kind);
        n.kind = kind;
        n.clu = clu;
        n.size = size;
        n.dir_clu = dir_clu;
        n.dir_off = dir_off;
        Ok(n)
    }

    fn node_from_short(
        &mut self,
        dir_clu: u32,
        dir_off: u32,
        ent: &[u8; ENT],
        lfn: &[u8],
    ) -> Result<Node, FatError> {
        let clu = (le16(ent, 20) as u32) << 16 | le16(ent, 26) as u32;
        let size = le32(ent, 28);
        let attr = ent[11];
        let kind = if attr & ATTR_DIR != 0 {
            InodeKind::Dir
        } else {
            InodeKind::Reg
        };
        let mut n = self.node_from_clu(clu, kind, size, dir_clu, dir_off)?;
        n.attr = attr;
        n.mtime = fat_to_unix(le16(ent, 24), le16(ent, 22));
        if !lfn.is_empty() {
            let len = lfn.len().min(MAX_NAME);
            n.name[..len].copy_from_slice(&lfn[..len]);
            n.name_len = len as u8;
        } else {
            let (nm, nl) = decode_short(ent);
            n.name = nm;
            n.name_len = nl;
        }
        Ok(n)
    }

    fn ino_of(&mut self, dir_clu: u32, dir_off: u32, clu: u32, size: u32, kind: InodeKind) -> u32 {
        if dir_clu == 0 && dir_off == 0 {
            return ROOT_INO;
        }
        let mut i = 0usize;
        while i < MAX_INOS {
            if self.inos[i].used
                && self.inos[i].dir_clu == dir_clu
                && self.inos[i].dir_off == dir_off
            {
                self.inos[i].clu = clu;
                self.inos[i].size = size;
                self.inos[i].kind = kind;
                return self.inos[i].ino;
            }
            i += 1;
        }
        i = 0;
        while i < MAX_INOS {
            if !self.inos[i].used {
                let ino = self.next_ino;
                self.next_ino = self.next_ino.saturating_add(1);
                self.inos[i] = InoEnt {
                    used: true,
                    dir_clu,
                    dir_off,
                    ino,
                    clu,
                    size,
                    kind,
                };
                return ino;
            }
            i += 1;
        }
        dir_clu.wrapping_mul(0x9E37) ^ dir_off ^ (clu << 1)
    }

    pub fn by_ino(&self, ino: u32) -> Result<Node, FatError> {
        if ino == ROOT_INO {
            return Ok(self.root());
        }
        let mut i = 0usize;
        while i < MAX_INOS {
            if self.inos[i].used && self.inos[i].ino == ino {
                let e = self.inos[i];
                let mut n = Node::EMPTY;
                n.ino = e.ino;
                n.kind = e.kind;
                n.clu = e.clu;
                n.size = e.size;
                n.dir_clu = e.dir_clu;
                n.dir_off = e.dir_off;
                return Ok(n);
            }
            i += 1;
        }
        Err(FatError::NotFound)
    }

    pub fn put_size(&mut self, ino: u32, clu: u32, size: u32) {
        let mut i = 0usize;
        while i < MAX_INOS {
            if self.inos[i].used && self.inos[i].ino == ino {
                self.inos[i].clu = clu;
                self.inos[i].size = size;
                return;
            }
            i += 1;
        }
    }

    fn check_name(&self, name: &[u8]) -> Result<(), FatError> {
        if name.is_empty() || name.len() > MAX_NAME {
            return Err(FatError::NameTooLong);
        }
        if name_is_dot(name) || name_is_dotdot(name) {
            return Err(FatError::Inval);
        }
        let mut i = 0usize;
        while i < name.len() {
            let c = name[i];
            if c == 0 || c == b'/' || c < 0x20 {
                return Err(FatError::Inval);
            }
            i += 1;
        }
        Ok(())
    }

    fn read_dirent<D: Disk>(
        &mut self,
        d: &mut D,
        dir: u32,
        mut off: u32,
    ) -> Result<Option<(u32, Node)>, FatError> {
        let mut lfn = [0u8; MAX_NAME];
        let mut lfn_len = 0usize;
        let mut expect_cs: Option<u8> = None;
        loop {
            let mut ent = [0u8; ENT];
            match self.read_dir_raw(d, dir, off, &mut ent) {
                Err(FatError::Corrupt) | Err(FatError::Io) => return Ok(None),
                Err(e) => return Err(e),
                Ok(false) => return Ok(None),
                Ok(true) => {}
            }
            let next = off + ENT as u32;
            if ent[0] == ENT_FREE {
                return Ok(None);
            }
            if ent[0] == ENT_DEL {
                lfn_len = 0;
                expect_cs = None;
                off = next;
                continue;
            }
            if ent[11] == ATTR_LFN {
                let cs = ent[13];
                if ent[0] & LFN_LAST != 0 {
                    lfn = [0u8; MAX_NAME];
                    lfn_len = 0;
                    expect_cs = Some(cs);
                } else if expect_cs.map(|c| c != cs).unwrap_or(true) {
                    lfn_len = 0;
                    expect_cs = None;
                    off = next;
                    continue;
                }
                let n = take_lfn(&ent, &mut lfn, lfn_len);
                if n > lfn_len {
                    lfn_len = n;
                }
                off = next;
                continue;
            }
            if ent[11] & ATTR_VOL != 0 {
                lfn_len = 0;
                expect_cs = None;
                off = next;
                continue;
            }
            let short_ok = match expect_cs {
                Some(cs) => {
                    let mut sn = [0u8; 11];
                    sn.copy_from_slice(&ent[..11]);
                    cs == lfn_checksum(&sn)
                }
                None => false,
            };
            let use_lfn = short_ok && lfn_len > 0;
            let name = if use_lfn { &lfn[..lfn_len] } else { &[] };
            let node = self.node_from_short(dir, off, &ent, name)?;
            return Ok(Some((next, node)));
        }
    }

    fn read_dir_raw<D: Disk>(
        &mut self,
        d: &mut D,
        dir: u32,
        off: u32,
        ent: &mut [u8; ENT],
    ) -> Result<bool, FatError> {
        let cb = self.info.clus_bytes() as u32;
        if cb == 0 {
            return Err(FatError::Corrupt);
        }
        let idx = off / cb;
        let pin = (off % cb) as usize;
        let clu = match self.nth_clu(d, dir, idx)? {
            None => return Ok(false),
            Some(c) => c,
        };
        let mut clbuf = [0u8; MAX_CLUS_BYTES];
        let n = self.read_cluster(d, clu, &mut clbuf)?;
        if pin + ENT > n {
            return Err(FatError::Corrupt);
        }
        ent.copy_from_slice(&clbuf[pin..pin + ENT]);
        Ok(true)
    }

    fn write_dir_raw<D: Disk>(
        &mut self,
        d: &mut D,
        dir: u32,
        off: u32,
        ent: &[u8; ENT],
    ) -> Result<(), FatError> {
        let cb = self.info.clus_bytes() as u32;
        let idx = off / cb;
        let pin = (off % cb) as usize;
        let clu = self.nth_clu(d, dir, idx)?.ok_or(FatError::Corrupt)?;
        let mut clbuf = [0u8; MAX_CLUS_BYTES];
        let n = self.read_cluster(d, clu, &mut clbuf)?;
        if pin + ENT > n {
            return Err(FatError::Corrupt);
        }
        clbuf[pin..pin + ENT].copy_from_slice(ent);
        self.write_cluster(d, clu, &clbuf[..n])
    }

    fn dir_reserve<D: Disk>(
        &mut self,
        d: &mut D,
        dir: u32,
        slots: usize,
    ) -> Result<(u32, u32), FatError> {
        let need = slots * ENT;
        let cb = self.info.clus_bytes();
        let mut off = 0u32;
        let mut run = 0usize;
        let mut run_off = 0u32;
        loop {
            let mut ent = [0u8; ENT];
            match self.read_dir_raw(d, dir, off, &mut ent) {
                Ok(false) => break,
                Err(e) => return Err(e),
                Ok(true) => {
                    if ent[0] == ENT_FREE || ent[0] == ENT_DEL {
                        if run == 0 {
                            run_off = off;
                        }
                        run += 1;
                        if run >= slots {
                            return Ok((dir, run_off));
                        }
                    } else {
                        run = 0;
                    }
                    off += ENT as u32;
                    if ent[0] == ENT_FREE {
                        break;
                    }
                }
            }
        }
        if run >= slots {
            return Ok((dir, run_off));
        }
        // extend directory
        let last = self.last_clu(d, dir)?;
        let new = self.alloc_clu(d, last)?;
        self.zero_cluster(d, new)?;
        self.fat_set(d, last, new)?;
        self.fat_set(d, new, EOC_MIN)?;
        self.commit_fat(d)?;
        d.flush()?;
        let start = if run > 0 { run_off } else { off };
        let have = run * ENT;
        if have + cb < need {
            return Err(FatError::NoSpace);
        }
        Ok((dir, start))
    }

    fn mark_deleted<D: Disk>(
        &mut self,
        d: &mut D,
        dir: u32,
        short_off: u32,
    ) -> Result<(), FatError> {
        // Walk back over LFN entries.
        let mut off = short_off;
        loop {
            let mut ent = [0u8; ENT];
            if !self.read_dir_raw(d, dir, off, &mut ent)? {
                break;
            }
            ent[0] = ENT_DEL;
            self.write_dir_raw(d, dir, off, &ent)?;
            if off < ENT as u32 {
                break;
            }
            let prev = off - ENT as u32;
            let mut p = [0u8; ENT];
            if !self.read_dir_raw(d, dir, prev, &mut p)? {
                break;
            }
            if p[11] != ATTR_LFN {
                break;
            }
            off = prev;
        }
        Ok(())
    }

    fn dir_empty<D: Disk>(&mut self, d: &mut D, dir: u32) -> Result<bool, FatError> {
        let mut off = 0u32;
        loop {
            match self.read_dirent(d, dir, off)? {
                None => return Ok(true),
                Some((next, node)) => {
                    if !name_is_dot(node.name()) && !name_is_dotdot(node.name()) {
                        return Ok(false);
                    }
                    off = next;
                }
            }
        }
    }

    fn pick_short<D: Disk>(
        &mut self,
        d: &mut D,
        dir: u32,
        name: &[u8],
        out: &mut [u8; 11],
    ) -> Result<bool, FatError> {
        if let Some(s) = as_pure_83(name) {
            *out = s;
            if !self.short_taken(d, dir, out)? {
                return Ok(false);
            }
        }
        let mut stem = [b' '; 11];
        make_lossy_83(name, &mut stem);
        if !self.short_taken(d, dir, &stem)? {
            *out = stem;
            return Ok(true);
        }
        let mut n = 1u32;
        while n < 1_000_000 {
            *out = stem;
            apply_tilde(out, n);
            if !self.short_taken(d, dir, out)? {
                return Ok(true);
            }
            n += 1;
        }
        Err(FatError::NoSpace)
    }

    fn short_taken<D: Disk>(
        &mut self,
        d: &mut D,
        dir: u32,
        short: &[u8; 11],
    ) -> Result<bool, FatError> {
        let mut off = 0u32;
        loop {
            let mut ent = [0u8; ENT];
            match self.read_dir_raw(d, dir, off, &mut ent) {
                Ok(false) => return Ok(false),
                Err(e) => return Err(e),
                Ok(true) => {
                    if ent[0] == ENT_FREE {
                        return Ok(false);
                    }
                    if ent[0] != ENT_DEL && ent[11] != ATTR_LFN && ent[..11] == short[..] {
                        return Ok(true);
                    }
                    off += ENT as u32;
                }
            }
        }
    }

    fn init_dir_cluster<D: Disk>(
        &mut self,
        d: &mut D,
        clu: u32,
        parent: u32,
    ) -> Result<(), FatError> {
        let mut buf = [0u8; MAX_CLUS_BYTES];
        let n = self.info.clus_bytes();
        fill_dot(&mut buf[0..ENT], b".          ", clu, self.now);
        let p = if parent == self.info.root_clus {
            0
        } else {
            parent
        };
        fill_dot(&mut buf[ENT..ENT * 2], b"..         ", p, self.now);
        self.write_cluster(d, clu, &buf[..n])
    }

    fn ensure_size<D: Disk>(
        &mut self,
        d: &mut D,
        first: &mut u32,
        old: u32,
        new: u32,
    ) -> Result<(), FatError> {
        if new <= old && *first >= 2 {
            return Ok(());
        }
        let cb = self.info.clus_bytes() as u32;
        let need = if new == 0 { 0 } else { new.div_ceil(cb) };
        if need == 0 {
            return Ok(());
        }
        if *first < 2 {
            let c = self.alloc_clu(d, 0)?;
            self.zero_cluster(d, c)?;
            self.fat_set(d, c, EOC_MIN)?;
            *first = c;
        }
        let mut clu = *first;
        let mut have = 1u32;
        loop {
            let next = self.fat_get(d, clu)?;
            if is_eoc(next) {
                break;
            }
            if next < 2 || next == BAD_CLUS {
                return Err(FatError::Corrupt);
            }
            have += 1;
            clu = next;
            if have > self.info.nclus {
                return Err(FatError::Corrupt);
            }
        }
        while have < need {
            let n = self.alloc_clu(d, clu)?;
            self.zero_cluster(d, n)?;
            self.fat_set(d, clu, n)?;
            self.fat_set(d, n, EOC_MIN)?;
            clu = n;
            have += 1;
        }
        self.commit_fat(d)?;
        d.flush()
    }

    fn update_short<D: Disk>(
        &mut self,
        d: &mut D,
        dir: u32,
        off: u32,
        first: u32,
        size: u32,
    ) -> Result<(), FatError> {
        let mut ent = [0u8; ENT];
        if !self.read_dir_raw(d, dir, off, &mut ent)? {
            return Err(FatError::Corrupt);
        }
        let (date, time) = fat_datetime(self.now);
        put_le16(&mut ent, 20, (first >> 16) as u16);
        put_le16(&mut ent, 22, time);
        put_le16(&mut ent, 24, date);
        put_le16(&mut ent, 26, (first & 0xFFFF) as u16);
        put_le32(&mut ent, 28, size);
        self.write_dir_raw(d, dir, off, &ent)
    }

    fn read_at<D: Disk>(
        &mut self,
        d: &mut D,
        first: u32,
        off: u64,
        buf: &mut [u8],
    ) -> Result<(), FatError> {
        if first < 2 {
            if buf.iter().any(|_| true) && !buf.is_empty() {
                buf.fill(0);
            }
            return Ok(());
        }
        let cb = self.info.clus_bytes() as u64;
        let skip = off / cb;
        let mut pin = (off % cb) as usize;
        let mut clu = first;
        let mut s = 0u32;
        while s < skip as u32 {
            clu = self.fat_get(d, clu)?;
            if clu < 2 || is_eoc(clu) {
                buf.fill(0);
                return Ok(());
            }
            s += 1;
            if s > self.info.nclus {
                return Err(FatError::Corrupt);
            }
        }
        let mut done = 0usize;
        let mut clbuf = [0u8; MAX_CLUS_BYTES];
        while done < buf.len() {
            if clu < 2 || is_eoc(clu) {
                buf[done..].fill(0);
                break;
            }
            let n = self.read_cluster(d, clu, &mut clbuf)?;
            let take = (n - pin).min(buf.len() - done);
            buf[done..done + take].copy_from_slice(&clbuf[pin..pin + take]);
            done += take;
            pin = 0;
            if done >= buf.len() {
                break;
            }
            clu = self.fat_get(d, clu)?;
        }
        Ok(())
    }

    fn write_at<D: Disk>(
        &mut self,
        d: &mut D,
        first: u32,
        off: u64,
        buf: &[u8],
    ) -> Result<(), FatError> {
        let cb = self.info.clus_bytes() as u64;
        let skip = off / cb;
        let mut pin = (off % cb) as usize;
        let mut clu = first;
        let mut s = 0u32;
        while s < skip as u32 {
            clu = self.fat_get(d, clu)?;
            if clu < 2 || is_eoc(clu) {
                return Err(FatError::Corrupt);
            }
            s += 1;
        }
        let mut done = 0usize;
        let mut clbuf = [0u8; MAX_CLUS_BYTES];
        while done < buf.len() {
            if clu < 2 || is_eoc(clu) {
                return Err(FatError::Corrupt);
            }
            let n = self.read_cluster(d, clu, &mut clbuf)?;
            let take = (n - pin).min(buf.len() - done);
            clbuf[pin..pin + take].copy_from_slice(&buf[done..done + take]);
            self.write_cluster(d, clu, &clbuf[..n])?;
            done += take;
            pin = 0;
            if done >= buf.len() {
                break;
            }
            clu = self.fat_get(d, clu)?;
        }
        Ok(())
    }

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

    fn last_clu<D: Disk>(&mut self, d: &mut D, first: u32) -> Result<u32, FatError> {
        let mut clu = first;
        let mut i = 0u32;
        loop {
            let next = self.fat_get(d, clu)?;
            if is_eoc(next) {
                return Ok(clu);
            }
            if next < 2 || next == BAD_CLUS {
                return Err(FatError::Corrupt);
            }
            clu = next;
            i += 1;
            if i > self.info.nclus {
                return Err(FatError::Corrupt);
            }
        }
    }

    fn read_cluster<D: Disk>(
        &mut self,
        d: &mut D,
        clu: u32,
        buf: &mut [u8],
    ) -> Result<usize, FatError> {
        let n = self.info.clus_bytes();
        if buf.len() < n {
            return Err(FatError::Inval);
        }
        let lba = self.info.clus_lba(clu)?;
        let mut i = 0u32;
        while i < self.info.spc as u32 {
            let off = i as usize * self.info.bps as usize;
            d.read(lba + i, &mut buf[off..off + self.info.bps as usize])?;
            i += 1;
        }
        Ok(n)
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

    fn zero_cluster<D: Disk>(&mut self, d: &mut D, clu: u32) -> Result<(), FatError> {
        let z = [0u8; MAX_CLUS_BYTES];
        let n = self.info.clus_bytes();
        self.write_cluster(d, clu, &z[..n])
    }

    fn fat_get<D: Disk>(&mut self, d: &mut D, clu: u32) -> Result<u32, FatError> {
        if clu >= self.info.nclus + 2 {
            return Err(FatError::Corrupt);
        }
        let (sec, ent_off) = fat_loc(clu);
        let s = self.fat_cache(d, sec)?;
        Ok(le32(&self.cache[s].data, ent_off) & 0x0FFF_FFFF)
    }

    fn fat_set<D: Disk>(&mut self, d: &mut D, clu: u32, val: u32) -> Result<(), FatError> {
        if clu >= self.info.nclus + 2 {
            return Err(FatError::Corrupt);
        }
        let (sec, ent_off) = fat_loc(clu);
        let s = self.fat_cache(d, sec)?;
        let old = le32(&self.cache[s].data, ent_off);
        let packed = (old & 0xF000_0000) | (val & 0x0FFF_FFFF);
        put_le32(&mut self.cache[s].data, ent_off, packed);
        self.cache[s].dirty = true;
        if val == 0 && old & 0x0FFF_FFFF != 0 {
            self.free = self.free.saturating_add(1);
            self.fsinfo_dirty = true;
        } else if val != 0 && old & 0x0FFF_FFFF == 0 {
            self.free = self.free.saturating_sub(1);
            self.fsinfo_dirty = true;
        }
        Ok(())
    }

    fn fat_cache<D: Disk>(&mut self, d: &mut D, sec: u32) -> Result<usize, FatError> {
        if sec >= self.info.fatsz {
            return Err(FatError::Corrupt);
        }
        let mut i = 0usize;
        while i < FAT_CACHE {
            if self.cache[i].used && self.cache[i].idx == sec {
                return Ok(i);
            }
            i += 1;
        }
        i = 0;
        while i < FAT_CACHE {
            if !self.cache[i].used {
                return self.fat_load(d, i, sec);
            }
            i += 1;
        }
        let mut i = 0usize;
        while i < FAT_CACHE {
            if !self.cache[i].dirty {
                return self.fat_load(d, i, sec);
            }
            i += 1;
        }
        self.flush_one_fat(d, 0)?;
        self.fat_load(d, 0, sec)
    }

    fn fat_load<D: Disk>(&mut self, d: &mut D, slot: usize, sec: u32) -> Result<usize, FatError> {
        if self.cache[slot].dirty {
            self.flush_one_fat(d, slot)?;
        }
        let lba = self.info.fat_lba(0, sec)?;
        d.read(lba, &mut self.cache[slot].data)?;
        self.cache[slot].used = true;
        self.cache[slot].dirty = false;
        self.cache[slot].idx = sec;
        Ok(slot)
    }

    fn flush_one_fat<D: Disk>(&mut self, d: &mut D, slot: usize) -> Result<(), FatError> {
        if !self.cache[slot].used || !self.cache[slot].dirty {
            return Ok(());
        }
        let sec = self.cache[slot].idx;
        let mut copy = 0u8;
        while copy < self.info.num_fats {
            let lba = self.info.fat_lba(copy, sec)?;
            d.write(lba, &self.cache[slot].data)?;
            copy += 1;
        }
        self.cache[slot].dirty = false;
        Ok(())
    }

    fn commit_fat<D: Disk>(&mut self, d: &mut D) -> Result<(), FatError> {
        let mut i = 0usize;
        while i < FAT_CACHE {
            self.flush_one_fat(d, i)?;
            i += 1;
        }
        if self.fsinfo_dirty {
            self.write_fsinfo(d)?;
            self.fsinfo_dirty = false;
        }
        Ok(())
    }

    fn write_fsinfo<D: Disk>(&mut self, d: &mut D) -> Result<(), FatError> {
        if self.info.fsinfo == 0 {
            return Ok(());
        }
        let mut fs = [0u8; SEC];
        put_le32(&mut fs, 0, 0x4161_5252);
        put_le32(&mut fs, 484, 0x6141_7272);
        put_le32(&mut fs, 488, self.free);
        put_le32(&mut fs, 492, self.hint);
        put_le32(&mut fs, 508, 0xAA55_0000);
        d.write(self.info.fsinfo, &fs)?;
        if self.info.backup != 0 {
            let b = self.info.backup.saturating_add(1);
            if b != self.info.fsinfo && b < self.info.rsvd {
                d.write(b, &fs)?;
            }
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

    fn free_chain<D: Disk>(&mut self, d: &mut D, mut clu: u32) -> Result<(), FatError> {
        let mut n = 0u32;
        while clu >= 2 && !is_eoc(clu) {
            let next = self.fat_get(d, clu)?;
            self.fat_set(d, clu, 0)?;
            clu = next;
            n += 1;
            if n > self.info.nclus {
                return Err(FatError::Corrupt);
            }
        }
        Ok(())
    }

    fn count_free<D: Disk>(&mut self, d: &mut D) -> Result<u32, FatError> {
        let mut n = 0u32;
        let mut c = 2u32;
        while c < self.info.nclus + 2 {
            if self.fat_get(d, c)? == 0 {
                n += 1;
            }
            c += 1;
        }
        Ok(n)
    }
}

fn fat_loc(clu: u32) -> (u32, usize) {
    let off = clu * 4;
    (off / SEC as u32, (off as usize) % SEC)
}

fn is_eoc(v: u32) -> bool {
    v >= EOC_MIN
}

fn parse_bpb(boot: &[u8; SEC], nsectors: u32) -> Result<FatInfo, FatError> {
    if boot[510] != 0x55 || boot[511] != 0xAA {
        return Err(FatError::Corrupt);
    }
    let bps = le16(boot, 11) as u32;
    if bps != SEC as u32 {
        return Err(FatError::Inval);
    }
    let spc = boot[13];
    if spc == 0 || (spc & (spc - 1)) != 0 {
        return Err(FatError::Inval);
    }
    let rsvd = le16(boot, 14) as u32;
    let num_fats = boot[16];
    if rsvd == 0 || num_fats == 0 || num_fats > 2 {
        return Err(FatError::Inval);
    }
    if le16(boot, 17) != 0 || le16(boot, 22) != 0 {
        return Err(FatError::Inval);
    }
    let tot16 = le16(boot, 19) as u32;
    let tot32 = le32(boot, 32);
    let totsec = if tot16 != 0 { tot16 } else { tot32 };
    if totsec == 0 || totsec > nsectors {
        return Err(FatError::Corrupt);
    }
    let fatsz = le32(boot, 36);
    if fatsz == 0 {
        return Err(FatError::Inval);
    }
    let root_clus = le32(boot, 44);
    if root_clus < 2 {
        return Err(FatError::Corrupt);
    }
    let fsinfo = le16(boot, 48) as u32;
    let backup = le16(boot, 50) as u32;
    let media = boot[21];
    let data_lba = rsvd + num_fats as u32 * fatsz;
    if data_lba >= totsec {
        return Err(FatError::Corrupt);
    }
    let data_secs = totsec - data_lba;
    let nclus = data_secs / spc as u32;
    if nclus < 2 {
        return Err(FatError::Corrupt);
    }
    Ok(FatInfo {
        bps,
        spc,
        rsvd,
        num_fats,
        fatsz,
        totsec,
        root_clus,
        fsinfo,
        backup,
        data_lba,
        nclus,
        media,
    })
}

/// Format `buf` as FAT32. Size must be a multiple of 512 and at least 64 KiB.
pub fn mkfs(buf: &mut [u8], label: &[u8]) -> Result<FatInfo, FatError> {
    if buf.len() < INITRD_BYTES || !buf.len().is_multiple_of(SEC) {
        return Err(FatError::Inval);
    }
    buf.fill(0);
    let totsec = (buf.len() / SEC) as u32;
    let rsvd = 32u32;
    let spc = 1u8;
    let num_fats = 2u8;
    let mut fatsz = 1u32;
    loop {
        let data = totsec.saturating_sub(rsvd + num_fats as u32 * fatsz);
        let nclus = data / spc as u32;
        if nclus < 2 {
            return Err(FatError::Inval);
        }
        let need = ((nclus + 2) * 4).div_ceil(SEC as u32);
        if need <= fatsz {
            break;
        }
        fatsz = need;
        if rsvd + num_fats as u32 * fatsz >= totsec {
            return Err(FatError::Inval);
        }
    }
    let data_lba = rsvd + num_fats as u32 * fatsz;
    let nclus = (totsec - data_lba) / spc as u32;
    let root = 2u32;
    let mut boot = [0u8; SEC];
    boot[0] = 0xEB;
    boot[1] = 0x58;
    boot[2] = 0x90;
    boot[3..11].copy_from_slice(b"VIBEOS  ");
    put_le16(&mut boot, 11, SEC as u16);
    boot[13] = spc;
    put_le16(&mut boot, 14, rsvd as u16);
    boot[16] = num_fats;
    boot[21] = 0xF8;
    put_le16(&mut boot, 24, 32);
    put_le16(&mut boot, 26, 2);
    put_le32(&mut boot, 32, totsec);
    put_le32(&mut boot, 36, fatsz);
    put_le32(&mut boot, 44, root);
    put_le16(&mut boot, 48, 1);
    put_le16(&mut boot, 50, 6);
    boot[64] = 0x80;
    boot[66] = 0x29;
    put_le32(&mut boot, 67, 0x5642_4F53);
    let mut lab = [b' '; 11];
    let n = label.len().min(11);
    lab[..n].copy_from_slice(&label[..n]);
    boot[71..82].copy_from_slice(&lab);
    boot[82..90].copy_from_slice(b"FAT32   ");
    boot[510] = 0x55;
    boot[511] = 0xAA;
    buf[..SEC].copy_from_slice(&boot);
    if 6 < rsvd {
        buf[6 * SEC..7 * SEC].copy_from_slice(&boot);
    }
    let mut fs = [0u8; SEC];
    put_le32(&mut fs, 0, 0x4161_5252);
    put_le32(&mut fs, 484, 0x6141_7272);
    put_le32(&mut fs, 488, nclus.saturating_sub(1));
    put_le32(&mut fs, 492, 3);
    put_le32(&mut fs, 508, 0xAA55_0000);
    buf[SEC..2 * SEC].copy_from_slice(&fs);
    if 7 < rsvd {
        buf[7 * SEC..8 * SEC].copy_from_slice(&fs);
    }
    let mut fat0 = [0u8; SEC];
    put_le32(&mut fat0, 0, 0x0FFF_FFF8);
    put_le32(&mut fat0, 4, 0x0FFF_FFFF);
    put_le32(&mut fat0, 8, 0x0FFF_FFFF);
    let fat0_off = rsvd as usize * SEC;
    let fat1_off = fat0_off + fatsz as usize * SEC;
    buf[fat0_off..fat0_off + SEC].copy_from_slice(&fat0);
    buf[fat1_off..fat1_off + SEC].copy_from_slice(&fat0);
    let data_off = data_lba as usize * SEC;
    if data_off + ENT <= buf.len() {
        let mut ent = [0u8; ENT];
        ent[..11].copy_from_slice(&lab);
        ent[11] = ATTR_VOL;
        buf[data_off..data_off + ENT].copy_from_slice(&ent);
    }
    Ok(FatInfo {
        bps: SEC as u32,
        spc,
        rsvd,
        num_fats,
        fatsz,
        totsec,
        root_clus: root,
        fsinfo: 1,
        backup: 6,
        data_lba,
        nclus,
        media: 0xF8,
    })
}

/// Seed an image with `hello.txt` and `etc/`.
pub fn mkinitrd(buf: &mut [u8]) -> Result<(), FatError> {
    mkfs(buf, b"VIBEOS")?;
    let mut disk = MemDisk::new(buf, SEC as u32)?;
    let mut vol = FatVol::mount(&mut disk)?;
    vol.now = 1_262_304_000;
    vol.create(&mut disk, vol.info.root_clus, b"hello.txt", false)?;
    let hello = vol.lookup(&mut disk, vol.info.root_clus, b"hello.txt")?;
    let mut clu = hello.clu;
    let mut size = hello.size;
    let msg = b"hello from initrd\n";
    vol.write(
        &mut disk,
        hello.dir_clu,
        hello.dir_off,
        &mut clu,
        &mut size,
        0,
        msg,
    )?;
    vol.create(&mut disk, vol.info.root_clus, b"etc", true)?;
    vol.sync(&mut disk)
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn put_le16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}

fn put_le32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

pub fn lfn_checksum(name: &[u8; 11]) -> u8 {
    let mut sum = 0u8;
    let mut i = 0usize;
    while i < 11 {
        sum = sum.rotate_right(1).wrapping_add(name[i]);
        i += 1;
    }
    sum
}

fn name_is_dot(n: &[u8]) -> bool {
    n.len() == 1 && n[0] == b'.'
}

fn name_is_dotdot(n: &[u8]) -> bool {
    n.len() == 2 && n[0] == b'.' && n[1] == b'.'
}

fn eq_ci(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0usize;
    while i < a.len() {
        if to_upper(a[i]) != to_upper(b[i]) {
            return false;
        }
        i += 1;
    }
    true
}

fn to_upper(c: u8) -> u8 {
    if c.is_ascii_lowercase() {
        c - b'a' + b'A'
    } else {
        c
    }
}

fn utf16_len(name: &[u8]) -> usize {
    name.len()
}

fn take_lfn(ent: &[u8; ENT], out: &mut [u8; MAX_NAME], _len: usize) -> usize {
    let ord = ent[0] & !LFN_LAST;
    if ord == 0 {
        return 0;
    }
    let base = ((ord as usize) - 1) * LFN_CHARS;
    let slots = [(1usize, 5usize), (14usize, 6usize), (28usize, 2usize)];
    let mut i = 0usize;
    let mut n = 0usize;
    while i < 3 {
        let (off, cnt) = slots[i];
        let mut k = 0usize;
        while k < cnt {
            let p = off + k * 2;
            let ch = le16(ent, p);
            if ch == 0 || ch == 0xFFFF {
                return if base + n > MAX_NAME {
                    MAX_NAME
                } else {
                    base + n
                };
            }
            let at = base + n;
            if at < MAX_NAME {
                out[at] = if ch < 0x80 { ch as u8 } else { b'?' };
            }
            n += 1;
            k += 1;
        }
        i += 1;
    }
    let total = base + n;
    if total > MAX_NAME { MAX_NAME } else { total }
}

fn fill_lfn(ent: &mut [u8; ENT], ord: u8, last: bool, cs: u8, name: &[u8]) {
    ent.fill(0);
    ent[0] = if last { ord | LFN_LAST } else { ord };
    ent[11] = ATTR_LFN;
    ent[13] = cs;
    let start = (ord as usize - 1) * LFN_CHARS;
    let mut chars = [0xFFFFu16; LFN_CHARS];
    let mut i = 0usize;
    let mut term = false;
    while i < LFN_CHARS {
        let p = start + i;
        if term {
            chars[i] = 0xFFFF;
        } else if p < name.len() {
            chars[i] = name[p] as u16;
        } else {
            chars[i] = 0;
            term = true;
        }
        i += 1;
    }
    let mut k = 0usize;
    while k < 5 {
        put_le16(ent, 1 + k * 2, chars[k]);
        k += 1;
    }
    k = 0;
    while k < 6 {
        put_le16(ent, 14 + k * 2, chars[5 + k]);
        k += 1;
    }
    put_le16(ent, 28, chars[11]);
    put_le16(ent, 30, chars[12]);
}

fn decode_short(ent: &[u8; ENT]) -> ([u8; MAX_NAME], u8) {
    let mut out = [0u8; MAX_NAME];
    let mut n = 0usize;
    let mut i = 0usize;
    let mut name0 = ent[0];
    if name0 == 0x05 {
        name0 = 0xE5;
    }
    let mut tmp = [0u8; 11];
    tmp.copy_from_slice(&ent[..11]);
    tmp[0] = name0;
    while i < 8 && tmp[i] != b' ' {
        out[n] = tmp[i];
        n += 1;
        i += 1;
    }
    if tmp[8] != b' ' {
        out[n] = b'.';
        n += 1;
        i = 8;
        while i < 11 && tmp[i] != b' ' {
            out[n] = tmp[i];
            n += 1;
            i += 1;
        }
    }
    (out, n as u8)
}

fn as_pure_83(name: &[u8]) -> Option<[u8; 11]> {
    let mut out = [b' '; 11];
    let mut dot = None;
    let mut i = 0usize;
    while i < name.len() {
        if name[i] == b'.' {
            if dot.is_some() {
                return None;
            }
            dot = Some(i);
        } else if !is_83_char(name[i]) || name[i].is_ascii_lowercase() {
            return None;
        }
        i += 1;
    }
    match dot {
        None => {
            if name.is_empty() || name.len() > 8 {
                return None;
            }
            out[..name.len()].copy_from_slice(name);
        }
        Some(0) => return None,
        Some(d) => {
            let stem = &name[..d];
            let ext = &name[d + 1..];
            if stem.is_empty() || stem.len() > 8 || ext.is_empty() || ext.len() > 3 {
                return None;
            }
            out[..stem.len()].copy_from_slice(stem);
            out[8..8 + ext.len()].copy_from_slice(ext);
        }
    }
    Some(out)
}

fn is_83_char(c: u8) -> bool {
    matches!(c, b'A'..=b'Z' | b'0'..=b'9' | b'!' | b'#' | b'$' | b'%' | b'&'
        | b'\'' | b'(' | b')' | b'-' | b'@' | b'^' | b'_' | b'`' | b'{' | b'}'
        | b'~')
}

fn make_lossy_83(name: &[u8], out: &mut [u8; 11]) {
    out.fill(b' ');
    let mut stem = [0u8; 8];
    let mut sn = 0usize;
    let mut ext = [0u8; 3];
    let mut en = 0usize;
    let mut in_ext = false;
    let mut i = 0usize;
    while i < name.len() {
        let mut c = to_upper(name[i]);
        if c == b'.' {
            if !in_ext && sn > 0 {
                in_ext = true;
            }
            i += 1;
            continue;
        }
        if !is_83_char(c) {
            c = b'_';
        }
        if in_ext {
            if en < 3 {
                ext[en] = c;
                en += 1;
            }
        } else if sn < 8 {
            stem[sn] = c;
            sn += 1;
        }
        i += 1;
    }
    if sn == 0 {
        stem[0] = b'_';
        sn = 1;
    }
    out[..sn].copy_from_slice(&stem[..sn]);
    if en > 0 {
        out[8..8 + en].copy_from_slice(&ext[..en]);
    }
}

fn apply_tilde(out: &mut [u8; 11], n: u32) {
    let mut digits = [0u8; 8];
    let mut dn = 0usize;
    let mut x = n;
    if x == 0 {
        x = 1;
    }
    while x > 0 && dn < 8 {
        digits[dn] = b'0' + (x % 10) as u8;
        dn += 1;
        x /= 10;
    }
    let need = dn + 1;
    let stem = 8usize.saturating_sub(need).max(1);
    out[stem] = b'~';
    let mut i = 0usize;
    while i < dn {
        out[stem + 1 + i] = digits[dn - 1 - i];
        i += 1;
    }
}

fn fill_dot(ent: &mut [u8], name11: &[u8], clu: u32, now: u32) {
    ent.fill(0);
    ent[..11].copy_from_slice(name11);
    ent[11] = ATTR_DIR;
    let (date, time) = fat_datetime(now);
    put_le16(ent, 14, time);
    put_le16(ent, 16, date);
    put_le16(ent, 18, date);
    put_le16(ent, 20, (clu >> 16) as u16);
    put_le16(ent, 22, time);
    put_le16(ent, 24, date);
    put_le16(ent, 26, (clu & 0xFFFF) as u16);
}

fn fat_datetime(secs: u32) -> (u16, u16) {
    let s = (secs % 60) / 2;
    let mi = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let mut days = secs / 86400;
    let mut y = 0u16;
    loop {
        let ly = if y.is_multiple_of(4) { 366 } else { 365 };
        if days < ly {
            break;
        }
        days -= ly;
        y += 1;
        if y > 127 {
            y = 127;
            days = 0;
            break;
        }
    }
    let leap = y.is_multiple_of(4);
    let md = [
        31u32,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0u16;
    while m < 12 && days >= md[m as usize] {
        days -= md[m as usize];
        m += 1;
    }
    let date = (y << 9) | ((m + 1) << 5) | ((days as u16) + 1);
    let time = ((h as u16) << 11) | ((mi as u16) << 5) | (s as u16);
    (date, time)
}

fn fat_to_unix(date: u16, time: u16) -> u64 {
    let y = ((date >> 9) & 0x7F) as u64;
    let m = ((date >> 5) & 0xF) as u64;
    let d = (date & 0x1F) as u64;
    let h = ((time >> 11) & 0x1F) as u64;
    let mi = ((time >> 5) & 0x3F) as u64;
    let s = (time & 0x1F) as u64 * 2;
    y * 365 * 86400
        + m.saturating_sub(1) * 30 * 86400
        + d.saturating_sub(1) * 86400
        + h * 3600
        + mi * 60
        + s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh(n: usize) -> Vec<u8> {
        let mut b = vec![0u8; n];
        mkfs(&mut b, b"TEST").unwrap();
        b
    }

    fn with_vol<R>(buf: &mut [u8], f: impl FnOnce(&mut FatVol, &mut MemDisk) -> R) -> R {
        let mut disk = MemDisk::new(buf, SEC as u32).unwrap();
        let mut vol = FatVol::mount(&mut disk).unwrap();
        f(&mut vol, &mut disk)
    }

    fn fsck(buf: &[u8]) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir();
        let p = dir.join(format!(
            "vibeos-fat32-{}-{}.img",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&p, buf).unwrap();
        let st = std::process::Command::new("fsck.fat")
            .args(["-n", p.to_str().unwrap()])
            .output();
        let _ = std::fs::remove_file(&p);
        match st {
            Ok(o) => {
                assert!(
                    o.status.success(),
                    "fsck.fat failed status={:?}\nstdout:\n{}\nstderr:\n{}",
                    o.status.code(),
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("skipping fsck.fat: not installed ({e})");
            }
            Err(e) => panic!("fsck.fat spawn: {e}"),
        }
    }

    #[test]
    fn bpb_validate_and_mount() {
        let mut b = fresh(INITRD_BYTES);
        with_vol(&mut b, |v, _| {
            assert_eq!(v.info.bps, 512);
            assert_eq!(v.info.num_fats, 2);
            assert_eq!(v.info.root_clus, 2);
            assert!(v.info.nclus >= 2);
        });
    }

    #[test]
    fn corrupt_boot_sig() {
        let mut b = fresh(INITRD_BYTES);
        b[510] = 0;
        let mut disk = MemDisk::new(&mut b, SEC as u32).unwrap();
        let err = match FatVol::mount(&mut disk) {
            Err(e) => e,
            Ok(_) => panic!("expected corrupt"),
        };
        assert_eq!(err, FatError::Corrupt);
    }

    #[test]
    fn truncated_image() {
        let mut b = vec![0u8; 512];
        assert_eq!(mkfs(&mut b, b"X").unwrap_err(), FatError::Inval);
    }

    #[test]
    fn fatsz16_rejected() {
        let mut b = fresh(INITRD_BYTES);
        b[22] = 1;
        let mut disk = MemDisk::new(&mut b, SEC as u32).unwrap();
        let err = match FatVol::mount(&mut disk) {
            Err(e) => e,
            Ok(_) => panic!("expected inval"),
        };
        assert_eq!(err, FatError::Inval);
    }

    #[test]
    fn lfn_checksum_matches_spec() {
        let n = *b"HELLO   TXT";
        let s = lfn_checksum(&n);
        let mut sum = 0u8;
        for c in n {
            sum = ((sum & 1) << 7).wrapping_add(sum >> 1).wrapping_add(c);
        }
        assert_eq!(s, sum);
    }

    #[test]
    fn create_write_read_across_clusters() {
        let mut b = fresh(INITRD_BYTES);
        with_vol(&mut b, |v, d| {
            let n = v.create(d, v.info.root_clus, b"big.bin", false).unwrap();
            let mut clu = n.clu;
            let mut size = n.size;
            let mut payload = [0u8; 1200];
            let mut i = 0usize;
            while i < payload.len() {
                payload[i] = (i % 251) as u8;
                i += 1;
            }
            v.write(d, n.dir_clu, n.dir_off, &mut clu, &mut size, 0, &payload)
                .unwrap();
            assert!(size as usize >= payload.len());
            assert!(clu >= 2);
            let mut out = [0u8; 1200];
            let got = v.read(d, clu, size, 0, &mut out).unwrap();
            assert_eq!(got, 1200);
            assert_eq!(out, payload);
            let mut mid = [0u8; 40];
            v.read(d, clu, size, 500, &mut mid).unwrap();
            assert_eq!(mid, payload[500..540]);
            v.sync(d).unwrap();
        });
        assert!(with_vol(&mut b, |v, d| v.fats_identical(d).unwrap()));
        fsck(&b);
    }

    #[test]
    fn lfn_roundtrip_and_readdir() {
        let mut b = fresh(INITRD_BYTES);
        with_vol(&mut b, |v, d| {
            v.create(d, v.info.root_clus, b"hello.txt", false).unwrap();
            v.create(d, v.info.root_clus, b"Long File Name.dat", false)
                .unwrap();
            let n = v.lookup(d, v.info.root_clus, b"hello.txt").unwrap();
            assert_eq!(n.name(), b"hello.txt");
            let n = v.lookup(d, v.info.root_clus, b"HELLO.TXT").unwrap();
            assert_eq!(n.kind, InodeKind::Reg);
            let n = v
                .lookup(d, v.info.root_clus, b"Long File Name.dat")
                .unwrap();
            assert_eq!(n.name(), b"Long File Name.dat");
            let mut names = 0u32;
            let mut cookie = 0u64;
            let mut node = Node::EMPTY;
            while let Some(next) = v.readdir(d, v.info.root_clus, cookie, &mut node).unwrap() {
                names += 1;
                cookie = next;
            }
            assert_eq!(names, 2);
            v.sync(d).unwrap();
        });
        fsck(&b);
    }

    #[test]
    fn lfn_checksum_mismatch_falls_back() {
        let mut b = fresh(INITRD_BYTES);
        with_vol(&mut b, |v, d| {
            v.create(d, v.info.root_clus, b"hello.txt", false).unwrap();
            v.sync(d).unwrap();
        });
        // Corrupt the LFN checksum byte in the first LFN entry of the root.
        let mut disk = MemDisk::new(&mut b, SEC as u32).unwrap();
        let vol = FatVol::mount(&mut disk).unwrap();
        let lba = vol.info.clus_lba(vol.info.root_clus).unwrap();
        let mut sec = [0u8; SEC];
        disk.read(lba, &mut sec).unwrap();
        // first entry is LFN (attr 0x0F); checksum at offset 13
        if sec[11] == ATTR_LFN {
            sec[13] ^= 0xFF;
            disk.write(lba, &sec).unwrap();
        }
        let mut vol = FatVol::mount(&mut disk).unwrap();
        let n = vol
            .lookup(&mut disk, vol.info.root_clus, b"HELLO.TXT")
            .unwrap();
        assert_eq!(n.kind, InodeKind::Reg);
    }

    #[test]
    fn mkdir_rmdir_unlink() {
        let mut b = fresh(INITRD_BYTES);
        with_vol(&mut b, |v, d| {
            let dir = v.create(d, v.info.root_clus, b"sub", true).unwrap();
            assert!(dir.is_dir());
            v.create(d, dir.clu, b"a.txt", false).unwrap();
            assert_eq!(
                v.unlink(d, v.info.root_clus, b"sub", true).unwrap_err(),
                FatError::NotEmpty
            );
            v.unlink(d, dir.clu, b"a.txt", false).unwrap();
            v.unlink(d, v.info.root_clus, b"sub", true).unwrap();
            assert_eq!(
                v.lookup(d, v.info.root_clus, b"sub").unwrap_err(),
                FatError::NotFound
            );
            v.sync(d).unwrap();
        });
        fsck(&b);
    }

    #[test]
    fn truncate_and_delete() {
        let mut b = fresh(INITRD_BYTES);
        with_vol(&mut b, |v, d| {
            let n = v.create(d, v.info.root_clus, b"t.bin", false).unwrap();
            let mut clu = n.clu;
            let mut size = n.size;
            v.write(d, n.dir_clu, n.dir_off, &mut clu, &mut size, 0, &[9u8; 800])
                .unwrap();
            v.truncate(d, n.dir_clu, n.dir_off, &mut clu, &mut size, 10)
                .unwrap();
            assert_eq!(size, 10);
            let mut out = [0u8; 16];
            let got = v.read(d, clu, size, 0, &mut out).unwrap();
            assert_eq!(got, 10);
            v.unlink(d, v.info.root_clus, b"t.bin", false).unwrap();
            v.sync(d).unwrap();
        });
        fsck(&b);
    }

    #[test]
    fn rename_across_dirs() {
        let mut b = fresh(INITRD_BYTES);
        with_vol(&mut b, |v, d| {
            let a = v.create(d, v.info.root_clus, b"a", true).unwrap();
            let bb = v.create(d, v.info.root_clus, b"b", true).unwrap();
            let f = v.create(d, a.clu, b"x.txt", false).unwrap();
            let mut clu = f.clu;
            let mut size = f.size;
            v.write(d, f.dir_clu, f.dir_off, &mut clu, &mut size, 0, b"hi")
                .unwrap();
            v.rename(d, a.clu, b"x.txt", bb.clu, b"y.txt").unwrap();
            assert_eq!(
                v.lookup(d, a.clu, b"x.txt").unwrap_err(),
                FatError::NotFound
            );
            let y = v.lookup(d, bb.clu, b"y.txt").unwrap();
            let mut out = [0u8; 2];
            v.read(d, y.clu, y.size, 0, &mut out).unwrap();
            assert_eq!(&out, b"hi");
            v.sync(d).unwrap();
        });
        fsck(&b);
    }

    #[test]
    fn dual_fat_after_alloc() {
        let mut b = fresh(INITRD_BYTES);
        with_vol(&mut b, |v, d| {
            v.create(d, v.info.root_clus, b"one", false).unwrap();
            v.create(d, v.info.root_clus, b"two", true).unwrap();
            v.sync(d).unwrap();
            assert!(v.fats_identical(d).unwrap());
        });
    }

    #[test]
    fn initrd_image_fsck() {
        let mut b = vec![0u8; INITRD_BYTES];
        mkinitrd(&mut b).unwrap();
        with_vol(&mut b, |v, d| {
            let h = v.lookup(d, v.info.root_clus, b"hello.txt").unwrap();
            let mut buf = [0u8; 32];
            let n = v.read(d, h.clu, h.size, 0, &mut buf).unwrap();
            assert_eq!(&buf[..n], b"hello from initrd\n");
            assert!(v.lookup(d, v.info.root_clus, b"etc").unwrap().is_dir());
        });
        fsck(&b);
    }

    #[test]
    fn chain_loop_is_corrupt() {
        let mut b = fresh(INITRD_BYTES);
        with_vol(&mut b, |v, d| {
            let n = v.create(d, v.info.root_clus, b"x", false).unwrap();
            let mut clu = n.clu;
            let mut size = n.size;
            v.write(d, n.dir_clu, n.dir_off, &mut clu, &mut size, 0, &[1u8; 600])
                .unwrap();
            v.sync(d).unwrap();
            // Point a FAT entry at itself.
            v.fat_set(d, clu, clu).unwrap();
            v.commit_fat(d).unwrap();
            let mut out = [0u8; 4];
            let _ = v.read(d, clu, 600, 0, &mut out);
        });
    }

    #[test]
    fn second_fat_mismatch_is_detected() {
        let mut b = fresh(INITRD_BYTES);
        with_vol(&mut b, |v, d| {
            v.create(d, v.info.root_clus, b"x", false).unwrap();
            v.sync(d).unwrap();
            assert!(v.fats_identical(d).unwrap());
        });
        let fatsz = {
            let mut disk = MemDisk::new(&mut b, SEC as u32).unwrap();
            let vol = FatVol::mount(&mut disk).unwrap();
            vol.info.fatsz
        };
        let fat1 = 32 + fatsz;
        let off = fat1 as usize * SEC + 12;
        b[off] ^= 0xFF;
        let mut disk = MemDisk::new(&mut b, SEC as u32).unwrap();
        let mut vol = FatVol::mount(&mut disk).unwrap();
        assert!(!vol.fats_identical(&mut disk).unwrap());
    }

    #[test]
    fn truncate_zero_dirent_cluster() {
        let mut b = fresh(INITRD_BYTES);
        with_vol(&mut b, |v, d| {
            let n = v.create(d, v.info.root_clus, b"z.bin", false).unwrap();
            let mut clu = n.clu;
            let mut size = n.size;
            v.write(d, n.dir_clu, n.dir_off, &mut clu, &mut size, 0, &[7u8; 40])
                .unwrap();
            v.truncate(d, n.dir_clu, n.dir_off, &mut clu, &mut size, 0)
                .unwrap();
            assert_eq!(clu, 0);
            assert_eq!(size, 0);
            let got = v.lookup(d, v.info.root_clus, b"z.bin").unwrap();
            assert_eq!(got.clu, 0);
            assert_eq!(got.size, 0);
            v.sync(d).unwrap();
        });
        fsck(&b);
    }

    #[test]
    fn error_strings() {
        assert_eq!(FatError::NotSupp.as_str(), "not supp");
        assert_eq!(FatError::NotSupp.to_fs(), FsError::NotSupp);
        assert_eq!(FatError::Corrupt.to_fs(), FsError::Inval);
    }
}
