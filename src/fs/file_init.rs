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
//! Path syscalls keep their reach until ROADMAP §10.4 routes them through
//! `Vfs`: [`open_routed`] resolves FAT and vibefs paths through the
//! backends' route tables, then opens the inode it finds as a `Vfs` file,
//! so reads, writes, seeks and closes share one table and one data path.

use vibeos::fs::{
    DirEntry, FileId, FileRef, FsError, InodeKind, InodeRef, MAX_NAME, MAX_PATH, O_CREAT,
    O_DIRECTORY, O_EXCL, O_RDONLY, O_TRUNC, O_WRONLY, OpenFlags, S_IFREG, SeekFrom, Stat,
    split_basename,
};

use crate::cell::IrqCell;
use crate::fat_init;
use crate::fs_init;
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

static CWD: IrqCell<CwdBuf> = IrqCell::new(cwd_root());

pub(crate) fn cwd_copy() -> ([u8; MAX_PATH], usize) {
    CWD.with(|c| {
        let mut buf = [0u8; MAX_PATH];
        buf[..c.len].copy_from_slice(&c.buf[..c.len]);
        (buf, c.len)
    })
}

pub(crate) fn set_cwd(p: &[u8]) {
    CWD.with(|c| {
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
        b"fat32" => fat_init::mount_dev(src, at, ro).map(|_| ()),
        b"vibefs" => vibefs_init::mount_dev(src, at, ro).map(|_| ()),
        b"ramfs" => fs_init::api()
            .mount_fs(None, at.as_bytes(), &fs_init::RAMFS, None, ro)
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

/// Write back an open file's offset, the one field `read`, `write` and
/// `seek` change: `refs` and `used` are never written from a snapshot,
/// and a slot freed and reused meanwhile fails with `Badf`.
#[allow(dead_code)]
pub fn put_file(f: &FileRef, offset: u64) -> Result<(), FsError> {
    seek(f, SeekFrom::Start(offset)).map(|_| ())
}

pub fn stat_path(path: &[u8]) -> Result<Stat, FsError> {
    with_abs(path, |p| fs_init::api().stat_path(None, p, true))
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn symlink_path(path: &[u8], target: &[u8]) -> Result<(), FsError> {
    with_abs(path, |p| fs_init::api().symlink(None, p, target))
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn link_path(old: &[u8], new: &[u8]) -> Result<(), FsError> {
    let (ob, on) = join_cwd(old)?;
    with_abs(new, |n| fs_init::api().link(None, &ob[..on], n))
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn truncate_path(path: &[u8], size: u64) -> Result<(), FsError> {
    with_abs(path, |p| fs_init::api().truncate(None, p, size))
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

/// Create regular file `path`, or empty it.
#[allow(dead_code)]
pub fn creat(path: &[u8]) -> Result<(), FsError> {
    let f = open(
        path,
        OpenFlags::from_bits(O_WRONLY | O_CREAT | O_TRUNC),
        0o644,
    )?;
    close(f)
}

// ---- path syscalls, until ROADMAP §10.4 routes them through `Vfs` ----

/// The volume a routed path is on, the rest of the path within it, and
/// the backend's walk.
struct Routed<'a> {
    vol: u8,
    rest: &'a [u8],
    walk_iget: fn(u8, &[u8]) -> Result<InodeRef, FsError>,
}

/// The volume serving absolute path `pb`: the longest mount prefix of
/// the FAT and vibefs route tables, the initrd by default.
fn route(pb: &[u8]) -> Routed<'_> {
    let (vv, vs) = vibefs_init::route(pb);
    let (fv, fs) = fat_init::route(pb);
    if vs > fs {
        Routed {
            vol: vv,
            rest: vibefs_init::routed_rest(pb, vs),
            walk_iget: vibefs_init::walk_iget,
        }
    } else {
        Routed {
            vol: fv,
            rest: fat_init::routed_rest(pb, fs),
            walk_iget: fat_init::walk_iget,
        }
    }
}

/// A counted reference to the `Vfs` inode absolute path `pb` names,
/// walked by its backend.
fn walk_abs(pb: &[u8]) -> Result<InodeRef, FsError> {
    let r = route(pb);
    (r.walk_iget)(r.vol, r.rest)
}

/// A counted reference to the directory absolute path `pb` is in, and
/// its last component.
fn vol_parent(pb: &[u8]) -> Result<(InodeRef, &[u8]), FsError> {
    let r = route(pb);
    let (parent, name) = split_basename(r.rest)?;
    if name.len() > MAX_NAME {
        return Err(FsError::NameTooLong);
    }
    let pth: &[u8] = if parent.is_empty() { b"/" } else { parent };
    let dir = (r.walk_iget)(r.vol, pth)?;
    Ok((dir, name))
}

/// Open `path` as the path syscalls do until ROADMAP §10.4: FAT and
/// vibefs only, walked through the route tables, then opened as the
/// `Vfs` file on the inode found.
pub fn open_routed(path: &[u8], flags: OpenFlags, mode: u32) -> Result<FileRef, FsError> {
    #[cfg(feature = "kernel_tests")]
    testing::routed_open();
    let (buf, n) = join_cwd(path)?;
    let pb = &buf[..n];
    let api = fs_init::api();
    let excl = flags.bits() & O_EXCL != 0;
    if flags.bits() & O_CREAT != 0 {
        match walk_abs(pb) {
            Ok(r) => {
                api.put(r);
                if excl {
                    return Err(FsError::Exists);
                }
            }
            Err(FsError::NotFound) => {
                let (dir, name) = vol_parent(pb)?;
                let perm = (mode & 0o7777) as u16;
                let r = api.create_in(&dir, name, InodeKind::Reg, perm | S_IFREG);
                api.put(dir);
                match r {
                    Ok(()) => {}
                    // Created since the walk: without O_EXCL, open it.
                    Err(FsError::Exists) if !excl => {}
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    }
    api.open_inode(walk_abs(pb)?, flags)
}

/// Hooks for the in-guest tests (AGENTS.md rule 9): atomics only, and no
/// wait here is longer than 10,000 `yield_now` calls.
#[cfg(feature = "kernel_tests")]
pub mod testing {
    use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use vibeos::limits::MAX_OPEN_FILES;

    use crate::fs_init;
    use crate::thread_init;

    static WRITE_YIELD: AtomicBool = AtomicBool::new(false);
    static HOLD: AtomicBool = AtomicBool::new(false);
    static HELD: AtomicBool = AtomicBool::new(false);
    static RELEASE: AtomicBool = AtomicBool::new(false);
    static OPEN_RACE: AtomicBool = AtomicBool::new(false);
    static ROUTED: AtomicU32 = AtomicU32::new(0);

    /// The most `yield_now` calls any wait here makes.
    const MAX_YIELDS: u32 = 10_000;

    /// Each [`super::write`] yields once between its backend I/O and its
    /// write-back to the open-file table.
    pub fn set_write_yield(on: bool) {
        WRITE_YIELD.store(on, Ordering::Release);
    }

    /// The next [`super::write`] waits between its backend I/O and its
    /// write-back until [`release_write`].
    pub fn hold_next_write() {
        RELEASE.store(false, Ordering::Release);
        HELD.store(false, Ordering::Release);
        HOLD.store(true, Ordering::Release);
    }

    /// Whether the held write has reached its wait.
    pub fn write_held() -> bool {
        HELD.load(Ordering::Acquire)
    }

    /// Let the held write go on.
    pub fn release_write() {
        RELEASE.store(true, Ordering::Release);
    }

    /// [`super::open`] with `O_CREAT` creates the file itself between its
    /// walk and its create, as another opener would.
    pub fn set_open_race(on: bool) {
        OPEN_RACE.store(on, Ordering::Release);
    }

    /// The `Vfs` File API's `open_race` hook.
    pub fn open_race() -> bool {
        OPEN_RACE.load(Ordering::Acquire)
    }

    /// The `Vfs` File API's `write_window` hook, with the VFS lock
    /// dropped.
    pub fn write_window() {
        if HOLD.swap(false, Ordering::AcqRel) {
            HELD.store(true, Ordering::Release);
            let mut n = 0u32;
            while !RELEASE.load(Ordering::Acquire) && n < MAX_YIELDS {
                thread_init::yield_now();
                n += 1;
            }
            HELD.store(false, Ordering::Release);
        }
        if WRITE_YIELD.load(Ordering::Acquire) {
            thread_init::yield_now();
        }
    }

    pub(super) fn routed_open() {
        ROUTED.fetch_add(1, Ordering::AcqRel);
    }

    /// `(Vfs::open` opens, routed opens) so far.
    pub fn open_counts() -> (u32, u32) {
        let v = fs_init::with(|v| v.stats.opens);
        (v, ROUTED.load(Ordering::Acquire))
    }

    /// Each open-file slot's `(used, refs, gen)`.
    pub fn table() -> [(bool, u16, u16); MAX_OPEN_FILES] {
        fs_init::with(|v| v.file_table())
    }
}

pub fn init() {
    set_cwd(b"/");
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
