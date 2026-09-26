//! FAT volumes: initrd + block-backed mounts. ROADMAP §8.2–8.3 / §8.6.
//!
//! The volume lives in BSS. A busy flag (not the IRQ-off mutex) is held
//! across I/O so RANK_DEVICE is not nested with the cache (DESIGN §2.1 /
//! #62 ACK). `sync` uses cache/device Flush (DESIGN §10.2).
//!
//! Two paths reach a volume. The File API takes the busy flag, waiting
//! for it, and then the VFS lock, briefly, to copy an inode's words in
//! and out; never the reverse. [`FatOps`] runs under the VFS lock, which
//! until ROADMAP §10.4's VFS-lock box is an IRQ-off spinlock, so it never
//! waits: it takes the flag with one compare-and-swap and returns `Busy`
//! when the volume is busy or sits on a block device, whose cache reads
//! may yield.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use vibeos::fat::{self, Disk, FatError, FatInode, FatVol, INITRD_BYTES, Node, SEC};
use vibeos::fs::{
    Dirent, FatFs, FsError, FsType, Inode, InodeHandle, InodeInfo, InodeKind, InodeOps, InodeRef,
    MAX_PATH, Name, OpCx, S_IFDIR_MODE, S_IFREG_MODE,
};
use vibeos::lock::RANK_DEVICE;

use crate::block_init;
use crate::cache_init;
use crate::cell::IrqCell;
use crate::fs_init;
use crate::sync_init::SpinMutex;
use crate::thread_init;
use crate::virtio_blk_init;

use vibeos::limits::MAX_FAT_VOLS as MAX_VOLS;
pub const VOL_INITRD: u8 = 0;
const MNT_MAX: usize = 2;
const MNT_PATH: usize = 64;

#[derive(Clone, Copy)]
enum Back {
    Initrd,
    Dev(u32),
}

struct Slot {
    vol: UnsafeCell<Option<FatVol>>,
    back: UnsafeCell<Back>,
    used: AtomicBool,
    busy: AtomicBool,
    /// The `Vfs` superblock the volume is mounted as.
    sb: AtomicU8,
}

unsafe impl Sync for Slot {}

impl Slot {
    const fn empty() -> Self {
        Self {
            vol: UnsafeCell::new(None),
            back: UnsafeCell::new(Back::Initrd),
            used: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            sb: AtomicU8::new(0),
        }
    }
}

#[derive(Clone, Copy)]
struct Mnt {
    used: bool,
    vol: u8,
    len: u8,
    path: [u8; MNT_PATH],
}

impl Mnt {
    const EMPTY: Self = Self {
        used: false,
        vol: 0,
        len: 0,
        path: [0; MNT_PATH],
    };
}

static SLOTS: [Slot; MAX_VOLS] = [Slot::empty(), Slot::empty()];
static ALLOC: SpinMutex<()> = SpinMutex::with_rank((), RANK_DEVICE);
static MNTS: SpinMutex<[Mnt; MNT_MAX]> =
    SpinMutex::with_rank([Mnt::EMPTY, Mnt::EMPTY], RANK_DEVICE);
static INITRD: IrqCell<[u8; INITRD_BYTES]> = IrqCell::new([0; INITRD_BYTES]);
static LIVE: AtomicBool = AtomicBool::new(false);
static NVOL: AtomicU8 = AtomicU8::new(0);

const INITRD_RO: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/initrd.fat"));

struct Io {
    back: Back,
}

impl Disk for Io {
    fn sector_size(&self) -> u32 {
        SEC as u32
    }

    fn nsectors(&self) -> u32 {
        match self.back {
            Back::Initrd => (INITRD_BYTES / SEC) as u32,
            Back::Dev(cache_init::DEV_RAM0) => block_init::capacity_sectors() as u32,
            Back::Dev(cache_init::DEV_VDA) => virtio_blk_init::capacity_sectors() as u32,
            Back::Dev(_) => 0,
        }
    }

