//! Kernel File API. ROADMAP §8.6 / §10.4. The shell's file commands
//! are in `shell::cmds::fs`.
//!
//! A thin File API over `Vfs` (C-FILEAPI): every function runs
//! [`fs_init::api`], which calls a backend only with the VFS lock dropped,
//! so a backend may wait for its volume and its disk. Each path function
//! has an `_at` form that takes the caller's walk base (`WalkBase`, the
//! root and working directory a process's or the shell's `DirRef`s hold):
//! an absolute path starts at its root, a relative one at its working
//! directory. The C-FILEAPI forms pass none, so a kernel thread's root and
//! working directory are `/`. `sync` issues a block Flush
//! (DESIGN §10.2). FAT rejects symlink/link with `Perm`; vibefs stores
//! POSIX mode and symlinks (docs/VIBEFS.md).
//!
//! Path syscalls (`open`, `execve`'s image) resolve through `Vfs` like
//! every other caller here, one component at a time, so a process reaches
//! every mounted filesystem, `/dev`, `/proc`, `/tmp` and `/sys` included.

use vibeos::block::MAX_BLOCKDEVS;
use vibeos::block::blockdev::BlockRef;
use vibeos::fs::{
    DirEntry, DirRef, FileId, FileRef, FileSystem, FsError, InodeKind, MAX_PATH, O_DIRECTORY,
    O_RDONLY, OpenFlags, PathRef, SeekFrom, Stat, WalkBase,
};
use vibeos::log::Level;

use crate::block::blockdev_init;
use crate::dev_init;
use crate::fat_init;
use crate::fs_init;
use crate::vibefs_init;

// ---- C-FILEAPI ----

#[cfg_attr(
    not(feature = "kernel_tests"),
    allow(
        dead_code,
        reason = "the File API's whole surface (C-FILEAPI and its `_at` forms); a production kernel calls part of it"
    )
)]
/// Open `path`; `O_CREAT` creates a regular file with `mode`, `O_TRUNC`
/// empties one.
pub fn open(path: &[u8], flags: OpenFlags, mode: u32) -> Result<FileRef, FsError> {
    open_at(None, path, flags, mode)
}

pub fn read(f: &FileRef, buf: &mut [u8]) -> Result<usize, FsError> {
    fs_init::api().read(f, buf)
}

pub fn write(f: &FileRef, buf: &[u8]) -> Result<usize, FsError> {
    fs_init::api().write(f, buf)
}

pub fn seek(f: &FileRef, pos: SeekFrom) -> Result<u64, FsError> {
    fs_init::api().seek(f, pos)
}

/// Drop one reference; the last frees the slot, changes its generation,
/// and puts the inode, with the VFS lock dropped for any release.
pub fn close(f: FileRef) -> Result<(), FsError> {
    fs_init::api().close(f)
}

pub fn stat(f: &FileRef) -> Result<Stat, FsError> {
    fs_init::api().stat(f)
}

/// Report each entry of open directory `f` to `cb`, `.` and `..` first,
/// until `cb` returns false; `cb` runs with the VFS lock dropped.
pub fn readdir(f: &FileRef, cb: &mut dyn FnMut(&DirEntry) -> bool) -> Result<(), FsError> {
    fs_init::api().readdir(f, cb)
}

/// Report the entries of open directory `f` from cookie `cookie` to
/// `emit`, each with the cookie after it, until `emit` returns false or
/// the entries run out; the cookie of the first entry not consumed. The
/// file position does not move, and `emit` runs with the VFS lock dropped.
pub fn readdir_from(
    f: &FileRef,
    cookie: u64,
    emit: &mut dyn FnMut(&DirEntry, u64) -> bool,
) -> Result<u64, FsError> {
    fs_init::api().readdir_from(f, cookie, emit)
}

/// Make directory `path`; one already there is kept.
pub fn mkdir(path: &[u8], mode: u32) -> Result<(), FsError> {
    mkdir_at(None, path, mode)
}

#[cfg_attr(
    not(feature = "kernel_tests"),
    allow(
        dead_code,
        reason = "the File API's whole surface (C-FILEAPI and its `_at` forms); a production kernel calls part of it"
    )
)]
pub fn unlink(path: &[u8]) -> Result<(), FsError> {
    unlink_at(None, path)
}

#[cfg_attr(
    not(feature = "kernel_tests"),
    allow(
        dead_code,
        reason = "the File API's whole surface (C-FILEAPI and its `_at` forms); a production kernel calls part of it"
    )
)]
pub fn rmdir(path: &[u8]) -> Result<(), FsError> {
    rmdir_at(None, path)
}

