//! Kernel File API. ROADMAP §8.6 / §10.4. The shell's file commands
//! are in `shell::cmds::fs`.
//!
//! A thin File API over `Vfs` (C-FILEAPI): every function runs
//! [`fs_init::api`], which calls a backend only with the VFS lock dropped,
//! so a backend may wait for its volume and its disk. Relative paths are
//! joined with the shell's working directory. `sync` issues a block Flush
//! (DESIGN §10.2). FAT rejects symlink/link with `NotSupp`; vibefs stores
//! POSIX mode and symlinks (docs/VIBEFS.md).
//!
//! Path syscalls (`open`, `execve`'s image) resolve through `Vfs` like
//! every other caller here, one component at a time, so a process reaches
//! every mounted filesystem, `/dev`, `/proc`, `/tmp` and `/sys` included.

use vibeos::block::MAX_BLOCKDEVS;
use vibeos::block::blockdev::BlockRef;
use vibeos::fs::{
    DirEntry, FileId, FileRef, FileSystem, FsError, InodeKind, MAX_PATH, O_DIRECTORY, O_RDONLY,
    OpenFlags, SeekFrom, Stat,
};
use vibeos::lock::RANK_DEVICE;
use vibeos::log::Level;

use crate::block::blockdev_init;
use crate::dev_init;
use crate::fat_init;
use crate::fs_init;
use crate::sync_init::SpinMutex;
use crate::vibefs_init;

struct CwdBuf {
    buf: [u8; MAX_PATH],
    len: usize,
}

const fn cwd_root() -> CwdBuf {
    let mut buf = [0u8; MAX_PATH];
    buf[0] = b'/';
    CwdBuf { buf, len: 1 }
}

static CWD: SpinMutex<CwdBuf> = SpinMutex::with_rank(cwd_root(), RANK_DEVICE);

/// Run `f` on the working directory.
fn with_cwd<R>(f: impl FnOnce(&mut CwdBuf) -> R) -> R {
    let mut g = CWD.lock();
    f(&mut g)
}

pub(crate) fn cwd_copy() -> ([u8; MAX_PATH], usize) {
    with_cwd(|c| {
        let mut buf = [0u8; MAX_PATH];
        buf[..c.len].copy_from_slice(&c.buf[..c.len]);
        (buf, c.len)
    })
}

pub(crate) fn set_cwd(p: &[u8]) {
    with_cwd(|c| {
        let n = p.len().min(MAX_PATH);
        c.buf[..n].copy_from_slice(&p[..n]);
        c.len = n;
    });
}

/// `path`, made absolute against the working directory, and its length.
pub(crate) fn join_cwd(p: &[u8]) -> Result<([u8; MAX_PATH], usize), FsError> {
    let mut out = [0u8; MAX_PATH];
    if p.first() == Some(&b'/') {
        if p.len() > MAX_PATH {
            return Err(FsError::NameTooLong);
        }
        out[..p.len()].copy_from_slice(p);
        return Ok((out, p.len()));
    }
    let cwd = cwd_copy();
    let mut n = cwd.1;
    if n > MAX_PATH {
        return Err(FsError::NameTooLong);
    }
    out[..n].copy_from_slice(&cwd.0[..n]);
    if n == 0 || out[n - 1] != b'/' {
        if n >= MAX_PATH {
            return Err(FsError::NameTooLong);
        }
        out[n] = b'/';
        n += 1;
    }
    let end = n.checked_add(p.len()).ok_or(FsError::NameTooLong)?;
    if end > MAX_PATH {
        return Err(FsError::NameTooLong);
    }
    out[n..end].copy_from_slice(p);
    Ok((out, end))
}

/// Run `f` on `path` made absolute.
fn with_abs<R>(path: &[u8], f: impl FnOnce(&[u8]) -> Result<R, FsError>) -> Result<R, FsError> {
    let (buf, n) = join_cwd(path)?;
    f(&buf[..n])
}

