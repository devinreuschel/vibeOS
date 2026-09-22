//! Shared kernfs directory tree. ROADMAP §8.4.
//!
//! One node table and sibling-linked dirs. Four skins (devfs, tmpfs,
//! procfs, sysfs) fill different nodes; they do not each keep a dentry
//! tree. tmpfs file data goes through the Phase 7 [`Cache`] plus a
//! fixed ramdisk so eviction works — not a grow-only Vec.

use core::cell::RefCell;

use crate::block::BlockError;
use crate::cache::{self, Backend, Cache, CacheKey, CacheStats, PAGE};

use super::{
    Dirent, FileSystem, FsError, FsType, Inode, InodeKind, InodeOps, MAX_KERN_NODES, MAX_MOUNTS,
    MAX_NAME, Name, S_IFBLK, S_IFCHR, S_IFDIR_MODE, S_IFLNK_MODE, S_IFMT, S_IFREG_MODE, Stat, Vfs,
};

pub const TMPFS_CACHE_PAGES: usize = 4;
pub const TMPFS_BACK_PAGES: usize = 16;
pub const TMPFS_BACK_BYTES: usize = PAGE * TMPFS_BACK_PAGES;
const TMPFS_DEV: u32 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KernKind {
    Dir,
    File,
    Null,
    Zero,
    Random,
    Urandom,
    Console,
    Tty,
    Block,
    ProcCmdline,
    ProcStatus,
    ProcMaps,
    ProcFdDir,
    SysAttr,
    Lnk,
}

impl KernKind {
    fn inode_kind(self) -> InodeKind {
        match self {
            KernKind::Dir | KernKind::ProcFdDir => InodeKind::Dir,
            KernKind::Lnk => InodeKind::Lnk,
            KernKind::Block => InodeKind::Blk,
            KernKind::Null
            | KernKind::Zero
            | KernKind::Random
            | KernKind::Urandom
            | KernKind::Console
            | KernKind::Tty => InodeKind::Chr,
            KernKind::File
            | KernKind::ProcCmdline
            | KernKind::ProcStatus
            | KernKind::ProcMaps
            | KernKind::SysAttr => InodeKind::Reg,
        }
    }
}

pub const SYS_ATTR_VENDOR: u8 = 0;
pub const SYS_ATTR_DEVICE: u8 = 1;
pub const SYS_ATTR_CLASS: u8 = 2;
pub const SYS_ATTR_DRIVER: u8 = 3;

#[derive(Clone, Copy)]
struct KernNode {
    used: bool,
    sb: u8,
    parent: u32,
    next: u32,
    child: u32,
    kind: KernKind,
    mode: u16,
    nlink: u32,
    size: u64,
    atime: u64,
    mtime: u64,
    ctime: u64,
    name: Name,
    target: [u8; MAX_NAME],
    target_len: u8,
    extent_page: u16,
    extent_pages: u16,
    tag: u64,
    tag2: u64,
}

impl KernNode {
    const EMPTY: Self = Self {
        used: false,
        sb: 0,
        parent: 0,
        next: 0,
        child: 0,
        kind: KernKind::Dir,
        mode: 0,
        nlink: 0,
        size: 0,
        atime: 0,
        mtime: 0,
        ctime: 0,
        name: Name::EMPTY,
        target: [0; MAX_NAME],
        target_len: 0,
        extent_page: 0,
        extent_pages: 0,
        tag: 0,
        tag2: 0,
    };
}

pub(crate) struct KernState {
    nodes: [KernNode; MAX_KERN_NODES],
    tmp_cache: Cache<TMPFS_CACHE_PAGES>,
    tmp_back: [u8; TMPFS_BACK_BYTES],
    tmp_bits: u64,
    rng: u64,
    pub cons_out: [u8; 64],
    pub cons_len: u8,
}

impl KernState {
    pub const fn new() -> Self {
        Self {
            nodes: [KernNode::EMPTY; MAX_KERN_NODES],
            tmp_cache: Cache::new(),
            tmp_back: [0u8; TMPFS_BACK_BYTES],
            tmp_bits: 0,
            rng: 0,
            cons_out: [0u8; 64],
            cons_len: 0,
        }
    }
}

struct SliceBack<'a> {
    data: RefCell<&'a mut [u8]>,
}

impl Backend for SliceBack<'_> {
    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let d = self.data.borrow();
        let o = offset as usize;
        if o.checked_add(buf.len())
            .map(|e| e > d.len())
            .unwrap_or(true)
        {
            return Err(BlockError::Inval);
        }
        buf.copy_from_slice(&d[o..o + buf.len()]);
        Ok(())
    }

    fn write(&self, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
        let mut d = self.data.borrow_mut();
        let o = offset as usize;
        if o.checked_add(buf.len())
            .map(|e| e > d.len())
            .unwrap_or(true)
        {
            return Err(BlockError::Inval);
        }
        d[o..o + buf.len()].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&self) -> Result<(), BlockError> {
        Ok(())
    }
}

pub struct DevFs;
pub struct TmpFs;
pub struct ProcFs;
pub struct SysFs;

impl FileSystem for DevFs {
    fn name(&self) -> &'static str {
        "devfs"
    }
    fn fstype(&self) -> FsType {
        FsType::Dev
    }
    fn fill_super(&self, vfs: &mut Vfs, sb: u8) -> Result<u32, FsError> {
        let root = kern_mk_root(vfs, sb)?;
        kern_mk_special(vfs, sb, root, b"null", KernKind::Null, 0)?;
        kern_mk_special(vfs, sb, root, b"zero", KernKind::Zero, 0)?;
        kern_mk_special(vfs, sb, root, b"random", KernKind::Random, 0)?;
        kern_mk_special(vfs, sb, root, b"urandom", KernKind::Urandom, 0)?;
        kern_mk_special(vfs, sb, root, b"console", KernKind::Console, 0)?;
        kern_mk_special(vfs, sb, root, b"tty", KernKind::Tty, 0)?;
        Ok(root)
    }
}

impl FileSystem for TmpFs {
    fn name(&self) -> &'static str {
        "tmpfs"
    }
    fn fstype(&self) -> FsType {
        FsType::Tmp
    }
    fn fill_super(&self, vfs: &mut Vfs, sb: u8) -> Result<u32, FsError> {
        kern_mk_root(vfs, sb)
    }
}

impl FileSystem for ProcFs {
    fn name(&self) -> &'static str {
        "procfs"
    }
    fn fstype(&self) -> FsType {
        FsType::Proc
    }
    fn fill_super(&self, vfs: &mut Vfs, sb: u8) -> Result<u32, FsError> {
        let root = kern_mk_root(vfs, sb)?;
        // Phase 9 owns Process. One kernel-thread stub so a shell-only
        // boot does not panic looking up cmdline/status/maps/fd.
        let p1 = kern_mk_dir(vfs, sb, root, b"1")?;
        kern_mk_special(vfs, sb, p1, b"cmdline", KernKind::ProcCmdline, 1)?;
        kern_mk_special(vfs, sb, p1, b"status", KernKind::ProcStatus, 1)?;
        kern_mk_special(vfs, sb, p1, b"maps", KernKind::ProcMaps, 1)?;
        let _fd = kern_mk_special(vfs, sb, p1, b"fd", KernKind::ProcFdDir, 1)?;
        kern_mk_lnk(vfs, sb, root, b"self", b"1")?;
        Ok(root)
    }
}

