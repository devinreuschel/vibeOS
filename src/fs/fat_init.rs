//! FAT volumes: initrd + block-backed mounts. ROADMAP §8.2–8.3 / §8.6.
//!
//! Each volume is an instance ([`FatVolume`], DESIGN §12.1 rule 1) on the
//! heap: the initrd's is owned by the root superblock, a device's by its
//! block registry entry (`blockdev_init::set_holder`), and the superblock
//! that shows it holds a reference. Each volume is one `BlockingMutex`
//! (DESIGN §2.1's level 4) that owns it, held across its I/O and taken
//! with a plain `lock()`: contention waits and never fails an operation.
//! `sync` uses cache/device Flush (DESIGN §10.2).
//!
//! [`FatOps`] is the one way to a volume's files: `Vfs`'s File API calls
//! it with the VFS lock dropped, so it waits for the volume lock. A FAT
//! inode is the `Vfs` inode keyed by its dirent location; its first
//! cluster and size are its slot's words (`Inode::words`), which only FAT
//! reads and writes, under the volume lock and with no VFS lock: FAT
//! never takes the VFS lock under a volume. FAT resolves one name per
//! `InodeOps::lookup` and never sees `.` or `..`, which `Vfs` resolves
//! itself.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use vibeos::block::BlockError;
use vibeos::block::blockdev::BlockRef;
use vibeos::dev::Instance;
use vibeos::fat::{self, Disk, FatError, FatInode, FatVol, Node, SEC};
use vibeos::fs::{
    Dirent, FileSystem, FsError, FsType, Inode, InodeInfo, InodeKind, InodeOps, InodeWords, Key,
    MAX_PATH, Name, OpCx, RenameSeen, S_IFDIR_MODE, S_IFREG_MODE, WalkBase,
};
use vibeos::kalloc::TryBox;
use vibeos::lock::RANK_DEVICE;

use crate::block::blockdev_init;
use crate::fs_init;
use crate::sync::blocking_init::BlockingMutex;
use crate::sync_init::SpinMutex;

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