#[allow(
    dead_code,
    reason = "C-FILEAPI's `rename` on paths; the shell and the in-guest tests call `rename_at`"
)]
pub fn rename(old: &[u8], new: &[u8]) -> Result<(), FsError> {
    rename_at(None, old, new)
}

#[cfg_attr(
    not(feature = "kernel_tests"),
    allow(
        dead_code,
        reason = "the File API's whole surface (C-FILEAPI and its `_at` forms); a production kernel calls part of it"
    )
)]
/// Mount `fstype` from `source` on `target`: `fat32` and `vibefs` from a
/// block device (`ram0`, `vda`), `ramfs` from nothing.
pub fn mount(source: &[u8], target: &[u8], fstype: &[u8], ro: bool) -> Result<(), FsError> {
    mount_at(None, source, target, fstype, ro)
}

#[cfg_attr(
    not(feature = "kernel_tests"),
    allow(
        dead_code,
        reason = "the File API's whole surface (C-FILEAPI and its `_at` forms); a production kernel calls part of it"
    )
)]
/// Unmount the mount whose root `target` names; its superblock's last
/// mount releases the volume.
pub fn umount(target: &[u8]) -> Result<(), FsError> {
    umount_at(None, target)
}

// ---- with a walk base ----

/// [`open`] from `base`.
pub fn open_at(
    base: Option<WalkBase>,
    path: &[u8],
    flags: OpenFlags,
    mode: u32,
) -> Result<FileRef, FsError> {
    fs_init::api().open(base, path, flags, mode)
}

/// `stat` of `path` from `base`, following a last symlink.
pub fn stat_at(base: Option<WalkBase>, path: &[u8]) -> Result<Stat, FsError> {
    fs_init::api().stat_path(base, path, true)
}

/// [`mkdir`] from `base`; one already there is kept.
pub fn mkdir_at(base: Option<WalkBase>, path: &[u8], mode: u32) -> Result<(), FsError> {
    match fs_init::api().mkdir(base, path, mode) {
        Ok(()) | Err(FsError::Exists) => Ok(()),
        Err(e) => Err(e),
    }
}

pub fn unlink_at(base: Option<WalkBase>, path: &[u8]) -> Result<(), FsError> {
    fs_init::api().unlink(base, path)
}

pub fn rmdir_at(base: Option<WalkBase>, path: &[u8]) -> Result<(), FsError> {
    fs_init::api().rmdir(base, path)
}

pub fn rename_at(base: Option<WalkBase>, old: &[u8], new: &[u8]) -> Result<(), FsError> {
    fs_init::api().rename(base, old, new)
}

#[cfg_attr(
    not(feature = "kernel_tests"),
    allow(
        dead_code,
        reason = "the File API's whole surface (C-FILEAPI and its `_at` forms); a production kernel calls part of it"
    )
)]
/// Make `path` a symlink to `target`, from `base`.
pub fn symlink_at(base: Option<WalkBase>, path: &[u8], target: &[u8]) -> Result<(), FsError> {
    fs_init::api().symlink(base, path, target)
}

#[cfg_attr(
    not(feature = "kernel_tests"),
    allow(
        dead_code,
        reason = "the File API's whole surface (C-FILEAPI and its `_at` forms); a production kernel calls part of it"
    )
)]
/// Hard link `new` to the regular file `old`, both from `base`.
pub fn link_at(base: Option<WalkBase>, old: &[u8], new: &[u8]) -> Result<(), FsError> {
    fs_init::api().link(base, old, new)
}

#[cfg_attr(
    not(feature = "kernel_tests"),
    allow(
        dead_code,
        reason = "the File API's whole surface (C-FILEAPI and its `_at` forms); a production kernel calls part of it"
    )
)]
/// Set the size of the regular file `path` names from `base`.
pub fn truncate_at(base: Option<WalkBase>, path: &[u8], size: u64) -> Result<(), FsError> {
    fs_init::api().truncate(base, path, size)
}

/// [`mount`] on `target` from `base`.
pub fn mount_at(
    base: Option<WalkBase>,
    source: &[u8],
    target: &[u8],
    fstype: &[u8],
    ro: bool,
) -> Result<(), FsError> {
    let src = core::str::from_utf8(source).map_err(|_| FsError::Inval)?;
    let at = core::str::from_utf8(target).map_err(|_| FsError::Inval)?;
    match fstype {
        b"fat32" => fat_init::mount_dev_at(base, src, at, ro),
        b"vibefs" => vibefs_init::mount_dev_at(base, src, at, ro),
        b"ramfs" => fs_init::api()
            .mount_fs(base, target, &fs_init::RAMFS, None, ro, None)
            .map(|_| ()),
        _ => Err(FsError::Inval),
    }
}

