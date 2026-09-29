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

use vibeos::fs::kernfs::{KernFs, KernSkin, KernState};
use vibeos::fs::{FileApi, FsType, Guarded, Hooks, RamFs, RamState, Vfs};
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

pub fn live() -> bool {
    LIVE.load(Ordering::Acquire)
}

/// The File API over the VFS (C-FILEAPI), with the in-guest tests'
/// hooks in a `kernel_tests` build.
pub fn api() -> FileApi<'static, SpinMutex<Vfs>> {
    FileApi::with_hooks(&VFS, hooks())
}

/// The File API's `write_window` stall, a `fn()`, for the in-guest tests
/// `file_table_fork_churn` and `file_table_stale_writeback_ebadf`; null
/// until `fs::ktest::hooks::install_hooks` arms it.
#[cfg(feature = "kernel_tests")]
pub(super) static WRITE_WINDOW: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
/// The File API's `open_race` hook, a `fn() -> bool`, for the in-guest
/// test `open_creat_exists_opens`; null until `fs::ktest::hooks::install_hooks`
/// arms it.
#[cfg(feature = "kernel_tests")]
pub(super) static OPEN_RACE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// The hooks the in-guest tests armed; each unarmed one is `Hooks::NONE`'s.
#[cfg(feature = "kernel_tests")]
fn hooks() -> Hooks {
    let mut h = Hooks::NONE;
    // Acquire: pairs with the Release stores in `fs::ktest::hooks::install_hooks`.
    let w = WRITE_WINDOW.load(Ordering::Acquire);
    if !w.is_null() {
        // SAFETY: invariant: a non-null `WRITE_WINDOW` holds a `fn()`;
        // established by `fs::ktest::hooks::install_hooks`, its only store.
        h.write_window = unsafe { core::mem::transmute::<*mut (), fn()>(w) };
    }
    let r = OPEN_RACE.load(Ordering::Acquire);
    if !r.is_null() {
        // SAFETY: invariant: a non-null `OPEN_RACE` holds a
        // `fn() -> bool`; established by `fs::ktest::hooks::install_hooks`, its
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
pub fn with<R>(f: impl FnOnce(&mut Vfs) -> R) -> R {
    let mut g = VFS.lock();
    f(&mut g)
}