impl FileSystem for SysFs {
    fn name(&self) -> &'static str {
        "sysfs"
    }
    fn fstype(&self) -> FsType {
        FsType::Sys
    }
    fn fill_super(&self, vfs: &mut Vfs, sb: u8) -> Result<u32, FsError> {
        let root = kern_mk_root(vfs, sb)?;
        let _devices = kern_mk_dir(vfs, sb, root, b"devices")?;
        let bus = kern_mk_dir(vfs, sb, root, b"bus")?;
        let pci = kern_mk_dir(vfs, sb, bus, b"pci")?;
        let _pdev = kern_mk_dir(vfs, sb, pci, b"devices")?;
        let _pdrv = kern_mk_dir(vfs, sb, pci, b"drivers")?;
        Ok(root)
    }
}

macro_rules! kern_ops {
    ($ty:ty) => {
        impl InodeOps for $ty {
            fn lookup(&self, vfs: &mut Vfs, dir_islot: u16, name: &[u8]) -> Result<u32, FsError> {
                kern_lookup(vfs, dir_islot, name)
            }
            fn create(
                &self,
                vfs: &mut Vfs,
                dir_islot: u16,
                name: &[u8],
                kind: InodeKind,
                mode: u16,
                target: Option<&[u8]>,
            ) -> Result<u32, FsError> {
                kern_create(vfs, dir_islot, name, kind, mode, target)
            }
            fn unlink(&self, vfs: &mut Vfs, dir_islot: u16, name: &[u8]) -> Result<(), FsError> {
                kern_unlink(vfs, dir_islot, name)
            }
            fn read(
                &self,
                vfs: &mut Vfs,
                islot: u16,
                off: u64,
                buf: &mut [u8],
            ) -> Result<usize, FsError> {
                kern_read(vfs, islot, off, buf)
            }
            fn write(
                &self,
                vfs: &mut Vfs,
                islot: u16,
                off: u64,
                buf: &[u8],
            ) -> Result<usize, FsError> {
                kern_write(vfs, islot, off, buf)
            }
            fn truncate(&self, vfs: &mut Vfs, islot: u16, size: u64) -> Result<(), FsError> {
                kern_truncate(vfs, islot, size)
            }
            fn readdir(
                &self,
                vfs: &mut Vfs,
                islot: u16,
                cookie: u64,
                out: &mut Dirent,
            ) -> Result<Option<u64>, FsError> {
                kern_readdir(vfs, islot, cookie, out)
            }
            fn stat(&self, vfs: &mut Vfs, islot: u16) -> Result<Stat, FsError> {
                kern_stat(vfs, islot)
            }
        }
    };
}

kern_ops!(DevFs);
kern_ops!(TmpFs);
kern_ops!(ProcFs);
kern_ops!(SysFs);

impl Vfs {
    /// Mount the four pseudo filesystems. Call after [`Vfs::mount_root`].
    /// FAT root cannot mkdir through VFS; those mount points are created
    /// via the File API and attached first (Exist / already-a-dir is ok).
    pub fn mount_pseudo(&mut self) -> Result<(), FsError> {
        self.ensure_mount_dir("/dev")?;
        self.ensure_mount_dir("/proc")?;
        self.ensure_mount_dir("/tmp")?;
        self.ensure_mount_dir("/sys")?;
        self.mount(None, "/dev", &DevFs)?;
        self.mount(None, "/proc", &ProcFs)?;
        self.mount(None, "/tmp", &TmpFs)?;
        self.mount(None, "/sys", &SysFs)?;
        Ok(())
    }

    fn ensure_mount_dir(&mut self, path: &str) -> Result<(), FsError> {
        match self.mkdir(None, path, 0o755) {
            Ok(_) | Err(FsError::Exists) => return Ok(()),
            Err(_) => {}
        }
        if let Ok(p) = self.resolve(None, path, true) {
            let islot = self.d_islot(p.dslot)?;
            if self.inodes[islot as usize].kind == InodeKind::Dir {
                return Ok(());
            }
            return Err(FsError::NotDir);
        }
        // FAT has no VFS mkdir. Plant a dir dentry so `mount` can cover it.
        let (parent, name) = super::split_basename(path.as_bytes())?;
        if super::name_is_dot(name) || super::name_is_dotdot(name) {
            return Err(FsError::Inval);
        }
        let dir = self.walk(None, parent, true)?;
        let sb = self.sb_of(dir.mount);
        let mut ino = 0x8000_0000u32;
        let mut i = 0usize;
        while i < name.len() {
            ino = ino.wrapping_mul(33).wrapping_add(name[i] as u32);
            i += 1;
        }
        let islot = self.fat_iget(sb, ino, InodeKind::Dir, 0, 0)?;
        match self.fat_dcache(dir, name, islot) {
            Ok(p) => {
                self.dentries[p.dslot as usize].pinned = true;
                Ok(())
            }
            Err(e) => {
                self.release_inode(islot);
                Err(e)
            }
        }
    }

    pub fn tmp_cache_stats(&self) -> CacheStats {
        self.kern.tmp_cache.stats
    }

    pub fn cons_captured(&self) -> &[u8] {
        &self.kern.cons_out[..self.kern.cons_len as usize]
    }

    /// Add a block device node under `/dev`. Name should match
    /// `block: <name>` (ram0, vda, ram0p1, …).
    pub fn devfs_add_block(&mut self, name: &[u8], size: u64) -> Result<u32, FsError> {
        let sb = sb_of_type(self, FsType::Dev).ok_or(FsError::Io)?;
        let root = self.supers[sb as usize].root_ino;
        kern_mk_special(self, sb, root, name, KernKind::Block, size)
    }

    /// PCI device + driver binding under `/sys`. `name` is the BDF
    /// (`00:01.0`). `driver` none or empty → unbound (`-`).
    pub fn sysfs_add_device(
        &mut self,
        name: &[u8],
        vendor: u16,
        device: u16,
        class: u8,
        driver: Option<&[u8]>,
    ) -> Result<(), FsError> {
        let sb = sb_of_type(self, FsType::Sys).ok_or(FsError::Io)?;
        let root = self.supers[sb as usize].root_ino;
        let devices = kern_lookup_ino(self, sb, root, b"devices")?;
        let bus = kern_lookup_ino(self, sb, root, b"bus")?;
        let pci = kern_lookup_ino(self, sb, bus, b"pci")?;
        let pci_devs = kern_lookup_ino(self, sb, pci, b"devices")?;
        let pci_drvs = kern_lookup_ino(self, sb, pci, b"drivers")?;

        let ddir = match kern_lookup_ino(self, sb, devices, name) {
            Ok(ino) => ino,
            Err(FsError::NotFound) => kern_mk_dir(self, sb, devices, name)?,
            Err(e) => return Err(e),
        };
        let packed = ((vendor as u64) << 16) | (device as u64);
        kern_mk_sys_attr(
            self,
            sb,
            ddir,
            b"vendor",
            SYS_ATTR_VENDOR,
            packed,
            class,
            None,
        )?;
        kern_mk_sys_attr(
            self,
            sb,
            ddir,
            b"device",
            SYS_ATTR_DEVICE,
            packed,
            class,
            None,
        )?;
        kern_mk_sys_attr(
            self,
            sb,
            ddir,
            b"class",
            SYS_ATTR_CLASS,
            packed,
            class,
            None,
        )?;
        let drv_bytes = match driver {
            Some(d) if !d.is_empty() => d,
            _ => b"-",
        };
        kern_mk_sys_attr(
            self,
            sb,
            ddir,
            b"driver",
            SYS_ATTR_DRIVER,
            packed,
            class,
            Some(drv_bytes),
        )?;

        let mut rel = [0u8; MAX_NAME];
        let rlen = rel_to_devices(name, &mut rel)?;
        kern_mk_lnk(self, sb, pci_devs, name, &rel[..rlen])?;

        if drv_bytes != b"-" {
            let ddir_drv = match kern_lookup_ino(self, sb, pci_drvs, drv_bytes) {
                Ok(ino) => ino,
                Err(FsError::NotFound) => kern_mk_dir(self, sb, pci_drvs, drv_bytes)?,
                Err(e) => return Err(e),
            };
            let _ = kern_mk_lnk(self, sb, ddir_drv, name, &rel[..rlen]);
        }
        Ok(())
    }
}