/// [`umount`] of `target` from `base`.
pub fn umount_at(base: Option<WalkBase>, target: &[u8]) -> Result<(), FsError> {
    fs_init::api().umount(base, target)
}

// ---- directory references ----

#[cfg_attr(
    feature = "vibefs_crash",
    allow(dead_code, reason = "the vibefs_crash kernel starts no process")
)]
/// A process's first root and working directory: two references to the
/// namespace root, or none while the VFS has no root.
pub fn ns_refs() -> Option<(DirRef, DirRef)> {
    let api = fs_init::api();
    let root = api.dir_root().ok()?;
    match api.dir_dup(root.at()) {
        Ok(cwd) => Some((root, cwd)),
        Err(_) => {
            api.dir_put(root);
            None
        }
    }
}

/// A reference to the directory `path` names from `base`; `NotDir` when
/// it names something else.
pub fn dir_get_at(base: Option<WalkBase>, path: &[u8]) -> Result<DirRef, FsError> {
    fs_init::api().dir_get(base, path)
}

/// Another reference to directory `at`, which a reference holds.
pub fn dir_dup(at: PathRef) -> Result<DirRef, FsError> {
    fs_init::api().dir_dup(at)
}

/// Drop a directory reference. Sleeps for the VFS lock: never under a
/// spinlock or the scheduler's lock.
pub fn dir_put(r: DirRef) {
    fs_init::api().dir_put(r);
}

/// The path of directory `at` from `base`'s root, into `out`; its length.
pub fn dir_path(base: Option<WalkBase>, at: PathRef, out: &mut [u8]) -> Result<usize, FsError> {
    fs_init::api().dir_path(base, at, out)
}

/// How many holders directory `at`'s dentry has (test-only:
/// `cwd_per_process`).
#[cfg(feature = "kernel_tests")]
pub fn dentry_refs(at: PathRef) -> u32 {
    fs_init::api().dentry_refs(at)
}

// ---- added ----

/// A new count on open file `id`, for one syscall.
pub fn fget(id: FileId) -> Result<FileRef, FsError> {
    fs_init::api().fget(id)
}

/// Extra process fd pointing at the same kernel file.
pub fn addref(id: FileId) -> Result<(), FsError> {
    fs_init::api().addref(id)
}

#[cfg_attr(
    not(feature = "kernel_tests"),
    allow(
        dead_code,
        reason = "the File API's whole surface (C-FILEAPI and its `_at` forms); a production kernel calls part of it"
    )
)]
pub fn stat_path(path: &[u8]) -> Result<Stat, FsError> {
    stat_at(None, path)
}

/// Write every mounted filesystem's dirty state to its device.
pub fn sync_fs() -> Result<(), FsError> {
    fs_init::api().sync()
}

/// Make `path` and every missing parent, from `base`.
pub fn mkdir_p_at(base: Option<WalkBase>, path: &[u8]) -> Result<(), FsError> {
    let pb = path;
    let mut i = 0usize;
    while i < pb.len() && pb[i] == b'/' {
        i += 1;
    }
    while i < pb.len() {
        let mut j = i;
        while j < pb.len() && pb[j] != b'/' {
            j += 1;
        }
        let slice = &pb[..j];
        if slice != b"/" {
            match stat_at(base, slice) {
                Ok(s) if s.kind == InodeKind::Dir => {}
                Ok(_) => return Err(FsError::NotDir),
                Err(FsError::NotFound) => mkdir_at(base, slice, 0o755)?,
                Err(e) => return Err(e),
            }
        }
        while j < pb.len() && pb[j] == b'/' {
            j += 1;
        }
        i = j;
    }
    Ok(())
}

/// Create regular file `path`, or empty it. Test-only: `ktest::fid`
/// and the fs in-guest tests call it.
#[cfg(feature = "kernel_tests")]
pub fn creat(path: &[u8]) -> Result<(), FsError> {
    use vibeos::fs::{O_CREAT, O_TRUNC, O_WRONLY};

    let f = open(
        path,
        OpenFlags::from_bits(O_WRONLY | O_CREAT | O_TRUNC),
        0o644,
    )?;
    close(f)
}

