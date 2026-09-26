//! vibefs volumes. ROADMAP §8.5 / docs/VIBEFS.md.
//!
//! BSS image by default. Busy flag (not IRQ-off mutex) across I/O
//! (DESIGN §2.1). `sync` uses disk `Flush`.
//!
//! [`VibeOps`] is the one way to a volume's files: `Vfs`'s File API calls
//! it with the VFS lock dropped, so it waits for the busy flag. The size
//! vibefs keeps is stored in the `Vfs` inode inside the busy section:
//! the busy flag first, the VFS lock second, never the reverse
//! (C-FILEAPI). The routing table (`route`) serves path syscalls until
//! ROADMAP §10.4 routes them through `Vfs`.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use vibeos::fs::{
    Dirent, FileSystem, FsError, FsType, Inode, InodeHandle, InodeInfo, InodeKind, InodeOps,
    InodeRef, Key, MAX_PATH, Name, OpCx, S_IFDIR_MODE, S_IFMT,
};
use vibeos::lock::RANK_DEVICE;
use vibeos::vibefs::{self, BLOCK, Disk, Error, Node, ROOT_INO, Vol};

use crate::block_init;
use crate::cache_init;
use crate::cell::IrqCell;
use crate::fs_init;
use crate::sync_init::SpinMutex;
use crate::thread_init;
use crate::virtio_blk_init;

pub const VOL_MEM: u8 = 0;
use vibeos::limits::MAX_VIBEFS_VOLS as MAX_VOLS;
const MNT_MAX: usize = 2;
const MNT_PATH: usize = 64;
pub const IMAGE_BYTES: usize = 256 * 1024;

const _: () = assert!(IMAGE_BYTES / BLOCK <= vibeos::vibefs::MAX_BLOCKS);
const _: () = assert!(IMAGE_BYTES.is_multiple_of(BLOCK));

#[derive(Clone, Copy)]
enum Media {
    Mem,
    Dev(u32),
}

struct Slot {
    vol: UnsafeCell<Vol>,
    back: UnsafeCell<Media>,
    used: AtomicBool,
    busy: AtomicBool,
    /// The `Vfs` superblock the volume is mounted as, [`NO_SB`] until then.
    sb: AtomicU8,
}

unsafe impl Sync for Slot {}

