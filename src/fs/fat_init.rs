//! FAT volumes: initrd + block-backed mounts. ROADMAP §8.2–8.3 / §8.6.
//!
//! Each volume is an instance ([`FatVolume`], DESIGN §12.1 rule 1) on the
//! heap: the initrd's is owned by the root superblock, a device's by its
//! block registry entry (`blockdev_init::set_holder`), and the superblock
//! that shows it holds a reference. A busy flag (not the IRQ-off mutex) is
//! held across I/O so RANK_DEVICE is not nested with the cache (DESIGN
//! §2.1 / #62 ACK). `sync` uses cache/device Flush (DESIGN §10.2).
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

use vibeos::block::BlockError;
use vibeos::block::blockdev::BlockRef;
use vibeos::dev::Instance;
use vibeos::fat::{self, Disk, FatError, FatInode, FatVol, Node, SEC};
use vibeos::fs::{
    Dirent, FileSystem, FsError, FsType, Inode, InodeHandle, InodeInfo, InodeKind, InodeOps,
    InodeRef, Key, MAX_PATH, Name, OpCx, S_IFDIR_MODE, S_IFREG_MODE,
};
use vibeos::kalloc::TryBox;
use vibeos::lock::RANK_DEVICE;

use crate::block::blockdev_init;
use crate::fs_init;
use crate::sync_init::SpinMutex;
use crate::thread_init;

/// A cache error as FAT sees it: a refused heap allocation stays
/// `NoMem` (ENOMEM), anything else is `Io`.
fn fat_io_err(e: BlockError) -> FatError {
    match e {
        BlockError::NoMem => FatError::NoMem,
        _ => FatError::Io,
    }
}
const MNT_MAX: usize = 2;
const MNT_PATH: usize = 64;

enum Media {
    Initrd,
    /// A registered block device; the volume's handle keeps it alive.
    Dev(BlockRef),
}

/// One FAT volume. Its `vol` cell belongs to the one thread holding
/// `busy`, or, before the instance is shared, to the path building it
/// (invariant I236). The volume is mounted in place (`FatVol::mount_in`),
/// never moved: it holds its cluster buffer and FAT cache, too large for a
/// kernel stack (DESIGN §4.5).
pub(crate) struct FatVolume {
    media: Media,
    pub(super) used: AtomicBool,
    pub(super) busy: AtomicBool,
    /// The root directory's first cluster, from the BPB at mount.
    root_clu: AtomicU32,
    /// The `Vfs` superblock the volume is mounted as, [`NO_SB`] until then.
    sb: AtomicU8,
    vol: UnsafeCell<TryBox<FatVol>>,
}

// SAFETY: invariant I236: one thread at a time reaches a volume's `vol`
// cell, through the busy flag `fs::fat_init::grab` takes, or the building
// path before the instance is shared, and its contents are `Send`
// (asserted below); established by `fs::fat_init::grab`.
unsafe impl Sync for FatVolume {}

const _: () = {
    const fn send<T: Send>() {}
    send::<FatVol>();
    send::<Media>();
};

/// A fresh volume's state, copied into each new instance in place.
static FAT_VOL_INIT: FatVol = FatVol::new();

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
/// The initrd module's bytes in the physmap, set once by [`init`]. Whoever
/// holds this lock is the one accessor of those bytes (`Io`'s
/// `Media::Initrd` arms); `None` while there is no initrd.
static INITRD: SpinMutex<Option<Span>> = SpinMutex::with_rank(None, RANK_DEVICE);

/// A module's physmap VA and length.
#[derive(Clone, Copy)]
pub(super) struct Span {
    va: u64,
    len: usize,
}

/// Run `f` on the initrd's span, holding the lock that owns its bytes.
pub(super) fn with_initrd<R>(f: impl FnOnce(&mut Option<Span>) -> R) -> R {
    let mut g = INITRD.lock();
    f(&mut g)
}

