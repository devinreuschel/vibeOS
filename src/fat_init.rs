//! FAT volumes: initrd + block-backed mounts. ROADMAP §8.2–8.3 / §8.6.
//!
//! The volume lives in BSS. A busy flag (not the IRQ-off mutex) is held
//! across I/O so RANK_DEVICE is not nested with the cache (DESIGN §2.1 /
//! #62 ACK). `sync` uses cache/device Flush (DESIGN §10.2).
//!
//! [`FatOps`] is the one way to a volume's files: `Vfs`'s File API calls
//! it with the VFS lock dropped, so it waits for the busy flag. A FAT
//! inode is the `Vfs` inode keyed by its dirent location; its first
//! cluster and size are `Vfs` inode words that only FAT reads and writes,
//! inside the busy section, through short VFS-lock sections
//! (`Vfs::inode_words`, `Vfs::set_inode_words`): the busy flag first, the
//! VFS lock second, never the reverse (C-FILEAPI). The routing table
//! (`route`) serves path syscalls until ROADMAP §10.4 routes them
//! through `Vfs`.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use vibeos::fat::{self, Disk, FatError, FatInode, FatVol, INITRD_BYTES, Node, SEC};
use vibeos::fs::{
    Dirent, FileSystem, FsError, FsType, Inode, InodeHandle, InodeInfo, InodeKind, InodeOps,
    InodeRef, Key, MAX_PATH, Name, OpCx, S_IFDIR_MODE, S_IFREG_MODE,
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
enum Media {
    Initrd,
    Dev(u32),
}

struct Slot {
    vol: UnsafeCell<Option<FatVol>>,
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
            vol: UnsafeCell::new(None),
            back: UnsafeCell::new(Media::Initrd),
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
static INITRD: IrqCell<[u8; INITRD_BYTES]> = IrqCell::new([0; INITRD_BYTES]);
static LIVE: AtomicBool = AtomicBool::new(false);
static NVOL: AtomicU8 = AtomicU8::new(0);

const INITRD_RO: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/initrd.fat"));

struct Io {
    back: Media,
}

impl Disk for Io {
    fn sector_size(&self) -> u32 {
        SEC as u32
    }

    fn nsectors(&self) -> u32 {
        match self.back {
            Media::Initrd => (INITRD_BYTES / SEC) as u32,
            Media::Dev(cache_init::DEV_RAM0) => block_init::capacity_sectors() as u32,
            Media::Dev(cache_init::DEV_VDA) => virtio_blk_init::capacity_sectors() as u32,
            Media::Dev(_) => 0,
        }
    }

    fn read(&mut self, lba: u32, buf: &mut [u8]) -> Result<(), FatError> {
        match self.back {
            Media::Initrd => {
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
            Media::Dev(dev) => cache_init::read(dev, lba as u64, buf).map_err(|_| FatError::Io),
        }
    }

    fn write(&mut self, lba: u32, buf: &[u8]) -> Result<(), FatError> {
        match self.back {
            Media::Initrd => {
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
            Media::Dev(dev) => cache_init::write(dev, lba as u64, buf).map_err(|_| FatError::Io),
        }
    }

    fn flush(&mut self) -> Result<(), FatError> {
        match self.back {
            Media::Initrd => Ok(()),
            Media::Dev(dev) => cache_init::flush(dev).map_err(|_| FatError::Io),
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

/// Run `f` on volume `id`, whose busy flag the caller took, and drop the
/// flag.
fn with_grabbed<R>(
    id: u8,
    f: impl FnOnce(&mut FatVol, &mut Io) -> Result<R, FsError>,
) -> Result<R, FsError> {
    let i = id as usize;
    // SAFETY: the busy flag of `SLOTS[i]`, which the caller took through
    // `fat_init::grab` and which is dropped only
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

/// A volume not mounted in `Vfs`.
const NO_SB: u8 = u8::MAX;

/// The superblock volume `id` is mounted as; `None` before its mount.
fn sb_of(id: u8) -> Option<u8> {
    let sb = SLOTS.get(id as usize)?.sb.load(Ordering::Acquire);
    (sb != NO_SB).then_some(sb)
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
/// `[first_clu, (dir_clu << 32) | dir_off]`. Every op runs with the VFS
/// lock dropped and reads the inode's words from `Vfs` inside the busy
/// section, never from the copy it is handed.
pub struct FatOps;

fn vol_of(cx: &OpCx<'_>) -> u8 {
    cx.private[0] as u8
}

impl InodeOps for FatOps {
    fn lookup(&self, cx: &mut OpCx<'_>, dir: &Inode, name: &[u8]) -> Result<InodeInfo, FsError> {
        with_slot(vol_of(cx), |v, d| {
            let (w, _) = words(dir.handle())?;
            Ok(node_info(&v.lookup(d, w.first_clu, name)?))
        })
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
        with_slot(vol_of(cx), |v, d| {
            let (w, _) = words(dir.handle())?;
            Ok(node_info(&v.create(d, w.first_clu, name, is_dir)?))
        })
    }

    /// Remove the dirent only: the victim's chain is freed by `evict` at
    /// its last put.
    fn unlink(&self, cx: &mut OpCx<'_>, dir: &mut Inode, name: &[u8]) -> Result<(), FsError> {
        remove(vol_of(cx), dir, name, false)
    }

    /// Remove the empty directory `name`; its cluster is freed by `evict`
    /// at its last put.
    fn rmdir(&self, cx: &mut OpCx<'_>, dir: &mut Inode, name: &[u8]) -> Result<(), FsError> {
        remove(vol_of(cx), dir, name, true)
    }

    /// The moved file's inode is re-keyed to its new dirent; a file the
    /// rename replaced is the one `Vfs` held for the new name, and its
    /// chain is freed by `evict` at its last put.
    fn rename(
        &self,
        cx: &mut OpCx<'_>,
        odir: &mut Inode,
        oname: &[u8],
        ndir: &mut Inode,
        nname: &[u8],
    ) -> Result<Option<Key>, FsError> {
        with_slot(vol_of(cx), |v, d| {
            let (o, _) = words(odir.handle())?;
            let (n, _) = words(ndir.handle())?;
            let m = v.rename(d, o.first_clu, oname, n.first_clu, nname)?;
            Ok((m.from != m.to).then_some([m.to.0, m.to.1, 0]))
        })
    }

    fn read(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        with_slot(vol_of(cx), |v, d| {
            let (w, _) = words(ino.handle())?;
            Ok(v.read_ino(d, &w, off, buf)?)
        })
    }

    fn write(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &[u8],
    ) -> Result<usize, FsError> {
        write_at(vol_of(cx), ino.handle(), off, false, buf).map(|(n, _)| n)
    }

    /// The size `O_APPEND` writes at is read in the write's busy section.
    fn write_append(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        buf: &[u8],
    ) -> Result<(usize, u64), FsError> {
        write_at(vol_of(cx), ino.handle(), 0, true, buf)
    }

    fn truncate(&self, cx: &mut OpCx<'_>, ino: &mut Inode, size: u64) -> Result<(), FsError> {
        let h = ino.handle();
        with_slot(vol_of(cx), |v, d| {
            let (mut w, linked) = words(h)?;
            let r = v.truncate_ino(d, &mut w, linked, size);
            store(h, &w)?;
            Ok(r?)
        })
    }

    /// The on-disk `.` and `..` are skipped: `Vfs::readdir` makes both.
    fn readdir(
        &self,
        cx: &mut OpCx<'_>,
        dir: &Inode,
        cookie: u64,
        out: &mut Dirent,
    ) -> Result<Option<u64>, FsError> {
        with_slot(vol_of(cx), |v, d| {
            let (w, _) = words(dir.handle())?;
            let mut c = cookie;
            let mut n = Node::EMPTY;
            loop {
                let Some(next) = v.readdir(d, w.first_clu, c, &mut n)? else {
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

    fn sync(&self, cx: &mut OpCx<'_>) -> Result<(), FsError> {
        sync(vol_of(cx))
    }

    fn evict(&self, cx: &mut OpCx<'_>, ino: &Inode) -> Result<(), FsError> {
        with_slot(vol_of(cx), |v, d| {
            let (w, linked) = words(ino.handle())?;
            if linked {
                return Ok(());
            }
            Ok(v.free_chain(d, w.first_clu)?)
        })
    }
}

/// Remove `name` from `dir`, a directory when `rmdir`.
fn remove(id: u8, dir: &Inode, name: &[u8], rmdir: bool) -> Result<(), FsError> {
    with_slot(id, |v, d| {
        let (w, _) = words(dir.handle())?;
        v.unlink(d, w.first_clu, name, rmdir)?;
        Ok(())
    })
}

/// Write `buf` at `off`, or at the inode's size when `append` is set;
/// the count written and the position written at. The words are stored
/// back even when the write fails: the chain may have grown.
fn write_at(
    id: u8,
    h: InodeHandle,
    off: u64,
    append: bool,
    buf: &[u8],
) -> Result<(usize, u64), FsError> {
    with_slot(id, |v, d| {
        let (mut w, linked) = words(h)?;
        let r = v.write_ino(d, &mut w, linked, off, append, buf);
        store(h, &w)?;
        Ok(r?)
    })
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

/// The words of the inode `h` names as `Vfs` holds them now, and whether
/// it still has its dirent. Called inside the busy section.
fn words(h: InodeHandle) -> Result<(FatInode, bool), FsError> {
    let w = fs_init::with(|vfs| vfs.inode_words(h))?;
    Ok((
        FatInode {
            dir_clu: w.key[0],
            dir_off: w.key[1],
            first_clu: w.private[0] as u32,
            size: w.size,
            kind: w.kind,
        },
        w.nlink != 0,
    ))
}

/// Store a FAT inode's words back into its `Vfs` inode, inside the busy
/// section they were read in.
fn store(h: InodeHandle, w: &FatInode) -> Result<(), FsError> {
    let private = [u64::from(w.first_clu), dirent_word(w.dir_clu, w.dir_off)];
    fs_init::with(|vfs| vfs.set_inode_words(h, private, w.size))
}

/// FAT32 registration for volume `vol`: its superblock's private words
/// are `[vol, 0]`, and its root inode's `[root_clu, 0]` from [`ROOT_CLU`].
pub struct FatFs {
    vol: u8,
}

static FAT_FS: [FatFs; MAX_VOLS] = [FatFs { vol: 0 }, FatFs { vol: 1 }];
/// Each volume's root cluster, stored before the volume is mounted.
static ROOT_CLU: [AtomicU32; MAX_VOLS] = [AtomicU32::new(0), AtomicU32::new(0)];

impl FileSystem for FatFs {
    fn name(&self) -> &'static str {
        "fat32"
    }

    fn fstype(&self) -> FsType {
        FsType::Fat
    }

    fn ops(&'static self) -> Option<&'static dyn InodeOps> {
        Some(&FatOps)
    }

    fn fill_super(&self, cx: &mut OpCx<'_>) -> Result<InodeInfo, FsError> {
        *cx.private = [u64::from(self.vol), 0];
        let root_clu = ROOT_CLU
            .get(self.vol as usize)
            .ok_or(FsError::Io)?
            .load(Ordering::Acquire);
        Ok(InodeInfo {
            key: [0, 0, 0],
            ino: fat::ROOT_INO,
            kind: InodeKind::Dir,
            mode: S_IFDIR_MODE,
            nlink: 2,
            size: 0,
            atime: cx.now,
            mtime: cx.now,
            ctime: cx.now,
            private: [u64::from(root_clu), 0],
        })
    }

    /// Record the superblock and route `at` to the volume, for the path
    /// syscalls' walk.
    fn on_mount(&self, cx: &mut OpCx<'_>, at: &[u8]) {
        let vol = vol_of(cx);
        if let Some(s) = SLOTS.get(vol as usize) {
            s.sb.store(cx.sb, Ordering::Release);
        }
        if at != b"/" && register_mnt(vol, at).is_err() {
            crate::klog!(
                vibeos::log::Level::Warn,
                "vibeOS: fat: no route for a mount"
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
                    "vibeOS: fat: sync at umount failed"
                );
            }
            drop_slot(vol);
        }
    }
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
    let mut io = Io {
        back: Media::Initrd,
    };
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
        *SLOTS[0].back.get() = Media::Initrd;
    }
    SLOTS[0].used.store(true, Ordering::Release);
    SLOTS[0].busy.store(false, Ordering::Release);
    NVOL.store(1, Ordering::Release);
    ROOT_CLU[VOL_INITRD as usize].store(root_clu, Ordering::Release);
    let root = fs_init::api().mount_root(&FAT_FS[VOL_INITRD as usize], None, false);
    LIVE.store(root.is_ok(), Ordering::Release);
}

/// Walk `path` on volume `id` and count a reference to the `Vfs` inode it
/// names, with the volume held: busy flag first, then the VFS lock, never
/// the reverse. A cached inode keeps its words; the walk's dirent never
/// overwrites them. For the path syscalls until ROADMAP §10.4 routes
/// them through `Vfs`.
pub fn walk_iget(id: u8, path: &[u8]) -> Result<InodeRef, FsError> {
    let sb = sb_of(id).ok_or(FsError::Io)?;
    with_slot(id, |v, d| {
        let n = v.walk(d, path)?;
        fs_init::with(|vfs| vfs.iget_key(sb, &node_info(&n)))
    })
}

pub fn sync(id: u8) -> Result<(), FsError> {
    with_slot(id, |v, d| Ok(v.sync(d)?))
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
    SLOTS[i].sb.store(NO_SB, Ordering::Release);
    drop_busy(id);
    recount();
}

/// Mount the FAT volume on block device `name` on `at`. A device `Vfs`
/// already mounts shares its superblock (`Busy` when `ro` differs);
/// otherwise the volume gets a slot, which is dropped again when `Vfs`
/// reports that another mount of the device won the race.
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
        let fs = FAT_FS.get(vol as usize).ok_or(FsError::Io)?;
        api.mount_fs(None, at.as_bytes(), fs, dev, ro)?;
        return Ok(vol);
    }
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
    recount();
    ROOT_CLU[id as usize].store(root_clu, Ordering::Release);
    match api.mount_fs(None, at.as_bytes(), &FAT_FS[id as usize], dev, ro) {
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

const _: () = {
    assert!(INITRD_BYTES.is_multiple_of(SEC));
    assert!(MAX_PATH >= MNT_PATH);
};
