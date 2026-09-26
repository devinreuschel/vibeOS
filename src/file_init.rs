//! Kernel File API + shell file commands. ROADMAP §8.6 / §10.4.
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
use vibeos::shell::{Command, LineEditor, MAX_COMMANDS};

use crate::cell::IrqCell;
use crate::console_init::Console;
use crate::fat_init;
use crate::fs_init;
use crate::shell_init;
use crate::vibefs_init;

use core::fmt::Write;

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

fn cwd_copy() -> ([u8; MAX_PATH], usize) {
    CWD.with(|c| {
        let mut buf = [0u8; MAX_PATH];
        buf[..c.len].copy_from_slice(&c.buf[..c.len]);
        (buf, c.len)
    })
}

fn set_cwd(p: &[u8]) {
    CWD.with(|c| {
        let n = p.len().min(MAX_PATH);
        c.buf[..n].copy_from_slice(&p[..n]);
        c.len = n;
    });
}

/// `path`, made absolute against the working directory, and its length.
fn join_cwd(p: &[u8]) -> Result<([u8; MAX_PATH], usize), FsError> {
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
    #[allow(dead_code)]
    pub fn open_counts() -> (u32, u32) {
        let v = fs_init::with(|v| v.stats.opens);
        (v, ROUTED.load(Ordering::Acquire))
    }

    /// Each open-file slot's `(used, refs, gen)`.
    pub fn table() -> [(bool, u16, u16); MAX_OPEN_FILES] {
        fs_init::with(|v| v.file_table())
    }
}

// ---- shell ----

fn err_line(op: &str, e: FsError) {
    let _ = writeln!(Console, "vibeOS: {op}: {}", e.as_str());
}

pub fn init() {
    set_cwd(b"/");
    let cmds = [
        Command {
            name: "ls",
            help: "list directory; -l long",
            run: cmd_ls,
        },
        Command {
            name: "cat",
            help: "print file",
            run: cmd_cat,
        },
        Command {
            name: "cp",
            help: "copy file",
            run: cmd_cp,
        },
        Command {
            name: "mv",
            help: "rename / move",
            run: cmd_mv,
        },
        Command {
            name: "rm",
            help: "unlink; -r recursive",
            run: cmd_rm,
        },
        Command {
            name: "mkdir",
            help: "create directory; -p parents",
            run: cmd_mkdir,
        },
        Command {
            name: "touch",
            help: "create or bump file",
            run: cmd_touch,
        },
        Command {
            name: "stat",
            help: "inode metadata",
            run: cmd_stat,
        },
        Command {
            name: "df",
            help: "volume space",
            run: cmd_df,
        },
        Command {
            name: "mount",
            help: "mount fat32|vibefs <dev> <path> | ramfs <path>",
            run: cmd_mount,
        },
        Command {
            name: "umount",
            help: "unmount path",
            run: cmd_umount,
        },
        Command {
            name: "sync",
            help: "flush FAT + vibefs + block Flush",
            run: cmd_sync,
        },
        Command {
            name: "cd",
            help: "change directory",
            run: cmd_cd,
        },
        Command {
            name: "pwd",
            help: "print cwd",
            run: cmd_pwd,
        },
    ];
    for c in cmds {
        let _ = shell_init::register(c);
    }
    let _ = MAX_COMMANDS;
}

/// Report each entry of directory `path` but `.` and `..` to `cb`, with
/// the VFS lock dropped.
fn list_dir(path: &[u8], cb: &mut dyn FnMut(&DirEntry)) -> Result<(), FsError> {
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
fn child_path(path: &[u8], name: &[u8], out: &mut [u8; MAX_PATH]) -> Result<usize, FsError> {
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

fn rm_r(path: &[u8]) -> Result<(), FsError> {
    let st = stat_path(path)?;
    if st.kind != InodeKind::Dir {
        return unlink(path);
    }
    let mut kids: [[u8; MAX_NAME]; 16] = [[0; MAX_NAME]; 16];
    let mut klens = [0u8; 16];
    let mut nk = 0usize;
    list_dir(path, &mut |d| {
        let n = d.name.as_bytes();
        if nk < 16 {
            kids[nk][..n.len()].copy_from_slice(n);
            klens[nk] = n.len() as u8;
            nk += 1;
        }
    })?;
    let mut i = 0usize;
    while i < nk {
        let mut child = [0u8; MAX_PATH];
        let n = child_path(path, &kids[i][..klens[i] as usize], &mut child)?;
        rm_r(&child[..n])?;
        i += 1;
    }
    rmdir(path)
}

#[cfg_attr(
    not(all(not(feature = "kernel_tests"), feature = "kernel_shell")),
    allow(dead_code)
)]
fn names_in(
    dirp: &[u8],
    prefix: &[u8],
    out: &mut [[u8; MAX_NAME]; 16],
    lens: &mut [u8; 16],
) -> usize {
    let path: &[u8] = if dirp.is_empty() { b"." } else { dirp };
    let mut n = 0usize;
    let _ = list_dir(path, &mut |d| {
        let nm = d.name.as_bytes();
        if nm.len() >= prefix.len() && nm[..prefix.len()].eq_ignore_ascii_case(prefix) && n < 16 {
            let l = nm.len().min(MAX_NAME);
            out[n][..l].copy_from_slice(&nm[..l]);
            lens[n] = l as u8;
            n += 1;
        }
    });
    n
}

