//! vibefs volumes. ROADMAP §8.5 / docs/VIBEFS.md.
//!
//! Each volume is an instance ([`VibeVolume`], DESIGN §12.1 rule 1) on the
//! heap: a memory-backed one, with its own heap image, is owned by the
//! superblock that shows it, and a device's by its block registry entry
//! (`blockdev_init::set_holder`). Busy flag (not IRQ-off mutex) across I/O
//! (DESIGN §2.1). `sync` uses disk `Flush`.
//!
//! [`VibeOps`] is the one way to a volume's files: `Vfs`'s File API calls
//! it with the VFS lock dropped, so it waits for the busy flag. The size
//! vibefs keeps is stored in the inode slot's words (`Inode::words`)
//! inside the busy section, with no VFS lock. The routing table (`route`) serves path syscalls until
//! ROADMAP §10.4 routes them through `Vfs`.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use vibeos::block::BlockError;
use vibeos::block::blockdev::BlockRef;
use vibeos::dev::Instance;
use vibeos::fs::{
    Dirent, FileSystem, FsError, FsType, Inode, InodeInfo, InodeKind, InodeOps, InodeRef, Key,
    MAX_PATH, Name, OpCx, S_IFDIR_MODE, S_IFMT,
};
use vibeos::kalloc::TryBox;
use vibeos::lock::RANK_DEVICE;
use vibeos::vibefs::{self, BLOCK, Disk, Error, Node, ROOT_INO, Vol};

use crate::block::blockdev_init;
use crate::fs_init;
use crate::sync_init::SpinMutex;
use crate::thread_init;

/// A cache error as vibefs sees it: a refused heap allocation stays
/// `NoMem` (ENOMEM), anything else is `Io`.
fn vibefs_io_err(e: BlockError) -> Error {
    match e {
        BlockError::NoMem => Error::NoMem,
        _ => Error::Io,
    }
}
const MNT_MAX: usize = 2;
const MNT_PATH: usize = 64;
pub const IMAGE_BYTES: usize = 256 * 1024;

const _: () = assert!(IMAGE_BYTES / BLOCK <= vibeos::vibefs::MAX_BLOCKS);
const _: () = assert!(IMAGE_BYTES.is_multiple_of(BLOCK));

/// A memory volume's image, which its lock makes one accessor's at a
/// time.
type Image = SpinMutex<TryBox<[u8; IMAGE_BYTES]>>;

enum Media {
    /// A heap image the volume owns.
    Mem(Image),
    /// A registered block device; the volume's handle keeps it alive.
    Dev(BlockRef),
}

/// One vibefs volume. Its `vol` cell belongs to the one thread holding
/// `busy`, or, before the instance is shared, to the path building it
/// (invariant I236). Built and mounted in place, never moved: it is too
/// large for a kernel stack (DESIGN §4.5).
pub(crate) struct VibeVolume {
    media: Media,
    pub(super) used: AtomicBool,
    pub(super) busy: AtomicBool,
    /// The `Vfs` superblock the volume is mounted as, [`NO_SB`] until then.
    sb: AtomicU8,
    vol: UnsafeCell<TryBox<Vol>>,
}

// SAFETY: invariant I236: one thread at a time reaches a volume's `vol`
// cell, through the busy flag `fs::vibefs_init::grab` takes, or the
// building path before the instance is shared, and its contents are
// `Send` (asserted below); established by `fs::vibefs_init::grab`.
unsafe impl Sync for VibeVolume {}

const _: () = {
    const fn send<T: Send>() {}
    send::<Vol>();
    send::<Media>();
};

/// A fresh volume's state, copied into each new instance in place.
static VOL_INIT: Vol = Vol::new();

struct Mnt {
    used: bool,
    vol: Option<Instance>,
    len: u8,
    path: [u8; MNT_PATH],
}

impl Mnt {
    const EMPTY: Self = Self {
        used: false,
        vol: None,
        len: 0,
        path: [0; MNT_PATH],
    };
}

static MNTS: SpinMutex<[Mnt; MNT_MAX]> =
    SpinMutex::with_rank([Mnt::EMPTY, Mnt::EMPTY], RANK_DEVICE);