fn set_target(buf: &mut [u8; MAX_NAME], len: &mut u8, t: &[u8]) -> Result<(), FsError> {
    if t.is_empty() || t.len() > MAX_NAME {
        return Err(FsError::Inval);
    }
    let mut i = 0usize;
    while i < t.len() {
        if t[i] == 0 {
            return Err(FsError::Inval);
        }
        i += 1;
    }
    buf[..t.len()].copy_from_slice(t);
    if t.len() < MAX_NAME {
        buf[t.len()..].fill(0);
    }
    *len = t.len() as u8;
    Ok(())
}

fn target_bytes(n: &KernNode) -> &[u8] {
    &n.target[..n.target_len as usize]
}

fn rel_to_devices(name: &[u8], out: &mut [u8; MAX_NAME]) -> Result<usize, FsError> {
    // ../../../devices/<name>
    let p = b"../../../devices/";
    if p.len() + name.len() > MAX_NAME {
        return Err(FsError::NameTooLong);
    }
    out[..p.len()].copy_from_slice(p);
    out[p.len()..p.len() + name.len()].copy_from_slice(name);
    Ok(p.len() + name.len())
}

fn sb_of_type(vfs: &Vfs, t: FsType) -> Option<u8> {
    let mut i = 0usize;
    while i < MAX_MOUNTS {
        if vfs.supers[i].used && vfs.supers[i].fstype == t {
            return Some(i as u8);
        }
        i += 1;
    }
    None
}

fn kern_idx(ino: u32) -> Option<usize> {
    if ino == 0 {
        return None;
    }
    let i = (ino - 1) as usize;
    if i >= MAX_KERN_NODES { None } else { Some(i) }
}

fn kern_get(vfs: &Vfs, sb: u8, ino: u32) -> Option<&KernNode> {
    let i = kern_idx(ino)?;
    let n = &vfs.kern.nodes[i];
    if n.used && n.sb == sb { Some(n) } else { None }
}

fn kern_get_mut(vfs: &mut Vfs, sb: u8, ino: u32) -> Option<&mut KernNode> {
    let i = kern_idx(ino)?;
    let n = &mut vfs.kern.nodes[i];
    if n.used && n.sb == sb { Some(n) } else { None }
}

pub(super) fn kern_nlink(vfs: &Vfs, sb: u8, ino: u32) -> u32 {
    kern_get(vfs, sb, ino).map(|n| n.nlink).unwrap_or(0)
}

pub(super) struct KernMeta {
    pub kind: InodeKind,
    pub mode: u16,
    pub nlink: u32,
    pub size: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
}

pub(super) fn kern_meta(vfs: &Vfs, sb: u8, ino: u32) -> Option<KernMeta> {
    let n = kern_get(vfs, sb, ino)?;
    Some(KernMeta {
        kind: n.kind.inode_kind(),
        mode: n.mode,
        nlink: n.nlink,
        size: n.size,
        atime: n.atime,
        mtime: n.mtime,
        ctime: n.ctime,
    })
}

fn kern_alloc(vfs: &mut Vfs, sb: u8) -> Result<u32, FsError> {
    let mut i = 0usize;
    while i < MAX_KERN_NODES {
        if !vfs.kern.nodes[i].used {
            vfs.kern.nodes[i] = KernNode::EMPTY;
            vfs.kern.nodes[i].used = true;
            vfs.kern.nodes[i].sb = sb;
            return Ok((i as u32) + 1);
        }
        i += 1;
    }
    Err(FsError::NoSpace)
}

fn kern_link(vfs: &mut Vfs, parent: u32, child: u32) {
    let Some(pi) = kern_idx(parent) else {
        return;
    };
    let Some(ci) = kern_idx(child) else {
        return;
    };
    vfs.kern.nodes[ci].parent = parent;
    vfs.kern.nodes[ci].next = vfs.kern.nodes[pi].child;
    vfs.kern.nodes[pi].child = child;
}

fn kern_unlink_child(vfs: &mut Vfs, parent: u32, child: u32) {
    let Some(pi) = kern_idx(parent) else {
        return;
    };
    let mut prev: Option<usize> = None;
    let mut cur = vfs.kern.nodes[pi].child;
    while cur != 0 {
        let Some(ci) = kern_idx(cur) else {
            break;
        };
        let next = vfs.kern.nodes[ci].next;
        if cur == child {
            match prev {
                None => vfs.kern.nodes[pi].child = next,
                Some(p) => vfs.kern.nodes[p].next = next,
            }
            vfs.kern.nodes[ci].next = 0;
            vfs.kern.nodes[ci].parent = 0;
            return;
        }
        prev = Some(ci);
        cur = next;
    }
}

fn kern_find_child(vfs: &Vfs, sb: u8, parent: u32, name: &[u8]) -> Option<u32> {
    let p = kern_get(vfs, sb, parent)?;
    let mut cur = p.child;
    while cur != 0 {
        let Some(n) = kern_get(vfs, sb, cur) else {
            break;
        };
        if n.name.eq_bytes(name) {
            return Some(cur);
        }
        cur = n.next;
    }
    None
}

fn kern_lookup_ino(vfs: &Vfs, sb: u8, parent: u32, name: &[u8]) -> Result<u32, FsError> {
    kern_find_child(vfs, sb, parent, name).ok_or(FsError::NotFound)
}

fn kern_mk_root(vfs: &mut Vfs, sb: u8) -> Result<u32, FsError> {
    let ino = kern_alloc(vfs, sb)?;
    let t = vfs.now;
    if let Some(n) = kern_get_mut(vfs, sb, ino) {
        n.kind = KernKind::Dir;
        n.mode = S_IFDIR_MODE;
        n.nlink = 2;
        n.atime = t;
        n.mtime = t;
        n.ctime = t;
        n.parent = ino;
    }
    Ok(ino)
}

fn kern_mk_dir(vfs: &mut Vfs, sb: u8, parent: u32, name: &[u8]) -> Result<u32, FsError> {
    if kern_find_child(vfs, sb, parent, name).is_some() {
        return Err(FsError::Exists);
    }
    let nm = Name::from_bytes(name)?;
    let ino = kern_alloc(vfs, sb)?;
    let t = vfs.now;
    if let Some(n) = kern_get_mut(vfs, sb, ino) {
        n.kind = KernKind::Dir;
        n.mode = S_IFDIR_MODE;
        n.nlink = 2;
        n.atime = t;
        n.mtime = t;
        n.ctime = t;
        n.name = nm;
    }
    kern_link(vfs, parent, ino);
    if let Some(p) = kern_get_mut(vfs, sb, parent) {
        p.nlink = p.nlink.saturating_add(1);
        p.mtime = t;
        p.ctime = t;
    }
    Ok(ino)
}

fn kern_mk_lnk(
    vfs: &mut Vfs,
    sb: u8,
    parent: u32,
    name: &[u8],
    target: &[u8],
) -> Result<u32, FsError> {
    if let Some(ino) = kern_find_child(vfs, sb, parent, name) {
        return Ok(ino);
    }
    let nm = Name::from_bytes(name)?;
    let ino = kern_alloc(vfs, sb)?;
    let t = vfs.now;
    if let Some(n) = kern_get_mut(vfs, sb, ino) {
        n.kind = KernKind::Lnk;
        n.mode = S_IFLNK_MODE;
        n.nlink = 1;
        n.size = target.len() as u64;
        n.atime = t;
        n.mtime = t;
        n.ctime = t;
        n.name = nm;
        if set_target(&mut n.target, &mut n.target_len, target).is_err() {
            n.used = false;
            return Err(FsError::Inval);
        }
    }
    kern_link(vfs, parent, ino);
    touch_dir(vfs, sb, parent, t);
    Ok(ino)
}

