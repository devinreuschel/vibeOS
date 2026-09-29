//! The VFS instance. ROADMAP §8.1 / §8.4 / §8.6.
//!
//! RANK_DEVICE (DESIGN §2.1), as are the ramfs and kernfs store locks
//! ([`RAMFS`], [`KERNFS`]), never nested. [`init`] makes the root: the FAT
//! initrd when it is live, otherwise ramfs. The File API module's `init`
//! runs the bring-up around it: the backends first, then kernfs skins on
//! `/dev` `/proc` `/tmp` `/sys` and vibefs on `/vibe`, each mountpoint made
//! with its `mkdir` and mounted through [`FileApi::mount_fs`]. Every
//! backend call runs through [`api`], with this lock dropped (C-FILEAPI).
//! No serial marker.

#[cfg(feature = "kernel_tests")]
use core::sync::atomic::AtomicPtr;
use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::fs::{
    FileApi, FsError, FsType, Guarded, Hooks, KernFs, KernSkin, KernState, PathRef, RamFs,
    RamState, Vfs,
};
use vibeos::lock::RANK_DEVICE;

use crate::sync_init::SpinMutex;

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

/// Make the root: the FAT initrd when `root_is_fat`, else a ramfs.
/// The File API module's `init` runs the rest of the bring-up.
pub fn init(root_is_fat: bool) {
    if root_is_fat {
        LIVE.store(true, Ordering::Release);
    } else {
        let ok = api().mount_root(&RAMFS, None, false).is_ok();
        LIVE.store(ok, Ordering::Release);
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

/// The in-guest tests' File API hooks (`Hooks`), which the File API
/// module's `init` installs in a `kernel_tests` build. Each unset one is `Hooks::NONE`'s.
#[cfg(feature = "kernel_tests")]
static WRITE_WINDOW: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
#[cfg(feature = "kernel_tests")]
static OPEN_RACE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Install the in-guest tests' File API hooks.
#[cfg(feature = "kernel_tests")]
pub fn set_test_hooks(write_window: fn(), open_race: fn() -> bool) {
    // Release: pairs with the Acquire loads in `hooks`.
    WRITE_WINDOW.store(write_window as *mut (), Ordering::Release);
    OPEN_RACE.store(open_race as *mut (), Ordering::Release);
}

#[cfg(feature = "kernel_tests")]
fn hooks() -> Hooks {
    let mut h = Hooks::NONE;
    // Acquire: pairs with the Release stores in `set_test_hooks`.
    let w = WRITE_WINDOW.load(Ordering::Acquire);
    if !w.is_null() {
        // SAFETY: invariant: a non-null `WRITE_WINDOW` holds a `fn()`;
        // established by `fs_init::set_test_hooks`, its only store.
        h.write_window = unsafe { core::mem::transmute::<*mut (), fn()>(w) };
    }
    let r = OPEN_RACE.load(Ordering::Acquire);
    if !r.is_null() {
        // SAFETY: invariant: a non-null `OPEN_RACE` holds a
        // `fn() -> bool`; established by `fs_init::set_test_hooks`, its
        // only store.
        h.open_race = unsafe { core::mem::transmute::<*mut (), fn() -> bool>(r) };
    }
    h
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