/// Run `f` on memory image `img`, holding its lock.
fn with_image<R>(img: &Image, f: impl FnOnce(&mut [u8; IMAGE_BYTES]) -> R) -> R {
    let mut g = img.lock();
    f(&mut g)
}

/// Take the memory image of volume `i` as a volume read does (the
/// in-guest test `cross_cpu_cells_ranked`); `false` when `i` is no
/// memory volume.
#[cfg(feature = "kernel_tests")]
pub(super) fn probe_image_of(i: &Instance) -> bool {
    match as_vibe(i).map(|v| &v.media) {
        Ok(Media::Mem(img)) => {
            with_image(img, |_| ());
            true
        }
        _ => false,
    }
}

static LIVE: AtomicBool = AtomicBool::new(false);

pub(super) struct Io<'a> {
    back: &'a Media,
}

fn secs_per_blk(bs: u32) -> Result<u32, Error> {
    if bs == 0 || !(BLOCK as u32).is_multiple_of(bs) {
        return Err(Error::Inval);
    }
    Ok(BLOCK as u32 / bs)
}

/// The logical block number of vibefs block `bno` on `r`, whose block
/// size must divide [`BLOCK`].
fn dev_lba(r: &BlockRef, bno: u32) -> Result<u64, Error> {
    let spb = secs_per_blk(r.logical_block_size().map_err(vibefs_io_err)?)?;
    u64::from(bno)
        .checked_mul(u64::from(spb))
        .ok_or(Error::Inval)
}

impl Disk for Io<'_> {
    fn nblocks(&self) -> u32 {
        match self.back {
            Media::Mem(_) => (IMAGE_BYTES / BLOCK) as u32,
            Media::Dev(r) => match (r.logical_block_size(), r.capacity_sectors()) {
                (Ok(bs), Ok(n)) => {
                    let blocks = n.saturating_mul(u64::from(bs)) / BLOCK as u64;
                    u32::try_from(blocks).unwrap_or(u32::MAX)
                }
                _ => 0,
            },
        }
    }

    fn read_block(&mut self, bno: u32, buf: &mut [u8; BLOCK]) -> Result<(), Error> {
        match self.back {
            Media::Mem(img) => {
                let off = (bno as usize).checked_mul(BLOCK).ok_or(Error::Inval)?;
                let end = off.checked_add(BLOCK).ok_or(Error::Inval)?;
                with_image(img, |data| {
                    if end > data.len() {
                        return Err(Error::Io);
                    }
                    buf.copy_from_slice(&data[off..end]);
                    Ok(())
                })
            }
            Media::Dev(r) => r.read(dev_lba(r, bno)?, buf).map_err(vibefs_io_err),
        }
    }

    fn write_block(&mut self, bno: u32, buf: &[u8; BLOCK]) -> Result<(), Error> {
        match self.back {
            Media::Mem(img) => {
                let off = (bno as usize).checked_mul(BLOCK).ok_or(Error::Inval)?;
                let end = off.checked_add(BLOCK).ok_or(Error::Inval)?;
                with_image(img, |data| {
                    if end > data.len() {
                        return Err(Error::Io);
                    }
                    data[off..end].copy_from_slice(buf);
                    Ok(())
                })
            }
            Media::Dev(r) => r.write(dev_lba(r, bno)?, buf).map_err(vibefs_io_err),
        }
    }

    fn flush(&mut self) -> Result<(), Error> {
        match self.back {
            Media::Mem(_) => Ok(()),
            Media::Dev(r) => r.flush().map_err(vibefs_io_err),
        }
    }
}

/// Take `v`'s busy flag, waiting for its holder.
fn grab(v: &VibeVolume) -> Result<(), Error> {
    if !v.used.load(Ordering::Acquire) {
        return Err(Error::Io);
    }
    let mut n = 0u32;
    while v
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
    if !v.used.load(Ordering::Acquire) {
        v.busy.store(false, Ordering::Release);
        return Err(Error::Io);
    }
    Ok(())
}

fn drop_busy(v: &VibeVolume) {
    v.busy.store(false, Ordering::Release);
}