    fn read(&mut self, lba: u32, buf: &mut [u8]) -> Result<(), FatError> {
        match self.back {
            Back::Initrd => {
                let ss = SEC;
                let off = (lba as usize).checked_mul(ss).ok_or(FatError::Inval)?;
                let end = off.checked_add(ss).ok_or(FatError::Inval)?;
                INITRD.with(|data| {
                    if end > data.len() || buf.len() != ss {
                        return Err(FatError::Io);
                    }
                    buf.copy_from_slice(&data[off..end]);
                    Ok(())
                })
            }
            Back::Dev(dev) => cache_init::read(dev, lba as u64, buf).map_err(|_| FatError::Io),
        }
    }

    fn write(&mut self, lba: u32, buf: &[u8]) -> Result<(), FatError> {
        match self.back {
            Back::Initrd => {
                let ss = SEC;
                let off = (lba as usize).checked_mul(ss).ok_or(FatError::Inval)?;
                let end = off.checked_add(ss).ok_or(FatError::Inval)?;
                INITRD.with(|data| {
                    if end > data.len() || buf.len() != ss {
                        return Err(FatError::Io);
                    }
                    data[off..end].copy_from_slice(buf);
                    Ok(())
                })
            }
            Back::Dev(dev) => cache_init::write(dev, lba as u64, buf).map_err(|_| FatError::Io),
        }
    }

    fn flush(&mut self) -> Result<(), FatError> {
        match self.back {
            Back::Initrd => Ok(()),
            Back::Dev(dev) => cache_init::flush(dev).map_err(|_| FatError::Io),
        }
    }
}

fn grab(id: u8) -> Result<(), FatError> {
    let i = id as usize;
    if i >= MAX_VOLS || !SLOTS[i].used.load(Ordering::Acquire) {
        return Err(FatError::Io);
    }
    let mut n = 0u32;
    while SLOTS[i]
        .busy
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        n = n.saturating_add(1);
        if n > 1_000_000 {
            return Err(FatError::Io);
        }
        thread_init::yield_now();
    }
    if !SLOTS[i].used.load(Ordering::Acquire) {
        SLOTS[i].busy.store(false, Ordering::Release);
        return Err(FatError::Io);
    }
    Ok(())
}

fn drop_busy(id: u8) {
    let i = id as usize;
    if i < MAX_VOLS {
        SLOTS[i].busy.store(false, Ordering::Release);
    }
}

/// Take volume `id`'s busy flag with one compare-and-swap: `Busy` when
/// another holder has it.
fn grab_now(id: u8) -> Result<(), FsError> {
    let i = id as usize;
    if i >= MAX_VOLS || !SLOTS[i].used.load(Ordering::Acquire) {
        return Err(FsError::Io);
    }
    if SLOTS[i]
        .busy
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(FsError::Busy);
    }
    if !SLOTS[i].used.load(Ordering::Acquire) {
        SLOTS[i].busy.store(false, Ordering::Release);
        return Err(FsError::Io);
    }
    Ok(())
}

/// Run `f` on volume `id`, whose busy flag the caller took, and drop the
/// flag.
fn with_grabbed<R>(
    id: u8,
    f: impl FnOnce(&mut FatVol, &mut Io) -> Result<R, FsError>,
) -> Result<R, FsError> {
    let i = id as usize;
    // SAFETY: the busy flag of `SLOTS[i]`, which the caller took through
    // `fat_init::grab` or `fat_init::grab_now` and which is dropped only
    // below, makes this thread the one accessor of the slot's `vol` and
    // `back` until `drop_busy`; `i < MAX_VOLS` was checked there.
    let r = unsafe {
        match (*SLOTS[i].vol.get()).as_mut() {
            None => Err(FsError::Io),
            Some(v) => {
                let mut io = Io {
                    back: *SLOTS[i].back.get(),
                };
                f(v, &mut io)
            }
        }
    };
    drop_busy(id);
    r
}