fn kern_mk_special(
    vfs: &mut Vfs,
    sb: u8,
    parent: u32,
    name: &[u8],
    kind: KernKind,
    tag: u64,
) -> Result<u32, FsError> {
    if let Some(ino) = kern_find_child(vfs, sb, parent, name) {
        return Ok(ino);
    }
    let nm = Name::from_bytes(name)?;
    let ino = kern_alloc(vfs, sb)?;
    let t = vfs.now;
    let size = match kind {
        KernKind::ProcCmdline => PROC_CMDLINE.len() as u64,
        KernKind::ProcStatus => PROC_STATUS.len() as u64,
        KernKind::ProcMaps => PROC_MAPS.len() as u64,
        KernKind::Block => tag,
        _ => 0,
    };
    let mode = match kind.inode_kind() {
        InodeKind::Dir => S_IFDIR_MODE,
        InodeKind::Lnk => S_IFLNK_MODE,
        InodeKind::Blk => S_IFBLK | 0o666,
        InodeKind::Chr => S_IFCHR | 0o666,
        InodeKind::Reg => S_IFREG_MODE,
    };
    let nlink = if kind.inode_kind() == InodeKind::Dir {
        2
    } else {
        1
    };
    if let Some(n) = kern_get_mut(vfs, sb, ino) {
        n.kind = kind;
        n.mode = mode;
        n.nlink = nlink;
        n.size = size;
        n.atime = t;
        n.mtime = t;
        n.ctime = t;
        n.name = nm;
        n.tag = tag;
    }
    kern_link(vfs, parent, ino);
    if kind.inode_kind() == InodeKind::Dir
        && let Some(p) = kern_get_mut(vfs, sb, parent)
    {
        p.nlink = p.nlink.saturating_add(1);
    }
    touch_dir(vfs, sb, parent, t);
    Ok(ino)
}

#[allow(clippy::too_many_arguments)] // sysfs attr packed PCI identity
fn kern_mk_sys_attr(
    vfs: &mut Vfs,
    sb: u8,
    parent: u32,
    name: &[u8],
    which: u8,
    packed: u64,
    class: u8,
    driver: Option<&[u8]>,
) -> Result<u32, FsError> {
    if let Some(ino) = kern_find_child(vfs, sb, parent, name) {
        return Ok(ino);
    }
    let nm = Name::from_bytes(name)?;
    let ino = kern_alloc(vfs, sb)?;
    let t = vfs.now;
    let size = match which {
        SYS_ATTR_VENDOR | SYS_ATTR_DEVICE => 7,
        SYS_ATTR_CLASS => 5,
        SYS_ATTR_DRIVER => {
            if let Some(d) = driver {
                (d.len() + 1) as u64
            } else {
                2
            }
        }
        _ => 0,
    };
    if let Some(n) = kern_get_mut(vfs, sb, ino) {
        n.kind = KernKind::SysAttr;
        n.mode = S_IFREG_MODE;
        n.nlink = 1;
        n.size = size;
        n.atime = t;
        n.mtime = t;
        n.ctime = t;
        n.name = nm;
        if let Some(d) = driver {
            let _ = set_target(&mut n.target, &mut n.target_len, d);
        }
        n.tag = packed;
        n.tag2 = ((class as u64) << 8) | (which as u64);
    }
    kern_link(vfs, parent, ino);
    touch_dir(vfs, sb, parent, t);
    Ok(ino)
}

fn touch_dir(vfs: &mut Vfs, sb: u8, ino: u32, t: u64) {
    if let Some(p) = kern_get_mut(vfs, sb, ino) {
        p.mtime = t;
        p.ctime = t;
    }
}

pub(super) fn kern_fill_inode(vfs: &mut Vfs, slot: u16, sb: u8, ino: u32) -> Result<(), FsError> {
    let m = kern_meta(vfs, sb, ino).ok_or(FsError::NotFound)?;
    vfs.inodes[slot as usize] = Inode {
        used: true,
        clock: true,
        refs: 0,
        sb,
        ino,
        kind: m.kind,
        mode: m.mode,
        nlink: m.nlink,
        size: m.size,
        atime: m.atime,
        mtime: m.mtime,
        ctime: m.ctime,
        data0: 0,
    };
    Ok(())
}

pub(super) fn kern_drop_sb(vfs: &mut Vfs, sb: u8) {
    let is_tmp = vfs.supers[sb as usize].fstype == FsType::Tmp;
    let mut i = 0usize;
    while i < MAX_KERN_NODES {
        if vfs.kern.nodes[i].used && vfs.kern.nodes[i].sb == sb {
            if vfs.kern.nodes[i].kind == KernKind::File {
                tmp_free_extent(vfs, i);
            }
            vfs.kern.nodes[i] = KernNode::EMPTY;
        }
        i += 1;
    }
    if is_tmp {
        vfs.kern.tmp_bits = 0;
        vfs.kern.tmp_cache.drop_dev(TMPFS_DEV);
        vfs.kern.tmp_back = [0u8; TMPFS_BACK_BYTES];
    }
}

pub(super) fn kern_try_free(vfs: &mut Vfs, sb: u8, ino: u32) {
    let Some(i) = kern_idx(ino) else {
        return;
    };
    if !vfs.kern.nodes[i].used || vfs.kern.nodes[i].sb != sb {
        return;
    }
    if vfs.kern.nodes[i].nlink != 0 {
        return;
    }
    let mut c = 0usize;
    while c < super::MAX_INODES {
        if vfs.inodes[c].used
            && vfs.inodes[c].sb == sb
            && vfs.inodes[c].ino == ino
            && vfs.inodes[c].refs != 0
        {
            return;
        }
        c += 1;
    }
    if vfs.kern.nodes[i].kind == KernKind::File {
        tmp_free_extent(vfs, i);
    }
    vfs.kern.nodes[i] = KernNode::EMPTY;
}

pub(super) fn kern_lookup(vfs: &mut Vfs, dir_islot: u16, name: &[u8]) -> Result<u32, FsError> {
    let sb = vfs.inodes[dir_islot as usize].sb;
    let ino = vfs.inodes[dir_islot as usize].ino;
    let n = kern_get(vfs, sb, ino).ok_or(FsError::NotFound)?;
    if n.kind.inode_kind() != InodeKind::Dir {
        return Err(FsError::NotDir);
    }
    kern_find_child(vfs, sb, ino, name).ok_or(FsError::NotFound)
}

pub(super) fn kern_create(
    vfs: &mut Vfs,
    dir_islot: u16,
    name: &[u8],
    kind: InodeKind,
    mode: u16,
    target: Option<&[u8]>,
) -> Result<u32, FsError> {
    let sb = vfs.inodes[dir_islot as usize].sb;
    if vfs.supers[sb as usize].fstype != FsType::Tmp {
        return Err(FsError::NotSupp);
    }
    let dir_ino = vfs.inodes[dir_islot as usize].ino;
    let nm = Name::from_bytes(name)?;
    if nm.is_dot() || nm.is_dotdot() {
        return Err(FsError::Inval);
    }
    match kind {
        InodeKind::Reg | InodeKind::Dir | InodeKind::Lnk => {}
        InodeKind::Chr | InodeKind::Blk => return Err(FsError::NotSupp),
    }
    if kind == InodeKind::Lnk {
        let t = target.ok_or(FsError::Inval)?;
        if t.is_empty() {
            return Err(FsError::Inval);
        }
    }
    let d = kern_get(vfs, sb, dir_ino).ok_or(FsError::NotFound)?;
    if d.kind.inode_kind() != InodeKind::Dir {
        return Err(FsError::NotDir);
    }
    if kern_find_child(vfs, sb, dir_ino, name).is_some() {
        return Err(FsError::Exists);
    }
    match kind {
        InodeKind::Dir => kern_mk_dir(vfs, sb, dir_ino, name),
        InodeKind::Lnk => kern_mk_lnk(vfs, sb, dir_ino, name, target.unwrap_or(b"")),
        InodeKind::Reg => {
            let ino = kern_alloc(vfs, sb)?;
            let t = vfs.now;
            if let Some(n) = kern_get_mut(vfs, sb, ino) {
                n.kind = KernKind::File;
                n.mode = (mode & !S_IFMT) | InodeKind::Reg.ifmt();
                n.nlink = 1;
                n.atime = t;
                n.mtime = t;
                n.ctime = t;
                n.name = nm;
            }
            kern_link(vfs, dir_ino, ino);
            touch_dir(vfs, sb, dir_ino, t);
            Ok(ino)
        }
        InodeKind::Chr | InodeKind::Blk => Err(FsError::NotSupp),
    }
}