/// Run `f` on volume `v`, whose busy flag the caller took, and drop the
/// flag.
fn with_grabbed<R, E>(
    v: &VibeVolume,
    f: impl FnOnce(&mut Vol, &mut Io) -> Result<R, E>,
) -> Result<R, E> {
    // SAFETY: invariant I236: the busy flag of `v`, which the caller took
    // and which is dropped only below, makes this thread the one accessor
    // of the volume's `vol` cell until `drop_busy`; established by
    // `fs::vibefs_init::grab`.
    let r = unsafe {
        let mut io = Io { back: &v.media };
        f(&mut *v.vol.get(), &mut io)
    };
    drop_busy(v);
    r
}

/// Run `f` on volume `v`, waiting for its busy flag. Never under the VFS
/// lock.
pub(super) fn with_slot<R>(
    v: &VibeVolume,
    f: impl FnOnce(&mut Vol, &mut Io) -> Result<R, Error>,
) -> Result<R, Error> {
    grab(v)?;
    with_grabbed(v, f)
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

/// Run `f` on volume `v`, waiting for its busy flag, with the VFS lock
/// dropped; a vibefs error becomes its `FsError`.
fn with_vol<R>(
    v: &VibeVolume,
    f: impl FnOnce(&mut Vol, &mut Io) -> Result<R, FsError>,
) -> Result<R, FsError> {
    grab(v).map_err(Error::to_fs)?;
    with_grabbed(v, f)
}

/// The vibefs volume instance `i` is; `Io` for any other.
fn as_vibe(i: &Instance) -> Result<&VibeVolume, FsError> {
    i.downcast_ref::<VibeVolume>().ok_or(FsError::Io)
}

/// Store the size vibefs keeps for `ino` in its slot's words
/// ([`Inode::words`]), inside the volume section it was read in; no VFS
/// lock.
fn store_size(ino: &Inode, size: u64) -> Result<(), FsError> {
    ino.words()?.set_size(size);
    Ok(())
}

/// vibefs's [`InodeOps`], behind a vibefs superblock's `ops` pointer. The
/// superblock holds the volume instance ([`OpCx::vol`]). Every op runs with the VFS
/// lock dropped; the size vibefs keeps is stored in the inode slot's
/// words in the op's busy section.
pub struct VibeOps;

/// The volume the superblock of `cx` shows.
fn vol_of<'a>(cx: &OpCx<'a>) -> Result<&'a VibeVolume, FsError> {
    as_vibe(cx.vol.ok_or(FsError::Io)?)
}

