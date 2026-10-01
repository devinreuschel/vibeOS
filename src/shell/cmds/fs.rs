//! File commands: `ls`, `cat`, `cp`, `mv`, `rm`, `mkdir`, `touch`, `stat`,
//! `df`, `mount`, `umount`, `sync`, `cd` and `pwd` (ROADMAP §8.6), over
//! `file_init`'s File API.
//!
//! The shell keeps its own working directory, a counted reference
//! ([`CWD_REF`]), and every path command resolves from [`shell_base`]:
//! a relative path from that directory, an absolute one from `/`. The
//! shell's commands run on one thread at a time (the REPL, or the ktest
//! registry), so a base a command copied stays live until it returns:
//! only `cd`, on that thread, puts the reference it names.
//!
//! Each command is a function `fn(args, out) -> Result<(), FsError>` that
//! writes its output to an [`Out`] sink, so an in-guest test can call it
//! with a buffer; the registered [`Command`] runs it on [`ConsoleOut`] and
//! prints `vibeOS: <cmd>: <error>` for the error it returns. A command
//! over several paths prints each earlier path's error itself and returns
//! the last one.

use core::fmt::{self, Write};

use vibeos::fs::{
    DirRef, FsError, InodeKind, MAX_NAME, MAX_PATH, O_CREAT, O_DIRECTORY, O_RDONLY, O_TRUNC,
    O_WRONLY, OpenFlags, PathRef, WalkBase,
};
use vibeos::lock::RANK_DEVICE;
use vibeos::shell::Command;

use crate::fat_init::{self, FatVolume};
use crate::file_init::{
    child_path, close, dir_get_at, dir_path, dir_put, list_dir, mkdir_at, mkdir_p_at, mount_at,
    open_at, read, readdir, rename_at, rmdir_at, stat_at, sync_fs, umount_at, unlink_at, write,
};
use crate::fs_init;
use crate::sync_init::SpinMutex;
use crate::vibefs_init::{self, VibeVolume};

/// Where a command's output goes: the console, or a test's buffer.
pub(crate) trait Out {
    fn put(&mut self, bytes: &[u8]);
}

/// The console (`console_init::write`).
pub(crate) struct ConsoleOut;

impl Out for ConsoleOut {
    fn put(&mut self, bytes: &[u8]) {
        crate::console_init::write(bytes);
    }
}

/// [`fmt::Write`] over an [`Out`], for formatted output.
struct Fmt<'a>(&'a mut dyn Out);

impl Write for Fmt<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.put(s.as_bytes());
        Ok(())
    }
}

/// Write `args` to `out`.
fn putf(out: &mut dyn Out, args: fmt::Arguments<'_>) {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "a write to an `Out` cannot fail: `Fmt::write_str` always returns `Ok` (DESIGN §2.5)"
    )]
    let _ = Fmt(out).write_fmt(args);
}

/// The shell's working directory: a reference to it, or `None` for `/`.
/// Never held across a File API call, which sleeps for the VFS lock.
static CWD_REF: SpinMutex<Option<DirRef>> = SpinMutex::with_rank(None, RANK_DEVICE);

/// The directory the shell's working-directory reference holds; `None`
/// for `/`.
pub(crate) fn shell_cwd() -> Option<PathRef> {
    CWD_REF.lock().as_ref().map(DirRef::at)
}

/// The walk base of the shell's path commands: the namespace root, and
/// its working directory. `None` means `/` for both.
pub(crate) fn shell_base() -> Option<WalkBase> {
    let cwd = shell_cwd()?;
    let root = fs_init::with(|v| v.root()).ok()?;
    Some(WalkBase { root, cwd })
}

/// Make the directory `path` names, from the shell's base, its working
/// directory; the old reference is put after the lock is released.
pub(crate) fn cd(path: &[u8]) -> Result<(), FsError> {
    let new = dir_get_at(shell_base(), path)?;
    let old = CWD_REF.lock().replace(new);
    if let Some(o) = old {
        dir_put(o);
    }
    Ok(())
}

