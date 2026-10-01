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

use core::fmt::Write;

use vibeos::fs::{
    DirRef, FsError, InodeKind, MAX_NAME, MAX_PATH, O_CREAT, O_RDONLY, O_TRUNC, O_WRONLY,
    OpenFlags, PathRef, WalkBase,
};
use vibeos::lock::RANK_DEVICE;
use vibeos::shell::Command;

use crate::console_init::Console;
use crate::fat_init::{self, FatVolume};
use crate::file_init::{
    child_path, close, dir_get_at, dir_path, dir_put, list_dir, mkdir_at, mkdir_p_at, mount_at,
    open_at, read, rename_at, rmdir_at, stat_at, sync_fs, umount_at, unlink_at, write,
};
use crate::fs_init;
use crate::sync_init::SpinMutex;
use crate::vibefs_init::{self, VibeVolume};

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

fn err_line(op: &str, e: FsError) {
    #[expect(
        clippy::let_underscore_must_use,
        reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
    )]
    let _ = writeln!(Console, "vibeOS: {op}: {}", e.as_str());
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
    let st = match stat_at(shell_base(), path.as_bytes()) {
        Ok(s) => s,
        Err(e) => {
            err_line("ls", e);
            return;
        }
    };
    if st.kind != InodeKind::Dir {
        if long {
            #[expect(
                clippy::let_underscore_must_use,
                reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
            )]
            let _ = writeln!(Console, "{} {} {}", st.kind.as_str(), st.size, path);
        } else {
            #[expect(
                clippy::let_underscore_must_use,
                reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
            )]
            let _ = writeln!(Console, "{path}");
        }
        return;
    }
    let r = list_dir(shell_base(), path.as_bytes(), &mut |d| {
        let name = d.name.as_bytes();
        if long {
            let mut child = [0u8; MAX_PATH];
            let size = child_path(path.as_bytes(), name, &mut child)
                .and_then(|n| stat_at(shell_base(), &child[..n]))
                .map_or(0, |s| s.size);
            #[expect(
                clippy::let_underscore_must_use,
                reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
            )]
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
        match open_at(
            shell_base(),
            args[i].as_bytes(),
            OpenFlags::from_bits(O_RDONLY),
            0,
        ) {
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
                if let Err(e) = close(f) {
                    err_line("cat", e);
                }
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
    let src = match open_at(
        shell_base(),
        args[1].as_bytes(),
        OpenFlags::from_bits(O_RDONLY),
        0,
    ) {
        Ok(f) => f,
        Err(e) => {
            err_line("cp", e);
            return;
        }
    };
    let dflags = OpenFlags::from_bits(O_WRONLY | O_CREAT | O_TRUNC);
    match open_at(shell_base(), args[2].as_bytes(), dflags, 0o644) {
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
            if let Err(e) = close(dst) {
                err_line("cp", e);
            }
        }
        Err(e) => err_line("cp", e),
    }
    if let Err(e) = close(src) {
        err_line("cp", e);
    }
}

fn cmd_mv(args: &[&str]) {
    if args.len() < 3 {
        err_line("mv", FsError::Inval);
        return;
    }
    if let Err(e) = rename_at(shell_base(), args[1].as_bytes(), args[2].as_bytes()) {
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
            unlink_at(shell_base(), args[i].as_bytes())
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
            mkdir_p_at(shell_base(), args[i].as_bytes())
        } else {
            mkdir_at(shell_base(), args[i].as_bytes(), 0o755)
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
        match open_at(
            shell_base(),
            args[i].as_bytes(),
            OpenFlags::from_bits(O_WRONLY | O_CREAT),
            0o644,
        ) {
            Ok(f) => {
                if let Err(e) = close(f) {
                    err_line("touch", e);
                }
            }
            Err(e) => err_line("touch", e),
        }
        i += 1;
    }
}

fn cmd_stat(args: &[&str]) {
    let path = args.get(1).copied().unwrap_or(".");
    match stat_at(shell_base(), path.as_bytes()) {
        Ok(s) => {
            #[expect(
                clippy::let_underscore_must_use,
                reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
            )]
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
    let root = fs_init::volume_at(b"/");
    let fat = root
        .as_ref()
        .map_err(|e| *e)
        .and_then(|v| v.downcast_ref::<FatVolume>().ok_or(FsError::Inval));
    match fat.and_then(fat_init::df) {
        Ok((ft, tot, free, nclus)) => {
            #[expect(
                clippy::let_underscore_must_use,
                reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
            )]
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
    let vibe = fs_init::volume_at(b"/vibe");
    if vibefs_init::live()
        && let Ok(v) = vibe.as_ref()
        && let Some(v) = v.downcast_ref::<VibeVolume>()
        && let Ok((ft, tot, free, nblk)) = vibefs_init::df(v)
    {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
        )]
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
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
        )]
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
    if let Err(e) = mkdir_p_at(shell_base(), target.as_bytes()) {
        err_line("mount", e);
        return;
    }
    match mount_at(
        shell_base(),
        source.as_bytes(),
        target.as_bytes(),
        args[1].as_bytes(),
        false,
    ) {
        Ok(()) if args[1] == "ramfs" => {
            #[expect(
                clippy::let_underscore_must_use,
                reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
            )]
            let _ = writeln!(Console, "vibeOS: mount: ramfs on {target}");
        }
        Ok(()) => {
            #[expect(
                clippy::let_underscore_must_use,
                reason = "a write to the console cannot fail: `Console::write_str` always returns `Ok` (DESIGN §2.5)"
            )]
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
    if let Err(e) = umount_at(shell_base(), args[1].as_bytes()) {
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
    if let Err(e) = cd(path.as_bytes()) {
        err_line("cd", e);
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
        Err(e) => err_line("pwd", e),
    }
}

fn rm_r(path: &[u8]) -> Result<(), FsError> {
    let st = stat_at(shell_base(), path)?;
    if st.kind != InodeKind::Dir {
        return unlink_at(shell_base(), path);
    }
    let mut kids: [[u8; MAX_NAME]; 16] = [[0; MAX_NAME]; 16];
    let mut klens = [0u8; 16];
    let mut nk = 0usize;
    list_dir(shell_base(), path, &mut |d| {
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
    rmdir_at(shell_base(), path)
}
