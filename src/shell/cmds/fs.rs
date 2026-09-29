//! File commands: `ls`, `cat`, `cp`, `mv`, `rm`, `mkdir`, `touch`, `stat`,
//! `df`, `mount`, `umount`, `sync`, `cd` and `pwd` (ROADMAP §8.6), over
//! `file_init`'s File API.

use core::fmt::Write;

use vibeos::fs::{
    FsError, InodeKind, MAX_NAME, MAX_PATH, O_CREAT, O_RDONLY, O_TRUNC, O_WRONLY, OpenFlags,
};
use vibeos::shell::Command;

use crate::console_init::Console;
use crate::fat_init;
use crate::file_init::{
    child_path, close, cwd_copy, join_cwd, list_dir, mkdir, mkdir_p, mount, open, read, rename,
    rmdir, set_cwd, stat_path, sync_fs, umount, unlink, write,
};
use crate::vibefs_init;

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
