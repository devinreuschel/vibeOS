//! Shared kernfs directory tree. ROADMAP §8.4.
//!
//! One node table and sibling-linked dirs, in one store ([`KernFs`])
//! outside `Vfs` behind a [`Guarded`] lock of its own. Four skins
//! ([`KernSkin`]: devfs, tmpfs, procfs, sysfs) fill different nodes; they
//! do not each keep a dentry tree. tmpfs file data goes through the
//! Phase 7 [`Cache`] plus a fixed ramdisk so eviction works — not a
//! grow-only Vec.

use core::cell::RefCell;

use crate::block::blockdev::BlockRef;
use crate::block::{BlockError, MAX_BLOCKDEVS};
use crate::cache::{self, Backend, Cache, CacheKey, CacheStats, PAGE};
use crate::limits::MAX_KERN_MOUNTS;

use super::{
    Dirent, FileSystem, FsError, FsType, Guarded, Inode, InodeInfo, InodeKind, InodeOps,
    MAX_KERN_NODES, MAX_NAME, Name, OpCx, S_IFBLK, S_IFCHR, S_IFDIR_MODE, S_IFLNK_MODE, S_IFMT,
    S_IFREG_MODE,
};

mod devfs;
mod node;
mod procfs;
mod sysfs;
mod tmpfs;

use devfs::{blk_read, blk_write};
use node::{
    kern_alloc, kern_create, kern_drop_sb, kern_find_child, kern_get, kern_get_mut, kern_idx,
    kern_info, kern_link, kern_lookup, kern_lookup_ino, kern_mk_dir, kern_mk_lnk, kern_mk_root,
    kern_mk_special, kern_read, kern_readdir, kern_readlink, kern_truncate, kern_try_free,
    kern_unlink, kern_write, set_target, target_bytes,
};
use procfs::{PROC_CMDLINE, PROC_MAPS, PROC_STATUS};
use sysfs::sys_attr_read;
use tmpfs::{
    TMPFS_BACK_BYTES, TMPFS_CACHE_PAGES, tmp_free_extent, tmp_read, tmp_truncate, tmp_write,
};

const TMPFS_DEV: u64 = 0;

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
    inst: u32,
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
        inst: 0,
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

/// A mounted skin of the store: its instance id, type and root node.
#[derive(Clone, Copy)]
struct Skin {
    used: bool,
    inst: u32,
    ty: FsType,
    root: u32,
}

impl Skin {
    const EMPTY: Self = Self {
        used: false,
        inst: 0,
        ty: FsType::Dev,
        root: 0,
    };
}

/// The one kernfs store: every node of every skin, tmpfs's page cache
/// and backing, and the instances mounted from it. Nodes carry the
/// instance id `fill_super` took from [`KernState::next_inst`], which
/// the superblock keeps in its private word 0.
pub struct KernState {
    nodes: [KernNode; MAX_KERN_NODES],
    tmp_cache: Cache<TMPFS_CACHE_PAGES>,
    tmp_back: [u8; TMPFS_BACK_BYTES],
    tmp_bits: u64,
    /// The clock the last op brought in.
    now: u64,
    next_inst: u32,
    skins: [Skin; MAX_KERN_MOUNTS],
    cons_out: [u8; 64],
    cons_len: u8,
    /// The devices devfs block nodes name; a node's `tag` is its slot.
    /// A slot is filled once and never overwritten, so no handle is
    /// dropped, perhaps for the last time, under the store's lock.
    blk: [Option<BlockRef>; MAX_BLOCKDEVS],
}

impl KernState {
    pub const fn new() -> Self {
        Self {
            nodes: [KernNode::EMPTY; MAX_KERN_NODES],
            tmp_cache: Cache::new(),
            tmp_back: [0u8; TMPFS_BACK_BYTES],
            tmp_bits: 0,
            now: 0,
            next_inst: 0,
            skins: [Skin::EMPTY; MAX_KERN_MOUNTS],
            cons_out: [0u8; 64],
            cons_len: 0,
            blk: [const { None }; MAX_BLOCKDEVS],
        }
    }

    /// The first mounted instance of skin `ty` and its root node.
    fn skin(&self, ty: FsType) -> Option<(u32, u32)> {
        self.skins
            .iter()
            .find(|s| s.used && s.ty == ty)
            .map(|s| (s.inst, s.root))
    }
}

impl Default for KernState {
    fn default() -> Self {
        Self::new()
    }
}

/// The kernfs store behind a [`Guarded`] lock of its own, which the four
/// skins share.
pub struct KernFs<S> {
    store: S,
}

impl<S: Guarded<KernState>> KernFs<S> {
    pub const fn new(store: S) -> Self {
        Self { store }
    }

