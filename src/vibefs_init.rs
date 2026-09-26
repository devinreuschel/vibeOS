//! vibefs volumes. ROADMAP §8.5 / docs/VIBEFS.md.
//!
//! BSS image by default. Busy flag (not IRQ-off mutex) across I/O
//! (DESIGN §2.1). `sync` uses disk `Flush`.
//!
//! [`VibeOps`] runs under the VFS lock, which until ROADMAP §10.4's
//! VFS-lock box is an IRQ-off spinlock, so it never waits: it takes the
//! busy flag with one compare-and-swap and returns `Busy` when the volume
//! is busy or sits on a block device. The File API takes the flag first
//! and the VFS lock second, never the reverse.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use vibeos::fs::{
    Dirent, FsError, FsType, Inode, InodeInfo, InodeKind, InodeOps, MAX_PATH, Name, OpCx, S_IFMT,
    VibeFs,
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
enum Back {
    Mem,
    Dev(u32),
}

struct Slot {
    vol: UnsafeCell<Vol>,
    back: UnsafeCell<Back>,
    used: AtomicBool,
    busy: AtomicBool,
}

unsafe impl Sync for Slot {}

impl Slot {
    const fn empty() -> Self {
        Self {
            vol: UnsafeCell::new(Vol::new()),
            back: UnsafeCell::new(Back::Mem),
            used: AtomicBool::new(false),
            busy: AtomicBool::new(false),
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
    back: Back,
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
            Back::Mem => (IMAGE_BYTES / BLOCK) as u32,
            Back::Dev(cache_init::DEV_RAM0) => {
                let bs = block_init::logical_block_size();
                let n = block_init::capacity_sectors();
                if bs == 0 {
                    0
                } else {
                    ((n * bs as u64) / BLOCK as u64) as u32
                }
            }
            Back::Dev(cache_init::DEV_VDA) => {
                let bs = virtio_blk_init::logical_block_size();
                let n = virtio_blk_init::capacity_sectors();
                if bs == 0 {
                    0
                } else {
                    (n.saturating_mul(bs as u64) / BLOCK as u64) as u32
                }
            }
            Back::Dev(_) => 0,
        }
    }