/// Tab: complete the word at the cursor against the current directory
/// (or the directory prefix of that word).
#[cfg_attr(
    not(all(not(feature = "kernel_tests"), feature = "kernel_shell")),
    allow(dead_code)
)]
pub fn complete_line(ed: &mut LineEditor) {
    let mut line_buf = [0u8; 128];
    let line_n = ed.line().len().min(128);
    line_buf[..line_n].copy_from_slice(&ed.line()[..line_n]);
    let line = &line_buf[..line_n];
    let cur = ed.cursor();
    let mut start = cur;
    while start > 0 && line[start - 1] != b' ' && line[start - 1] != b'\t' {
        start -= 1;
    }
    let word = &line[start..cur];
    let first = {
        let mut i = 0usize;
        while i < start && (line[i] == b' ' || line[i] == b'\t') {
            i += 1;
        }
        i == start
    };
    if first && !word.contains(&b'/') {
        complete_cmd(ed, start, word);
        return;
    }
    let slash = word.iter().rposition(|&c| c == b'/');
    let (dirp, pref) = match slash {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => (&b""[..], word),
    };
    let mut names = [[0u8; MAX_NAME]; 16];
    let mut lens = [0u8; 16];
    let n = names_in(dirp, pref, &mut names, &mut lens);
    if n == 0 {
        return;
    }
    let common = common_prefix(&names, &lens, n);
    if common <= pref.len() && n > 1 {
        return;
    }
    let fill = &names[0][..common];
    apply_word(ed, start, cur, dirp, fill, n == 1);
}

#[cfg_attr(
    not(all(not(feature = "kernel_tests"), feature = "kernel_shell")),
    allow(dead_code)
)]
fn complete_cmd(ed: &mut LineEditor, start: usize, pref: &[u8]) {
    let mut hit: Option<&'static str> = None;
    let mut n = 0u32;
    let names = [
        "help", "echo", "meminfo", "uptime", "cpus", "dmesg", "ps", "panic", "reboot", "poweroff",
        "ls", "cat", "cp", "mv", "rm", "mkdir", "touch", "stat", "df", "mount", "umount", "sync",
        "cd", "pwd", "blk", "lspci", "devices",
    ];
    for nm in names {
        if nm.as_bytes().starts_with(pref) {
            n += 1;
            hit = Some(nm);
            if n > 1 {
                break;
            }
        }
    }
    if n == 1
        && let Some(h) = hit
    {
        apply_word(ed, start, ed.cursor(), b"", h.as_bytes(), true);
    }
    let _ = shell_init::command_count();
}

#[cfg_attr(
    not(all(not(feature = "kernel_tests"), feature = "kernel_shell")),
    allow(dead_code)
)]
fn common_prefix(names: &[[u8; MAX_NAME]; 16], lens: &[u8; 16], n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let mut c = lens[0] as usize;
    let mut i = 1usize;
    while i < n {
        let mut k = 0usize;
        let a = &names[0][..c.min(lens[0] as usize)];
        let b = &names[i][..lens[i] as usize];
        while k < a.len() && k < b.len() && to_up(a[k]) == to_up(b[k]) {
            k += 1;
        }
        if k < c {
            c = k;
        }
        i += 1;
    }
    c
}

#[cfg_attr(
    not(all(not(feature = "kernel_tests"), feature = "kernel_shell")),
    allow(dead_code)
)]
fn to_up(c: u8) -> u8 {
    if c.is_ascii_lowercase() {
        c - b'a' + b'A'
    } else {
        c
    }
}