pub(super) fn kern_unlink(vfs: &mut Vfs, dir_islot: u16, name: &[u8]) -> Result<(), FsError> {
    let sb = vfs.inodes[dir_islot as usize].sb;
    if vfs.supers[sb as usize].fstype != FsType::Tmp {
        return Err(FsError::NotSupp);
    }
    let dir_ino = vfs.inodes[dir_islot as usize].ino;
    let child = kern_find_child(vfs, sb, dir_ino, name).ok_or(FsError::NotFound)?;
    let kind = kern_get(vfs, sb, child).ok_or(FsError::NotFound)?.kind;
    if kind.inode_kind() == InodeKind::Dir {
        let ch = kern_get(vfs, sb, child).ok_or(FsError::NotFound)?;
        if ch.child != 0 {
            return Err(FsError::NotEmpty);
        }
    }
    let t = vfs.now;
    kern_unlink_child(vfs, dir_ino, child);
    if kind.inode_kind() == InodeKind::Dir
        && let Some(p) = kern_get_mut(vfs, sb, dir_ino)
    {
        p.nlink = p.nlink.saturating_sub(1);
    }
    if let Some(c) = kern_get_mut(vfs, sb, child) {
        c.nlink = c.nlink.saturating_sub(1);
        c.ctime = t;
    }
    let mut i = 0usize;
    while i < super::MAX_INODES {
        if vfs.inodes[i].used && vfs.inodes[i].sb == sb && vfs.inodes[i].ino == child {
            vfs.inodes[i].nlink = kern_nlink(vfs, sb, child);
            vfs.inodes[i].ctime = t;
        }
        i += 1;
    }
    kern_try_free(vfs, sb, child);
    touch_dir(vfs, sb, dir_ino, t);
    vfs.inodes[dir_islot as usize].nlink = kern_nlink(vfs, sb, dir_ino);
    Ok(())
}

pub(super) fn kern_read(
    vfs: &mut Vfs,
    islot: u16,
    off: u64,
    buf: &mut [u8],
) -> Result<usize, FsError> {
    let sb = vfs.inodes[islot as usize].sb;
    let ino = vfs.inodes[islot as usize].ino;
    let kind = kern_get(vfs, sb, ino).ok_or(FsError::NotFound)?.kind;
    match kind {
        KernKind::Dir | KernKind::ProcFdDir => Err(FsError::IsDir),
        KernKind::Null => Ok(0),
        KernKind::Zero => {
            buf.fill(0);
            Ok(buf.len())
        }
        KernKind::Random | KernKind::Urandom => {
            let mut i = crate::entropy::hw_fill(buf);
            if i < buf.len() {
                // Weak xorshift fallback when virtio-rng and rdrand are dry.
                crate::entropy::warn_xorshift();
                if i == 0 {
                    crate::entropy::set_last_source(crate::entropy::Source::XorShift);
                }
                while i < buf.len() {
                    let x = mix_rng(vfs);
                    let b = x.to_le_bytes();
                    let n = (buf.len() - i).min(8);
                    buf[i..i + n].copy_from_slice(&b[..n]);
                    i += n;
                }
            }
            Ok(buf.len())
        }
        KernKind::Console | KernKind::Tty => Ok(0),
        KernKind::Block => Err(FsError::NotSupp),
        KernKind::Lnk => {
            let n = kern_get(vfs, sb, ino).ok_or(FsError::NotFound)?;
            copy_off(target_bytes(n), off, buf)
        }
        KernKind::ProcCmdline => copy_off(PROC_CMDLINE, off, buf),
        KernKind::ProcStatus => copy_off(PROC_STATUS, off, buf),
        KernKind::ProcMaps => copy_off(PROC_MAPS, off, buf),
        KernKind::SysAttr => sys_attr_read(vfs, sb, ino, off, buf),
        KernKind::File => tmp_read(vfs, sb, ino, off, buf),
    }
}

pub(super) fn kern_write(
    vfs: &mut Vfs,
    islot: u16,
    off: u64,
    buf: &[u8],
) -> Result<usize, FsError> {
    let sb = vfs.inodes[islot as usize].sb;
    let ino = vfs.inodes[islot as usize].ino;
    let kind = kern_get(vfs, sb, ino).ok_or(FsError::NotFound)?.kind;
    match kind {
        KernKind::Dir | KernKind::ProcFdDir => Err(FsError::IsDir),
        KernKind::Null | KernKind::Zero => Ok(buf.len()),
        KernKind::Random | KernKind::Urandom => Ok(buf.len()),
        KernKind::Console | KernKind::Tty => {
            // Accept the write. Do not fan out to serial here: VFS
            // holds RANK_DEVICE and DESIGN §2.1 forbids holding that
            // across serial.
            let n = buf.len().min(64);
            vfs.kern.cons_out[..n].copy_from_slice(&buf[..n]);
            vfs.kern.cons_len = n as u8;
            Ok(buf.len())
        }
        KernKind::Block => Err(FsError::NotSupp),
        KernKind::Lnk
        | KernKind::ProcCmdline
        | KernKind::ProcStatus
        | KernKind::ProcMaps
        | KernKind::SysAttr => Err(FsError::Inval),
        KernKind::File => tmp_write(vfs, sb, ino, off, buf),
    }
}

pub(super) fn kern_truncate(vfs: &mut Vfs, islot: u16, size: u64) -> Result<(), FsError> {
    let sb = vfs.inodes[islot as usize].sb;
    let ino = vfs.inodes[islot as usize].ino;
    let kind = kern_get(vfs, sb, ino).ok_or(FsError::NotFound)?.kind;
    match kind {
        KernKind::File => tmp_truncate(vfs, sb, ino, size),
        KernKind::Dir | KernKind::ProcFdDir => Err(FsError::IsDir),
        _ => Err(FsError::Inval),
    }
}

pub(super) fn kern_readdir(
    vfs: &mut Vfs,
    islot: u16,
    cookie: u64,
    out: &mut Dirent,
) -> Result<Option<u64>, FsError> {
    let sb = vfs.inodes[islot as usize].sb;
    let ino = vfs.inodes[islot as usize].ino;
    let n = kern_get(vfs, sb, ino).ok_or(FsError::NotFound)?;
    if n.kind.inode_kind() != InodeKind::Dir {
        return Err(FsError::NotDir);
    }
    let mut cur = n.child;
    let mut i = 0u64;
    while cur != 0 {
        if i == cookie {
            let c = kern_get(vfs, sb, cur).ok_or(FsError::NotFound)?;
            out.ino = cur;
            out.kind = c.kind.inode_kind();
            out.name = c.name;
            return Ok(Some(cookie + 1));
        }
        let next = kern_get(vfs, sb, cur).ok_or(FsError::NotFound)?.next;
        cur = next;
        i += 1;
    }
    Ok(None)
}

