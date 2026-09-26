//! VFS bring-up. ROADMAP §8.1 / §8.4 / §8.6.
//!
//! RANK_DEVICE (DESIGN §2.1), as are the ramfs and kernfs store locks
//! ([`RAMFS`], [`KERNFS`]). FAT initrd is root when live; otherwise
//! ramfs. Then kernfs skins on `/dev` `/proc` `/tmp` `/sys`, whose mount
//! points `mount_pseudo` makes through the root's `InodeOps`. File I/O
//! drops this lock before block waits. No serial marker.

use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::fs::{
    FileSystem, FsError, FsType, Guarded, KernFs, KernSkin, KernState, PathRef, RamFs, RamState,
    Vfs,
};
use vibeos::lock::RANK_DEVICE;

use crate::block_init;
use crate::dev_init;
use crate::fat_init;
use crate::file_init;
use crate::part_init;
use crate::sync_init::SpinMutex;
use crate::vibefs_init;
use crate::virtio_blk_init;

static VFS: SpinMutex<Vfs> = SpinMutex::with_rank(Vfs::new(), RANK_DEVICE);
/// Every ramfs instance's nodes: the root when FAT is not live, and each
/// `mount ramfs`.
pub static RAMFS: RamFs<SpinMutex<RamState>> =
    RamFs::new(SpinMutex::with_rank(RamState::new(), RANK_DEVICE));
/// The kernfs store the four pseudo filesystems share.
pub static KERNFS: KernFs<SpinMutex<KernState>> =
    KernFs::new(SpinMutex::with_rank(KernState::new(), RANK_DEVICE));
pub static DEVFS: KernSkin<SpinMutex<KernState>> = KernSkin::new(&KERNFS, FsType::Dev);
pub static PROCFS: KernSkin<SpinMutex<KernState>> = KernSkin::new(&KERNFS, FsType::Proc);
pub static TMPFS: KernSkin<SpinMutex<KernState>> = KernSkin::new(&KERNFS, FsType::Tmp);
pub static SYSFS: KernSkin<SpinMutex<KernState>> = KernSkin::new(&KERNFS, FsType::Sys);
static LIVE: AtomicBool = AtomicBool::new(false);

pub fn init() {
    fat_init::init();
    vibefs_init::init();
    if fat_init::live() {
        LIVE.store(true, Ordering::Release);
    } else {
        let ok = {
            let mut g = VFS.lock();
            g.mount_root_fs(&RAMFS).is_ok()
        };
        LIVE.store(ok, Ordering::Release);
    }
    if live() {
        let _ = with(mount_pseudo);
        populate_devfs();
        populate_sysfs();
        attach_vibefs();
    }
    file_init::init();
}

fn attach_vibefs() {
    if !vibefs_init::live() {
        return;
    }
    let _ = file_init::mkdir("/vibe", 0o755);
    let _ = file_init::vfs_attach("/vibe");
    let _ = vibefs_init::mount_mem("/vibe");
}

/// Mount the four pseudo filesystems on `/dev`, `/proc`, `/tmp` and
/// `/sys`, each mountpoint made with `mkdir` through the root's ops (an
/// existing directory is kept).
fn mount_pseudo(v: &mut Vfs) -> Result<(), FsError> {
    let skins: [(&str, &'static dyn FileSystem); 4] = [
        ("/dev", &DEVFS),
        ("/proc", &PROCFS),
        ("/tmp", &TMPFS),
        ("/sys", &SYSFS),
    ];
    for (at, fs) in skins {
        match v.mkdir(None, at, 0o755) {
            Ok(_) | Err(FsError::Exists) => {}
            Err(e) => return Err(e),
        }
        v.mount(None, at, fs)?;
    }
    Ok(())
}

fn populate_devfs() {
    let sz = block_init::capacity_sectors().saturating_mul(block_init::logical_block_size() as u64);
    let _ = KERNFS.devfs_add_block(block_init::name().as_bytes(), sz);
    if virtio_blk_init::live() {
        let sz = virtio_blk_init::capacity_sectors()
            .saturating_mul(virtio_blk_init::logical_block_size() as u64);
        let _ = KERNFS.devfs_add_block(virtio_blk_init::name().as_bytes(), sz);
    }
    let n = part_init::count();
    let mut i = 0usize;
    while i < n {
        if let Some((name, nsect, bs, _)) = part_init::info(i) {
            let sz = nsect.saturating_mul(bs as u64);
            let _ = KERNFS.devfs_add_block(name.as_bytes(), sz);
        }
        i += 1;
    }
}

fn hex_nib(d: u8) -> u8 {
    if d < 10 { b'0' + d } else { b'a' + (d - 10) }
}

fn bdf_name(bus: u8, device: u8, function: u8, out: &mut [u8; 8]) -> &[u8] {
    // "00:01.0"
    out[0] = hex_nib(bus >> 4);
    out[1] = hex_nib(bus & 0xf);
    out[2] = b':';
    out[3] = hex_nib(device >> 4);
    out[4] = hex_nib(device & 0xf);
    out[5] = b'.';
    out[6] = hex_nib(function & 0xf);
    &out[..7]
}

fn populate_sysfs() {
    let mut i = 0usize;
    while let Some(d) = dev_init::get(i) {
        let mut name = [0u8; 8];
        let bdf = bdf_name(d.addr.bus, d.addr.device, d.addr.function, &mut name);
        let drv = d.bound.map(|s| s.as_bytes());
        let _ = KERNFS.sysfs_add_device(bdf, d.vendor, d.device_id, d.class, drv);
        i += 1;
    }
}

impl<T: Send> Guarded<T> for SpinMutex<T> {
    fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let mut g = self.lock();
        f(&mut g)
    }
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn live() -> bool {
    LIVE.load(Ordering::Acquire)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn with<R>(f: impl FnOnce(&mut Vfs) -> R) -> R {
    let mut g = VFS.lock();
    f(&mut g)
}

#[allow(dead_code)]
pub fn root() -> Result<PathRef, FsError> {
    if !live() {
        return Err(FsError::Io);
    }
    with(|v| v.root())
}