#[cfg_attr(
    not(all(not(feature = "kernel_tests"), feature = "kernel_shell")),
    allow(dead_code)
)]
fn apply_word(
    ed: &mut LineEditor,
    start: usize,
    cur: usize,
    dirp: &[u8],
    name: &[u8],
    unique: bool,
) {
    let mut tmp = [0u8; 128];
    let mut n = 0usize;
    n += copy_to(&mut tmp[n..], dirp);
    n += copy_to(&mut tmp[n..], name);
    if unique {
        n += copy_to(&mut tmp[n..], b" ");
    }
    let line = ed.line();
    let rest = if cur < line.len() { &line[cur..] } else { &[] };
    let mut neu = [0u8; 128];
    let mut m = 0usize;
    m += copy_to(&mut neu[m..], &line[..start]);
    m += copy_to(&mut neu[m..], &tmp[..n]);
    m += copy_to(&mut neu[m..], rest);
    ed.set_line(&neu[..m]);
}

#[cfg_attr(
    not(all(not(feature = "kernel_tests"), feature = "kernel_shell")),
    allow(dead_code)
)]
fn copy_to(dst: &mut [u8], src: &[u8]) -> usize {
    let n = src.len().min(dst.len());
    dst[..n].copy_from_slice(&src[..n]);
    n
}

fn cmd_ls(args: &[&str]) {
    let mut long = false;
    let mut path = ".";
    let mut i = 1usize;
    while i < args.len() {
        match args[i] {
            "-l" => long = true,
            s => path = s,
        }
        i += 1;
    }
    let st = match stat_path(path.as_bytes()) {
        Ok(s) => s,
        Err(e) => {
            err_line("ls", e);
            return;
        }
    };
    if st.kind != InodeKind::Dir {
        if long {
            let _ = writeln!(Console, "{} {} {}", st.kind.as_str(), st.size, path);
        } else {
            let _ = writeln!(Console, "{path}");
        }
        return;
    }
    let r = list_dir(path.as_bytes(), &mut |d| {
        let name = d.name.as_bytes();
        if long {
            let mut child = [0u8; MAX_PATH];
            let size = child_path(path.as_bytes(), name, &mut child)
                .and_then(|n| stat_path(&child[..n]))
                .map_or(0, |s| s.size);
            let _ = write!(Console, "{} {:>8} ", d.kind.as_str(), size);
        }
        crate::console_init::write(name);
        crate::console_init::write(b"\n");
    });
    if let Err(e) = r {
        err_line("ls", e);
    }
}

fn cmd_cat(args: &[&str]) {
    if args.len() < 2 {
        err_line("cat", FsError::Inval);
        return;
    }
    let mut i = 1usize;
    while i < args.len() {
        match open(args[i].as_bytes(), OpenFlags::from_bits(O_RDONLY), 0) {
            Ok(f) => {
                let mut buf = [0u8; 512];
                loop {
                    match read(&f, &mut buf) {
                        Ok(0) => break,
                        Ok(n) => crate::console_init::write(&buf[..n]),
                        Err(e) => {
                            err_line("cat", e);
                            break;
                        }
                    }
                }
                let _ = close(f);
            }
            Err(e) => err_line("cat", e),
        }
        i += 1;
    }
}

fn cmd_cp(args: &[&str]) {
    if args.len() < 3 {
        err_line("cp", FsError::Inval);
        return;
    }
    let src = match open(args[1].as_bytes(), OpenFlags::from_bits(O_RDONLY), 0) {
        Ok(f) => f,
        Err(e) => {
            err_line("cp", e);
            return;
        }
    };
    let dflags = OpenFlags::from_bits(O_WRONLY | O_CREAT | O_TRUNC);
    match open(args[2].as_bytes(), dflags, 0o644) {
        Ok(dst) => {
            let mut buf = [0u8; 512];
            loop {
                match read(&src, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if write(&dst, &buf[..n]).is_err() {
                            err_line("cp", FsError::Io);
                            break;
                        }
                    }
                    Err(e) => {
                        err_line("cp", e);
                        break;
                    }
                }
            }
            let _ = close(dst);
        }
        Err(e) => err_line("cp", e),
    }
    let _ = close(src);
}

fn cmd_mv(args: &[&str]) {
    if args.len() < 3 {
        err_line("mv", FsError::Inval);
        return;
    }
    if let Err(e) = rename(args[1].as_bytes(), args[2].as_bytes()) {
        err_line("mv", e);
    }
}

fn cmd_rm(args: &[&str]) {
    let mut rec = false;
    let mut i = 1usize;
    while i < args.len() {
        if args[i] == "-r" {
            rec = true;
            i += 1;
            continue;
        }
        let r = if rec {
            rm_r(args[i].as_bytes())
        } else {
            unlink(args[i].as_bytes())
        };
        if let Err(e) = r {
            err_line("rm", e);
        }
        i += 1;
    }
}