/// The filesystem bring-up: the FAT and vibefs backends, the root
/// (`fs_init::init`), then while it is live the pseudo filesystems, devfs
/// and sysfs, and vibefs on `/vibe`.
pub fn init() {
    fat_init::init();
    fs_init::init(fat_init::live());
    if fs_init::live() {
        if let Err(e) = mount_pseudo() {
            crate::klog!(
                Level::Warn,
                "vibeOS: fs: pseudo filesystems not mounted: {}",
                e.as_str()
            );
        }
        populate_devfs();
        populate_sysfs();
        attach_vibefs();
    }
}

fn attach_vibefs() {
    if let Err(e) = mkdir(b"/vibe", 0o755).and_then(|()| vibefs_init::mount_mem("/vibe")) {
        crate::klog!(Level::Warn, "vibeOS: fs: /vibe not mounted: {}", e.as_str());
    }
}

/// Mount the four pseudo filesystems on `/dev`, `/proc`, `/tmp` and
/// `/sys`, each mountpoint made with `mkdir` through the root's ops (an
/// existing directory is kept).
fn mount_pseudo() -> Result<(), FsError> {
    let skins: [(&[u8], &'static dyn FileSystem); 4] = [
        (b"/dev", &fs_init::DEVFS),
        (b"/proc", &fs_init::PROCFS),
        (b"/tmp", &fs_init::TMPFS),
        (b"/sys", &fs_init::SYSFS),
    ];
    for (at, fs) in skins {
        mkdir(at, 0o755)?;
        fs_init::api().mount_fs(None, at, fs, None, false, None)?;
    }
    Ok(())
}

/// One devfs block node per registered block device, disks and
/// partitions alike. The registry is read into a stack array first, so no
/// registry lock is held under the VFS's.
fn populate_devfs() {
    let mut all: [Option<BlockRef>; MAX_BLOCKDEVS] = [const { None }; MAX_BLOCKDEVS];
    let n = blockdev_init::snapshot(&mut all);
    let mut failed = 0u32;
    let mut last = None;
    for r in all.iter().take(n).flatten() {
        if let Err(e) = fs_init::KERNFS.devfs_add_block(r) {
            failed = failed.saturating_add(1);
            last = Some(e);
        }
    }
    if let Some(e) = last {
        crate::klog!(
            Level::Warn,
            "vibeOS: fs: {} block devfs nodes not added, last: {}",
            failed,
            e.as_str()
        );
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
    let mut failed = 0u32;
    let mut last = None;
    let mut i = 0usize;
    while let Some(d) = dev_init::get(i) {
        let mut name = [0u8; 8];
        let bdf = bdf_name(d.addr.bus, d.addr.device, d.addr.function, &mut name);
        let drv = dev_init::bound(&d).map(|s| s.as_bytes());
        if let Err(e) = fs_init::KERNFS.sysfs_add_device(bdf, d.vendor, d.device_id, d.class, drv) {
            failed = failed.saturating_add(1);
            last = Some(e);
        }
        i += 1;
    }
    if let Some(e) = last {
        crate::klog!(
            Level::Warn,
            "vibeOS: fs: {} sysfs devices not added, last: {}",
            failed,
            e.as_str()
        );
    }
}

/// Report each entry of directory `path`, from `base`, but `.` and `..`
/// to `cb`, with the VFS lock dropped.
pub(crate) fn list_dir(
    base: Option<WalkBase>,
    path: &[u8],
    cb: &mut dyn FnMut(&DirEntry),
) -> Result<(), FsError> {
    let f = open_at(base, path, OpenFlags::from_bits(O_RDONLY | O_DIRECTORY), 0)?;
    let r = readdir(&f, &mut |d| {
        let n = d.name.as_bytes();
        if n != b"." && n != b".." {
            cb(d);
        }
        true
    });
    let c = close(f);
    r.and(c)
}

/// `path/name` into `out`; its length.
pub(crate) fn child_path(
    path: &[u8],
    name: &[u8],
    out: &mut [u8; MAX_PATH],
) -> Result<usize, FsError> {
    let mut n = path.len();
    out.get_mut(..n)
        .ok_or(FsError::NameTooLong)?
        .copy_from_slice(path);
    if n == 0 || out[n - 1] != b'/' {
        if n >= MAX_PATH {
            return Err(FsError::NameTooLong);
        }
        out[n] = b'/';
        n += 1;
    }
    let end = n.checked_add(name.len()).ok_or(FsError::NameTooLong)?;
    if end > MAX_PATH {
        return Err(FsError::NameTooLong);
    }
    out[n..end].copy_from_slice(name);
    Ok(end)
}