    /// Run `f` on the store.
    pub fn with<R>(&self, f: impl FnOnce(&mut KernState) -> R) -> R {
        self.store.with(f)
    }
}

/// One skin of the store: devfs, tmpfs, procfs or sysfs, by `ty`.
pub struct KernSkin<S: 'static> {
    fs: &'static KernFs<S>,
    ty: FsType,
}

impl<S: 'static> KernSkin<S> {
    pub const fn new(fs: &'static KernFs<S>, ty: FsType) -> Self {
        Self { fs, ty }
    }
}

/// What a kernfs op knows besides the store: its instance, its skin and
/// the clock.
#[derive(Clone, Copy)]
struct Kx {
    inst: u32,
    ty: FsType,
    now: u64,
}

impl<S: Guarded<KernState> + Sync + 'static> KernSkin<S> {
    /// Run `f` on the store with the op's [`Kx`], and the clock brought in.
    fn op<R>(&self, cx: &OpCx<'_>, f: impl FnOnce(&mut KernState, Kx) -> R) -> R {
        let x = Kx {
            inst: cx.private[0] as u32,
            ty: self.ty,
            now: cx.now,
        };
        self.fs.with(|k| {
            k.now = x.now;
            f(k, x)
        })
    }
}

impl<S: Guarded<KernState> + Sync + 'static> FileSystem for KernSkin<S> {
    fn name(&self) -> &'static str {
        self.ty.as_str()
    }

    fn fstype(&self) -> FsType {
        self.ty
    }

    fn ops(&'static self) -> Option<&'static dyn InodeOps> {
        Some(self)
    }

    fn fill_super(&self, cx: &mut OpCx<'_>) -> Result<InodeInfo, FsError> {
        let (now, ty) = (cx.now, self.ty);
        let (inst, info) = self.fs.with(|k| {
            k.now = now;
            let slot = k
                .skins
                .iter()
                .position(|s| !s.used)
                .ok_or(FsError::NoSpace)?;
            let inst = k.next_inst.wrapping_add(1).max(1);
            let root = kern_mk_root(k, now, inst)?;
            let filled = fill_skin(k, now, inst, root, ty);
            if let Err(e) = filled {
                kern_drop_sb(k, inst, false);
                return Err(e);
            }
            k.next_inst = inst;
            k.skins[slot] = Skin {
                used: true,
                inst,
                ty,
                root,
            };
            Ok((inst, kern_info(k, inst, root)?))
        })?;
        *cx.private = [u64::from(inst), 0];
        Ok(info)
    }
}

/// Make skin `ty`'s fixed nodes under `root`.
fn fill_skin(k: &mut KernState, now: u64, inst: u32, root: u32, ty: FsType) -> Result<(), FsError> {
    match ty {
        FsType::Dev => {
            kern_mk_special(k, now, inst, root, b"null", KernKind::Null, 0)?;
            kern_mk_special(k, now, inst, root, b"zero", KernKind::Zero, 0)?;
            kern_mk_special(k, now, inst, root, b"random", KernKind::Random, 0)?;
            kern_mk_special(k, now, inst, root, b"urandom", KernKind::Urandom, 0)?;
            kern_mk_special(k, now, inst, root, b"console", KernKind::Console, 0)?;
            kern_mk_special(k, now, inst, root, b"tty", KernKind::Tty, 0)?;
        }
        FsType::Proc => {
            // Phase 9 owns Process. One kernel-thread stub so a shell-only
            // boot does not panic looking up cmdline/status/maps/fd.
            let p1 = kern_mk_dir(k, now, inst, root, b"1")?;
            kern_mk_special(k, now, inst, p1, b"cmdline", KernKind::ProcCmdline, 1)?;
            kern_mk_special(k, now, inst, p1, b"status", KernKind::ProcStatus, 1)?;
            kern_mk_special(k, now, inst, p1, b"maps", KernKind::ProcMaps, 1)?;
            kern_mk_special(k, now, inst, p1, b"fd", KernKind::ProcFdDir, 1)?;
            kern_mk_lnk(k, now, inst, root, b"self", b"1")?;
        }
        FsType::Sys => {
            kern_mk_dir(k, now, inst, root, b"devices")?;
            let bus = kern_mk_dir(k, now, inst, root, b"bus")?;
            let pci = kern_mk_dir(k, now, inst, bus, b"pci")?;
            kern_mk_dir(k, now, inst, pci, b"devices")?;
            kern_mk_dir(k, now, inst, pci, b"drivers")?;
        }
        FsType::Tmp => {}
        FsType::Ram | FsType::Fat | FsType::Vibe => return Err(FsError::Inval),
    }
    Ok(())
}