fn cmd_mkdir(args: &[&str]) {
    let mut p = false;
    let mut i = 1usize;
    while i < args.len() {
        if args[i] == "-p" {
            p = true;
            i += 1;
            continue;
        }
        let r = if p {
            mkdir_p(args[i].as_bytes())
        } else {
            mkdir(args[i].as_bytes(), 0o755)
        };
        if let Err(e) = r {
            err_line("mkdir", e);
        }
        i += 1;
    }
}

fn cmd_touch(args: &[&str]) {
    if args.len() < 2 {
        err_line("touch", FsError::Inval);
        return;
    }
    let mut i = 1usize;
    while i < args.len() {
        match open(
            args[i].as_bytes(),
            OpenFlags::from_bits(O_WRONLY | O_CREAT),
            0o644,
        ) {
            Ok(f) => {
                let _ = close(f);
            }
            Err(e) => err_line("touch", e),
        }
        i += 1;
    }
}

fn cmd_stat(args: &[&str]) {
    let path = args.get(1).copied().unwrap_or(".");
    match stat_path(path.as_bytes()) {
        Ok(s) => {
            let _ = writeln!(
                Console,
                "vibeOS: stat: ino {} {} size {} mode {:o}",
                s.ino,
                s.kind.as_str(),
                s.size,
                s.mode
            );
        }
        Err(e) => err_line("stat", e),
    }
}

fn cmd_df(_args: &[&str]) {
    match fat_init::df(fat_init::VOL_INITRD) {
        Ok((ft, tot, free, nclus)) => {
            let _ = writeln!(
                Console,
                "vibeOS: df: {} total {} free {} clusters {}",
                ft.as_str(),
                tot,
                free,
                nclus
            );
        }
        Err(e) => err_line("df", e),
    }
    if vibefs_init::live()
        && let Ok((ft, tot, free, nblk)) = vibefs_init::df(vibefs_init::VOL_MEM)
    {
        let _ = writeln!(
            Console,
            "vibeOS: df: {} total {} free {} blocks {}",
            ft.as_str(),
            tot,
            free,
            nblk
        );
    }
}

fn cmd_mount(args: &[&str]) {
    if args.len() < 2 {
        let _ = writeln!(
            Console,
            "vibeOS: mount: fat32 <dev> <path> | vibefs <dev> <path> | ramfs <path>"
        );
        return;
    }
    let (source, target) = match args[1] {
        "fat32" | "vibefs" if args.len() >= 4 => (args[2], args[3]),
        "ramfs" if args.len() >= 3 => ("none", args[2]),
        _ => {
            err_line("mount", FsError::Inval);
            return;
        }
    };
    let _ = mkdir_p(target.as_bytes());
    match mount(
        source.as_bytes(),
        target.as_bytes(),
        args[1].as_bytes(),
        false,
    ) {
        Ok(()) if args[1] == "ramfs" => {
            let _ = writeln!(Console, "vibeOS: mount: ramfs on {target}");
        }
        Ok(()) => {
            let _ = writeln!(Console, "vibeOS: mount: {} {source} on {target}", args[1]);
        }
        Err(e) => err_line("mount", e),
    }
}

fn cmd_umount(args: &[&str]) {
    if args.len() < 2 {
        err_line("umount", FsError::Inval);
        return;
    }
    if let Err(e) = umount(args[1].as_bytes()) {
        err_line("umount", e);
    }
}

fn cmd_sync(_args: &[&str]) {
    if let Err(e) = sync_fs() {
        err_line("sync", e);
    }
}

fn cmd_cd(args: &[&str]) {
    let path = args.get(1).copied().unwrap_or("/");
    let (abs, n) = match join_cwd(path.as_bytes()) {
        Ok(a) => a,
        Err(e) => {
            err_line("cd", e);
            return;
        }
    };
    match stat_path(path.as_bytes()) {
        Ok(s) if s.kind == InodeKind::Dir => {
            if n == 0 {
                set_cwd(b"/");
            } else {
                set_cwd(&abs[..n]);
            }
        }
        Ok(_) => err_line("cd", FsError::NotDir),
        Err(e) => err_line("cd", e),
    }
}

fn cmd_pwd(_args: &[&str]) {
    let (b, n) = cwd_copy();
    crate::console_init::write(&b[..n]);
    crate::console_init::write(b"\n");
}