    fn read_block(&mut self, bno: u32, buf: &mut [u8; BLOCK]) -> Result<(), Error> {
        match self.back {
            Back::Mem => {
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
            Back::Dev(dev) => {
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
            Back::Mem => {
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
            Back::Dev(dev) => {
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
            Back::Mem => Ok(()),
            Back::Dev(dev) => cache_init::flush(dev).map_err(|_| Error::Io),
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
fn with_grabbed<R, E>(id: u8, f: impl FnOnce(&mut Vol, &mut Io) -> Result<R, E>) -> Result<R, E> {
    let i = id as usize;
    // SAFETY: the busy flag of `SLOTS[i]`, which the caller took through
    // `vibefs_init::grab` or `vibefs_init::grab_now` and which is dropped
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

/// Run `f` on volume `id` without waiting, as [`VibeOps`] must under the
/// VFS lock: `Busy` when the volume is busy or on a block device.
fn with_slot_now<R>(
    id: u8,
    f: impl FnOnce(&mut Vol, &mut Io) -> Result<R, Error>,
) -> Result<R, FsError> {
    grab_now(id)?;
    with_grabbed(id, |v, io| match io.back {
        Back::Mem => f(v, io).map_err(Error::to_fs),
        Back::Dev(_) => Err(FsError::Busy),
    })
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

/// vibefs's [`InodeOps`], behind a vibefs superblock's `ops` pointer. The
/// superblock's private words are `[vol, 0]`.
pub struct VibeOps;

fn vol_of(cx: &OpCx<'_>) -> u8 {
    cx.private[0] as u8
}

impl InodeOps for VibeOps {
    fn lookup(&self, cx: &mut OpCx<'_>, dir: &Inode, name: &[u8]) -> Result<InodeInfo, FsError> {
        with_slot_now(vol_of(cx), |v, d| {
            Ok(node_info(&v.lookup(d, dir.key[0], name)?))
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
        with_slot_now(vol_of(cx), |v, d| {
            Ok(node_info(
                &v.create(d, dir.key[0], name, kind, mode, target)?,
            ))
        })
    }

    fn unlink(&self, cx: &mut OpCx<'_>, dir: &mut Inode, name: &[u8]) -> Result<(), FsError> {
        with_slot_now(vol_of(cx), |v, d| v.unlink(d, dir.key[0], name, false))
    }

    fn read(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        with_slot_now(vol_of(cx), |v, d| v.read(d, ino.key[0], off, buf))
    }

    fn write(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &[u8],
    ) -> Result<usize, FsError> {
        let n = with_slot_now(vol_of(cx), |v, d| v.write(d, ino.key[0], off, buf))?;
        let end = off.checked_add(n as u64).ok_or(FsError::FileTooBig)?;
        ino.size = ino.size.max(end);
        Ok(n)
    }

    fn truncate(&self, cx: &mut OpCx<'_>, ino: &mut Inode, size: u64) -> Result<(), FsError> {
        with_slot_now(vol_of(cx), |v, d| v.truncate(d, ino.key[0], size))?;
        ino.size = size;
        Ok(())
    }

    /// `.` and `..` are skipped should vibefs list them: `Vfs::readdir`
    /// makes both.
    fn readdir(
        &self,
        cx: &mut OpCx<'_>,
        dir: &Inode,
        cookie: u64,
        out: &mut Dirent,
    ) -> Result<Option<u64>, FsError> {
        let r = with_slot_now(vol_of(cx), |v, d| {
            let mut c = cookie;
            let mut n = Node::EMPTY;
            loop {
                let Some(next) = v.readdir(d, dir.key[0], c, &mut n)? else {
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

    fn readlink(&self, cx: &mut OpCx<'_>, ino: &Inode, buf: &mut [u8]) -> Result<usize, FsError> {
        with_slot_now(vol_of(cx), |v, d| v.readlink(d, ino.key[0], buf))
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
    IMAGE.with(|buf| buf.fill(0));
    let mut io = Io { back: Back::Mem };
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
        *SLOTS[0].back.get() = Back::Mem;
    }
    SLOTS[0].used.store(true, Ordering::Release);
    SLOTS[0].busy.store(false, Ordering::Release);
    NVOL.store(1, Ordering::Release);
    LIVE.store(true, Ordering::Release);
}

pub fn walk(id: u8, path: &[u8]) -> Result<Node, FsError> {
    with_slot(id, |v, d| v.walk(d, path)).map_err(Error::to_fs)
}

#[allow(dead_code)]
pub fn lookup(id: u8, dir: u32, name: &[u8]) -> Result<Node, FsError> {
    with_slot(id, |v, d| v.lookup(d, dir, name)).map_err(Error::to_fs)
}

pub fn readdir(id: u8, dir: u32, cookie: u64, out: &mut Node) -> Result<Option<u64>, FsError> {
    with_slot(id, |v, d| v.readdir(d, dir, cookie, out)).map_err(Error::to_fs)
}

pub fn read(id: u8, ino: u32, off: u64, buf: &mut [u8]) -> Result<usize, FsError> {
    with_slot(id, |v, d| v.read(d, ino, off, buf)).map_err(Error::to_fs)
}

/// Write `buf` at `off`, or at the inode's size when `append` is set;
/// the count written and the position written at.
pub fn write(
    id: u8,
    ino: u32,
    off: u64,
    append: bool,
    buf: &[u8],
) -> Result<(usize, u64), FsError> {
    with_slot(id, |v, d| {
        let pos = if append { v.file_size(ino)? } else { off };
        Ok((v.write(d, ino, pos, buf)?, pos))
    })
    .map_err(Error::to_fs)
}

/// The size of inode `ino`.
pub fn size(id: u8, ino: u32) -> Result<u64, FsError> {
    with_slot(id, |v, _| v.file_size(ino)).map_err(Error::to_fs)
}

pub fn create(
    id: u8,
    dir: u32,
    name: &[u8],
    kind: vibeos::fs::InodeKind,
    mode: u16,
    target: Option<&[u8]>,
) -> Result<Node, FsError> {
    with_slot(id, |v, d| v.create(d, dir, name, kind, mode, target)).map_err(Error::to_fs)
}

pub fn unlink(id: u8, dir: u32, name: &[u8], rmdir: bool) -> Result<(), FsError> {
    with_slot(id, |v, d| v.unlink(d, dir, name, rmdir)).map_err(Error::to_fs)
}

pub fn truncate(id: u8, ino: u32, new: u64) -> Result<(), FsError> {
    with_slot(id, |v, d| v.truncate(d, ino, new)).map_err(Error::to_fs)
}

pub fn rename(
    id: u8,
    src_dir: u32,
    src_name: &[u8],
    dst_dir: u32,
    dst_name: &[u8],
) -> Result<(), FsError> {
    with_slot(id, |v, d| v.rename(d, src_dir, src_name, dst_dir, dst_name)).map_err(Error::to_fs)
}

#[allow(dead_code)]
pub fn readlink(id: u8, ino: u32, buf: &mut [u8]) -> Result<usize, FsError> {
    with_slot(id, |v, d| v.readlink(d, ino, buf)).map_err(Error::to_fs)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn snapshot(id: u8, name: &[u8]) -> Result<(), FsError> {
    with_slot(id, |v, d| v.snapshot(d, name)).map_err(Error::to_fs)
}

pub fn sync(id: u8) -> Result<(), FsError> {
    with_slot(id, |v, d| v.sync(d)).map_err(Error::to_fs)
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
        let (tot, free, n) = v.df();
        Ok((FsType::Vibe, tot, free, n))
    })
    .map_err(Error::to_fs)
}

#[allow(dead_code)]
pub fn probe_dev(name: &str) -> bool {
    let back = match name {
        "ram0" => Back::Dev(cache_init::DEV_RAM0),
        "vda" => Back::Dev(cache_init::DEV_VDA),
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
    register_mnt(VOL_MEM, at)?;
    match fs_init::with(|v| {
        v.mount(
            None,
            at,
            &VibeFs {
                root_ino: ROOT_INO,
                vol: VOL_MEM,
                ops: Some(&VibeOps),
            },
        )
    }) {
        Ok(_) => Ok(VOL_MEM),
        Err(e) => {
            let _ = unregister_mnt(at);
            Err(e)
        }
    }
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
    if let Err(e) = register_mnt(id, at) {
        drop_slot(id);
        return Err(e);
    }
    match fs_init::with(|v| {
        v.mount(
            None,
            at,
            &VibeFs {
                root_ino: ROOT_INO,
                vol: id,
                ops: Some(&VibeOps),
            },
        )
    }) {
        Ok(_) => Ok(id),
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
            if v.fstype_at(p).ok() == Some(FsType::Vibe) {
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
    assert!(MAX_PATH >= MNT_PATH);
};

/// QEMU-kill workload. Marker `vibeOS: vibefs: wr N` is not a boot
/// contract line. Printed *before* the write+fsync so a kill can land
/// inside `write` / `Flush` (docs/VIBEFS.md §12).
#[cfg(feature = "vibefs_crash")]
pub fn crash_loop() -> ! {
    use crate::file_init;
    use crate::x86;
    use vibeos::fs::{O_CREAT, O_RDWR, O_TRUNC};

    let _ = file_init::mkdir("/crash", 0o755);
    let _ = file_init::vfs_attach("/crash");
    if mount_dev("vda", "/crash").is_err() {
        crate::marker!("vibeOS: vibefs: mount fail");
        x86::halt();
    }
    crate::marker!("vibeOS: vibefs: crash-ready");
    let mut i = 0u32;
    loop {
        crate::marker!("vibeOS: vibefs: wr {i}");
        if let Ok(fid) = file_init::open("/crash/w", O_RDWR | O_CREAT | O_TRUNC, 0o644) {
            let mut buf = [0u8; 300];
            let mut k = 0usize;
            while k < buf.len() {
                buf[k] = i.wrapping_add(k as u32) as u8;
                k += 1;
            }
            let _ = file_init::write(fid, &buf);
            let _ = file_init::close(fid);
        }
        let _ = file_init::sync_fs();
        i = i.wrapping_add(1);
        thread_init::yield_now();
    }
}