pub(crate) const COMMANDS: &[Command] = &[
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

fn err_line(out: &mut dyn Out, op: &str, e: FsError) {
    putf(out, format_args!("vibeOS: {op}: {}\n", e.as_str()));
}

/// Run command `f` on the console; print the error it returns.
fn on_console(op: &str, f: fn(&[&str], &mut dyn Out) -> Result<(), FsError>, args: &[&str]) {
    let mut out = ConsoleOut;
    if let Err(e) = f(args, &mut out) {
        err_line(&mut out, op, e);
    }
}

/// Run `f` on each path in `args` after the command name, with whether
/// `flag` came before it; `flag` itself is no path. An error but the last
/// is printed here, and the last path's result is returned.
fn each(
    out: &mut dyn Out,
    op: &str,
    args: &[&str],
    flag: Option<&str>,
    mut f: impl FnMut(&str, bool, &mut dyn Out) -> Result<(), FsError>,
) -> Result<(), FsError> {
    let mut on = false;
    let mut last = Ok(());
    for &a in args.iter().skip(1) {
        if Some(a) == flag {
            on = true;
            continue;
        }
        if let Err(e) = core::mem::replace(&mut last, f(a, on, out)) {
            err_line(out, op, e);
        }
    }
    last
}

fn cmd_ls(args: &[&str]) {
    on_console("ls", ls, args);
}

fn cmd_cat(args: &[&str]) {
    on_console("cat", cat, args);
}

fn cmd_cp(args: &[&str]) {
    on_console("cp", cp, args);
}

fn cmd_mv(args: &[&str]) {
    on_console("mv", mv, args);
}

fn cmd_rm(args: &[&str]) {
    on_console("rm", rm, args);
}

fn cmd_mkdir(args: &[&str]) {
    on_console("mkdir", mkdir, args);
}

fn cmd_touch(args: &[&str]) {
    on_console("touch", touch, args);
}

fn cmd_stat(args: &[&str]) {
    on_console("stat", stat, args);
}

fn cmd_df(args: &[&str]) {
    on_console("df", df, args);
}

fn cmd_mount(args: &[&str]) {
    on_console("mount", mount, args);
}

fn cmd_umount(args: &[&str]) {
    on_console("umount", umount, args);
}

fn cmd_sync(args: &[&str]) {
    on_console("sync", sync, args);
}

/// `ls [-l] [path]`: a directory's entries but `.` and `..`, with `-l`
/// each one's kind and size; anything else, its own name.
pub(crate) fn ls(args: &[&str], out: &mut dyn Out) -> Result<(), FsError> {
    let mut long = false;
    let mut path = ".";
    for &a in args.iter().skip(1) {
        match a {
            "-l" => long = true,
            s => path = s,
        }
    }
    let st = stat_at(shell_base(), path.as_bytes())?;
    if st.kind != InodeKind::Dir {
        if long {
            putf(
                out,
                format_args!("{} {} {}\n", st.kind.as_str(), st.size, path),
            );
        } else {
            putf(out, format_args!("{path}\n"));
        }
        return Ok(());
    }
    list_dir(shell_base(), path.as_bytes(), &mut |d| {
        let name = d.name.as_bytes();
        if long {
            let mut child = [0u8; MAX_PATH];
            let size = child_path(path.as_bytes(), name, &mut child)
                .and_then(|n| stat_at(shell_base(), &child[..n]))
                .map_or(0, |s| s.size);
            putf(out, format_args!("{} {:>8} ", d.kind.as_str(), size));
        }
        out.put(name);
        out.put(b"\n");
    })
}

/// `cat path...`: each file's bytes.
pub(crate) fn cat(args: &[&str], out: &mut dyn Out) -> Result<(), FsError> {
    if args.len() < 2 {
        return Err(FsError::Inval);
    }
    each(out, "cat", args, None, |p, _, out| {
        let f = open_at(
            shell_base(),
            p.as_bytes(),
            OpenFlags::from_bits(O_RDONLY),
            0,
        )?;
        let mut buf = [0u8; 512];
        let r = loop {
            match read(&f, &mut buf) {
                Ok(0) => break Ok(()),
                Ok(n) => out.put(buf.get(..n).unwrap_or(&[])),
                Err(e) => break Err(e),
            }
        };
        let c = close(f);
        r.and(c)
    })
}

/// `cp src dst`: copy `src`'s bytes to `dst`, created or emptied.
pub(crate) fn cp(args: &[&str], _out: &mut dyn Out) -> Result<(), FsError> {
    let (Some(s), Some(d)) = (args.get(1), args.get(2)) else {
        return Err(FsError::Inval);
    };
    let src = open_at(
        shell_base(),
        s.as_bytes(),
        OpenFlags::from_bits(O_RDONLY),
        0,
    )?;
    let dflags = OpenFlags::from_bits(O_WRONLY | O_CREAT | O_TRUNC);
    let r = match open_at(shell_base(), d.as_bytes(), dflags, 0o644) {
        Ok(dst) => {
            let r = copy(&src, &dst);
            let c = close(dst);
            r.and(c)
        }
        Err(e) => Err(e),
    };
    let c = close(src);
    r.and(c)
}

fn copy(src: &vibeos::fs::FileRef, dst: &vibeos::fs::FileRef) -> Result<(), FsError> {
    let mut buf = [0u8; 512];
    loop {
        let n = read(src, &mut buf)?;
        if n == 0 {
            return Ok(());
        }
        let chunk = buf.get(..n).ok_or(FsError::Io)?;
        if write(dst, chunk)? != n {
            return Err(FsError::Io);
        }
    }
}

/// `mv old new`: rename.
pub(crate) fn mv(args: &[&str], _out: &mut dyn Out) -> Result<(), FsError> {
    let (Some(old), Some(new)) = (args.get(1), args.get(2)) else {
        return Err(FsError::Inval);
    };
    rename_at(shell_base(), old.as_bytes(), new.as_bytes())
}

/// `rm [-r] path...`: unlink each path; after `-r`, a directory and all
/// it holds.
pub(crate) fn rm(args: &[&str], out: &mut dyn Out) -> Result<(), FsError> {
    each(out, "rm", args, Some("-r"), |p, rec, _| {
        if rec {
            rm_r(shell_base(), p.as_bytes())
        } else {
            unlink_at(shell_base(), p.as_bytes())
        }
    })
}

/// `mkdir [-p] path...`: make each directory; after `-p`, with every
/// missing parent.
pub(crate) fn mkdir(args: &[&str], out: &mut dyn Out) -> Result<(), FsError> {
    each(out, "mkdir", args, Some("-p"), |p, parents, _| {
        if parents {
            mkdir_p_at(shell_base(), p.as_bytes())
        } else {
            mkdir_at(shell_base(), p.as_bytes(), 0o755)
        }
    })
}

/// `touch path...`: create each file that is missing.
pub(crate) fn touch(args: &[&str], out: &mut dyn Out) -> Result<(), FsError> {
    if args.len() < 2 {
        return Err(FsError::Inval);
    }
    each(out, "touch", args, None, |p, _, _| {
        let flags = OpenFlags::from_bits(O_WRONLY | O_CREAT);
        close(open_at(shell_base(), p.as_bytes(), flags, 0o644)?)
    })
}

/// `stat [path]`: inode number, kind, size and mode.
pub(crate) fn stat(args: &[&str], out: &mut dyn Out) -> Result<(), FsError> {
    let path = args.get(1).copied().unwrap_or(".");
    let s = stat_at(shell_base(), path.as_bytes())?;
    putf(
        out,
        format_args!(
            "vibeOS: stat: ino {} {} size {} mode {:o}\n",
            s.ino,
            s.kind.as_str(),
            s.size,
            s.mode
        ),
    );
    Ok(())
}

/// `df`: the root FAT volume's space, then `/vibe`'s when vibefs is live.
pub(crate) fn df(_args: &[&str], out: &mut dyn Out) -> Result<(), FsError> {
    let root = fs_init::volume_at(b"/");
    let fat = root
        .as_ref()
        .map_err(|e| *e)
        .and_then(|v| v.downcast_ref::<FatVolume>().ok_or(FsError::Inval));
    let r = fat.and_then(fat_init::df).map(|(ft, tot, free, nclus)| {
        putf(
            out,
            format_args!(
                "vibeOS: df: {} total {} free {} clusters {}\n",
                ft.as_str(),
                tot,
                free,
                nclus
            ),
        );
    });
    let vibe = fs_init::volume_at(b"/vibe");
    if vibefs_init::live()
        && let Ok(v) = vibe.as_ref()
        && let Some(v) = v.downcast_ref::<VibeVolume>()
        && let Ok((ft, tot, free, nblk)) = vibefs_init::df(v)
    {
        if let Err(e) = r {
            err_line(out, "df", e);
        }
        putf(
            out,
            format_args!(
                "vibeOS: df: {} total {} free {} blocks {}\n",
                ft.as_str(),
                tot,
                free,
                nblk
            ),
        );
        return Ok(());
    }
    r
}

/// `mount fat32|vibefs <dev> <path>` or `mount ramfs <path>`: mount on
/// `path`, made with its parents first.
pub(crate) fn mount(args: &[&str], out: &mut dyn Out) -> Result<(), FsError> {
    let Some(&fstype) = args.get(1) else {
        out.put(b"vibeOS: mount: fat32 <dev> <path> | vibefs <dev> <path> | ramfs <path>\n");
        return Ok(());
    };
    let (source, target) = match (fstype, args.get(2), args.get(3)) {
        ("fat32" | "vibefs", Some(&s), Some(&t)) => (s, t),
        ("ramfs", Some(&t), _) => ("none", t),
        _ => return Err(FsError::Inval),
    };
    mkdir_p_at(shell_base(), target.as_bytes())?;
    mount_at(
        shell_base(),
        source.as_bytes(),
        target.as_bytes(),
        fstype.as_bytes(),
        false,
    )?;
    if fstype == "ramfs" {
        putf(out, format_args!("vibeOS: mount: ramfs on {target}\n"));
    } else {
        putf(
            out,
            format_args!("vibeOS: mount: {fstype} {source} on {target}\n"),
        );
    }
    Ok(())
}

/// `umount path`: unmount the mount whose root `path` names.
pub(crate) fn umount(args: &[&str], _out: &mut dyn Out) -> Result<(), FsError> {
    let path = args.get(1).ok_or(FsError::Inval)?;
    umount_at(shell_base(), path.as_bytes())
}

/// `sync`: write every filesystem's dirty state to its device.
pub(crate) fn sync(_args: &[&str], _out: &mut dyn Out) -> Result<(), FsError> {
    sync_fs()
}

fn cmd_cd(args: &[&str]) {
    let path = args.get(1).copied().unwrap_or("/");
    if let Err(e) = cd(path.as_bytes()) {
        err_line(&mut ConsoleOut, "cd", e);
    }
}

fn cmd_pwd(_args: &[&str]) {
    let Some(cwd) = shell_cwd() else {
        crate::console_init::write(b"/\n");
        return;
    };
    let mut buf = [0u8; MAX_PATH];
    match dir_path(None, cwd, &mut buf) {
        Ok(n) => {
            crate::console_init::write(buf.get(..n).unwrap_or(b"?"));
            crate::console_init::write(b"\n");
        }
        Err(e) => err_line(&mut ConsoleOut, "pwd", e),
    }
}

/// Most directory levels `rm -r` descends below its argument: each level
/// adds at least two bytes (`/` and a name) to a path of at most
/// `MAX_PATH`.
const RM_MAX_DEPTH: usize = MAX_PATH / 2;
const _: () = assert!(MAX_PATH <= u16::MAX as usize && RM_MAX_DEPTH * 2 <= MAX_PATH);

/// One directory entry `rm -r` acts on: its name and kind.
struct Entry {
    name: [u8; MAX_NAME],
    len: usize,
    kind: InodeKind,
}

/// `path` and everything below it, without recursion: one path buffer
/// and an explicit stack of the end offsets of the directories above the
/// current one. Each step takes the current directory's first entry
/// other than `.` and `..`: a directory is pushed, anything else (a
/// symlink included, never followed) unlinked; an empty directory is
/// removed and popped. Each step removes an entry or goes one level
/// deeper, and the depth is bounded, so the walk ends. A child path past
/// `MAX_PATH`, or a stack full, is `NameTooLong`.
fn rm_r(base: Option<WalkBase>, path: &[u8]) -> Result<(), FsError> {
    let st = fs_init::api().stat_path(base, path, false)?;
    if st.kind != InodeKind::Dir {
        return unlink_at(base, path);
    }
    let mut buf = [0u8; MAX_PATH];
    buf.get_mut(..path.len())
        .ok_or(FsError::NameTooLong)?
        .copy_from_slice(path);
    let mut len = path.len();
    let mut ends = [0u16; RM_MAX_DEPTH];
    let mut depth = 0usize;
    loop {
        let cur = buf.get(..len).ok_or(FsError::NameTooLong)?;
        let Some(e) = first_entry(base, cur)? else {
            rmdir_at(base, cur)?;
            let Some(up) = depth.checked_sub(1) else {
                return Ok(());
            };
            depth = up;
            len = usize::from(*ends.get(up).ok_or(FsError::NameTooLong)?);
            continue;
        };
        let child = append(&mut buf, len, e.name.get(..e.len).unwrap_or(&[]))?;
        if e.kind == InodeKind::Dir {
            let slot = ends.get_mut(depth).ok_or(FsError::NameTooLong)?;
            *slot = u16::try_from(len).map_err(|_| FsError::NameTooLong)?;
            depth = depth.checked_add(1).ok_or(FsError::NameTooLong)?;
            len = child;
        } else {
            unlink_at(base, buf.get(..child).ok_or(FsError::NameTooLong)?)?;
        }
    }
}

/// The first entry of directory `dir` other than `.` and `..`; `None`
/// when it is empty. The `readdir` callback only copies: nothing calls
/// the File API from inside it.
fn first_entry(base: Option<WalkBase>, dir: &[u8]) -> Result<Option<Entry>, FsError> {
    let f = open_at(base, dir, OpenFlags::from_bits(O_RDONLY | O_DIRECTORY), 0)?;
    let mut found: Option<Entry> = None;
    let r = readdir(&f, &mut |d| {
        let n = d.name.as_bytes();
        if n == b"." || n == b".." {
            return true;
        }
        let mut e = Entry {
            name: [0; MAX_NAME],
            len: n.len(),
            kind: d.kind,
        };
        match e.name.get_mut(..n.len()) {
            Some(dst) => {
                dst.copy_from_slice(n);
                found = Some(e);
                false
            }
            None => true,
        }
    });
    let c = close(f);
    r.and(c)?;
    Ok(found)
}

/// Append `/name` to the path `buf[..len]` (no `/` after one already
/// there); the new length, or `NameTooLong` past `MAX_PATH`.
fn append(buf: &mut [u8; MAX_PATH], len: usize, name: &[u8]) -> Result<usize, FsError> {
    let mut n = len;
    if buf.get(..len).and_then(|b| b.last()) != Some(&b'/') {
        *buf.get_mut(n).ok_or(FsError::NameTooLong)? = b'/';
        n = n.checked_add(1).ok_or(FsError::NameTooLong)?;
    }
    let end = n.checked_add(name.len()).ok_or(FsError::NameTooLong)?;
    buf.get_mut(n..end)
        .ok_or(FsError::NameTooLong)?
        .copy_from_slice(name);
    Ok(end)
}