/// The physmap VA and length of the initrd module (`boot::BootInfo::initrd`),
/// only when the physmap maps all of it: below `paging_init::map_end`, or
/// above it through the leaves `paging_init::install` adds for modules. The
/// module is not usable RAM, so no frame allocator hands its frames out.
fn initrd_span() -> Option<(u64, usize)> {
    let r = crate::boot::info().initrd()?;
    let len = usize::try_from(r.end.checked_sub(r.start)?).ok()?;
    let va = r.start.checked_add(crate::paging_init::HHDM_BASE)?;
    let last = r.end.checked_sub(1)?;
    let mapped = |pa: u64| {
        let va = pa.checked_add(crate::paging_init::HHDM_BASE)?;
        let (got, _, _) = crate::paging_init::translate(vibeos::paging::VirtAddr(va))?;
        (got.as_u64() == pa).then_some(())
    };
    if len == 0 {
        return None;
    }
    if r.end > crate::paging_init::map_end() {
        // `install` maps a module's pages in order, so its first and last
        // page being there means all of it is.
        mapped(r.start)?;
        mapped(last)?;
    }
    Some((va, len))
}

/// Where sector `lba` lies in `span`: its VA, when all of it is inside.
fn sector_va(span: &Span, lba: u32) -> Result<u64, FatError> {
    let off = (lba as usize).checked_mul(SEC).ok_or(FatError::Inval)?;
    let end = off.checked_add(SEC).ok_or(FatError::Inval)?;
    if end > span.len {
        return Err(FatError::Io);
    }
    span.va.checked_add(off as u64).ok_or(FatError::Io)
}

static LIVE: AtomicBool = AtomicBool::new(false);

struct Io<'a> {
    back: &'a Media,
}

impl Disk for Io<'_> {
    fn sector_size(&self) -> u32 {
        SEC as u32
    }

    fn nsectors(&self) -> u32 {
        match self.back {
            Media::Initrd => with_initrd(|span| {
                span.map_or(0, |s| u32::try_from(s.len / SEC).unwrap_or(u32::MAX))
            }),
            Media::Dev(r) => r
                .capacity_sectors()
                .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX)),
        }
    }

    fn read(&mut self, lba: u32, buf: &mut [u8]) -> Result<(), FatError> {
        match self.back {
            Media::Initrd => with_initrd(|span| {
                let span = span.as_ref().ok_or(FatError::Io)?;
                let va = sector_va(span, lba)?;
                if buf.len() != SEC {
                    return Err(FatError::Io);
                }
                // SAFETY: the initrd's sector at `va` is `SEC` bytes of the
                // module, mapped by the physmap and never handed out as
                // RAM (`fs::fat_init::initrd_span`), inside the span
                // (`fs::fat_init::sector_va`); the `INITRD` lock held here
                // makes this the one accessor of those bytes, and `buf` is
                // `SEC` bytes of this thread's own memory, checked here.
                unsafe {
                    core::ptr::copy_nonoverlapping(va as *const u8, buf.as_mut_ptr(), SEC);
                }
                Ok(())
            }),
            Media::Dev(r) => r.read(u64::from(lba), buf).map_err(fat_io_err),
        }
    }

    fn write(&mut self, lba: u32, buf: &[u8]) -> Result<(), FatError> {
        match self.back {
            Media::Initrd => with_initrd(|span| {
                let span = span.as_ref().ok_or(FatError::Io)?;
                let va = sector_va(span, lba)?;
                if buf.len() != SEC {
                    return Err(FatError::Io);
                }
                // SAFETY: the initrd's sector at `va` is `SEC` writable
                // bytes of the module, mapped by the physmap and never
                // handed out as RAM (`fs::fat_init::initrd_span`), inside
                // the span (`fs::fat_init::sector_va`); the `INITRD` lock
                // held here makes this the one accessor of those bytes, and
                // `buf` is `SEC` bytes, checked here.
                unsafe {
                    core::ptr::copy_nonoverlapping(buf.as_ptr(), va as *mut u8, SEC);
                }
                Ok(())
            }),
            Media::Dev(r) => {
                #[cfg(feature = "kernel_tests")]
                crate::fs::ktest::on_cache_write();
                r.write(u64::from(lba), buf).map_err(fat_io_err)
            }
        }
    }

    fn flush(&mut self) -> Result<(), FatError> {
        match self.back {
            Media::Initrd => Ok(()),
            Media::Dev(r) => r.flush().map_err(fat_io_err),
        }
    }
}

/// Take `v`'s busy flag, waiting for its holder.
fn grab(v: &FatVolume) -> Result<(), FatError> {
    if !v.used.load(Ordering::Acquire) {
        return Err(FatError::Io);
    }
    let mut n = 0u32;
    while v
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
    if !v.used.load(Ordering::Acquire) {
        v.busy.store(false, Ordering::Release);
        return Err(FatError::Io);
    }
    Ok(())
}