impl Slot {
    const fn empty() -> Self {
        Self {
            vol: UnsafeCell::new(Vol::new()),
            back: UnsafeCell::new(Media::Mem),
            used: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            sb: AtomicU8::new(NO_SB),
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
static IMAGE: IrqCell<[u8; IMAGE_BYTES]> = IrqCell::new([0; IMAGE_BYTES]);
static LIVE: AtomicBool = AtomicBool::new(false);
static NVOL: AtomicU8 = AtomicU8::new(0);

struct Io {
    back: Media,
}

fn secs_per_blk(bs: u32) -> Result<u32, Error> {
    if bs == 0 || !(BLOCK as u32).is_multiple_of(bs) {
        return Err(Error::Inval);
    }
    Ok(BLOCK as u32 / bs)
}

impl Disk for Io {
    fn nblocks(&self) -> u32 {
        match self.back {
            Media::Mem => (IMAGE_BYTES / BLOCK) as u32,
            Media::Dev(cache_init::DEV_RAM0) => {
                let bs = block_init::logical_block_size();
                let n = block_init::capacity_sectors();
                if bs == 0 {
                    0
                } else {
                    ((n * bs as u64) / BLOCK as u64) as u32
                }
            }
            Media::Dev(cache_init::DEV_VDA) => {
                let bs = virtio_blk_init::logical_block_size();
                let n = virtio_blk_init::capacity_sectors();
                if bs == 0 {
                    0
                } else {
                    (n.saturating_mul(bs as u64) / BLOCK as u64) as u32
                }
            }
            Media::Dev(_) => 0,
        }
    }

    fn read_block(&mut self, bno: u32, buf: &mut [u8; BLOCK]) -> Result<(), Error> {
        match self.back {
            Media::Mem => {
                let off = (bno as usize).checked_mul(BLOCK).ok_or(Error::Inval)?;
                let end = off.checked_add(BLOCK).ok_or(Error::Inval)?;
                IMAGE.with(|data| {
                    if end > data.len() {
                        return Err(Error::Io);
                    }
                    buf.copy_from_slice(&data[off..end]);
                    Ok(())
                })
            }
            Media::Dev(dev) => {
                let bs = match dev {
                    cache_init::DEV_RAM0 => block_init::logical_block_size(),
                    _ => virtio_blk_init::logical_block_size(),
                };
                let spb = secs_per_blk(bs)?;
                let lba = bno as u64 * spb as u64;
                cache_init::read(dev, lba, buf).map_err(|_| Error::Io)
            }
        }
    }

    fn write_block(&mut self, bno: u32, buf: &[u8; BLOCK]) -> Result<(), Error> {
        match self.back {
            Media::Mem => {
                let off = (bno as usize).checked_mul(BLOCK).ok_or(Error::Inval)?;
                let end = off.checked_add(BLOCK).ok_or(Error::Inval)?;
                IMAGE.with(|data| {
                    if end > data.len() {
                        return Err(Error::Io);
                    }
                    data[off..end].copy_from_slice(buf);
                    Ok(())
                })
            }
            Media::Dev(dev) => {
                let bs = match dev {
                    cache_init::DEV_RAM0 => block_init::logical_block_size(),
                    _ => virtio_blk_init::logical_block_size(),
                };
                let spb = secs_per_blk(bs)?;
                let lba = bno as u64 * spb as u64;
                cache_init::write(dev, lba, buf).map_err(|_| Error::Io)
            }
        }
    }

    fn flush(&mut self) -> Result<(), Error> {
        match self.back {
            Media::Mem => Ok(()),
            Media::Dev(dev) => cache_init::flush(dev).map_err(|_| Error::Io),
        }
    }
}

fn grab(id: u8) -> Result<(), Error> {
    let i = id as usize;
    if i >= MAX_VOLS || !SLOTS[i].used.load(Ordering::Acquire) {
        return Err(Error::Io);
    }
    let mut n = 0u32;
    while SLOTS[i]
        .busy
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        n = n.saturating_add(1);
        if n > 1_000_000 {
            return Err(Error::Io);
        }
        thread_init::yield_now();
    }
    if !SLOTS[i].used.load(Ordering::Acquire) {
        SLOTS[i].busy.store(false, Ordering::Release);
        return Err(Error::Io);
    }
    Ok(())
}

fn drop_busy(id: u8) {
    let i = id as usize;
    if i < MAX_VOLS {
        SLOTS[i].busy.store(false, Ordering::Release);
    }
}

/// Run `f` on volume `id`, whose busy flag the caller took, and drop the
/// flag.
fn with_grabbed<R, E>(id: u8, f: impl FnOnce(&mut Vol, &mut Io) -> Result<R, E>) -> Result<R, E> {
    let i = id as usize;
    // SAFETY: the busy flag of `SLOTS[i]`, which the caller took through
    // `vibefs_init::grab` and which is dropped
    // only below, makes this thread the one accessor of the slot's `vol`
    // and `back` until `drop_busy`; `i < MAX_VOLS` was checked there.
    let r = unsafe {
        let v = &mut *SLOTS[i].vol.get();
        let mut io = Io {
            back: *SLOTS[i].back.get(),
        };
        f(v, &mut io)
    };
    drop_busy(id);
    r
}

/// Run `f` on volume `id`, waiting for its busy flag. Never under the VFS
/// lock.
fn with_slot<R>(id: u8, f: impl FnOnce(&mut Vol, &mut Io) -> Result<R, Error>) -> Result<R, Error> {
    grab(id)?;
    with_grabbed(id, f)
}

/// The one conversion from a vibefs inode's fields to its `Vfs` inode:
/// key `[ino, 0, 0]`, the mode's permission bits from the node and its
/// type from `kind`.
pub fn inode_info(
    ino: u32,
    kind: InodeKind,
    mode: u16,
    nlink: u32,
    size: u64,
    mtime: u64,
) -> InodeInfo {
    InodeInfo {
        key: [ino, 0, 0],
        ino,
        kind,
        mode: (mode & !S_IFMT) | kind.ifmt(),
        nlink,
        size,
        atime: mtime,
        mtime,
        ctime: mtime,
        private: [0; 2],
    }
}

fn node_info(n: &Node) -> InodeInfo {
    inode_info(n.ino, n.kind, n.mode, n.nlink, n.size, n.mtime)
}

/// Run `f` on volume `id`, waiting for its busy flag, with the VFS lock
/// dropped; a vibefs error becomes its `FsError`.
fn with_vol<R>(
    id: u8,
    f: impl FnOnce(&mut Vol, &mut Io) -> Result<R, FsError>,
) -> Result<R, FsError> {
    grab(id).map_err(Error::to_fs)?;
    with_grabbed(id, f)
}

/// Store the size of the inode `h` names, which vibefs keeps too, in its
/// `Vfs` inode, inside the busy section it was read in.
fn store_size(h: InodeHandle, size: u64) -> Result<(), FsError> {
    fs_init::with(|vfs| {
        let w = vfs.inode_words(h)?;
        vfs.set_inode_words(h, w.private, size)
    })
}

/// vibefs's [`InodeOps`], behind a vibefs superblock's `ops` pointer. The
/// superblock's private words are `[vol, 0]`. Every op runs with the VFS
/// lock dropped; the size vibefs keeps is stored in the `Vfs` inode in the
/// op's busy section.
pub struct VibeOps;

fn vol_of(cx: &OpCx<'_>) -> u8 {
    cx.private[0] as u8
}

impl InodeOps for VibeOps {
    fn lookup(&self, cx: &mut OpCx<'_>, dir: &Inode, name: &[u8]) -> Result<InodeInfo, FsError> {
        with_vol(vol_of(cx), |v, d| {
            let n = v.lookup(d, dir.key[0], name).map_err(Error::to_fs)?;
            Ok(node_info(&n))
        })
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
        with_vol(vol_of(cx), |v, d| {
            let n = v
                .create(d, dir.key[0], name, kind, mode, target)
                .map_err(Error::to_fs)?;
            Ok(node_info(&n))
        })
    }

    fn unlink(&self, cx: &mut OpCx<'_>, dir: &mut Inode, name: &[u8]) -> Result<(), FsError> {
        with_vol(vol_of(cx), |v, d| {
            v.unlink(d, dir.key[0], name, false).map_err(Error::to_fs)
        })
    }

    fn rmdir(&self, cx: &mut OpCx<'_>, dir: &mut Inode, name: &[u8]) -> Result<(), FsError> {
        with_vol(vol_of(cx), |v, d| {
            v.unlink(d, dir.key[0], name, true).map_err(Error::to_fs)
        })
    }

    /// An inode's key is its number, which a rename keeps.
    fn rename(
        &self,
        cx: &mut OpCx<'_>,
        odir: &mut Inode,
        oname: &[u8],
        ndir: &mut Inode,
        nname: &[u8],
    ) -> Result<Option<Key>, FsError> {
        with_vol(vol_of(cx), |v, d| {
            v.rename(d, odir.key[0], oname, ndir.key[0], nname)
                .map_err(Error::to_fs)?;
            Ok(None)
        })
    }

    fn read(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        with_vol(vol_of(cx), |v, d| {
            v.read(d, ino.key[0], off, buf).map_err(Error::to_fs)
        })
    }

    fn write(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &[u8],
    ) -> Result<usize, FsError> {
        write_at(vol_of(cx), ino, Some(off), buf).map(|(n, _)| n)
    }

    /// The size `O_APPEND` writes at is read in the write's busy section.
    fn write_append(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        buf: &[u8],
    ) -> Result<(usize, u64), FsError> {
        write_at(vol_of(cx), ino, None, buf)
    }

    fn truncate(&self, cx: &mut OpCx<'_>, ino: &mut Inode, size: u64) -> Result<(), FsError> {
        let (key, h) = (ino.key[0], ino.handle());
        with_vol(vol_of(cx), |v, d| {
            v.truncate(d, key, size).map_err(Error::to_fs)?;
            store_size(h, size)
        })
    }

    /// `.` and `..` are skipped should vibefs list them: `Vfs` makes
    /// both.
    fn readdir(
        &self,
        cx: &mut OpCx<'_>,
        dir: &Inode,
        cookie: u64,
        out: &mut Dirent,
    ) -> Result<Option<u64>, FsError> {
        let r = with_vol(vol_of(cx), |v, d| {
            let mut c = cookie;
            let mut n = Node::EMPTY;
            loop {
                let Some(next) = v.readdir(d, dir.key[0], c, &mut n).map_err(Error::to_fs)? else {
                    return Ok(None);
                };
                c = next;
                if n.name() != b"." && n.name() != b".." {
                    return Ok(Some((c, n)));
                }
            }
        })?;
        let Some((c, n)) = r else {
            return Ok(None);
        };
        out.ino = n.ino;
        out.kind = n.kind;
        out.name = Name::from_bytes(n.name())?;
        Ok(Some(c))
    }

    fn getattr(&self, cx: &mut OpCx<'_>, ino: &mut Inode) -> Result<(), FsError> {
        let key = ino.key[0];
        ino.size = with_vol(vol_of(cx), |v, _| v.file_size(key).map_err(Error::to_fs))?;
        Ok(())
    }

    fn readlink(&self, cx: &mut OpCx<'_>, ino: &Inode, buf: &mut [u8]) -> Result<usize, FsError> {
        with_vol(vol_of(cx), |v, d| {
            v.readlink(d, ino.key[0], buf).map_err(Error::to_fs)
        })
    }

    fn sync(&self, cx: &mut OpCx<'_>) -> Result<(), FsError> {
        sync(vol_of(cx))
    }
}

/// Write `buf` at `off`, or at the file's size when there is none; the
/// count written and the position written at. The new size goes into
/// the `Vfs` inode in the same busy section.
fn write_at(id: u8, ino: &Inode, off: Option<u64>, buf: &[u8]) -> Result<(usize, u64), FsError> {
    let (key, h) = (ino.key[0], ino.handle());
    with_vol(id, |v, d| {
        let pos = match off {
            Some(o) => o,
            None => v.file_size(key).map_err(Error::to_fs)?,
        };
        let n = v.write(d, key, pos, buf).map_err(Error::to_fs)?;
        store_size(h, v.file_size(key).map_err(Error::to_fs)?)?;
        Ok((n, pos))
    })
}

/// vibefs registration for volume `vol`: its superblock's private words
/// are `[vol, 0]`.
pub struct VibeFs {
    vol: u8,
}

static VIBE_FS: [VibeFs; MAX_VOLS] = [VibeFs { vol: 0 }, VibeFs { vol: 1 }];

impl FileSystem for VibeFs {
    fn name(&self) -> &'static str {
        "vibefs"
    }

    fn fstype(&self) -> FsType {
        FsType::Vibe
    }

    fn ops(&'static self) -> Option<&'static dyn InodeOps> {
        Some(&VibeOps)
    }

    fn fill_super(&self, cx: &mut OpCx<'_>) -> Result<InodeInfo, FsError> {
        *cx.private = [u64::from(self.vol), 0];
        Ok(InodeInfo {
            key: [ROOT_INO, 0, 0],
            ino: ROOT_INO,
            kind: InodeKind::Dir,
            mode: S_IFDIR_MODE,
            nlink: 2,
            size: 0,
            atime: cx.now,
            mtime: cx.now,
            ctime: cx.now,
            private: [0, 0],
        })
    }

    fn max_bytes(&self) -> u64 {
        vibefs::MAX_FILE_SIZE
    }

    /// Record the superblock and route `at` to the volume, for the path
    /// syscalls' walk.
    fn on_mount(&self, cx: &mut OpCx<'_>, at: &[u8]) {
        let vol = vol_of(cx);
        if let Some(s) = SLOTS.get(vol as usize) {
            s.sb.store(cx.sb, Ordering::Release);
        }
        if register_mnt(vol, at).is_err() {
            crate::klog!(
                vibeos::log::Level::Warn,
                "vibeOS: vibefs: no route for a mount"
            );
        }
    }

    /// Drop `at`'s route; after the last mount, sync the volume and free
    /// its slot.
    fn on_umount(&self, cx: &mut OpCx<'_>, at: &[u8], last: bool) {
        let vol = vol_of(cx);
        let _ = unregister_mnt(at);
        if last {
            if sync(vol).is_err() {
                crate::klog!(
                    vibeos::log::Level::Warn,
                    "vibeOS: vibefs: sync at umount failed"
                );
            }
            if let Some(s) = SLOTS.get(vol as usize) {
                s.sb.store(NO_SB, Ordering::Release);
            }
            drop_slot(vol);
        }
    }
}

/// A volume not mounted in `Vfs`.
const NO_SB: u8 = u8::MAX;

/// The superblock volume `id` is mounted as; `None` before its mount.
fn sb_of(id: u8) -> Option<u8> {
    let sb = SLOTS.get(id as usize)?.sb.load(Ordering::Acquire);
    (sb != NO_SB).then_some(sb)
}

pub fn live() -> bool {
    LIVE.load(Ordering::Acquire)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn nvol() -> u8 {
    NVOL.load(Ordering::Acquire)
}

pub fn init() {
    IMAGE.with(|buf| buf.fill(0));
    let mut io = Io { back: Media::Mem };
    let vref = unsafe { &mut *SLOTS[0].vol.get() };
    vref.clear();
    if vibefs::mkfs(&mut io, b"vibe", vref).is_err() {
        LIVE.store(false, Ordering::Release);
        return;
    }
    if vibefs::mount(&mut io, vref).is_err() {
        LIVE.store(false, Ordering::Release);
        return;
    }
    unsafe {
        *SLOTS[0].back.get() = Media::Mem;
    }
    SLOTS[0].used.store(true, Ordering::Release);
    SLOTS[0].busy.store(false, Ordering::Release);
    NVOL.store(1, Ordering::Release);
    LIVE.store(true, Ordering::Release);
}

/// Walk `path` on volume `id` and count a reference to the `Vfs` inode it
/// names, with the volume held: busy flag first, then the VFS lock. For
/// the path syscalls until ROADMAP §10.4 routes them through `Vfs`.
pub fn walk_iget(id: u8, path: &[u8]) -> Result<InodeRef, FsError> {
    let sb = sb_of(id).ok_or(FsError::Io)?;
    with_vol(id, |v, d| {
        let n = v.walk(d, path).map_err(Error::to_fs)?;
        fs_init::with(|vfs| vfs.iget_key(sb, &node_info(&n)))
    })
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn snapshot(id: u8, name: &[u8]) -> Result<(), FsError> {
    with_slot(id, |v, d| v.snapshot(d, name)).map_err(Error::to_fs)
}

pub fn sync(id: u8) -> Result<(), FsError> {
    with_slot(id, |v, d| v.sync(d)).map_err(Error::to_fs)
}

pub fn df(id: u8) -> Result<(FsType, u64, u64, u32), FsError> {
    with_slot(id, |v, _| {
        let (tot, free, n) = v.df();
        Ok((FsType::Vibe, tot, free, n))
    })
    .map_err(Error::to_fs)
}

#[allow(dead_code)]
pub fn probe_dev(name: &str) -> bool {
    let back = match name {
        "ram0" => Media::Dev(cache_init::DEV_RAM0),
        "vda" => Media::Dev(cache_init::DEV_VDA),
        _ => return false,
    };
    let mut io = Io { back };
    vibefs::probe(&mut io)
}

/// `(vol, strip)`: strip==0 means no vibe mount on this path.
pub fn route(path: &[u8]) -> (u8, usize) {
    let mnts = {
        let g = MNTS.lock();
        *g
    };
    let mut best = 0usize;
    let mut vol = 0u8;
    let mut hit = false;
    let mut i = 0usize;
    while i < MNT_MAX {
        if mnts[i].used {
            let n = mnts[i].len as usize;
            let p = &mnts[i].path[..n];
            if (path == p || (path.len() > n && path[..n] == p[..] && path[n] == b'/')) && n >= best
            {
                best = n;
                vol = mnts[i].vol;
                hit = true;
            }
        }
        i += 1;
    }
    if hit { (vol, best) } else { (0, 0) }
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

fn register_mnt(vol: u8, p: &[u8]) -> Result<(), FsError> {
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

fn unregister_mnt(p: &[u8]) -> Option<u8> {
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

fn recount() {
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

fn drop_slot(id: u8) {
    if id == VOL_MEM {
        return;
    }
    let i = id as usize;
    if i >= MAX_VOLS {
        return;
    }
    let _ = grab(id);
    SLOTS[i].used.store(false, Ordering::Release);
    drop_busy(id);
    recount();
}

pub fn mount_mem(at: &str) -> Result<u8, FsError> {
    if !live() {
        return Err(FsError::Io);
    }
    let fs = &VIBE_FS[VOL_MEM as usize];
    fs_init::api().mount_fs(None, at.as_bytes(), fs, None, false)?;
    Ok(VOL_MEM)
}

/// Mount the vibefs volume on block device `name` on `at`. A device
/// `Vfs` already mounts shares its superblock (`Busy` when `ro`
/// differs); otherwise the volume gets a slot, which is dropped again
/// when `Vfs` reports that another mount of the device won the race.
pub fn mount_dev(name: &str, at: &str, ro: bool) -> Result<u8, FsError> {
    let (back, dev) = match name {
        "ram0" => {
            if !block_init::live() {
                return Err(FsError::Io);
            }
            (Media::Dev(cache_init::DEV_RAM0), cache_init::DEV_RAM0)
        }
        "vda" => {
            if !virtio_blk_init::live() {
                return Err(FsError::Io);
            }
            (Media::Dev(cache_init::DEV_VDA), cache_init::DEV_VDA)
        }
        _ => return Err(FsError::Inval),
    };
    let dev = Some(u64::from(dev));
    let api = fs_init::api();
    if let Some(sb) = dev.and_then(|d| fs_init::with(|v| v.super_of_dev(d))) {
        let vol = fs_init::with(|v| v.sb_private(sb))?[0] as u8;
        let fs = VIBE_FS.get(vol as usize).ok_or(FsError::Io)?;
        api.mount_fs(None, at.as_bytes(), fs, dev, ro)?;
        return Ok(vol);
    }
    let mut io = Io { back };
    if !vibefs::probe(&mut io) {
        return Err(FsError::Inval);
    }
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
    let vref = unsafe { &mut *SLOTS[id as usize].vol.get() };
    if vibefs::mount(&mut io, vref).is_err() {
        SLOTS[id as usize].used.store(false, Ordering::Release);
        SLOTS[id as usize].busy.store(false, Ordering::Release);
        return Err(FsError::Inval);
    }
    unsafe {
        *SLOTS[id as usize].back.get() = back;
    }
    SLOTS[id as usize].busy.store(false, Ordering::Release);
    recount();
    match api.mount_fs(None, at.as_bytes(), &VIBE_FS[id as usize], dev, ro) {
        Ok(m) if m.shared => {
            drop_slot(id);
            Ok(fs_init::with(|v| v.sb_private(m.sb))?[0] as u8)
        }
        Ok(_) => Ok(id),
        Err(e) => {
            drop_slot(id);
            Err(e)
        }
    }
}

const _: () = {
    assert!(MAX_PATH >= MNT_PATH);
};

/// QEMU-kill workload. Marker `vibeOS: vibefs: wr N` is not a boot
/// contract line. Printed *before* the write+fsync so a kill can land
/// inside `write` / `Flush` (docs/VIBEFS.md §12).
#[cfg(feature = "vibefs_crash")]
/// The `vibefs_crash` build's workload (VIBEFS §12 item 2, ROADMAP §10.2).
/// Mounts `vda` at `/crash`, commits iteration 0, prints `crash-ready`, then
/// for each N from 1 prints `wr N` and commits iteration N: `/crash/w` with
/// `O_TRUNC`, 300 bytes of `(N + k) as u8`, close, `sync_fs`. Any error
/// prints a registered failure line and halts, so the harness sees it.
pub fn crash_loop() -> ! {
    use crate::file_init;
    use crate::x86;

    /// One committed iteration of `crash_loop`. The error is the text its
    /// failure line carries.
    fn crash_iter(n: u32) -> Result<(), &'static str> {
        use crate::file_init;
        use vibeos::fs::{O_CREAT, O_RDWR, O_TRUNC, OpenFlags};

        let flags = OpenFlags::from_bits(O_RDWR | O_CREAT | O_TRUNC);
        let f = file_init::open(b"/crash/w", flags, 0o644).map_err(|e| e.as_str())?;
        let mut buf = [0u8; 300];
        for (k, b) in buf.iter_mut().enumerate() {
            *b = n.wrapping_add(k as u32) as u8;
        }
        let wrote = file_init::write(&f, &buf);
        let closed = file_init::close(f);
        match wrote {
            Ok(w) if w == buf.len() => {}
            Ok(_) => return Err("short write"),
            Err(e) => return Err(e.as_str()),
        }
        closed.map_err(|e| e.as_str())?;
        file_init::sync_fs().map_err(|e| e.as_str())
    }

    if let Err(e) = file_init::mkdir(b"/crash", 0o755)
        .and_then(|()| mount_dev("vda", "/crash", false).map(|_| ()))
    {
        crate::marker!("vibeOS: vibefs: mount fail {}", e.as_str());
        x86::halt();
    }
    if let Err(e) = crash_iter(0) {
        crate::marker!("vibeOS: vibefs: sync fail {e}");
        x86::halt();
    }
    crate::marker!("vibeOS: vibefs: crash-ready");
    let mut n = 1u32;
    loop {
        crate::marker!("vibeOS: vibefs: wr {n}");
        if let Err(e) = crash_iter(n) {
            crate::marker!("vibeOS: vibefs: sync fail {e}");
            x86::halt();
        }
        n = n.wrapping_add(1);
        thread_init::yield_now();
    }
}
