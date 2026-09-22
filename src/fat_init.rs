//! FAT volumes: initrd + block-backed mounts. ROADMAP §8.2–8.3 / §8.6.
//!
//! The volume lives in BSS. A busy flag (not the IRQ-off mutex) is held
//! across I/O so RANK_DEVICE is not nested with the cache and VFS is
//! never held at the same time (DESIGN §2.1 / #62 ACK). `sync` uses
//! cache/device Flush, not Barrier (DESIGN §10.2).

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use vibeos::fat::{self, Disk, FatError, FatVol, INITRD_BYTES, Node, SEC};
use vibeos::fs::{FatFs, FsError, FsType, MAX_PATH};
use vibeos::lock::RANK_DEVICE;

use crate::block_init;
use crate::cache_init;
use crate::cell::IrqCell;
use crate::fs_init;
use crate::sync_init::SpinMutex;
use crate::thread_init;
use crate::virtio_blk_init;

const MAX_VOLS: usize = 2;
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
}

unsafe impl Sync for Slot {}

impl Slot {
    const fn empty() -> Self {
        Self {
            vol: UnsafeCell::new(None),
            back: UnsafeCell::new(Back::Initrd),
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

fn with_slot<R>(
    id: u8,
    f: impl FnOnce(&mut FatVol, &mut Io) -> Result<R, FatError>,
) -> Result<R, FatError> {
    grab(id)?;
    let i = id as usize;
    let r = unsafe {
        let vol = (*SLOTS[i].vol.get()).as_mut();
        match vol {
            None => Err(FatError::Io),
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
    let ok = fs_init::with(|v| {
        v.mount_root_fs(&FatFs {
            root_clu,
            vol: VOL_INITRD,
        })
        .is_ok()
    });
    LIVE.store(ok, Ordering::Release);
}

#[allow(dead_code)]
pub fn lookup(id: u8, dir_clu: u32, name: &[u8]) -> Result<Node, FsError> {
    with_slot(id, |v, d| v.lookup(d, dir_clu, name)).map_err(FatError::to_fs)
}

pub fn walk(id: u8, path: &[u8]) -> Result<Node, FsError> {
    with_slot(id, |v, d| v.walk(d, path)).map_err(FatError::to_fs)
}

pub fn readdir(id: u8, dir_clu: u32, cookie: u64, out: &mut Node) -> Result<Option<u64>, FsError> {
    with_slot(id, |v, d| v.readdir(d, dir_clu, cookie, out)).map_err(FatError::to_fs)
}

pub fn read(id: u8, clu: u32, size: u32, off: u64, buf: &mut [u8]) -> Result<usize, FsError> {
    with_slot(id, |v, d| v.read(d, clu, size, off, buf)).map_err(FatError::to_fs)
}

#[allow(clippy::too_many_arguments)] // FAT dirent + cluster + size update
pub fn write(
    id: u8,
    dir_clu: u32,
    dir_off: u32,
    ino: u32,
    first: &mut u32,
    size: &mut u32,
    off: u64,
    buf: &[u8],
) -> Result<usize, FsError> {
    let (n, clu, sz) = with_slot(id, |v, d| {
        let mut c = *first;
        let mut s = *size;
        let n = v.write(d, dir_clu, dir_off, &mut c, &mut s, off, buf)?;
        v.put_size(ino, c, s);
        Ok((n, c, s))
    })
    .map_err(FatError::to_fs)?;
    *first = clu;
    *size = sz;
    Ok(n)
}

pub fn create(id: u8, dir_clu: u32, name: &[u8], dir: bool) -> Result<Node, FsError> {
    with_slot(id, |v, d| v.create(d, dir_clu, name, dir)).map_err(FatError::to_fs)
}

pub fn unlink(id: u8, dir_clu: u32, name: &[u8], rmdir: bool) -> Result<(), FsError> {
    with_slot(id, |v, d| v.unlink(d, dir_clu, name, rmdir)).map_err(FatError::to_fs)
}

pub fn truncate(
    id: u8,
    dir_clu: u32,
    dir_off: u32,
    ino: u32,
    first: &mut u32,
    size: &mut u32,
    new: u32,
) -> Result<(), FsError> {
    let (c, s) = with_slot(id, |v, d| {
        let mut c = *first;
        let mut s = *size;
        v.truncate(d, dir_clu, dir_off, &mut c, &mut s, new)?;
        v.put_size(ino, c, s);
        Ok((c, s))
    })
    .map_err(FatError::to_fs)?;
    *first = c;
    *size = s;
    Ok(())
}

pub fn rename(
    id: u8,
    src_dir: u32,
    src_name: &[u8],
    dst_dir: u32,
    dst_name: &[u8],
) -> Result<(), FsError> {
    with_slot(id, |v, d| v.rename(d, src_dir, src_name, dst_dir, dst_name)).map_err(FatError::to_fs)
}

pub fn sync(id: u8) -> Result<(), FsError> {
    with_slot(id, |v, d| v.sync(d)).map_err(FatError::to_fs)
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
    .map_err(FatError::to_fs)
}

#[allow(dead_code)]
pub fn by_ino(id: u8, ino: u32) -> Result<Node, FsError> {
    with_slot(id, |v, _| v.by_ino(ino)).map_err(FatError::to_fs)
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
    let vol = FatVol::mount(&mut io).map_err(FatError::to_fs)?;
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
    match fs_init::with(|v| v.mount(None, at, &FatFs { root_clu, vol: id })) {
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
            if v.fstype_at(p).ok() == Some(FsType::Fat) {
                v.fat_vol_of(p).ok()
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