impl InodeOps for VibeOps {
    fn lookup(&self, cx: &mut OpCx<'_>, dir: &Inode, name: &[u8]) -> Result<InodeInfo, FsError> {
        with_vol(vol_of(cx)?, |v, d| {
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
        with_vol(vol_of(cx)?, |v, d| {
            let n = v
                .create(d, dir.key[0], name, kind, mode, target)
                .map_err(Error::to_fs)?;
            Ok(node_info(&n))
        })
    }

    fn unlink(&self, cx: &mut OpCx<'_>, dir: &mut Inode, name: &[u8]) -> Result<(), FsError> {
        with_vol(vol_of(cx)?, |v, d| {
            v.unlink(d, dir.key[0], name, false).map_err(Error::to_fs)
        })
    }

    fn rmdir(&self, cx: &mut OpCx<'_>, dir: &mut Inode, name: &[u8]) -> Result<(), FsError> {
        with_vol(vol_of(cx)?, |v, d| {
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
        with_vol(vol_of(cx)?, |v, d| {
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
        with_vol(vol_of(cx)?, |v, d| {
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
        write_at(vol_of(cx)?, ino, Some(off), buf).map(|(n, _)| n)
    }

    /// The size `O_APPEND` writes at is read in the write's busy section.
    fn write_append(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        buf: &[u8],
    ) -> Result<(usize, u64), FsError> {
        write_at(vol_of(cx)?, ino, None, buf)
    }

    fn truncate(&self, cx: &mut OpCx<'_>, ino: &mut Inode, size: u64) -> Result<(), FsError> {
        let (key, ino): (u32, &Inode) = (ino.key[0], ino);
        with_vol(vol_of(cx)?, |v, d| {
            v.truncate(d, key, size).map_err(Error::to_fs)?;
            store_size(ino, size)
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
        let r = with_vol(vol_of(cx)?, |v, d| {
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
        ino.size = with_vol(vol_of(cx)?, |v, _| v.file_size(key).map_err(Error::to_fs))?;
        Ok(())
    }

    fn readlink(&self, cx: &mut OpCx<'_>, ino: &Inode, buf: &mut [u8]) -> Result<usize, FsError> {
        with_vol(vol_of(cx)?, |v, d| {
            v.readlink(d, ino.key[0], buf).map_err(Error::to_fs)
        })
    }

    fn sync(&self, cx: &mut OpCx<'_>) -> Result<(), FsError> {
        sync(vol_of(cx)?)
    }
}

/// Write `buf` at `off`, or at the file's size when there is none; the
/// count written and the position written at. The new size goes into
/// the `Vfs` inode in the same busy section.
fn write_at(
    vol: &VibeVolume,
    ino: &Inode,
    off: Option<u64>,
    buf: &[u8],
) -> Result<(usize, u64), FsError> {
    let key = ino.key[0];
    with_vol(vol, |v, d| {
        let pos = match off {
            Some(o) => o,
            None => v.file_size(key).map_err(Error::to_fs)?,
        };
        let n = v.write(d, key, pos, buf).map_err(Error::to_fs)?;
        store_size(ino, v.file_size(key).map_err(Error::to_fs)?)?;
        Ok((n, pos))
    })
}

/// vibefs's registration. A superblock holds its volume instance; its
/// private words are unused.
pub struct VibeFs;

static VIBE_FS: VibeFs = VibeFs;

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
        vol_of(cx)?;
        *cx.private = [0, 0];
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
        if let Ok(v) = vol_of(cx) {
            v.sb.store(cx.sb, Ordering::Release);
        }
        if register_mnt(cx.vol.cloned(), at).is_err() {
            crate::klog!(
                vibeos::log::Level::Warn,
                "vibeOS: vibefs: no route for a mount"
            );
        }
    }

    /// Drop `at`'s route. After the superblock's last mount, which is the
    /// last to show the volume (one superblock per volume), sync the
    /// volume, retire it, and take a device's from its entry; the
    /// superblock's own reference goes when its slot is freed, and with it
    /// a memory volume.
    fn on_umount(&self, cx: &mut OpCx<'_>, at: &[u8], last: bool) {
        drop(unregister_mnt(at));
        if !last {
            return;
        }
        let Ok(vol) = vol_of(cx) else {
            return;
        };
        let mnt = core::str::from_utf8(at).unwrap_or("?");
        if let Err(e) = sync(vol) {
            crate::klog_ratelimited!(
                1000,
                vibeos::log::Level::Warn,
                "vibeOS: vibefs: sync of {} at umount failed: {}",
                mnt,
                e.as_str()
            );
        }
        vol.sb.store(NO_SB, Ordering::Release);
        if let Err(e) = drop_slot(vol) {
            crate::klog_ratelimited!(
                1000,
                vibeos::log::Level::Warn,
                "vibeOS: vibefs: volume of {} still held at umount, kept: {}",
                mnt,
                e.as_str()
            );
            return;
        }
        if let Media::Dev(r) = &vol.media {
            drop(blockdev_init::take_holder(r));
        }
    }
}

/// A volume not mounted in `Vfs`.
const NO_SB: u8 = u8::MAX;

pub fn live() -> bool {
    LIVE.load(Ordering::Acquire)
}

/// A new, unmounted volume instance on `media`, whose `Vol` is a heap copy
/// of a fresh one; `mount` fills it in place while this thread alone holds
/// it.
fn new_volume(
    media: Media,
    mount: impl FnOnce(&mut Vol, &mut Io) -> Result<(), Error>,
) -> Result<Instance, FsError> {
    let vol = crate::fs::boxed_copy(&VOL_INIT).map_err(|_| FsError::NoMem)?;
    let mut v = VibeVolume {
        media,
        used: AtomicBool::new(false),
        busy: AtomicBool::new(false),
        sb: AtomicU8::new(NO_SB),
        vol: UnsafeCell::new(vol),
    };
    let mut io = Io { back: &v.media };
    mount(v.vol.get_mut(), &mut io).map_err(Error::to_fs)?;
    v.used.store(true, Ordering::Release);
    vibeos::dev::instance(v).map_err(|_| FsError::NoMem)
}

/// Walk `path` on volume `vol` and count a reference to the `Vfs` inode it
/// names, with the volume held: busy flag first, then the VFS lock. For
/// the path syscalls until ROADMAP §10.4 routes them through `Vfs`.
pub fn walk_iget(vol: &Instance, path: &[u8]) -> Result<InodeRef, FsError> {
    let v = as_vibe(vol)?;
    let sb = v.sb.load(Ordering::Acquire);
    if sb == NO_SB {
        return Err(FsError::Io);
    }
    with_vol(v, |fv, d| {
        let n = fv.walk(d, path).map_err(Error::to_fs)?;
        fs_init::with(|vfs| vfs.iget_key(sb, &node_info(&n)))
    })
}

pub fn sync(vol: &VibeVolume) -> Result<(), FsError> {
    with_slot(vol, |v, d| v.sync(d)).map_err(Error::to_fs)
}

pub fn df(vol: &VibeVolume) -> Result<(FsType, u64, u64, u32), FsError> {
    with_slot(vol, |v, _| {
        let (tot, free, n) = v.df();
        Ok((FsType::Vibe, tot, free, n))
    })
    .map_err(Error::to_fs)
}

/// `(vol, strip)`: strip==0 means no vibe mount on this path.
pub fn route(path: &[u8]) -> (Option<Instance>, usize) {
    let g = MNTS.lock();
    let mut best = 0usize;
    let mut vol = None;
    for m in g.iter() {
        if m.used {
            let n = m.len as usize;
            let p = &m.path[..n];
            if (path == p || (path.len() > n && path[..n] == p[..] && path[n] == b'/')) && n >= best
            {
                best = n;
                vol = m.vol.as_ref();
            }
        }
    }
    match vol {
        Some(v) => (Some(v.clone()), best),
        None => (None, 0),
    }
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

fn register_mnt(vol: Option<Instance>, p: &[u8]) -> Result<(), FsError> {
    if p.is_empty() || p.len() > MNT_PATH {
        return Err(FsError::NameTooLong);
    }
    let mut g = MNTS.lock();
    let m = g.iter_mut().find(|m| !m.used).ok_or(FsError::NoSpace)?;
    m.used = true;
    m.vol = vol;
    m.len = p.len() as u8;
    m.path[..p.len()].copy_from_slice(p);
    Ok(())
}

/// Plant commit defect `p` in the volume mounted at `at`, found by its
/// route as `unregister_mnt` finds it at umount (test-only: the
/// `vibefs_crash` build, docs/VIBEFS.md §12).
#[cfg(feature = "vibefs_crash")]
pub(crate) fn set_plant(at: &[u8], p: vibefs::Plant) -> Result<(), FsError> {
    let vol = {
        let g = MNTS.lock();
        g.iter()
            .find(|m| m.used && m.path.get(..m.len as usize) == Some(at))
            .and_then(|m| m.vol.clone())
    };
    let vol = vol.ok_or(FsError::NotFound)?;
    with_slot(as_vibe(&vol)?, |v, _| {
        v.set_plant(p);
        Ok(())
    })
    .map_err(Error::to_fs)
}

/// Take `p`'s route out; the caller drops its volume reference unlocked.
fn unregister_mnt(p: &[u8]) -> Option<Instance> {
    let mut g = MNTS.lock();
    let m = g
        .iter_mut()
        .find(|m| m.used && m.path.get(..m.len as usize) == Some(p))?;
    m.used = false;
    m.len = 0;
    m.vol.take()
}

/// Retire volume `vol`: its `used` flag clears, so every later op fails.
/// When `grab` cannot take its busy flag the volume is left as it is,
/// still owned by its holder, and the error is returned.
pub(super) fn drop_slot(vol: &VibeVolume) -> Result<(), Error> {
    grab(vol)?;
    vol.used.store(false, Ordering::Release);
    drop_busy(vol);
    Ok(())
}

/// Mount a fresh memory-backed vibefs on `at`: a new instance with its own
/// zeroed heap image, made and mounted in place, which the superblock owns.
pub fn mount_mem(at: &str) -> Result<(), FsError> {
    let img = crate::fs::boxed_zeroed::<IMAGE_BYTES>().map_err(|_| FsError::NoMem)?;
    let media = Media::Mem(SpinMutex::with_rank(img, RANK_DEVICE));
    let vol = new_volume(media, |v, io| {
        vibefs::mkfs(io, b"vibe", v)?;
        vibefs::mount(io, v)
    })?;
    #[cfg(feature = "kernel_tests")]
    if at == "/vibe" {
        *crate::fs::ktest::VIBE_MEM.lock() = Some(vol.clone());
    }
    fs_init::api().mount_fs(None, at.as_bytes(), &VIBE_FS, None, false, Some(vol))?;
    LIVE.store(true, Ordering::Release);
    Ok(())
}

/// A used memory volume that nothing mounts or reaches, for the
/// busy-flag test `fs_drop_slot_busy_keeps_slot` (test-only).
#[cfg(feature = "kernel_tests")]
pub(super) fn spare_volume() -> Result<Instance, FsError> {
    let img = crate::fs::boxed_zeroed::<IMAGE_BYTES>().map_err(|_| FsError::NoMem)?;
    let vol = crate::fs::boxed_copy(&VOL_INIT).map_err(|_| FsError::NoMem)?;
    vibeos::dev::instance(VibeVolume {
        media: Media::Mem(SpinMutex::with_rank(img, RANK_DEVICE)),
        used: AtomicBool::new(true),
        busy: AtomicBool::new(false),
        sb: AtomicU8::new(NO_SB),
        vol: UnsafeCell::new(vol),
    })
    .map_err(|_| FsError::NoMem)
}

/// Make a fresh vibefs on block device `name`, which nothing may hold
/// (test-only, AGENTS.md rule 9: `block_two_disk_instances`).
#[cfg(feature = "kernel_tests")]
pub fn mkfs_dev(name: &str) -> Result<(), FsError> {
    let r = blockdev_init::lookup(name.as_bytes()).ok_or(FsError::NotFound)?;
    if blockdev_init::holder(&r).is_some() {
        return Err(FsError::Busy);
    }
    let mut vol = crate::fs::boxed_copy(&VOL_INIT).map_err(|_| FsError::NoMem)?;
    let media = Media::Dev(r);
    let mut io = Io { back: &media };
    vibefs::mkfs(&mut io, name.as_bytes(), &mut vol).map_err(Error::to_fs)?;
    vibefs::Disk::flush(&mut io).map_err(Error::to_fs)
}

/// Whether `r` carries a vibefs. Out of line, so its block buffer's frame
/// is gone before the mount's (DESIGN §4.5: the mount runs on 16 KiB).
#[inline(never)]
fn probe_dev(r: &BlockRef) -> bool {
    let media = Media::Dev(r.clone());
    vibefs::probe(&mut Io { back: &media })
}

/// Mount the vibefs volume on block device `name` on `at`. A device whose
/// entry holds a vibefs volume shares it, and `Vfs` shares its superblock
/// (`Busy` when `ro` differs); one holding another filesystem's volume is
/// `Busy`. Otherwise the volume is built, becomes the entry's holder, and
/// is taken back if the mount fails.
pub fn mount_dev(name: &str, at: &str, ro: bool) -> Result<(), FsError> {
    let r = blockdev_init::lookup(name.as_bytes()).ok_or(FsError::NotFound)?;
    let dev = Some(r.id());
    let api = fs_init::api();
    if let Some(h) = blockdev_init::holder(&r) {
        as_vibe(&h).map_err(|_| FsError::Busy)?;
        return api
            .mount_fs(None, at.as_bytes(), &VIBE_FS, dev, ro, Some(h))
            .map(|_| ());
    }
    if !probe_dev(&r) {
        return Err(FsError::Inval);
    }
    let vol = new_volume(Media::Dev(r.clone()), |v, io| vibefs::mount(io, v))?;
    blockdev_init::set_holder(&r, vol.clone()).map_err(|e| match e {
        BlockError::Exists => FsError::Busy,
        _ => FsError::Io,
    })?;
    match api.mount_fs(None, at.as_bytes(), &VIBE_FS, dev, ro, Some(vol.clone())) {
        Ok(_) => Ok(()),
        Err(e) => {
            if !fs_init::with(|v| v.shows_volume(&vol)) {
                drop(blockdev_init::take_holder(&r));
            }
            Err(e)
        }
    }
}

const _: () = {
    assert!(MAX_PATH >= MNT_PATH);
};
