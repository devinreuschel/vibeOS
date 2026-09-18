//! Kernel File API + shell file commands. ROADMAP §8.6.
//!
//! VFS lock is not held across FAT/block I/O. `sync` issues a block
//! Flush (DESIGN §10.2). FAT rejects symlink/link with `NotSupp`.

use core::sync::atomic::{AtomicUsize, Ordering};

use vibeos::fat::Node;
use vibeos::fs::{
    self, split_basename, Dirent, FsError, FsType, InodeKind, Name, PathRef, Stat, MAX_NAME,
    MAX_PATH, O_ACCMODE, O_APPEND, O_CREAT, O_DIRECTORY, O_EXCL, O_RDONLY, O_RDWR, O_TRUNC,
    O_WRONLY, SEEK_CUR, SEEK_END, SEEK_SET, S_IFDIR_MODE, S_IFREG_MODE,
};
use vibeos::lock::RANK_DEVICE;
use vibeos::shell::{Command, LineEditor, MAX_COMMANDS};

use crate::console_init::Console;
use crate::fat_init;
use crate::fs_init;
use crate::shell_init;
use crate::sync_init::SpinMutex;

use core::fmt::Write;

const MAX_OPEN: usize = 16;

#[derive(Clone, Copy)]
struct OpenFile {
    used: bool,
    vol: u8,
    flags: u32,
    offset: u64,
    ino: u32,
    kind: InodeKind,
    clu: u32,
    size: u32,
    dir_clu: u32,
    dir_off: u32,
}

impl OpenFile {
    const EMPTY: Self = Self {
        used: false,
        vol: 0,
        flags: 0,
        offset: 0,
        ino: 0,
        kind: InodeKind::Reg,
        clu: 0,
        size: 0,
        dir_clu: 0,
        dir_off: 0,
    };
}

static FILES: SpinMutex<[OpenFile; MAX_OPEN]> =
    SpinMutex::with_rank([OpenFile::EMPTY; MAX_OPEN], RANK_DEVICE);

static CWD_LEN: AtomicUsize = AtomicUsize::new(1);
static mut CWD_BUF: [u8; MAX_PATH] = {
    let mut b = [0u8; MAX_PATH];
    b[0] = b'/';
    b
};

fn cwd_bytes() -> &'static [u8] {
    let n = CWD_LEN.load(Ordering::Acquire);
    unsafe { &CWD_BUF[..n] }
}

fn set_cwd(p: &[u8]) {
    let n = p.len().min(MAX_PATH);
    unsafe {
        CWD_BUF[..n].copy_from_slice(&p[..n]);
    }
    CWD_LEN.store(n, Ordering::Release);
}

fn join_cwd(path: &str) -> Result<[u8; MAX_PATH], FsError> {
    let p = path.as_bytes();
    let mut out = [0u8; MAX_PATH];
    if p.first() == Some(&b'/') {
        if p.len() > MAX_PATH {
            return Err(FsError::NameTooLong);
        }
        out[..p.len()].copy_from_slice(p);
        return Ok(out);
    }
    let cwd = cwd_bytes();
    let mut n = cwd.len();
    if n > MAX_PATH {
        return Err(FsError::NameTooLong);
    }
    out[..n].copy_from_slice(cwd);
    if n == 0 || out[n - 1] != b'/' {
        if n >= MAX_PATH {
            return Err(FsError::NameTooLong);
        }
        out[n] = b'/';
        n += 1;
    }
    if n + p.len() > MAX_PATH {
        return Err(FsError::NameTooLong);
    }
    out[n..n + p.len()].copy_from_slice(p);
    Ok(out)
}

fn path_used(buf: &[u8; MAX_PATH]) -> &[u8] {
    let mut n = buf.len();
    while n > 0 && buf[n - 1] == 0 {
        n -= 1;
    }
    &buf[..n]
}

fn vol_walk(path: &str) -> Result<(u8, Node), FsError> {
    let abs = join_cwd(path)?;
    walk_abs(path_used(&abs))
}

fn is_kernfs(t: FsType) -> bool {
    match t {
        FsType::Dev | FsType::Tmp | FsType::Proc | FsType::Sys => true,
        FsType::Ram | FsType::Fat => false,
    }
}