pub(super) fn kern_stat(vfs: &mut Vfs, islot: u16) -> Result<Stat, FsError> {
    let sb = vfs.inodes[islot as usize].sb;
    let ino = vfs.inodes[islot as usize].ino;
    if let Some(m) = kern_meta(vfs, sb, ino) {
        return Ok(Stat {
            ino,
            kind: m.kind,
            mode: m.mode,
            nlink: m.nlink,
            size: m.size,
            atime: m.atime,
            mtime: m.mtime,
            ctime: m.ctime,
        });
    }
    let n = &vfs.inodes[islot as usize];
    Ok(Stat {
        ino: n.ino,
        kind: n.kind,
        mode: n.mode,
        nlink: n.nlink,
        size: n.size,
        atime: n.atime,
        mtime: n.mtime,
        ctime: n.ctime,
    })
}

pub(super) fn kern_readlink(vfs: &mut Vfs, islot: u16, buf: &mut [u8]) -> Result<usize, FsError> {
    let sb = vfs.inodes[islot as usize].sb;
    let ino = vfs.inodes[islot as usize].ino;
    let n = kern_get(vfs, sb, ino).ok_or(FsError::NotFound)?;
    match n.kind {
        KernKind::Lnk => {
            let t = target_bytes(n);
            let n = t.len().min(buf.len());
            buf[..n].copy_from_slice(&t[..n]);
            Ok(n)
        }
        KernKind::Dir | KernKind::ProcFdDir => Err(FsError::IsDir),
        _ => Err(FsError::Inval),
    }
}

fn copy_off(src: &[u8], off: u64, buf: &mut [u8]) -> Result<usize, FsError> {
    if off as usize >= src.len() {
        return Ok(0);
    }
    let start = off as usize;
    let n = (src.len() - start).min(buf.len());
    buf[..n].copy_from_slice(&src[start..start + n]);
    Ok(n)
}

const PROC_CMDLINE: &[u8] = b"vibeos\0";
const PROC_STATUS: &[u8] = b"Name:\tvibeos\nState:\tR (running)\nPid:\t1\nThreads:\t1\n";
const PROC_MAPS: &[u8] = b"# no user mappings; process objects are phase 9\n";

fn sys_attr_read(vfs: &Vfs, sb: u8, ino: u32, off: u64, buf: &mut [u8]) -> Result<usize, FsError> {
    let n = kern_get(vfs, sb, ino).ok_or(FsError::NotFound)?;
    let which = (n.tag2 & 0xff) as u8;
    let class = ((n.tag2 >> 8) & 0xff) as u8;
    let vendor = (n.tag >> 16) as u16;
    let device = (n.tag & 0xffff) as u16;
    let mut tmp = [0u8; 64];
    let len = match which {
        SYS_ATTR_VENDOR => fmt_hex_u16(vendor, &mut tmp),
        SYS_ATTR_DEVICE => fmt_hex_u16(device, &mut tmp),
        SYS_ATTR_CLASS => fmt_hex_u8(class, &mut tmp),
        SYS_ATTR_DRIVER => {
            let d = target_bytes(n);
            if d.is_empty() {
                tmp[0] = b'-';
                tmp[1] = b'\n';
                2
            } else {
                let n = d.len().min(62);
                tmp[..n].copy_from_slice(&d[..n]);
                tmp[n] = b'\n';
                n + 1
            }
        }
        _ => 0,
    };
    copy_off(&tmp[..len], off, buf)
}

fn hex_nib(d: u8) -> u8 {
    if d < 10 { b'0' + d } else { b'a' + (d - 10) }
}

fn fmt_hex_u16(n: u16, out: &mut [u8]) -> usize {
    if out.len() < 7 {
        return 0;
    }
    out[0] = b'0';
    out[1] = b'x';
    out[2] = hex_nib(((n >> 12) & 0xf) as u8);
    out[3] = hex_nib(((n >> 8) & 0xf) as u8);
    out[4] = hex_nib(((n >> 4) & 0xf) as u8);
    out[5] = hex_nib((n & 0xf) as u8);
    out[6] = b'\n';
    7
}

fn fmt_hex_u8(n: u8, out: &mut [u8]) -> usize {
    if out.len() < 5 {
        return 0;
    }
    out[0] = b'0';
    out[1] = b'x';
    out[2] = hex_nib((n >> 4) & 0xf);
    out[3] = hex_nib(n & 0xf);
    out[4] = b'\n';
    5
}

fn mix_rng(vfs: &mut Vfs) -> u64 {
    let mut x = vfs.kern.rng;
    if x == 0 {
        x = vfs.now ^ 0x9E37_79B9_7F4A_7C15;
        if x == 0 {
            x = 1;
        }
    }
    // xorshift64. Not a CSPRNG.
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    vfs.kern.rng = x;
    x
}

fn bit_get(bits: u64, i: usize) -> bool {
    i < 64 && (bits & (1u64 << i)) != 0
}

fn tmp_alloc_run(vfs: &mut Vfs, n: usize) -> Result<u16, FsError> {
    if n == 0 {
        return Ok(0);
    }
    if n > TMPFS_BACK_PAGES {
        return Err(FsError::NoSpace);
    }
    let bits = vfs.kern.tmp_bits;
    let mut i = 0usize;
    while i + n <= TMPFS_BACK_PAGES {
        let mut ok = true;
        let mut j = 0usize;
        while j < n {
            if bit_get(bits, i + j) {
                ok = false;
                break;
            }
            j += 1;
        }
        if ok {
            let mut b = bits;
            let mut j = 0usize;
            while j < n {
                b |= 1u64 << (i + j);
                j += 1;
            }
            vfs.kern.tmp_bits = b;
            let start = i * PAGE;
            vfs.kern.tmp_back[start..start + n * PAGE].fill(0);
            tmp_invalidate_pages(vfs, i as u16, n as u16);
            return Ok(i as u16);
        }
        i += 1;
    }
    Err(FsError::NoSpace)
}

fn tmp_free_run(vfs: &mut Vfs, start: u16, n: u16) {
    if n == 0 {
        return;
    }
    let mut b = vfs.kern.tmp_bits;
    let mut j = 0u16;
    while j < n {
        let i = start as usize + j as usize;
        if i < 64 {
            b &= !(1u64 << i);
        }
        j += 1;
    }
    vfs.kern.tmp_bits = b;
    tmp_invalidate_pages(vfs, start, n);
}

fn tmp_invalidate_pages(vfs: &mut Vfs, start: u16, n: u16) {
    let mut j = 0u16;
    while j < n {
        let off = (start as u64 + j as u64) * PAGE as u64;
        vfs.kern
            .tmp_cache
            .invalidate(CacheKey::page(TMPFS_DEV, off));
        j += 1;
    }
}

fn tmp_free_extent(vfs: &mut Vfs, idx: usize) {
    let start = vfs.kern.nodes[idx].extent_page;
    let n = vfs.kern.nodes[idx].extent_pages;
    tmp_free_run(vfs, start, n);
    vfs.kern.nodes[idx].extent_page = 0;
    vfs.kern.nodes[idx].extent_pages = 0;
}

fn tmp_pages_for(size: u64) -> usize {
    if size == 0 {
        0
    } else {
        (size as usize).div_ceil(PAGE)
    }
}

