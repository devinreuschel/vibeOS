//! VFS bring-up. ROADMAP §8.1 / §8.4 / §8.6.
//!
//! RANK_DEVICE (DESIGN §2.1), as are the ramfs and kernfs store locks
//! ([`RAMFS`], [`KERNFS`]), never nested. FAT initrd is root when live;
//! otherwise ramfs. Then kernfs skins on `/dev` `/proc` `/tmp` `/sys` and
//! vibefs on `/vibe`, each mountpoint made with `file_init::mkdir` and
//! mounted through [`FileApi::mount_fs`]. Every backend call runs through
//! [`api`], with this lock dropped (C-FILEAPI). No serial marker.

use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::fs::{
    FileApi, FileSystem, FsError, FsType, Guarded, Hooks, KernFs, KernSkin, KernState, PathRef,
    RamFs, RamState, Vfs,
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
        let ok = api().mount_root(&RAMFS, None, false).is_ok();
        LIVE.store(ok, Ordering::Release);
    }
    if live() {
        let _ = mount_pseudo();
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
    let _ = file_init::mkdir(b"/vibe", 0o755);
    let _ = vibefs_init::mount_mem("/vibe");
}

/// Mount the four pseudo filesystems on `/dev`, `/proc`, `/tmp` and
/// `/sys`, each mountpoint made with `mkdir` through the root's ops (an
/// existing directory is kept).
fn mount_pseudo() -> Result<(), FsError> {
    let skins: [(&[u8], &'static dyn FileSystem); 4] = [
        (b"/dev", &DEVFS),
        (b"/proc", &PROCFS),
        (b"/tmp", &TMPFS),
        (b"/sys", &SYSFS),
    ];
    for (at, fs) in skins {
        file_init::mkdir(at, 0o755)?;
        api().mount_fs(None, at, fs, None, false)?;
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

/// The File API over the VFS (C-FILEAPI), with the in-guest tests'
/// hooks in a `kernel_tests` build.
pub fn api() -> FileApi<'static, SpinMutex<Vfs>> {
    FileApi::with_hooks(&VFS, hooks())
}

#[cfg(feature = "kernel_tests")]
fn hooks() -> Hooks {
    Hooks {
        write_window: file_init::testing::write_window,
        open_race: file_init::testing::open_race,
    }
}

#[cfg(not(feature = "kernel_tests"))]
fn hooks() -> Hooks {
    Hooks::NONE
}

/// Run `f` under the VFS lock. `f` never calls a backend: every `Vfs`
/// method that reaches one runs through [`api`].
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