/// Run `f` on volume `id`, waiting for its busy flag. Never under the VFS
/// lock.
fn with_slot<R>(
    id: u8,
    f: impl FnOnce(&mut FatVol, &mut Io) -> Result<R, FsError>,
) -> Result<R, FsError> {
    grab(id)?;
    with_grabbed(id, f)
}

/// Run `f` on volume `id` without waiting, as [`FatOps`] must under the
/// VFS lock: `Busy` when the volume is busy or on a block device.
fn with_slot_now<R>(
    id: u8,
    f: impl FnOnce(&mut FatVol, &mut Io) -> Result<R, FsError>,
) -> Result<R, FsError> {
    grab_now(id)?;
    with_grabbed(id, |v, io| match io.back {
        Back::Initrd => f(v, io),
        Back::Dev(_) => Err(FsError::Busy),
    })
}

/// The superblock volume `id` is mounted as.
fn sb_of(id: u8) -> u8 {
    SLOTS
        .get(id as usize)
        .map_or(0, |s| s.sb.load(Ordering::Acquire))
}

/// The one conversion from a FAT file's dirent fields to its `Vfs`
/// inode: key `[dir_clu, dir_off, 0]` (the root is `[0, 0, 0]`), private
/// words `[first_clu, (dir_clu << 32) | dir_off]`, `st_ino` from
/// [`fat::stat_ino`].
pub fn inode_info(
    dir_clu: u32,
    dir_off: u32,
    first_clu: u32,
    size: u64,
    kind: InodeKind,
    mtime: u64,
) -> InodeInfo {
    let dir = kind == InodeKind::Dir;
    InodeInfo {
        key: [dir_clu, dir_off, 0],
        ino: fat::stat_ino(dir_clu, dir_off),
        kind,
        mode: if dir { S_IFDIR_MODE } else { S_IFREG_MODE },
        nlink: if dir { 2 } else { 1 },
        size,
        atime: mtime,
        mtime,
        ctime: mtime,
        private: [u64::from(first_clu), dirent_word(dir_clu, dir_off)],
    }
}

/// FAT's [`InodeOps`], behind a FAT superblock's `ops` pointer. The
/// superblock's private words are `[vol, 0]`; an inode's are
/// `[first_clu, (dir_clu << 32) | dir_off]`.
pub struct FatOps;

fn vol_of(cx: &OpCx<'_>) -> u8 {
    cx.private[0] as u8
}

impl InodeOps for FatOps {
    fn lookup(&self, cx: &mut OpCx<'_>, dir: &Inode, name: &[u8]) -> Result<InodeInfo, FsError> {
        let clu = dir.private[0] as u32;
        with_slot_now(vol_of(cx), |v, d| Ok(node_info(&v.lookup(d, clu, name)?)))
    }

    fn create(
        &self,
        cx: &mut OpCx<'_>,
        dir: &mut Inode,
        name: &[u8],
        kind: InodeKind,
        _mode: u16,
        _target: Option<&[u8]>,
    ) -> Result<InodeInfo, FsError> {
        let is_dir = match kind {
            InodeKind::Reg => false,
            InodeKind::Dir => true,
            InodeKind::Lnk | InodeKind::Chr | InodeKind::Blk => return Err(FsError::NotSupp),
        };
        let clu = dir.private[0] as u32;
        with_slot_now(vol_of(cx), |v, d| {
            Ok(node_info(&v.create(d, clu, name, is_dir)?))
        })
    }

    /// Remove the dirent only: the victim's chain is freed by `evict` at
    /// its last put.
    fn unlink(&self, cx: &mut OpCx<'_>, dir: &mut Inode, name: &[u8]) -> Result<(), FsError> {
        let clu = dir.private[0] as u32;
        with_slot_now(vol_of(cx), |v, d| {
            v.unlink(d, clu, name, false)?;
            Ok(())
        })
    }

    fn read(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        let w = words(ino);
        with_slot_now(vol_of(cx), |v, d| Ok(v.read_ino(d, &w, off, buf)?))
    }