#[derive(Clone, Copy)]
struct VfsLsEnt {
    name: [u8; MAX_NAME],
    nlen: u8,
    kind: InodeKind,
    size: u64,
}

impl VfsLsEnt {
    const EMPTY: Self = Self {
        name: [0; MAX_NAME],
        nlen: 0,
        kind: InodeKind::Reg,
        size: 0,
    };
}

const VFS_LS_MAX: usize = 32;

/// Snapshot a kernfs node. None = not kernfs (caller uses FAT).
/// Does not print; VFS is not held across serial.
fn vfs_ls_snap(
    path: &str,
) -> Result<Option<(InodeKind, u64, [VfsLsEnt; VFS_LS_MAX], usize)>, FsError> {
    let abs = join_cwd(path)?;
    let s = core::str::from_utf8(path_used(&abs)).map_err(|_| FsError::Inval)?;
    fs_init::with(|v| {
        let p = match v.resolve(None, s, true) {
            Ok(p) => p,
            Err(FsError::NotFound) | Err(FsError::Io) => return Ok(None),
            Err(e) => return Err(e),
        };
        if !is_kernfs(v.fstype_at(p)?) {
            return Ok(None);
        }
        let st = v.stat(None, s)?;
        if st.kind != InodeKind::Dir {
            return Ok(Some((st.kind, st.size, [VfsLsEnt::EMPTY; VFS_LS_MAX], 0)));
        }
        let mut ents = [VfsLsEnt::EMPTY; VFS_LS_MAX];
        let mut n = 0usize;
        let mut d = Dirent {
            ino: 0,
            kind: InodeKind::Reg,
            name: Name::EMPTY,
        };
        let mut cookie = 0u64;
        loop {
            match v.readdir(p, cookie, &mut d) {
                Ok(None) => break,
                Ok(Some(next)) => {
                    cookie = next;
                    let nb = d.name.as_bytes();
                    if nb == b"." || nb == b".." {
                        continue;
                    }
                    if n < VFS_LS_MAX {
                        let l = nb.len().min(MAX_NAME);
                        ents[n].name[..l].copy_from_slice(&nb[..l]);
                        ents[n].nlen = l as u8;
                        ents[n].kind = d.kind;
                        ents[n].size = 0;
                        n += 1;
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(Some((st.kind, st.size, ents, n)))
    })
}

fn walk_abs(pb: &[u8]) -> Result<(u8, Node), FsError> {
    let (vol, strip) = fat_init::route(pb);
    let rest = fat_init::routed_rest(pb, strip);
    Ok((vol, fat_init::walk(vol, rest)?))
}

fn vol_parent(path: &str) -> Result<(u8, Node, [u8; MAX_NAME], u8), FsError> {
    let abs = join_cwd(path)?;
    let pb = path_used(&abs);
    let (vol, strip) = fat_init::route(pb);
    let rest = fat_init::routed_rest(pb, strip);
    let (parent, name) = split_basename(rest)?;
    if name.len() > MAX_NAME {
        return Err(FsError::NameTooLong);
    }
    let dir = fat_init::walk(vol, if parent.is_empty() { b"/" } else { parent })?;
    if dir.kind != InodeKind::Dir {
        return Err(FsError::NotDir);
    }
    let mut nb = [0u8; MAX_NAME];
    nb[..name.len()].copy_from_slice(name);
    Ok((vol, dir, nb, name.len() as u8))
}

fn alloc_fid(f: OpenFile) -> Result<u16, FsError> {
    let mut g = FILES.lock();
    let mut i = 0usize;
    while i < MAX_OPEN {
        if !g[i].used {
            g[i] = f;
            g[i].used = true;
            return Ok(i as u16);
        }
        i += 1;
    }
    Err(FsError::NoSpace)
}

fn get_file(fid: u16) -> Result<OpenFile, FsError> {
    let g = FILES.lock();
    let i = fid as usize;
    if i >= MAX_OPEN || !g[i].used {
        return Err(FsError::Badf);
    }
    Ok(g[i])
}

fn put_file(fid: u16, f: OpenFile) -> Result<(), FsError> {
    let mut g = FILES.lock();
    let i = fid as usize;
    if i >= MAX_OPEN || !g[i].used {
        return Err(FsError::Badf);
    }
    g[i] = f;
    g[i].used = true;
    Ok(())
}

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
            help: "mount fat32 <dev> <path> | ramfs <path>",
            run: cmd_mount,
        },
        Command {
            name: "umount",
            help: "unmount path",
            run: cmd_umount,
        },
        Command {
            name: "sync",
            help: "flush FAT + block Flush",
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

pub fn open(path: &str, flags: u32, _mode: u16) -> Result<u16, FsError> {
    if flags & O_CREAT != 0 {
        match vol_walk(path) {
            Ok(_) => {
                if flags & O_EXCL != 0 {
                    return Err(FsError::Exists);
                }
            }
            Err(FsError::NotFound) => {
                let (vol, dir, name, nlen) = vol_parent(path)?;
                fat_init::create(vol, dir.clu, &name[..nlen as usize], false)?;
            }
            Err(e) => return Err(e),
        }
    }
    let (vol, mut node) = vol_walk(path)?;
    if node.kind == InodeKind::Dir {
        let acc = flags & O_ACCMODE;
        if acc == O_WRONLY || acc == O_RDWR || flags & O_TRUNC != 0 {
            return Err(FsError::IsDir);
        }
    } else if flags & O_DIRECTORY != 0 {
        return Err(FsError::NotDir);
    }
    if flags & O_TRUNC != 0 && node.kind == InodeKind::Reg {
        let mut clu = node.clu;
        let mut size = node.size;
        fat_init::truncate(
            vol,
            node.dir_clu,
            node.dir_off,
            node.ino,
            &mut clu,
            &mut size,
            0,
        )?;
        node.clu = clu;
        node.size = size;
    }
    alloc_fid(OpenFile {
        used: true,
        vol,
        flags,
        offset: 0,
        ino: node.ino,
        kind: node.kind,
        clu: node.clu,
        size: node.size,
        dir_clu: node.dir_clu,
        dir_off: node.dir_off,
    })
}

pub fn close(fid: u16) -> Result<(), FsError> {
    let mut g = FILES.lock();
    let i = fid as usize;
    if i >= MAX_OPEN || !g[i].used {
        return Err(FsError::Badf);
    }
    g[i] = OpenFile::EMPTY;
    Ok(())
}

pub fn read(fid: u16, buf: &mut [u8]) -> Result<usize, FsError> {
    let mut f = get_file(fid)?;
    if f.flags & O_ACCMODE == O_WRONLY {
        return Err(FsError::Inval);
    }
    if f.kind == InodeKind::Dir {
        return Err(FsError::IsDir);
    }
    let n = fat_init::read(f.vol, f.clu, f.size, f.offset, buf)?;
    f.offset = f.offset.saturating_add(n as u64);
    put_file(fid, f)?;
    Ok(n)
}

pub fn write(fid: u16, buf: &[u8]) -> Result<usize, FsError> {
    let mut f = get_file(fid)?;
    if f.flags & O_ACCMODE == O_RDONLY {
        return Err(FsError::Inval);
    }
    if f.kind == InodeKind::Dir {
        return Err(FsError::IsDir);
    }
    if f.flags & O_APPEND != 0 {
        f.offset = f.size as u64;
    }
    let n = fat_init::write(
        f.vol,
        f.dir_clu,
        f.dir_off,
        f.ino,
        &mut f.clu,
        &mut f.size,
        f.offset,
        buf,
    )?;
    f.offset = f.offset.saturating_add(n as u64);
    put_file(fid, f)?;
    Ok(n)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn seek(fid: u16, off: i64, whence: u32) -> Result<u64, FsError> {
    let mut f = get_file(fid)?;
    let base = match whence {
        SEEK_SET => 0i64,
        SEEK_CUR => f.offset as i64,
        SEEK_END => f.size as i64,
        _ => return Err(FsError::Inval),
    };
    let n = base.saturating_add(off);
    if n < 0 {
        return Err(FsError::Inval);
    }
    f.offset = n as u64;
    put_file(fid, f)?;
    Ok(n as u64)
}

pub fn stat_path(path: &str) -> Result<Stat, FsError> {
    let (_vol, node) = vol_walk(path)?;
    Ok(Stat {
        ino: node.ino,
        kind: node.kind,
        mode: if node.kind == InodeKind::Dir {
            S_IFDIR_MODE
        } else {
            S_IFREG_MODE
        },
        nlink: if node.kind == InodeKind::Dir { 2 } else { 1 },
        size: node.size as u64,
        atime: node.mtime,
        mtime: node.mtime,
        ctime: node.mtime,
    })
}

pub fn mkdir_one(path: &str) -> Result<(), FsError> {
    let (vol, dir, name, nlen) = vol_parent(path)?;
    fat_init::create(vol, dir.clu, &name[..nlen as usize], true)?;
    Ok(())
}

pub fn mkdir_p(path: &str) -> Result<(), FsError> {
    let abs = join_cwd(path)?;
    let pb = path_used(&abs);
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
            let s = core::str::from_utf8(slice).map_err(|_| FsError::Inval)?;
            match vol_walk(s) {
                Ok((_, n)) if n.kind == InodeKind::Dir => {}
                Ok(_) => return Err(FsError::NotDir),
                Err(FsError::NotFound) => mkdir_one(s)?,
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

pub fn vfs_attach(path: &str) -> Result<PathRef, FsError> {
    let (vol, node) = vol_walk(path)?;
    let abs = join_cwd(path)?;
    let pb = path_used(&abs);
    let (parent, name) = split_basename(pb)?;
    let _ = vol;
    fs_init::with(|v| {
        let pdir = if parent.is_empty() || parent == b"/" {
            v.root()?
        } else {
            let s = core::str::from_utf8(parent).map_err(|_| FsError::Inval)?;
            v.resolve(None, s, true)?
        };
        let islot = v.fat_iget(0, node.ino, node.kind, node.size as u64, node.clu)?;
        match v.fat_dcache(pdir, name, islot) {
            Ok(p) => Ok(p),
            Err(e) => {
                v.release_inode(islot);
                Err(e)
            }
        }
    })
}

pub fn unlink_path(path: &str, rmdir: bool) -> Result<(), FsError> {
    let (vol, dir, name, nlen) = vol_parent(path)?;
    fat_init::unlink(vol, dir.clu, &name[..nlen as usize], rmdir)?;
    if let Ok(pref) = fs_init::with(|v| v.resolve(None, "/", true)) {
        fs_init::with(|v| v.drop_name(pref, &name[..nlen as usize]));
    }
    Ok(())
}

fn rm_r(path: &str) -> Result<(), FsError> {
    let (vol, node) = vol_walk(path)?;
    if node.kind == InodeKind::Dir {
        let mut cookie = 0u64;
        let mut kids: [[u8; MAX_NAME]; 16] = [[0; MAX_NAME]; 16];
        let mut klens = [0u8; 16];
        let mut nk = 0usize;
        loop {
            let mut n = Node::EMPTY;
            match fat_init::readdir(vol, node.clu, cookie, &mut n)? {
                None => break,
                Some(next) => {
                    cookie = next;
                    if n.name() == b"." || n.name() == b".." {
                        continue;
                    }
                    if nk < 16 {
                        let l = n.name_len as usize;
                        kids[nk][..l].copy_from_slice(n.name());
                        klens[nk] = n.name_len;
                        nk += 1;
                    }
                }
            }
        }
        let abs = join_cwd(path)?;
        let mut i = 0usize;
        while i < nk {
            let mut child = [0u8; MAX_PATH];
            let pb = path_used(&abs);
            let mut n = pb.len();
            child[..n].copy_from_slice(pb);
            if n == 0 || child[n - 1] != b'/' {
                child[n] = b'/';
                n += 1;
            }
            let ln = klens[i] as usize;
            child[n..n + ln].copy_from_slice(&kids[i][..ln]);
            let s = core::str::from_utf8(&child[..n + ln]).map_err(|_| FsError::Inval)?;
            rm_r(s)?;
            i += 1;
        }
        unlink_path(path, true)
    } else {
        unlink_path(path, false)
    }
}

pub fn rename_path(old: &str, new: &str) -> Result<(), FsError> {
    let (sv, sd, sn, sl) = vol_parent(old)?;
    let (dv, dd, dn, dl) = vol_parent(new)?;
    if sv != dv {
        return Err(FsError::Inval);
    }
    fat_init::rename(
        sv,
        sd.clu,
        &sn[..sl as usize],
        dd.clu,
        &dn[..dl as usize],
    )
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn symlink_path(_path: &str, _target: &str) -> Result<(), FsError> {
    Err(FsError::NotSupp)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn link_path(_old: &str, _new: &str) -> Result<(), FsError> {
    Err(FsError::NotSupp)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn truncate_path(path: &str, size: u64) -> Result<(), FsError> {
    let (vol, node) = vol_walk(path)?;
    if node.kind != InodeKind::Reg {
        return Err(FsError::IsDir);
    }
    if size > u32::MAX as u64 {
        return Err(FsError::Inval);
    }
    let mut clu = node.clu;
    let mut sz = node.size;
    fat_init::truncate(
        vol,
        node.dir_clu,
        node.dir_off,
        node.ino,
        &mut clu,
        &mut sz,
        size as u32,
    )
}

pub fn sync_fs() -> Result<(), FsError> {
    fat_init::sync_all()
}

fn names_in(
    vol: u8,
    dir_clu: u32,
    prefix: &[u8],
    out: &mut [[u8; MAX_NAME]; 16],
    lens: &mut [u8; 16],
) -> usize {
    let mut n = 0usize;
    let mut cookie = 0u64;
    loop {
        let mut node = Node::EMPTY;
        match fat_init::readdir(vol, dir_clu, cookie, &mut node) {
            Ok(Some(next)) => {
                cookie = next;
                let nm = node.name();
                if nm == b"." || nm == b".." {
                    continue;
                }
                if nm.len() >= prefix.len() && nm[..prefix.len()].eq_ignore_ascii_case(prefix) {
                    if n < 16 {
                        let l = nm.len().min(MAX_NAME);
                        out[n][..l].copy_from_slice(&nm[..l]);
                        lens[n] = l as u8;
                        n += 1;
                    }
                }
            }
            _ => break,
        }
    }
    n
}

fn vfs_complete_names(
    dirp: &[u8],
    prefix: &[u8],
    out: &mut [[u8; MAX_NAME]; 16],
    lens: &mut [u8; 16],
) -> Option<usize> {
    let path = if dirp.is_empty() {
        "."
    } else {
        core::str::from_utf8(dirp).ok()?
    };
    let (_, _, ents, n) = vfs_ls_snap(path).ok()??;
    let mut k = 0usize;
    let mut i = 0usize;
    while i < n {
        let nm = &ents[i].name[..ents[i].nlen as usize];
        if nm.len() >= prefix.len() && nm[..prefix.len()].eq_ignore_ascii_case(prefix) {
            if k < 16 {
                let l = nm.len().min(MAX_NAME);
                out[k][..l].copy_from_slice(&nm[..l]);
                lens[k] = l as u8;
                k += 1;
            }
        }
        i += 1;
    }
    Some(k)
}

/// Tab: complete the word at the cursor against the current directory
/// (or the directory prefix of that word).
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
    let n = match vfs_complete_names(dirp, pref, &mut names, &mut lens) {
        Some(n) => n,
        None => {
            let dir_node = if dirp.is_empty() {
                walk_abs(cwd_bytes())
            } else if dirp.first() == Some(&b'/') {
                let mut t = [0u8; MAX_PATH];
                let n = dirp.len().min(MAX_PATH);
                t[..n].copy_from_slice(&dirp[..n]);
                walk_abs(&t[..n])
            } else {
                let mut t = [0u8; MAX_PATH];
                let cwd = cwd_bytes();
                let mut n = cwd.len();
                t[..n].copy_from_slice(cwd);
                if n > 0 && t[n - 1] != b'/' {
                    t[n] = b'/';
                    n += 1;
                }
                let add = dirp.len().min(MAX_PATH.saturating_sub(n));
                t[n..n + add].copy_from_slice(&dirp[..add]);
                walk_abs(&t[..n + add])
            };
            let Ok((vol, dir)) = dir_node else {
                return;
            };
            if dir.kind != InodeKind::Dir {
                return;
            }
            names_in(vol, dir.clu, pref, &mut names, &mut lens)
        }
    };
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

fn complete_cmd(ed: &mut LineEditor, start: usize, pref: &[u8]) {
    let mut hit: Option<&'static str> = None;
    let mut n = 0u32;
    let names = [
        "help", "echo", "meminfo", "uptime", "cpus", "dmesg", "ps", "panic", "reboot",
        "poweroff", "ls", "cat", "cp", "mv", "rm", "mkdir", "touch", "stat", "df", "mount",
        "umount", "sync", "cd", "pwd", "blk", "lspci", "devices",
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
    if n == 1 {
        if let Some(h) = hit {
            apply_word(ed, start, ed.cursor(), b"", h.as_bytes(), true);
        }
    }
    let _ = shell_init::command_count();
}

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

fn to_up(c: u8) -> u8 {
    if c.is_ascii_lowercase() {
        c - b'a' + b'A'
    } else {
        c
    }
}

fn apply_word(ed: &mut LineEditor, start: usize, cur: usize, dirp: &[u8], name: &[u8], unique: bool) {
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
    match vfs_ls_snap(path) {
        Ok(Some((kind, size, ents, n))) => {
            if kind != InodeKind::Dir {
                if long {
                    let _ = writeln!(Console, "{} {} {}", kind.as_str(), size, path);
                } else {
                    let _ = writeln!(Console, "{path}");
                }
                return;
            }
            let mut i = 0usize;
            while i < n {
                if long {
                    let k = ents[i].kind.as_str();
                    let _ = write!(Console, "{k} {:>8} ", ents[i].size);
                    console_init_write_name(&ents[i].name[..ents[i].nlen as usize]);
                    console_init_write(b"\n");
                } else {
                    console_init_write_name(&ents[i].name[..ents[i].nlen as usize]);
                    console_init_write(b"\n");
                }
                i += 1;
            }
            return;
        }
        Ok(None) => {}
        Err(e) => {
            err_line("ls", e);
            return;
        }
    }
    let (vol, node) = match vol_walk(path) {
        Ok(n) => n,
        Err(e) => {
            err_line("ls", e);
            return;
        }
    };
    let dir_clu = if node.kind == InodeKind::Dir {
        node.clu
    } else {
        if long {
            let _ = writeln!(
                Console,
                "{} {} {}",
                node.kind.as_str(),
                node.size,
                path
            );
        } else {
            let _ = writeln!(Console, "{path}");
        }
        return;
    };
    let mut cookie = 0u64;
    loop {
        let mut n = Node::EMPTY;
        match fat_init::readdir(vol, dir_clu, cookie, &mut n) {
            Ok(None) => break,
            Ok(Some(next)) => {
                cookie = next;
                if n.name() == b"." || n.name() == b".." {
                    continue;
                }
                if long {
                    let k = n.kind.as_str();
                    let _ = write!(Console, "{k} {:>8} ", n.size);
                    console_init_write_name(n.name());
                    console_init_write(b"\n");
                } else {
                    console_init_write_name(n.name());
                    console_init_write(b"\n");
                }
            }
            Err(e) => {
                err_line("ls", e);
                return;
            }
        }
    }
}

fn console_init_write(b: &[u8]) {
    crate::console_init::write(b);
}

fn console_init_write_name(n: &[u8]) {
    crate::console_init::write(n);
}

fn cmd_cat(args: &[&str]) {
    if args.len() < 2 {
        err_line("cat", FsError::Inval);
        return;
    }
    let mut i = 1usize;
    while i < args.len() {
        match open(args[i], O_RDONLY, 0) {
            Ok(fid) => {
                let mut buf = [0u8; 512];
                loop {
                    match read(fid, &mut buf) {
                        Ok(0) => break,
                        Ok(n) => crate::console_init::write(&buf[..n]),
                        Err(e) => {
                            err_line("cat", e);
                            break;
                        }
                    }
                }
                let _ = close(fid);
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
    match open(args[1], O_RDONLY, 0) {
        Ok(src) => {
            match open(args[2], O_WRONLY | O_CREAT | O_TRUNC, 0o644) {
                Ok(dst) => {
                    let mut buf = [0u8; 512];
                    loop {
                        match read(src, &mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                if write(dst, &buf[..n]).is_err() {
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
        Err(e) => err_line("cp", e),
    }
}

fn cmd_mv(args: &[&str]) {
    if args.len() < 3 {
        err_line("mv", FsError::Inval);
        return;
    }
    if let Err(e) = rename_path(args[1], args[2]) {
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
            rm_r(args[i])
        } else {
            unlink_path(args[i], false)
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
            mkdir_p(args[i])
        } else {
            mkdir_one(args[i])
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
        match open(args[i], O_WRONLY | O_CREAT, 0o644) {
            Ok(fid) => {
                let _ = close(fid);
            }
            Err(e) => err_line("touch", e),
        }
        i += 1;
    }
}

fn cmd_stat(args: &[&str]) {
    let path = args.get(1).copied().unwrap_or(".");
    match stat_path(path) {
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
}

fn cmd_mount(args: &[&str]) {
    if args.len() < 2 {
        let _ = writeln!(Console, "vibeOS: mount: fat32 <dev> <path> | ramfs <path>");
        return;
    }
    match args[1] {
        "fat32" => {
            if args.len() < 4 {
                err_line("mount", FsError::Inval);
                return;
            }
            let _ = mkdir_p(args[3]);
            let _ = vfs_attach(args[3]);
            match fat_init::mount_dev(args[2], args[3]) {
                Ok(_) => {
                    let _ = writeln!(Console, "vibeOS: mount: fat32 {} on {}", args[2], args[3]);
                }
                Err(e) => err_line("mount", e),
            }
        }
        "ramfs" => {
            if args.len() < 3 {
                err_line("mount", FsError::Inval);
                return;
            }
            let _ = mkdir_p(args[2]);
            let _ = vfs_attach(args[2]);
            match fs_init::with(|v| v.mount(None, args[2], &fs::RamFs)) {
                Ok(_) => {
                    let _ = writeln!(Console, "vibeOS: mount: ramfs on {}", args[2]);
                }
                Err(e) => err_line("mount", e),
            }
        }
        _ => err_line("mount", FsError::Inval),
    }
}

fn cmd_umount(args: &[&str]) {
    if args.len() < 2 {
        err_line("umount", FsError::Inval);
        return;
    }
    if let Err(e) = fat_init::umount(args[1]) {
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
    let abs = match join_cwd(path) {
        Ok(a) => a,
        Err(e) => {
            err_line("cd", e);
            return;
        }
    };
    match vol_walk(path) {
        Ok((_, n)) if n.kind == InodeKind::Dir => {
            let pb = path_used(&abs);
            let mut norm = [0u8; MAX_PATH];
            let nlen = if pb.is_empty() {
                norm[0] = b'/';
                1
            } else {
                let n = pb.len().min(MAX_PATH);
                norm[..n].copy_from_slice(&pb[..n]);
                n
            };
            set_cwd(&norm[..nlen]);
        }
        Ok(_) => err_line("cd", FsError::NotDir),
        Err(e) => err_line("cd", e),
    }
}

fn cmd_pwd(_args: &[&str]) {
    crate::console_init::write(cwd_bytes());
    crate::console_init::write(b"\n");
}

#[allow(dead_code)]
pub fn mkdir(path: &str, _mode: u16) -> Result<(), FsError> {
    mkdir_one(path)
}

#[allow(dead_code)]
pub fn creat(path: &str) -> Result<(), FsError> {
    let fid = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0o644)?;
    close(fid)
}

const _: () = {
    let _ = (SEEK_SET, SEEK_CUR, SEEK_END, PathRef { mount: 0, dslot: 0 });
};