fn drop_busy(v: &FatVolume) {
    v.busy.store(false, Ordering::Release);
}

/// Run `f` on volume `v`, whose busy flag the caller took, and drop the
/// flag.
fn with_grabbed<R>(
    v: &FatVolume,
    f: impl FnOnce(&mut FatVol, &mut Io) -> Result<R, FsError>,
) -> Result<R, FsError> {
    // SAFETY: invariant I236: the busy flag of `v`, which the caller took
    // and which is dropped only below, makes this thread the one accessor
    // of the volume's `vol` cell until `drop_busy`; established by
    // `fs::fat_init::grab`.
    let r = unsafe {
        let mut io = Io { back: &v.media };
        f(&mut *v.vol.get(), &mut io)
    };
    drop_busy(v);
    r
}

/// Run `f` on volume `v`, waiting for its busy flag. Never under the VFS
/// lock.
fn with_vol<R>(
    v: &FatVolume,
    f: impl FnOnce(&mut FatVol, &mut Io) -> Result<R, FsError>,
) -> Result<R, FsError> {
    grab(v)?;
    with_grabbed(v, f)
}

/// A volume not mounted in `Vfs`.
const NO_SB: u8 = u8::MAX;

/// The FAT volume instance `i` is; `Io` for any other.
fn as_fat(i: &Instance) -> Result<&FatVolume, FsError> {
    i.downcast_ref::<FatVolume>().ok_or(FsError::Io)
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

/// The volume the superblock of `cx` shows.
fn vol_of<'a>(cx: &OpCx<'a>) -> Result<&'a FatVolume, FsError> {
    as_fat(cx.vol.ok_or(FsError::Io)?)
}