fn tmp_ensure(vfs: &mut Vfs, sb: u8, ino: u32, new_size: u64) -> Result<(), FsError> {
    let Some(idx) = kern_idx(ino) else {
        return Err(FsError::NotFound);
    };
    if !vfs.kern.nodes[idx].used || vfs.kern.nodes[idx].sb != sb {
        return Err(FsError::NotFound);
    }
    let need = tmp_pages_for(new_size);
    let have = vfs.kern.nodes[idx].extent_pages as usize;
    if need <= have {
        return Ok(());
    }
    if have == 0 {
        let p = tmp_alloc_run(vfs, need)?;
        vfs.kern.nodes[idx].extent_page = p;
        vfs.kern.nodes[idx].extent_pages = need as u16;
        return Ok(());
    }
    let start = vfs.kern.nodes[idx].extent_page as usize;
    let extra = need - have;
    let mut can = true;
    let mut j = 0usize;
    while j < extra {
        if start + have + j >= TMPFS_BACK_PAGES || bit_get(vfs.kern.tmp_bits, start + have + j) {
            can = false;
            break;
        }
        j += 1;
    }
    if can {
        let mut b = vfs.kern.tmp_bits;
        let mut j = 0usize;
        while j < extra {
            b |= 1u64 << (start + have + j);
            j += 1;
        }
        vfs.kern.tmp_bits = b;
        let off = (start + have) * PAGE;
        vfs.kern.tmp_back[off..off + extra * PAGE].fill(0);
        tmp_invalidate_pages(vfs, (start + have) as u16, extra as u16);
        vfs.kern.nodes[idx].extent_pages = need as u16;
        return Ok(());
    }
    let newp = tmp_alloc_run(vfs, need)?;
    let old = vfs.kern.nodes[idx].extent_page as usize;
    let oldn = have;
    let dst = newp as usize * PAGE;
    let src = old * PAGE;
    if oldn > 0 {
        let nbytes = oldn * PAGE;
        vfs.kern.tmp_back.copy_within(src..src + nbytes, dst);
    }
    tmp_free_run(vfs, old as u16, oldn as u16);
    vfs.kern.nodes[idx].extent_page = newp;
    vfs.kern.nodes[idx].extent_pages = need as u16;
    Ok(())
}

fn tmp_byte_off(vfs: &Vfs, sb: u8, ino: u32, logical: u64) -> Result<u64, FsError> {
    let n = kern_get(vfs, sb, ino).ok_or(FsError::NotFound)?;
    if n.extent_pages == 0 {
        return Err(FsError::Io);
    }
    Ok((n.extent_page as u64) * PAGE as u64 + logical)
}

fn tmp_rw_cache(
    vfs: &mut Vfs,
    byte_off: u64,
    buf: &mut [u8],
    write: bool,
    src: &[u8],
) -> Result<(), FsError> {
    let cache = &mut vfs.kern.tmp_cache;
    let back = SliceBack {
        data: RefCell::new(&mut vfs.kern.tmp_back[..]),
    };
    let r = if write {
        cache::cached_write(cache, &back, TMPFS_DEV, byte_off, src)
    } else {
        cache::cached_read(cache, &back, TMPFS_DEV, byte_off, buf)
    };
    r.map_err(|_| FsError::Io)
}

fn tmp_read(vfs: &mut Vfs, sb: u8, ino: u32, off: u64, buf: &mut [u8]) -> Result<usize, FsError> {
    let size = kern_get(vfs, sb, ino).ok_or(FsError::NotFound)?.size;
    if off >= size {
        return Ok(0);
    }
    let avail = (size - off) as usize;
    let n = avail.min(buf.len());
    if n == 0 {
        return Ok(0);
    }
    let byte_off = tmp_byte_off(vfs, sb, ino, off)?;
    tmp_rw_cache(vfs, byte_off, &mut buf[..n], false, b"")?;
    Ok(n)
}

fn tmp_write(vfs: &mut Vfs, sb: u8, ino: u32, off: u64, buf: &[u8]) -> Result<usize, FsError> {
    if buf.is_empty() {
        return Ok(0);
    }
    let end = off.saturating_add(buf.len() as u64);
    tmp_ensure(vfs, sb, ino, end)?;
    let byte_off = tmp_byte_off(vfs, sb, ino, off)?;
    let mut dummy = [0u8; 1];
    tmp_rw_cache(vfs, byte_off, &mut dummy, true, buf)?;
    let t = vfs.now;
    if let Some(n) = kern_get_mut(vfs, sb, ino) {
        if end > n.size {
            n.size = end;
        }
        n.mtime = t;
        n.ctime = t;
    }
    Ok(buf.len())
}