impl<S: Guarded<KernState> + Sync + 'static> InodeOps for KernSkin<S> {
    fn lookup(&self, cx: &mut OpCx<'_>, dir: &Inode, name: &[u8]) -> Result<InodeInfo, FsError> {
        self.op(cx, |k, x| kern_lookup(k, x, dir, name))
    }

    fn create(
        &self,
        cx: &mut OpCx<'_>,
        dir: &mut Inode,
        name: &[u8],
        kind: InodeKind,
        mode: u16,
        target: Option<&[u8]>,
    ) -> Result<InodeInfo, FsError> {
        self.op(cx, |k, x| kern_create(k, x, dir, name, kind, mode, target))
    }

    fn unlink(&self, cx: &mut OpCx<'_>, dir: &mut Inode, name: &[u8]) -> Result<(), FsError> {
        self.op(cx, |k, x| kern_unlink(k, x, dir, name))
    }

    fn rmdir(&self, cx: &mut OpCx<'_>, dir: &mut Inode, name: &[u8]) -> Result<(), FsError> {
        self.op(cx, |k, x| kern_unlink(k, x, dir, name))
    }

    fn read(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        // A block node's I/O runs with the store unlocked, through a clone
        // of its handle taken under the lock.
        if let Some(dev) = self.op(cx, |k, x| kern_block_of(k, x.inst, ino.key[0])) {
            return blk_read(&dev, off, buf);
        }
        self.op(cx, |k, x| kern_read(k, x, ino, off, buf))
    }

    fn write(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &[u8],
    ) -> Result<usize, FsError> {
        if let Some(dev) = self.op(cx, |k, x| kern_block_of(k, x.inst, ino.key[0])) {
            return blk_write(&dev, off, buf);
        }
        self.op(cx, |k, x| kern_write(k, x, ino, off, buf))
    }

    fn truncate(&self, cx: &mut OpCx<'_>, ino: &mut Inode, size: u64) -> Result<(), FsError> {
        self.op(cx, |k, x| kern_truncate(k, x, ino, size))
    }

    fn readdir(
        &self,
        cx: &mut OpCx<'_>,
        dir: &Inode,
        cookie: u64,
        out: &mut Dirent,
    ) -> Result<Option<u64>, FsError> {
        self.op(cx, |k, x| kern_readdir(k, x, dir, cookie, out))
    }

    fn getattr(&self, cx: &mut OpCx<'_>, ino: &mut Inode) -> Result<(), FsError> {
        self.op(cx, |k, x| {
            if let Some(n) = kern_get(k, x.inst, ino.key[0]) {
                n.meta_into(ino);
            }
        });
        Ok(())
    }

    fn readlink(&self, cx: &mut OpCx<'_>, ino: &Inode, buf: &mut [u8]) -> Result<usize, FsError> {
        self.op(cx, |k, x| kern_readlink(k, x, ino, buf))
    }

    /// The console and the tty cannot seek (`ESPIPE`), as Linux's.
    fn check_seek(&self, cx: &mut OpCx<'_>, ino: &Inode) -> Result<(), FsError> {
        self.op(cx, |k, x| {
            match kern_get(k, x.inst, ino.key[0]).map(|n| n.kind) {
                Some(KernKind::Console | KernKind::Tty) => Err(FsError::SPipe),
                _ => Ok(()),
            }
        })
    }

    fn evict(&self, cx: &mut OpCx<'_>, ino: &Inode) -> Result<(), FsError> {
        self.op(cx, |k, x| kern_try_free(k, x.inst, ino.key[0]));
        Ok(())
    }

    fn kill_sb(&self, cx: &mut OpCx<'_>) {
        self.op(cx, |k, x| {
            kern_drop_sb(k, x.inst, x.ty == FsType::Tmp);
            for s in k.skins.iter_mut() {
                if s.used && s.inst == x.inst {
                    *s = Skin::EMPTY;
                }
            }
        });
    }
}

/// A clone of the handle block node `ino` names, or `None` for any other
/// node.
fn kern_block_of(k: &KernState, inst: u32, ino: u32) -> Option<BlockRef> {
    let n = kern_get(k, inst, ino)?;
    if n.kind != KernKind::Block {
        return None;
    }
    let slot = usize::try_from(n.tag).ok()?;
    k.blk.get(slot)?.clone()
}

fn touch_dir(k: &mut KernState, inst: u32, ino: u32, t: u64) {
    if let Some(p) = kern_get_mut(k, inst, ino) {
        p.mtime = t;
        p.ctime = t;
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

#[cfg(test)]
pub(crate) mod tests;
