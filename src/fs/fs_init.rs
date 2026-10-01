//! The VFS instance. ROADMAP §8.1 / §8.4 / §8.6 / §10.4.
//!
//! The VFS lock is a `BlockingMutex` over the namespace tables (DESIGN
//! §2.1's sleeping tier, level 1): it is taken only from thread context
//! with IF=1 and no spinlock held, and never under a volume lock. The
//! ramfs and kernfs store locks ([`RAMFS`], [`KERNFS`]) are RANK_DEVICE
//! spinlocks, never nested; tmpfs's data ops run under kernfs's. The
//! inode words data I/O changes live in [`INODE_WORDS`], outside the
//! lock. [`init`] makes the root: the FAT
//! initrd when it is live, otherwise ramfs. The File API module's `init`
//! runs the bring-up around it: the backends first, then kernfs skins on
//! `/dev` `/proc` `/tmp` `/sys` and vibefs on `/vibe`, each mountpoint made
//! with its `mkdir` and mounted through [`FileApi::mount_fs`]. Every
//! backend call runs through [`api`], with this lock dropped (C-FILEAPI).
//! No serial marker.

#[cfg(feature = "kernel_tests")]
use core::sync::atomic::AtomicPtr;
use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::dev::Instance;
use vibeos::fs::kernfs::{KernFs, KernSkin, KernState};
use vibeos::fs::{
    FileApi, FsError, FsType, Guarded, Hooks, RamFs, RamState, Vfs, VfsSizes, WordsTable,
    words_table,
};
use vibeos::kalloc::AllocError;
use vibeos::lock::RANK_DEVICE;

use crate::cell::BootCell;
use crate::fs::StoreLock;
use crate::sync::blocking_init::BlockingMutex;

/// Each inode slot's size, link count and private words (`InodeWords`):
/// a heap table of `limits::MAX_INODES`, set by [`init_tables`].
static INODE_WORDS: BootCell<WordsTable> = BootCell::new();
/// The VFS, built by [`init_tables`] before `irq: enabled` with its tables
/// at `VfsSizes::KERNEL` (ROADMAP §10.4, D1): this cell is its one
/// construction site.
static VFS: BootCell<BlockingMutex<Vfs>> = BootCell::new();
/// Every ramfs instance's nodes: the root when FAT is not live, and each
/// `mount ramfs`.
pub static RAMFS: RamFs<StoreLock<RamState>> =
    RamFs::new(StoreLock::with_rank(RamState::new(), RANK_DEVICE));
/// The kernfs store the four pseudo filesystems share.
pub static KERNFS: KernFs<StoreLock<KernState>> =
    KernFs::new(StoreLock::with_rank(KernState::new(), RANK_DEVICE));
pub static DEVFS: KernSkin<StoreLock<KernState>> = KernSkin::new(&KERNFS, FsType::Dev);
pub static PROCFS: KernSkin<StoreLock<KernState>> = KernSkin::new(&KERNFS, FsType::Proc);
pub static TMPFS: KernSkin<StoreLock<KernState>> = KernSkin::new(&KERNFS, FsType::Tmp);
pub static SYSFS: KernSkin<StoreLock<KernState>> = KernSkin::new(&KERNFS, FsType::Sys);
static LIVE: AtomicBool = AtomicBool::new(false);

/// Allocate the VFS's tables at `VfsSizes::KERNEL` and its inode words, and
/// install them. Once, on the BSP before `irq: enabled` and before any file
/// is opened; on failure nothing is installed, and the caller halts the
/// boot.
pub fn init_tables() -> Result<(), AllocError> {
    let words = words_table(VfsSizes::KERNEL.inodes)?;
    // SAFETY: `BootCell::set`'s contract: its one write, on the BSP before
    // `smp: done` and before any reader, since no VFS exists yet to hand
    // out an inode's words; established here, called once from
    // `main::boot_rest`.
    unsafe { INODE_WORDS.set(words) };
    let vfs = Vfs::new(&VfsSizes::KERNEL, INODE_WORDS.get())?;
    // SAFETY: `BootCell::set`'s contract: the VFS cell's one write, on the
    // BSP before `smp: done` and before any reader of `fs_init::api` or
    // `fs_init::with`; established here.
    unsafe { VFS.set(BlockingMutex::new(vfs)) };
    Ok(())
}

/// Make the root: the FAT initrd when `root_is_fat`, else a ramfs.
/// The File API module's `init` runs the rest of the bring-up.
pub fn init(root_is_fat: bool) {
    if root_is_fat {
        LIVE.store(true, Ordering::Release);
    } else {
        let ok = api().mount_root(&RAMFS, None, false, None).is_ok();
        LIVE.store(ok, Ordering::Release);
    }
}

/// The VFS lock, as the File API takes it: the clock is read before the
/// lock and stamps `Vfs::now`. Sleeps: thread context only (DESIGN §2.1).
impl Guarded<Vfs> for BlockingMutex<Vfs> {
    fn with<R>(&self, f: impl FnOnce(&mut Vfs) -> R) -> R {
        let t = now();
        let mut g = self.lock();
        g.now = t;
        f(&mut g)
    }
}

/// The wall clock in unix seconds, which the filesystems stamp times
/// with; 0 without an RTC (`time_init::unix_time_s`), which FAT records as
/// 1980-01-01.
pub(crate) fn now() -> u64 {
    crate::time_init::unix_time_s().unwrap_or(0)
}

pub fn live() -> bool {
    LIVE.load(Ordering::Acquire)
}

/// The File API over the VFS (C-FILEAPI), with the in-guest tests'
/// hooks in a `kernel_tests` build.
pub fn api() -> FileApi<'static, BlockingMutex<Vfs>> {
    FileApi::with_hooks(VFS.get(), hooks())
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

/// Run `f` under the VFS lock. It sleeps for the lock: never from IRQ
/// context, with IF=0, under a spinlock, or under a volume lock. `f`
/// never calls a backend: every `Vfs` method that reaches one runs
/// through [`api`].
pub fn with<R>(f: impl FnOnce(&mut Vfs) -> R) -> R {
    VFS.get().with(f)
}

/// A reference to the volume instance of the filesystem mounted at `path`
/// (`Vfs::volume_of`); `Inval` for one without.
pub fn volume_at(path: &[u8]) -> Result<Instance, FsError> {
    let api = api();
    let p = api.walk(None, path, true)?;
    let v = with(|v| v.volume_of(p));
    api.put_path(p);
    v
}