fn tmp_truncate(vfs: &mut Vfs, sb: u8, ino: u32, size: u64) -> Result<(), FsError> {
    let Some(idx) = kern_idx(ino) else {
        return Err(FsError::NotFound);
    };
    if !vfs.kern.nodes[idx].used || vfs.kern.nodes[idx].sb != sb {
        return Err(FsError::NotFound);
    }
    let old = vfs.kern.nodes[idx].size;
    if size > old {
        tmp_ensure(vfs, sb, ino, size)?;
    } else {
        let need = tmp_pages_for(size) as u16;
        let have = vfs.kern.nodes[idx].extent_pages;
        if need < have {
            let start = vfs.kern.nodes[idx].extent_page;
            tmp_free_run(vfs, start + need, have - need);
            vfs.kern.nodes[idx].extent_pages = need;
            if need == 0 {
                vfs.kern.nodes[idx].extent_page = 0;
            }
        }
    }
    let t = vfs.now;
    vfs.kern.nodes[idx].size = size;
    vfs.kern.nodes[idx].mtime = t;
    vfs.kern.nodes[idx].ctime = t;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{O_CREAT, O_RDWR, SEEK_SET};

    fn boot() -> Vfs {
        let mut v = Vfs::new();
        v.mount_root().unwrap();
        v.mount_pseudo().unwrap();
        v
    }

    #[test]
    fn mount_pseudo_on_fat_root() {
        let mut v = Vfs::new();
        v.mount_root_fs(&crate::fs::FatFs {
            root_clu: 2,
            vol: 0,
        })
        .unwrap();
        v.mount_pseudo().unwrap();
        assert_eq!(v.stat(None, "/dev").unwrap().kind, InodeKind::Dir);
        assert_eq!(v.stat(None, "/proc").unwrap().kind, InodeKind::Dir);
        assert_eq!(v.stat(None, "/tmp").unwrap().kind, InodeKind::Dir);
        assert_eq!(v.stat(None, "/sys").unwrap().kind, InodeKind::Dir);
        let fid = v.open(None, "/dev/null", O_RDWR, 0).unwrap();
        assert_eq!(v.write(fid, b"x").unwrap(), 1);
        v.close(fid).unwrap();
    }

    fn readdir_names(v: &mut Vfs, path: &str, out: &mut [[u8; 16]; 32]) -> usize {
        let dir = v.resolve(None, path, true).unwrap();
        let mut d = Dirent {
            ino: 0,
            kind: InodeKind::Reg,
            name: Name::EMPTY,
        };
        let mut cookie = 0u64;
        let mut n = 0usize;
        loop {
            match v.readdir(dir, cookie, &mut d).unwrap() {
                None => break,
                Some(next) => {
                    if n < 32 {
                        let b = d.name.as_bytes();
                        let k = b.len().min(16);
                        out[n][..k].copy_from_slice(&b[..k]);
                        out[n][k..].fill(0);
                    }
                    n += 1;
                    cookie = next;
                }
            }
        }
        n
    }

    fn has_name(v: &mut Vfs, path: &str, want: &[u8]) -> bool {
        let mut names = [[0u8; 16]; 32];
        let n = readdir_names(v, path, &mut names);
        let mut i = 0usize;
        while i < n {
            let mut l = 0usize;
            while l < 16 && names[i][l] != 0 {
                l += 1;
            }
            if &names[i][..l] == want {
                return true;
            }
            i += 1;
        }
        false
    }

    #[test]
    fn mounts_exist() {
        let mut v = boot();
        assert_eq!(v.stat(None, "/dev").unwrap().kind, InodeKind::Dir);
        assert_eq!(v.stat(None, "/proc").unwrap().kind, InodeKind::Dir);
        assert_eq!(v.stat(None, "/tmp").unwrap().kind, InodeKind::Dir);
        assert_eq!(v.stat(None, "/sys").unwrap().kind, InodeKind::Dir);
    }

    #[test]
    fn devfs_char_nodes() {
        let mut v = boot();
        assert!(has_name(&mut v, "/dev", b"null"));
        assert!(has_name(&mut v, "/dev", b"zero"));
        assert!(has_name(&mut v, "/dev", b"random"));
        assert!(has_name(&mut v, "/dev", b"console"));
        assert!(has_name(&mut v, "/dev", b"tty"));
        assert_eq!(v.stat(None, "/dev/null").unwrap().kind, InodeKind::Chr);
        assert_eq!(v.stat(None, "/dev/zero").unwrap().kind, InodeKind::Chr);
        let fid = v.open(None, "/dev/null", O_RDWR, 0).unwrap();
        assert_eq!(v.write(fid, b"drop").unwrap(), 4);
        let mut buf = [0xFFu8; 8];
        assert_eq!(v.read(fid, &mut buf).unwrap(), 0);
        v.close(fid).unwrap();
        let z = v.open(None, "/dev/zero", O_RDWR, 0).unwrap();
        let mut buf = [0xFFu8; 8];
        assert_eq!(v.read(z, &mut buf).unwrap(), 8);
        assert_eq!(buf, [0u8; 8]);
        v.close(z).unwrap();
    }

    #[test]
    fn devfs_random_does_not_block() {
        let mut v = boot();
        v.now = 0x1234_5678;
        let fid = v.open(None, "/dev/random", O_RDWR, 0).unwrap();
        let mut a = [0u8; 16];
        let mut b = [0u8; 16];
        assert_eq!(v.read(fid, &mut a).unwrap(), 16);
        assert_eq!(v.read(fid, &mut b).unwrap(), 16);
        assert_ne!(a, b);
        v.close(fid).unwrap();
        let u = v.open(None, "/dev/urandom", O_RDWR, 0).unwrap();
        assert_eq!(v.read(u, &mut a).unwrap(), 16);
        v.close(u).unwrap();
    }

    #[test]
    fn devfs_block_names() {
        let mut v = boot();
        v.devfs_add_block(b"ram0", 256 * 512).unwrap();
        v.devfs_add_block(b"vda", 1024 * 512).unwrap();
        v.devfs_add_block(b"ram0p1", 32 * 512).unwrap();
        assert!(has_name(&mut v, "/dev", b"ram0"));
        assert!(has_name(&mut v, "/dev", b"vda"));
        assert!(has_name(&mut v, "/dev", b"ram0p1"));
        let s = v.stat(None, "/dev/ram0").unwrap();
        assert_eq!(s.kind, InodeKind::Blk);
        assert_eq!(s.size, 256 * 512);
        let fid = v.open(None, "/dev/ram0", O_RDWR, 0).unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(v.read(fid, &mut buf).unwrap_err(), FsError::NotSupp);
        v.close(fid).unwrap();
    }

    #[test]
    fn tmpfs_uses_cache_and_evicts() {
        let mut v = boot();
        let fid = v.open(None, "/tmp/big", O_RDWR | O_CREAT, 0o644).unwrap();
        let one = [0x5Au8; 1];
        let mut i = 0u64;
        while i < 6 {
            v.seek(fid, (i * PAGE as u64) as i64, SEEK_SET).unwrap();
            assert_eq!(v.write(fid, &one).unwrap(), 1);
            i += 1;
        }
        assert!(
            v.tmp_cache_stats().evicts >= 1,
            "tmpfs must evict through the Phase 7 cache, not pin a Vec"
        );
        v.seek(fid, 0, SEEK_SET).unwrap();
        let mut out = [0u8; 1];
        assert_eq!(v.read(fid, &mut out).unwrap(), 1);
        assert_eq!(out[0], 0x5A);
        v.close(fid).unwrap();
        assert_eq!(v.stat(None, "/tmp/big").unwrap().size, 5 * PAGE as u64 + 1);
    }

    #[test]
    fn tmpfs_mkdir_and_unlink() {
        let mut v = boot();
        v.mkdir(None, "/tmp/a", 0o755).unwrap();
        v.creat(None, "/tmp/a/f", 0o644).unwrap();
        let fid = v.open(None, "/tmp/a/f", O_RDWR, 0).unwrap();
        assert_eq!(v.write(fid, b"hi").unwrap(), 2);
        v.close(fid).unwrap();
        v.unlink(None, "/tmp/a/f").unwrap();
        assert_eq!(v.stat(None, "/tmp/a/f").unwrap_err(), FsError::NotFound);
    }

    #[test]
    fn procfs_stubs_with_only_kernel_thread() {
        let mut v = boot();
        assert!(has_name(&mut v, "/proc", b"1"));
        assert!(has_name(&mut v, "/proc", b"self"));
        let s = v.stat(None, "/proc/self").unwrap();
        assert_eq!(s.kind, InodeKind::Dir);
        assert!(has_name(&mut v, "/proc/1", b"cmdline"));
        assert!(has_name(&mut v, "/proc/1", b"status"));
        assert!(has_name(&mut v, "/proc/1", b"maps"));
        assert!(has_name(&mut v, "/proc/1", b"fd"));
        let fid = v.open(None, "/proc/1/cmdline", O_RDWR, 0).unwrap();
        let mut buf = [0u8; 16];
        let n = v.read(fid, &mut buf).unwrap();
        assert!(n > 0);
        assert_eq!(&buf[..6], b"vibeos");
        v.close(fid).unwrap();
        let st = v.open(None, "/proc/1/status", O_RDWR, 0).unwrap();
        let n = v.read(st, &mut buf).unwrap();
        assert!(n > 0);
        v.close(st).unwrap();
        assert_eq!(v.stat(None, "/proc/1/fd").unwrap().kind, InodeKind::Dir);
        assert_eq!(
            v.mkdir(None, "/proc/nope", 0o755).unwrap_err(),
            FsError::NotSupp
        );
    }

    #[test]
    fn sysfs_device_tree() {
        let mut v = boot();
        v.sysfs_add_device(b"00:01.0", 0x1af4, 0x1042, 0x01, Some(b"virtio-blk"))
            .unwrap();
        v.sysfs_add_device(b"00:02.0", 0x8086, 0x100e, 0x02, None)
            .unwrap();
        assert!(has_name(&mut v, "/sys/devices", b"00:01.0"));
        let fid = v
            .open(None, "/sys/devices/00:01.0/vendor", O_RDWR, 0)
            .unwrap();
        let mut buf = [0u8; 16];
        let n = v.read(fid, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"0x1af4\n");
        v.close(fid).unwrap();
        let d = v
            .open(None, "/sys/devices/00:01.0/driver", O_RDWR, 0)
            .unwrap();
        let n = v.read(d, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"virtio-blk\n");
        v.close(d).unwrap();
        let unbound = v
            .open(None, "/sys/devices/00:02.0/driver", O_RDWR, 0)
            .unwrap();
        let n = v.read(unbound, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"-\n");
        v.close(unbound).unwrap();
        assert!(has_name(&mut v, "/sys/bus/pci/drivers", b"virtio-blk"));
        let lnk = v.lstat(None, "/sys/bus/pci/devices/00:01.0").unwrap();
        assert_eq!(lnk.kind, InodeKind::Lnk);
    }

    #[test]
    fn console_write_captured() {
        let mut v = boot();
        let fid = v.open(None, "/dev/console", O_RDWR, 0).unwrap();
        assert_eq!(v.write(fid, b"hi").unwrap(), 2);
        assert_eq!(v.cons_captured(), b"hi");
        v.close(fid).unwrap();
    }
}