    fn write(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &[u8],
    ) -> Result<usize, FsError> {
        let mut w = words(ino);
        let linked = ino.nlink != 0;
        let r = with_slot_now(vol_of(cx), |v, d| {
            Ok(v.write_ino(d, &mut w, linked, off, false, buf))
        });
        store(ino, &w);
        Ok(r??.0)
    }

    fn truncate(&self, cx: &mut OpCx<'_>, ino: &mut Inode, size: u64) -> Result<(), FsError> {
        let mut w = words(ino);
        let linked = ino.nlink != 0;
        let r = with_slot_now(vol_of(cx), |v, d| {
            Ok(v.truncate_ino(d, &mut w, linked, size))
        });
        store(ino, &w);
        Ok(r??)
    }

    /// The on-disk `.` and `..` are skipped: `Vfs::readdir` makes both.
    fn readdir(
        &self,
        cx: &mut OpCx<'_>,
        dir: &Inode,
        cookie: u64,
        out: &mut Dirent,
    ) -> Result<Option<u64>, FsError> {
        let clu = dir.private[0] as u32;
        with_slot_now(vol_of(cx), |v, d| {
            let mut c = cookie;
            let mut n = Node::EMPTY;
            loop {
                let Some(next) = v.readdir(d, clu, c, &mut n)? else {
                    return Ok(None);
                };
                c = next;
                if n.name() != b"." && n.name() != b".." {
                    break;
                }
            }
            out.ino = n.ino;
            out.kind = n.kind;
            out.name = Name::from_bytes(n.name())?;
            Ok(Some(c))
        })
    }

    fn evict(&self, cx: &mut OpCx<'_>, ino: &Inode) -> Result<(), FsError> {
        if ino.nlink != 0 {
            return Ok(());
        }
        let first = ino.private[0] as u32;
        with_slot_now(vol_of(cx), |v, d| Ok(v.free_chain(d, first)?))
    }
}

fn node_info(n: &Node) -> InodeInfo {
    inode_info(
        n.dir_clu,
        n.dir_off,
        n.clu,
        u64::from(n.size),
        n.kind,
        n.mtime,
    )
}

fn dirent_word(dir_clu: u32, dir_off: u32) -> u64 {
    (u64::from(dir_clu) << 32) | u64::from(dir_off)
}

/// A FAT inode's words, copied out of its `Vfs` inode.
fn words(n: &Inode) -> FatInode {
    FatInode {
        dir_clu: n.key[0],
        dir_off: n.key[1],
        first_clu: n.private[0] as u32,
        size: n.size,
        kind: n.kind,
    }
}

/// Write a FAT inode's words back into its `Vfs` inode.
fn store(n: &mut Inode, w: &FatInode) {
    n.private = [u64::from(w.first_clu), dirent_word(w.dir_clu, w.dir_off)];
    n.size = w.size;
}