// ---- C-FILEAPI ----

/// Open `path`; `O_CREAT` creates a regular file with `mode`, `O_TRUNC`
/// empties one.
pub fn open(path: &[u8], flags: OpenFlags, mode: u32) -> Result<FileRef, FsError> {
    with_abs(path, |p| fs_init::api().open(None, p, flags, mode))
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

/// Make directory `path`; one already there is kept.
pub fn mkdir(path: &[u8], mode: u32) -> Result<(), FsError> {
    match with_abs(path, |p| fs_init::api().mkdir(None, p, mode)) {
        Ok(()) | Err(FsError::Exists) => Ok(()),
        Err(e) => Err(e),
    }
}

pub fn unlink(path: &[u8]) -> Result<(), FsError> {
    unlink_path(path, false)
}

pub fn rmdir(path: &[u8]) -> Result<(), FsError> {
    unlink_path(path, true)
}

fn unlink_path(path: &[u8], dir: bool) -> Result<(), FsError> {
    with_abs(path, |p| {
        let api = fs_init::api();
        if dir {
            api.rmdir(None, p)
        } else {
            api.unlink(None, p)
        }
    })
}

pub fn rename(old: &[u8], new: &[u8]) -> Result<(), FsError> {
    let (ob, on) = join_cwd(old)?;
    with_abs(new, |n| fs_init::api().rename(None, &ob[..on], n))
}

/// Mount `fstype` from `source` on `target`: `fat32` and `vibefs` from a
/// block device (`ram0`, `vda`), `ramfs` from nothing.
pub fn mount(source: &[u8], target: &[u8], fstype: &[u8], ro: bool) -> Result<(), FsError> {
    let src = core::str::from_utf8(source).map_err(|_| FsError::Inval)?;
    let (buf, n) = join_cwd(target)?;
    let at = core::str::from_utf8(&buf[..n]).map_err(|_| FsError::Inval)?;
    match fstype {
        b"fat32" => fat_init::mount_dev(src, at, ro),
        b"vibefs" => vibefs_init::mount_dev(src, at, ro),
        b"ramfs" => fs_init::api()
            .mount_fs(None, at.as_bytes(), &fs_init::RAMFS, None, ro, None)
            .map(|_| ()),
        _ => Err(FsError::Inval),
    }
}

/// Unmount the mount whose root `target` names; its superblock's last
/// mount releases the volume.
pub fn umount(target: &[u8]) -> Result<(), FsError> {
    with_abs(target, |p| fs_init::api().umount(None, p))
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

pub fn stat_path(path: &[u8]) -> Result<Stat, FsError> {
    with_abs(path, |p| fs_init::api().stat_path(None, p, true))
}

/// Write every mounted filesystem's dirty state to its device.
pub fn sync_fs() -> Result<(), FsError> {
    fs_init::api().sync()
}

/// Make `path` and every missing parent.
pub fn mkdir_p(path: &[u8]) -> Result<(), FsError> {
    let (buf, len) = join_cwd(path)?;
    let pb = &buf[..len];
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
        if slice.len() > 1 {
            match stat_path(slice) {
                Ok(s) if s.kind == InodeKind::Dir => {}
                Ok(_) => return Err(FsError::NotDir),
                Err(FsError::NotFound) => mkdir(slice, 0o755)?,
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
/// and sysfs, and vibefs on `/vibe`, and last the working directory.
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
    set_cwd(b"/");
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

/// Report each entry of directory `path` but `.` and `..` to `cb`, with
/// the VFS lock dropped.
pub(crate) fn list_dir(path: &[u8], cb: &mut dyn FnMut(&DirEntry)) -> Result<(), FsError> {
    let f = open(path, OpenFlags::from_bits(O_RDONLY | O_DIRECTORY), 0)?;
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
    let (buf, mut n) = join_cwd(path)?;
    out[..n].copy_from_slice(&buf[..n]);
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