/// One FAT volume, behind its lock. The volume is mounted in place on the
/// heap (`FatVol::mount_in`) before the lock wraps its box, and never
/// moved: it holds its cluster buffer and FAT cache, too large for a
/// kernel stack (DESIGN §4.5).
pub(crate) struct FatVolume {
    media: Media,
    /// Cleared, under the lock, when the volume is retired: every later op
    /// fails with `Io`.
    pub(super) used: AtomicBool,
    /// The root directory's first cluster, from the BPB at mount.
    root_clu: AtomicU32,
    /// The `Vfs` superblock the volume is mounted as, [`NO_SB`] until then.
    sb: AtomicU8,
    vol: BlockingMutex<TryBox<FatVol>>,
}

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
    let va = r.start.checked_add(crate::paging_init::hhdm_offset())?;
    let last = r.end.checked_sub(1)?;
    let mapped = |pa: u64| {
        let va = pa.checked_add(crate::paging_init::hhdm_offset())?;
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
    /// The device's logical block size, so a FAT mount on a disk of
    /// 4 KiB blocks is refused with `Inval` (`FatVol::mount_in`), as
    /// Linux refuses a 512-byte-sector volume there, rather than failing
    /// its first 512-byte read with `Io`.
    fn sector_size(&self) -> u32 {
        match self.back {
            Media::Initrd => SEC as u32,
            Media::Dev(r) => r.logical_block_size().unwrap_or(0),
        }
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
            Media::Dev(r) => {
                #[cfg(feature = "kernel_tests")]
                crate::fs::ktest::blk_request_hook();
                r.read(u64::from(lba), buf).map_err(fat_io_err)
            }
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
                crate::fs::ktest::blk_request_hook();
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

/// Run `f` on volume `v` under its lock, waiting for the holder; `Io`
/// once the volume is retired. `FatVol::now` is the wall clock
/// (`fs_init::now`) for what `f` stamps. Never under the VFS lock.
fn with_vol<R>(
    v: &FatVolume,
    f: impl FnOnce(&mut FatVol, &mut Io) -> Result<R, FsError>,
) -> Result<R, FsError> {
    let mut g = v.vol.lock();
    // Acquire: pairs with the Release store in `drop_slot`.
    if !v.used.load(Ordering::Acquire) {
        return Err(FsError::Io);
    }
    g.now = fs_init::now();
    let mut io = Io { back: &v.media };
    f(&mut g, &mut io)
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
/// `[first_clu, (dir_clu << 32) | dir_off]`, its dirent, which a rename
/// moves and makes [`NO_DIRENT`] for the file it replaces before `Vfs`
/// re-keys or unlinks either. Every op runs with the VFS
/// lock dropped and reads the inode's size and words from its slot's
/// words inside the busy section, never from the copy it is handed.
pub struct FatOps;

/// The volume the superblock of `cx` shows.
fn vol_of<'a>(cx: &OpCx<'a>) -> Result<&'a FatVolume, FsError> {
    as_fat(cx.vol.ok_or(FsError::Io)?)
}

impl InodeOps for FatOps {
    /// FAT compares names without regard to case, so the dentry cache
    /// does too: `/VIBE` finds the `vibe` dentry a mount is on.
    fn name_eq(&self, cached: &[u8], asked: &[u8]) -> bool {
        vibeos::fs::fat::eq_ci(cached, asked)
    }

    fn lookup(&self, cx: &mut OpCx<'_>, dir: &Inode, name: &[u8]) -> Result<InodeInfo, FsError> {
        with_vol(vol_of(cx)?, |v, d| {
            let (w, _) = words(dir)?;
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
            InodeKind::Lnk | InodeKind::Chr | InodeKind::Blk => return Err(FsError::Perm),
        };
        with_vol(vol_of(cx)?, |v, d| {
            let (w, _) = words(dir)?;
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
        seen: RenameSeen,
    ) -> Result<Option<Key>, FsError> {
        let moved = with_vol(vol_of(cx)?, |v, d| {
            let (o, _) = words(odir)?;
            let (n, _) = words(ndir)?;
            let src = key_at(v, d, o.first_clu, oname)?;
            let tgt = key_at(v, d, n.first_clu, nname)?;
            seen.check(src, tgt)?;
            let m = v.rename(d, o.first_clu, oname, n.first_clu, nname)?;
            // `Vfs` re-keys the moved inode and unlinks the replaced one
            // only after this returns, and a write on either can take the
            // volume lock first: the moved inode's words name its new
            // dirent, and the replaced one's none, since its slot now holds
            // the moved file's entry.
            if let Some(w) = seen.src_words {
                set_dirent(w, dirent_word(m.to.0, m.to.1));
            }
            if m.replaced.is_some()
                && let Some(w) = seen.tgt_words
            {
                set_dirent(w, NO_DIRENT);
            }
            Ok((m.from != m.to).then_some([m.to.0, m.to.1, 0]))
        })?;
        #[cfg(feature = "kernel_tests")]
        crate::fs::ktest::fat_rename_hook();
        Ok(moved)
    }

    fn read(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        let vol = vol_of(cx)?;
        with_vol(vol, |v, d| {
            #[cfg(feature = "kernel_tests")]
            if matches!(vol.media, Media::Dev(_)) {
                crate::fs::ktest::fat_read_hook();
            }
            let (w, _) = words(ino)?;
            v.read_ino(d, &w, off, buf)
        })
    }

    fn write(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &[u8],
    ) -> Result<usize, FsError> {
        write_at(vol_of(cx)?, ino, off, false, buf).map(|(n, _)| n)
    }

    /// The size `O_APPEND` writes at is read in the write's busy section.
    fn write_append(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        buf: &[u8],
    ) -> Result<(usize, u64), FsError> {
        write_at(vol_of(cx)?, ino, 0, true, buf)
    }

    fn truncate(&self, cx: &mut OpCx<'_>, ino: &mut Inode, size: u64) -> Result<(), FsError> {
        let ino: &Inode = ino;
        with_vol(vol_of(cx)?, |v, d| {
            let (mut w, linked) = words(ino)?;
            let r = v.truncate_ino(d, &mut w, linked, size);
            store(ino, &w)?;
            r
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
            let (w, _) = words(dir)?;
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

    /// After the superblock's last mount, which is the last to show the
    /// volume (one superblock per volume), and its sync: retire the volume
    /// and take it from its device's entry. The superblock's own reference
    /// goes when its slot is freed.
    fn release(&self, cx: &mut OpCx<'_>) {
        let Ok(vol) = vol_of(cx) else {
            return;
        };
        drop_slot(vol);
        if let Media::Dev(r) = &vol.media {
            drop(blockdev_init::take_holder(r));
        }
    }

    fn evict(&self, cx: &mut OpCx<'_>, ino: &Inode) -> Result<(), FsError> {
        with_vol(vol_of(cx)?, |v, d| {
            let (w, linked) = words(ino)?;
            if linked {
                return Ok(());
            }
            v.free_chain(d, w.first_clu)
        })
    }
}

/// Remove `name` from `dir`, a directory when `rmdir`.
fn remove(vol: &FatVolume, dir: &Inode, name: &[u8], rmdir: bool) -> Result<(), FsError> {
    with_vol(vol, |v, d| {
        let (w, _) = words(dir)?;
        v.unlink(d, w.first_clu, name, rmdir)?;
        Ok(())
    })
}

/// Write `buf` at `off`, or at the inode's size when `append` is set;
/// the count written and the position written at. The words are stored
/// back even when the write fails: the chain may have grown.
fn write_at(
    vol: &FatVolume,
    ino: &Inode,
    off: u64,
    append: bool,
    buf: &[u8],
) -> Result<(usize, u64), FsError> {
    with_vol(vol, |v, d| {
        let (mut w, linked) = words(ino)?;
        let r = v.write_ino(d, &mut w, linked, off, append, buf);
        store(ino, &w)?;
        r
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

/// The key of the file `name` names in the directory at cluster `dir`,
/// `None` when it names none: what a rename checks its walks against.
fn key_at(v: &mut FatVol, d: &mut Io, dir: u32, name: &[u8]) -> Result<Option<Key>, FsError> {
    match v.lookup(d, dir, name) {
        Ok(n) => Ok(Some(node_info(&n).key)),
        Err(FsError::NotFound) => Ok(None),
        Err(e) => Err(e),
    }
}

fn dirent_word(dir_clu: u32, dir_off: u32) -> u64 {
    (u64::from(dir_clu) << 32) | u64::from(dir_off)
}

/// The dirent word of an inode whose dirent a rename gave to another
/// file ([`FatOps::rename`]): no cluster is `u32::MAX`.
const NO_DIRENT: u64 = u64::MAX;

/// Point inode words `w` at dirent word `at`, keeping its first cluster;
/// inside the volume section, where every change to FAT's words is made.
fn set_dirent(w: &InodeWords, at: u64) {
    w.set_private([w.private()[0], at]);
}

/// The words of inode `ino` as its slot holds them now
/// ([`Inode::words`]), and whether it still has its dirent: its kind from
/// the op's copy, and its dirent, first cluster, size and link count from
/// the slot's words, whose dirent a rename moves before `Vfs` re-keys
/// the copy. Called inside the volume section; no VFS lock.
fn words(ino: &Inode) -> Result<(FatInode, bool), FsError> {
    let w = ino.words()?;
    let [first, at] = w.private();
    Ok((
        FatInode {
            dir_clu: (at >> 32) as u32,
            dir_off: at as u32,
            first_clu: first as u32,
            size: w.size(),
            kind: ino.kind,
        },
        w.nlink() != 0 && at != NO_DIRENT,
    ))
}

/// Store a FAT inode's first cluster, dirent and size into its slot's
/// words, inside the volume section they were read in.
fn store(ino: &Inode, w: &FatInode) -> Result<(), FsError> {
    let words = ino.words()?;
    words.set_private([u64::from(w.first_clu), dirent_word(w.dir_clu, w.dir_off)]);
    words.set_size(w.size);
    Ok(())
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

    fn max_bytes(&self) -> u64 {
        fat::MAX_FILE_SIZE
    }

    fn fill_super(&self, cx: &mut OpCx<'_>) -> Result<InodeInfo, FsError> {
        // Acquire: pairs with nothing; set once when the volume is built.
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

    /// Record the superblock, and `at` and the volume in the mount table
    /// (`MNTS`).
    fn on_mount(&self, cx: &mut OpCx<'_>, at: &[u8]) {
        if let Ok(v) = vol_of(cx) {
            // Release: pairs with nothing; nothing reads it yet.
            v.sb.store(cx.sb, Ordering::Release);
        }
        if at != b"/" && register_mnt(cx.vol.cloned(), at).is_err() {
            crate::klog!(
                vibeos::log::Level::Warn,
                "vibeOS: fat: no mount-table slot for a mount"
            );
        }
    }

    /// Drop `at`'s mount-table entry. The superblock's last unmount then
    /// syncs it and releases its volume through [`FatOps`] (`Vfs`'s
    /// unmount runs `sync`, then `release`).
    fn on_umount(&self, _cx: &mut OpCx<'_>, at: &[u8], _last: bool) {
        drop(unregister_mnt(at));
    }
}

pub fn live() -> bool {
    // Acquire: pairs with the Release stores in `init`.
    LIVE.load(Ordering::Acquire)
}

/// A new volume instance on `media`, mounted in place: the heap copy of a
/// fresh volume is this thread's alone until the instance is shared.
fn new_volume(media: Media) -> Result<Instance, FsError> {
    let mut fv = crate::fs::boxed_copy(&FAT_VOL_INIT).map_err(|_| FsError::NoMem)?;
    fv.mount_in(&mut Io { back: &media })?;
    let root = fv.info.root_clus;
    vibeos::dev::instance(FatVolume {
        media,
        used: AtomicBool::new(true),
        root_clu: AtomicU32::new(root),
        sb: AtomicU8::new(NO_SB),
        vol: BlockingMutex::new(fv),
    })
    .map_err(|_| FsError::NoMem)
}

/// A used volume that nothing mounts or reaches, for the volume-lock test
/// `fs_drop_slot_waits_for_holder` (test-only).
#[cfg(feature = "kernel_tests")]
pub(super) fn spare_volume() -> Result<Instance, FsError> {
    let fv = crate::fs::boxed_copy(&FAT_VOL_INIT).map_err(|_| FsError::NoMem)?;
    vibeos::dev::instance(FatVolume {
        media: Media::Initrd,
        used: AtomicBool::new(true),
        root_clu: AtomicU32::new(0),
        sb: AtomicU8::new(NO_SB),
        vol: BlockingMutex::new(fv),
    })
    .map_err(|_| FsError::NoMem)
}

/// Run `f` holding volume `v`'s lock, as another holder would
/// (test-only: `fs_drop_slot_waits_for_holder`).
#[cfg(feature = "kernel_tests")]
pub(super) fn hold<R>(v: &FatVolume, f: impl FnOnce() -> R) -> R {
    let _g = v.vol.lock();
    f()
}

/// Mount the initrd module as the root, or leave `LIVE` false when Limine
/// loaded none (or the physmap does not cover it) and `fs_init` mounts the
/// ramfs root. The root superblock owns the initrd's volume.
pub fn init() {
    let Some((va, len)) = initrd_span() else {
        // Release: pairs with the Acquire load in `live`.
        LIVE.store(false, Ordering::Release);
        return;
    };
    with_initrd(|span| *span = Some(Span { va, len }));
    let Ok(vol) = new_volume(Media::Initrd) else {
        // Release: pairs with the Acquire load in `live`.
        LIVE.store(false, Ordering::Release);
        return;
    };
    let root = fs_init::api().mount_root(&FAT_FS, None, false, Some(vol));
    // Release: pairs with the Acquire load in `live`.
    LIVE.store(root.is_ok(), Ordering::Release);
}

/// Whether the directory `dirs` names from the root of volume `vol`
/// holds `name`, compared as FAT compares names (without regard to case):
/// one `FatVol::lookup` per name under the volume lock (test-only:
/// `fat_initrd_dev_no_null`).
#[cfg(feature = "kernel_tests")]
pub fn ktest_dir_has(vol: &Instance, dirs: &[&[u8]], name: &[u8]) -> Result<bool, FsError> {
    with_vol(as_fat(vol)?, |fv, d| {
        let mut clu = fv.info.root_clus;
        for dn in dirs {
            let n = fv.lookup(d, clu, dn)?;
            if n.kind != InodeKind::Dir {
                return Err(FsError::NotDir);
            }
            clu = n.clu;
        }
        match fv.lookup(d, clu, name) {
            Ok(_) => Ok(true),
            Err(FatError::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    })
}

/// The size and bytes of file `name` in the root directory of the root's
/// FAT volume, read through its dirent on disk rather than an inode
/// (test-only: `fat_rename_racing_writes`); `buf` takes what fits.
#[cfg(feature = "kernel_tests")]
pub fn ktest_root_file(name: &[u8], buf: &mut [u8]) -> Result<(u64, usize), FsError> {
    let root = root_volume()?;
    with_vol(as_fat(&root)?, |v, d| {
        let n = v.lookup(d, v.info.root_clus, name)?;
        let got = v.read_ino(d, &FatInode::of_node(&n), 0, buf)?;
        Ok((u64::from(n.size), got))
    })
}

pub fn sync(vol: &FatVolume) -> Result<(), FsError> {
    with_vol(vol, |v, d| v.sync(d))
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

/// The root superblock's FAT volume: the initrd's (test-only).
#[cfg(feature = "kernel_tests")]
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

/// Take `p`'s mount-table entry out; the caller drops its volume reference unlocked.
fn unregister_mnt(p: &[u8]) -> Option<Instance> {
    let mut g = MNTS.lock();
    let m = g
        .iter_mut()
        .find(|m| m.used && m.path.get(..m.len as usize) == Some(p))?;
    m.used = false;
    m.len = 0;
    m.vol.take()
}

/// Retire volume `vol`: clear it and its `used` flag under its lock,
/// waiting for any holder, so every later op fails.
pub(super) fn drop_slot(vol: &FatVolume) {
    let mut g = vol.vol.lock();
    g.clear();
    // Release: pairs with the Acquire load in `with_vol`.
    vol.used.store(false, Ordering::Release);
    // Release: pairs with nothing; nothing reads it yet.
    vol.sb.store(NO_SB, Ordering::Release);
}

/// Mount the FAT volume on block device `name` on `at`. A device whose
/// entry holds a FAT volume shares it, and `Vfs` shares its superblock
/// (`Busy` when `ro` differs); one holding another filesystem's volume is
/// `Busy`. Otherwise the volume is built, becomes the entry's holder, and
/// is taken back if the mount fails.
#[cfg(feature = "kernel_tests")]
pub fn mount_dev(name: &str, at: &str, ro: bool) -> Result<(), FsError> {
    mount_dev_at(None, name, at, ro)
}

/// [`mount_dev`] on `at` from walk base `base`.
pub fn mount_dev_at(base: Option<WalkBase>, name: &str, at: &str, ro: bool) -> Result<(), FsError> {
    let r = blockdev_init::lookup(name.as_bytes()).ok_or(FsError::NotFound)?;
    let dev = Some(r.id());
    let api = fs_init::api();
    if let Some(h) = blockdev_init::holder(&r) {
        as_fat(&h).map_err(|_| FsError::Busy)?;
        return api
            .mount_fs(base, at.as_bytes(), &FAT_FS, dev, ro, Some(h))
            .map(|_| ());
    }
    let vol = new_volume(Media::Dev(r.clone()))?;
    blockdev_init::set_holder(&r, vol.clone()).map_err(|e| match e {
        BlockError::Exists => FsError::Busy,
        _ => FsError::Io,
    })?;
    match api.mount_fs(base, at.as_bytes(), &FAT_FS, dev, ro, Some(vol.clone())) {
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