pub fn live() -> bool {
    LIVE.load(Ordering::Acquire)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn nvol() -> u8 {
    NVOL.load(Ordering::Acquire)
}

pub fn init() {
    let ok_image = INITRD.with(|buf| {
        if INITRD_RO.len() == INITRD_BYTES {
            buf.copy_from_slice(INITRD_RO);
            true
        } else {
            buf.fill(0);
            fat::mkinitrd(buf).is_ok()
        }
    });
    if !ok_image {
        LIVE.store(false, Ordering::Release);
        return;
    }
    let mut io = Io { back: Back::Initrd };
    let vol = match FatVol::mount(&mut io) {
        Ok(v) => v,
        Err(_) => {
            LIVE.store(false, Ordering::Release);
            return;
        }
    };
    let root_clu = vol.info.root_clus;
    unsafe {
        *SLOTS[0].vol.get() = Some(vol);
        *SLOTS[0].back.get() = Back::Initrd;
    }
    SLOTS[0].used.store(true, Ordering::Release);
    SLOTS[0].busy.store(false, Ordering::Release);
    NVOL.store(1, Ordering::Release);
    let sb = fs_init::with(|v| {
        let root = v.mount_root_fs(&FatFs {
            root_clu,
            vol: VOL_INITRD,
            ops: Some(&FatOps),
        })?;
        v.sb_of_path(root)
    });
    if let Ok(sb) = sb {
        SLOTS[0].sb.store(sb, Ordering::Release);
    }
    LIVE.store(sb.is_ok(), Ordering::Release);
}

#[allow(dead_code)]
pub fn lookup(id: u8, dir_clu: u32, name: &[u8]) -> Result<Node, FsError> {
    with_slot(id, |v, d| Ok(v.lookup(d, dir_clu, name)?))
}

pub fn walk(id: u8, path: &[u8]) -> Result<Node, FsError> {
    with_slot(id, |v, d| Ok(v.walk(d, path)?))
}

pub fn readdir(id: u8, dir_clu: u32, cookie: u64, out: &mut Node) -> Result<Option<u64>, FsError> {
    with_slot(id, |v, d| Ok(v.readdir(d, dir_clu, cookie, out)?))
}

/// Walk `path` and count a reference to the `Vfs` inode it names, with
/// the volume held: busy flag first, then the VFS lock, never the
/// reverse.
pub fn walk_iget(id: u8, path: &[u8]) -> Result<(Node, InodeRef), FsError> {
    let sb = sb_of(id);
    with_slot(id, |v, d| {
        let n = v.walk(d, path)?;
        let r = fs_init::with(|vfs| vfs.iget_key(sb, &node_info(&n)))?;
        Ok((n, r))
    })
}

/// Drop a reference; the last one on an unlinked file frees its clusters.
pub fn iput(id: u8, r: InodeRef) -> Result<(), FsError> {
    match fs_init::with(|v| v.put_ref(r)) {
        None => Ok(()),
        Some(gone) => with_slot(id, |v, d| Ok(v.free_chain(d, gone.private[0] as u32)?)),
    }
}

/// The size of the referenced inode `h`.
pub fn size(_id: u8, h: InodeHandle) -> Result<u64, FsError> {
    fs_init::with(|v| Ok(v.inode(h)?.size))
}

pub fn read(id: u8, h: InodeHandle, off: u64, buf: &mut [u8]) -> Result<usize, FsError> {
    with_slot(id, |v, d| {
        let w = fs_init::with(|vfs| vfs.inode(h).map(words))?;
        Ok(v.read_ino(d, &w, off, buf)?)
    })
}

/// Run `f` on inode `h`'s words with the volume held, and write them
/// back, even when `f` fails: the chain may have grown. The VFS lock is
/// taken twice, briefly, and never across the volume I/O.
fn with_words<R>(
    id: u8,
    h: InodeHandle,
    f: impl FnOnce(&mut FatVol, &mut Io, &mut FatInode, bool) -> Result<R, FatError>,
) -> Result<R, FsError> {
    with_slot(id, |v, d| {
        let (mut w, linked) = fs_init::with(|vfs| vfs.inode(h).map(|n| (words(n), n.nlink != 0)))?;
        let r = f(v, d, &mut w, linked);
        fs_init::with(|vfs| vfs.inode_mut(h).map(|n| store(n, &w)))?;
        Ok(r?)
    })
}

/// Write `buf` at `off`, or at the inode's size when `append` is set;
/// the count written and the position written at.
pub fn write(
    id: u8,
    h: InodeHandle,
    off: u64,
    append: bool,
    buf: &[u8],
) -> Result<(usize, u64), FsError> {
    with_words(id, h, |v, d, w, linked| {
        v.write_ino(d, w, linked, off, append, buf)
    })
}

pub fn create(id: u8, dir_clu: u32, name: &[u8], dir: bool) -> Result<Node, FsError> {
    let sb = sb_of(id);
    with_slot(id, |v, d| {
        let n = v.create(d, dir_clu, name, dir)?;
        fs_init::with(|vfs| vfs.drop_negatives(sb));
        Ok(n)
    })
}

/// Free a removed entry's clusters now unless a `Vfs` inode still holds
/// it; its last put frees them then.
fn release_removed(vol: &mut FatVol, d: &mut Io, sb: u8, gone: &FatInode) -> Result<(), FsError> {
    let held = fs_init::with(|vfs| vfs.forget(sb, [gone.dir_clu, gone.dir_off, 0]));
    if !held {
        vol.free_chain(d, gone.first_clu)?;
    }
    Ok(())
}

pub fn unlink(id: u8, dir_clu: u32, name: &[u8], rmdir: bool) -> Result<(), FsError> {
    let sb = sb_of(id);
    with_slot(id, |v, d| {
        let gone = v.unlink(d, dir_clu, name, rmdir)?;
        release_removed(v, d, sb, &gone)
    })
}

pub fn truncate(id: u8, h: InodeHandle, new: u64) -> Result<(), FsError> {
    with_words(id, h, |v, d, w, linked| v.truncate_ino(d, w, linked, new))
}

pub fn rename(
    id: u8,
    src_dir: u32,
    src_name: &[u8],
    dst_dir: u32,
    dst_name: &[u8],
) -> Result<(), FsError> {
    let sb = sb_of(id);
    with_slot(id, |v, d| {
        let m = v.rename(d, src_dir, src_name, dst_dir, dst_name)?;
        if let Some(gone) = m.replaced {
            release_removed(v, d, sb, &gone)?;
        }
        fs_init::with(|vfs| vfs.rekey(sb, [m.from.0, m.from.1, 0], [m.to.0, m.to.1, 0]))
    })
}

pub fn sync(id: u8) -> Result<(), FsError> {
    with_slot(id, |v, d| Ok(v.sync(d)?))
}

pub fn sync_all() -> Result<(), FsError> {
    let mut i = 0u8;
    while i < MAX_VOLS as u8 {
        if SLOTS[i as usize].used.load(Ordering::Acquire) {
            sync(i)?;
        }
        i += 1;
    }
    Ok(())
}

pub fn df(id: u8) -> Result<(FsType, u64, u64, u32), FsError> {
    with_slot(id, |v, _| {
        Ok((
            FsType::Fat,
            v.info.data_bytes(),
            v.free_bytes(),
            v.info.nclus,
        ))
    })
}

/// `(vol, strip)`: skip `strip` bytes of `path`; if nothing remains, walk `"/"`.
pub fn route(path: &[u8]) -> (u8, usize) {
    let mnts = {
        let g = MNTS.lock();
        *g
    };
    let mut best = 0usize;
    let mut vol = VOL_INITRD;
    let mut i = 0usize;
    while i < MNT_MAX {
        if mnts[i].used {
            let n = mnts[i].len as usize;
            let p = &mnts[i].path[..n];
            if (path == p || (path.len() > n && path[..n] == p[..] && path[n] == b'/')) && n >= best
            {
                best = n;
                vol = mnts[i].vol;
            }
        }
        i += 1;
    }
    (vol, best)
}

pub fn routed_rest(path: &[u8], strip: usize) -> &[u8] {
    if strip == 0 {
        if path.is_empty() { b"/" } else { path }
    } else if strip >= path.len() {
        b"/"
    } else {
        &path[strip..]
    }
}

fn register_mnt(vol: u8, at: &str) -> Result<(), FsError> {
    let p = at.as_bytes();
    if p.is_empty() || p.len() > MNT_PATH {
        return Err(FsError::NameTooLong);
    }
    let mut g = MNTS.lock();
    let mut i = 0usize;
    while i < MNT_MAX {
        if !g[i].used {
            g[i].used = true;
            g[i].vol = vol;
            g[i].len = p.len() as u8;
            g[i].path[..p.len()].copy_from_slice(p);
            return Ok(());
        }
        i += 1;
    }
    Err(FsError::NoSpace)
}

fn unregister_mnt(at: &str) -> Option<u8> {
    let p = at.as_bytes();
    let mut g = MNTS.lock();
    let mut i = 0usize;
    while i < MNT_MAX {
        if g[i].used {
            let n = g[i].len as usize;
            if n == p.len() && g[i].path[..n] == p[..] {
                let vol = g[i].vol;
                g[i] = Mnt::EMPTY;
                return Some(vol);
            }
        }
        i += 1;
    }
    None
}

fn drop_slot(id: u8) {
    if id == VOL_INITRD {
        return;
    }
    let i = id as usize;
    if i >= MAX_VOLS {
        return;
    }
    let _ = grab(id);
    unsafe {
        *SLOTS[i].vol.get() = None;
    }
    SLOTS[i].used.store(false, Ordering::Release);
    drop_busy(id);
    let mut n = 0u8;
    let mut k = 0usize;
    while k < MAX_VOLS {
        if SLOTS[k].used.load(Ordering::Acquire) {
            n += 1;
        }
        k += 1;
    }
    NVOL.store(n, Ordering::Release);
}

pub fn mount_dev(name: &str, at: &str) -> Result<u8, FsError> {
    let back = match name {
        "ram0" => {
            if !block_init::live() {
                return Err(FsError::Io);
            }
            Back::Dev(cache_init::DEV_RAM0)
        }
        "vda" => {
            if !virtio_blk_init::live() {
                return Err(FsError::Io);
            }
            Back::Dev(cache_init::DEV_VDA)
        }
        _ => return Err(FsError::Inval),
    };
    let mut io = Io { back };
    let vol = FatVol::mount(&mut io)?;
    let root_clu = vol.info.root_clus;
    let id = {
        let _g = ALLOC.lock();
        let mut i = 1usize;
        while i < MAX_VOLS {
            if !SLOTS[i].used.load(Ordering::Acquire) {
                SLOTS[i].busy.store(true, Ordering::Release);
                SLOTS[i].used.store(true, Ordering::Release);
                break;
            }
            i += 1;
        }
        if i >= MAX_VOLS {
            return Err(FsError::NoSpace);
        }
        i as u8
    };
    unsafe {
        *SLOTS[id as usize].vol.get() = Some(vol);
        *SLOTS[id as usize].back.get() = back;
    }
    SLOTS[id as usize].busy.store(false, Ordering::Release);
    let mut n = 0u8;
    let mut k = 0usize;
    while k < MAX_VOLS {
        if SLOTS[k].used.load(Ordering::Acquire) {
            n += 1;
        }
        k += 1;
    }
    NVOL.store(n, Ordering::Release);
    if let Err(e) = register_mnt(id, at) {
        drop_slot(id);
        return Err(e);
    }
    match fs_init::with(|v| {
        let m = v.mount(
            None,
            at,
            &FatFs {
                root_clu,
                vol: id,
                ops: Some(&FatOps),
            },
        )?;
        v.sb_of_mount(m)
    }) {
        Ok(sb) => {
            SLOTS[id as usize].sb.store(sb, Ordering::Release);
            Ok(id)
        }
        Err(e) => {
            let _ = unregister_mnt(at);
            drop_slot(id);
            Err(e)
        }
    }
}

pub fn umount(at: &str) -> Result<(), FsError> {
    let mut vol = unregister_mnt(at);
    if vol.is_none() {
        vol = fs_init::with(|v| {
            let p = v.resolve(None, at, true).ok()?;
            if v.fstype_at(p).ok() == Some(FsType::Fat) {
                v.sb_private(p).ok().map(|w| w[0] as u8)
            } else {
                None
            }
        });
    }
    if let Some(id) = vol {
        let _ = sync(id);
    }
    let r = fs_init::with(|v| v.umount(None, at));
    if let Some(id) = vol {
        drop_slot(id);
    }
    r
}

const _: () = {
    assert!(INITRD_BYTES.is_multiple_of(SEC));
    assert!(MAX_PATH >= MNT_PATH);
};