impl InodeOps for FatOps {
    fn lookup(&self, cx: &mut OpCx<'_>, dir: &Inode, name: &[u8]) -> Result<InodeInfo, FsError> {
        with_vol(vol_of(cx)?, |v, d| {
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
        with_vol(vol_of(cx)?, |v, d| {
            let (w, _) = words(dir.handle())?;
            Ok(node_info(&v.create(d, w.first_clu, name, is_dir)?))
        })
    }

    /// Remove the dirent only: the victim's chain is freed by `evict` at
    /// its last put.
    fn unlink(&self, cx: &mut OpCx<'_>, dir: &mut Inode, name: &[u8]) -> Result<(), FsError> {
        remove(vol_of(cx)?, dir, name, false)
    }

    /// Remove the empty directory `name`; its cluster is freed by `evict`
    /// at its last put.
    fn rmdir(&self, cx: &mut OpCx<'_>, dir: &mut Inode, name: &[u8]) -> Result<(), FsError> {
        remove(vol_of(cx)?, dir, name, true)
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
        with_vol(vol_of(cx)?, |v, d| {
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
        with_vol(vol_of(cx)?, |v, d| {
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
        write_at(vol_of(cx)?, ino.handle(), off, false, buf).map(|(n, _)| n)
    }

    /// The size `O_APPEND` writes at is read in the write's busy section.
    fn write_append(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        buf: &[u8],
    ) -> Result<(usize, u64), FsError> {
        write_at(vol_of(cx)?, ino.handle(), 0, true, buf)
    }

    fn truncate(&self, cx: &mut OpCx<'_>, ino: &mut Inode, size: u64) -> Result<(), FsError> {
        let h = ino.handle();
        with_vol(vol_of(cx)?, |v, d| {
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
        with_vol(vol_of(cx)?, |v, d| {
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
        sync(vol_of(cx)?)
    }

    fn evict(&self, cx: &mut OpCx<'_>, ino: &Inode) -> Result<(), FsError> {
        with_vol(vol_of(cx)?, |v, d| {
            let (w, linked) = words(ino.handle())?;
            if linked {
                return Ok(());
            }
            Ok(v.free_chain(d, w.first_clu)?)
        })
    }
}

/// Remove `name` from `dir`, a directory when `rmdir`.
fn remove(vol: &FatVolume, dir: &Inode, name: &[u8], rmdir: bool) -> Result<(), FsError> {
    with_vol(vol, |v, d| {
        let (w, _) = words(dir.handle())?;
        v.unlink(d, w.first_clu, name, rmdir)?;
        Ok(())
    })
}

/// Write `buf` at `off`, or at the inode's size when `append` is set;
/// the count written and the position written at. The words are stored
/// back even when the write fails: the chain may have grown.
fn write_at(
    vol: &FatVolume,
    h: InodeHandle,
    off: u64,
    append: bool,
    buf: &[u8],
) -> Result<(usize, u64), FsError> {
    with_vol(vol, |v, d| {
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

/// FAT32's registration. A superblock holds its volume instance; its
/// private words are unused, and its root inode's are `[root_clu, 0]`.
pub struct FatFs;

static FAT_FS: FatFs = FatFs;

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
        let root_clu = vol_of(cx)?.root_clu.load(Ordering::Acquire);
        *cx.private = [0, 0];
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
        if let Ok(v) = vol_of(cx) {
            v.sb.store(cx.sb, Ordering::Release);
        }
        if at != b"/" && register_mnt(cx.vol.cloned(), at).is_err() {
            crate::klog!(
                vibeos::log::Level::Warn,
                "vibeOS: fat: no route for a mount"
            );
        }
    }

    /// Drop `at`'s route. After the superblock's last mount, which is the
    /// last to show the volume (one superblock per volume), sync the
    /// volume, retire it, and take it from its device's entry; the
    /// superblock's own reference goes when its slot is freed.
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
                "vibeOS: fat: sync of {} at umount failed: {}",
                mnt,
                e.as_str()
            );
        }
        if let Err(e) = drop_slot(vol) {
            crate::klog_ratelimited!(
                1000,
                vibeos::log::Level::Warn,
                "vibeOS: fat: volume of {} still held at umount, kept: {}",
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

pub fn live() -> bool {
    LIVE.load(Ordering::Acquire)
}

/// A new volume instance on `media`, mounted in place: the heap copy of a
/// fresh volume is this thread's alone until the instance is shared.
fn new_volume(media: Media) -> Result<Instance, FsError> {
    let fv = crate::fs::boxed_copy(&FAT_VOL_INIT).map_err(|_| FsError::NoMem)?;
    let mut v = FatVolume {
        media,
        used: AtomicBool::new(false),
        busy: AtomicBool::new(false),
        root_clu: AtomicU32::new(0),
        sb: AtomicU8::new(NO_SB),
        vol: UnsafeCell::new(fv),
    };
    let mut io = Io { back: &v.media };
    let fat = v.vol.get_mut();
    fat.mount_in(&mut io)?;
    let root = fat.info.root_clus;
    v.root_clu.store(root, Ordering::Release);
    v.used.store(true, Ordering::Release);
    vibeos::dev::instance(v).map_err(|_| FsError::NoMem)
}

/// A used volume that nothing mounts or reaches, for the busy-flag test
/// `fs_drop_slot_busy_keeps_slot` (test-only).
#[cfg(feature = "kernel_tests")]
pub(super) fn spare_volume() -> Result<Instance, FsError> {
    let fv = crate::fs::boxed_copy(&FAT_VOL_INIT).map_err(|_| FsError::NoMem)?;
    vibeos::dev::instance(FatVolume {
        media: Media::Initrd,
        used: AtomicBool::new(true),
        busy: AtomicBool::new(false),
        root_clu: AtomicU32::new(0),
        sb: AtomicU8::new(NO_SB),
        vol: UnsafeCell::new(fv),
    })
    .map_err(|_| FsError::NoMem)
}

/// Mount the initrd module as the root, or leave `LIVE` false when Limine
/// loaded none (or the physmap does not cover it) and `fs_init` mounts the
/// ramfs root. The root superblock owns the initrd's volume.
pub fn init() {
    let Some((va, len)) = initrd_span() else {
        LIVE.store(false, Ordering::Release);
        return;
    };
    with_initrd(|span| *span = Some(Span { va, len }));
    let Ok(vol) = new_volume(Media::Initrd) else {
        LIVE.store(false, Ordering::Release);
        return;
    };
    let root = fs_init::api().mount_root(&FAT_FS, None, false, Some(vol));
    LIVE.store(root.is_ok(), Ordering::Release);
}

/// Walk `path` on volume `vol` and count a reference to the `Vfs` inode it
/// names, with the volume held: busy flag first, then the VFS lock, never
/// the reverse. A cached inode keeps its words; the walk's dirent never
/// overwrites them. For the path syscalls until ROADMAP §10.4 routes
/// them through `Vfs`.
pub fn walk_iget(vol: &Instance, path: &[u8]) -> Result<InodeRef, FsError> {
    let v = as_fat(vol)?;
    let sb = v.sb.load(Ordering::Acquire);
    if sb == NO_SB {
        return Err(FsError::Io);
    }
    with_vol(v, |fv, d| {
        let n = fv.walk(d, path)?;
        fs_init::with(|vfs| vfs.iget_key(sb, &node_info(&n)))
    })
}

pub fn sync(vol: &FatVolume) -> Result<(), FsError> {
    with_vol(vol, |v, d| Ok(v.sync(d)?))
}

pub fn df(vol: &FatVolume) -> Result<(FsType, u64, u64, u32), FsError> {
    with_vol(vol, |v, _| {
        Ok((
            FsType::Fat,
            v.info.data_bytes(),
            v.free_bytes(),
            v.info.nclus,
        ))
    })
}

/// The root superblock's FAT volume: the initrd's.
pub fn root_volume() -> Result<Instance, FsError> {
    fs_init::with(|v| v.root().and_then(|p| v.volume_of(p)))
}

/// The mounted initrd's image bytes, from its BPB, and its free bytes;
/// `None` when it is not mounted (`initrd_module_sized`).
#[cfg(feature = "kernel_tests")]
pub fn initrd_geometry() -> Option<(u64, u64)> {
    let root = root_volume().ok()?;
    with_vol(as_fat(&root).ok()?, |v, _| {
        let bytes = u64::from(v.info.totsec).checked_mul(u64::from(v.info.bps));
        Ok(bytes.map(|b| (b, v.free_bytes())))
    })
    .ok()
    .flatten()
}

/// `(vol, strip)`: skip `strip` bytes of `path`; if nothing remains, walk
/// `"/"`. `None` is the root volume.
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
    (vol.cloned(), best)
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

/// Retire volume `vol`: clear it and its `used` flag, so every later op
/// fails. When `grab` cannot take its busy flag the volume is left as it
/// is, still owned by its holder, and the error is returned.
pub(super) fn drop_slot(vol: &FatVolume) -> Result<(), FatError> {
    grab(vol)?;
    // SAFETY: invariant I236: `grab` above took the volume's busy flag,
    // which is dropped only below; established by `fs::fat_init::grab`.
    unsafe {
        (*vol.vol.get()).clear();
    }
    vol.used.store(false, Ordering::Release);
    vol.sb.store(NO_SB, Ordering::Release);
    drop_busy(vol);
    Ok(())
}

/// Mount the FAT volume on block device `name` on `at`. A device whose
/// entry holds a FAT volume shares it, and `Vfs` shares its superblock
/// (`Busy` when `ro` differs); one holding another filesystem's volume is
/// `Busy`. Otherwise the volume is built, becomes the entry's holder, and
/// is taken back if the mount fails.
pub fn mount_dev(name: &str, at: &str, ro: bool) -> Result<(), FsError> {
    let r = blockdev_init::lookup(name.as_bytes()).ok_or(FsError::NotFound)?;
    let dev = Some(r.id());
    let api = fs_init::api();
    if let Some(h) = blockdev_init::holder(&r) {
        as_fat(&h).map_err(|_| FsError::Busy)?;
        return api
            .mount_fs(None, at.as_bytes(), &FAT_FS, dev, ro, Some(h))
            .map(|_| ());
    }
    let vol = new_volume(Media::Dev(r.clone()))?;
    blockdev_init::set_holder(&r, vol.clone()).map_err(|e| match e {
        BlockError::Exists => FsError::Busy,
        _ => FsError::Io,
    })?;
    match api.mount_fs(None, at.as_bytes(), &FAT_FS, dev, ro, Some(vol.clone())) {
        Ok(_) => Ok(()),
        Err(e) => {
            if !fs_init::with(|v| v.shows_volume(&vol)) {
                drop(blockdev_init::take_holder(&r));
            }
            Err(e)
        }
    }
}

const _: () = assert!(MAX_PATH >= MNT_PATH);
