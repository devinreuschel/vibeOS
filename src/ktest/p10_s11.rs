//! In-guest tests of P10-S11, VFS single dispatch I: backend InodeOps and inode identity (DESIGN §8.2).

use vibeos::fs::{Dirent, FsError, InodeKind, Name, O_CREAT, O_RDWR, SEEK_SET, Stat, Vfs};

use super::{Outcome, Test, test};
use crate::fat_init;
use crate::file_init;
use crate::fs_init;
use crate::vibefs_init;

pub(super) const TESTS: &[Test] = &[
    test("vfs_fat_ops_initrd", test_vfs_fat_ops_initrd),
    test(
        "vfs_fat_unlinked_open_inode",
        test_vfs_fat_unlinked_open_inode,
    ),
    test(
        "vfs_fat_file_api_one_inode",
        test_vfs_fat_file_api_one_inode,
    ),
    test("vfs_vibe_ops_mem", test_vfs_vibe_ops_mem),
];

/// A failed step and its error.
type Step<T> = Result<T, (&'static str, FsError)>;

/// Run one `Vfs` call under the VFS lock, naming it for the failure line.
fn vfs<T>(what: &'static str, f: impl FnOnce(&mut Vfs) -> Result<T, FsError>) -> Step<T> {
    fs_init::with(f).map_err(|e| (what, e))
}

fn vstat(path: &str) -> Result<Stat, FsError> {
    fs_init::with(|v| v.stat(None, path))
}

fn failed(r: Step<()>) -> Outcome {
    match r {
        Ok(()) => Outcome::Ok,
        Err((what, e)) => crate::fail_fmt!("{what}: {}", e.as_str()),
    }
}

/// How often `dir` lists `.`, `..` and `named` through `Vfs`.
fn count_names(dir: &str, named: &[u8]) -> Step<(u32, u32, u32)> {
    let p = vfs("resolve", |v| v.resolve(None, dir, true))?;
    let mut d = Dirent {
        ino: 0,
        kind: InodeKind::Reg,
        name: Name::EMPTY,
    };
    let (mut dot, mut dotdot, mut hit) = (0u32, 0u32, 0u32);
    let mut cookie = 0u64;
    while let Some(next) = vfs("readdir", |v| v.readdir(p, cookie, &mut d))? {
        let n = d.name.as_bytes();
        if n == b"." {
            dot += 1;
        } else if n == b".." {
            dotdot += 1;
        } else if n.eq_ignore_ascii_case(named) {
            hit += 1;
        }
        cookie = next;
    }
    Ok((dot, dotdot, hit))
}

fn mkdir_s11() -> Step<()> {
    match fs_init::with(|v| v.mkdir(None, "/s11", 0o755)) {
        Ok(_) | Err(FsError::Exists) => Ok(()),
        Err(e) => Err(("mkdir /s11", e)),
    }
}

/// Free bytes on the initrd volume, read outside the VFS lock.
fn initrd_free() -> Step<u64> {
    fat_init::df(fat_init::VOL_INITRD)
        .map(|(_, _, free, _)| free)
        .map_err(|e| ("df", e))
}

fn test_vfs_fat_ops_initrd() -> Outcome {
    if !fat_init::live() {
        return Outcome::Skip("no FAT initrd");
    }
    failed(fat_ops_initrd())
}

fn fat_ops_initrd() -> Step<()> {
    let name = match file_init::stat_path("/hello.txt") {
        Ok(_) => "/hello.txt",
        Err(_) => "/HELLO.TXT",
    };
    let api = file_init::stat_path(name).map_err(|e| ("file api stat", e))?;
    let vs = vstat(name).map_err(|e| ("vfs stat", e))?;
    if (vs.size, vs.ino, vs.kind) != (api.size, api.ino, InodeKind::Reg) {
        return Err(("hello.txt size or ino", FsError::Io));
    }
    if count_names("/", b"hello.txt")? != (1, 1, 1) {
        return Err(("readdir / names", FsError::Io));
    }
    mkdir_s11()?;
    vfs("creat", |v| v.creat(None, "/s11/f", 0o644))?;
    let one = vfs("open 1", |v| v.open(None, "/s11/f", O_RDWR, 0))?;
    let two = vfs("open 2", |v| v.open(None, "/s11/f", O_RDWR, 0))?;
    let mut data = [0u8; 300];
    for (i, b) in data.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(13);
    }
    let mut back = [0u8; 300];
    let wrote = vfs("write", |v| v.write(one, &data));
    let read = vfs("read", |v| v.read(two, &mut back));
    let size = vstat("/s11/f").map(|s| s.size).map_err(|e| ("stat", e));
    let c1 = vfs("close 1", |v| v.close(one));
    let c2 = vfs("close 2", |v| v.close(two));
    if wrote? != 300 || read? != 300 || back != data {
        return Err(("300-byte round trip", FsError::Io));
    }
    if size? != 300 {
        return Err(("size after write", FsError::Io));
    }
    c1?;
    c2?;
    vfs("unlink", |v| v.unlink(None, "/s11/f"))?;
    match (vstat("/s11/f"), file_init::stat_path("/s11/f")) {
        (Err(FsError::NotFound), Err(FsError::NotFound)) => Ok(()),
        _ => Err(("unlinked file still found", FsError::Io)),
    }
}

fn test_vfs_fat_unlinked_open_inode() -> Outcome {
    if !fat_init::live() {
        return Outcome::Skip("no FAT initrd");
    }
    failed(fat_unlinked_open_inode())
}

fn fat_unlinked_open_inode() -> Step<()> {
    mkdir_s11()?;
    let base = initrd_free()?;
    let old = [0x5Au8; 1500];
    let fid = vfs("open", |v| v.open(None, "/s11/u", O_RDWR | O_CREAT, 0o644))?;
    let r = unlinked_open_body(fid, &old);
    let c = vfs("close", |v| v.close(fid));
    r?;
    c?;
    let after = initrd_free()?;
    vfs("cleanup unlink", |v| v.unlink(None, "/s11/u"))?;
    if after != base {
        return Err(("free bytes after close differ from baseline", FsError::Io));
    }
    Ok(())
}

fn unlinked_open_body(fid: u16, old: &[u8; 1500]) -> Step<()> {
    if vfs("write", |v| v.write(fid, old))? != old.len() {
        return Err(("short write", FsError::Io));
    }
    vfs("unlink", |v| v.unlink(None, "/s11/u"))?;
    vfs("creat again", |v| v.creat(None, "/s11/u", 0o644))?;
    if vstat("/s11/u").map_err(|e| ("stat new", e))?.size != 0 {
        return Err(("new file not empty", FsError::Io));
    }
    vfs("seek", |v| v.seek(fid, 0, SEEK_SET))?;
    let mut back = [0u8; 1500];
    if vfs("read old", |v| v.read(fid, &mut back))? != old.len() || &back != old {
        return Err(("old bytes", FsError::Io));
    }
    Ok(())
}

fn test_vfs_fat_file_api_one_inode() -> Outcome {
    if !fat_init::live() {
        return Outcome::Skip("no FAT initrd");
    }
    failed(fat_file_api_one_inode())
}

fn fat_file_api_one_inode() -> Step<()> {
    mkdir_s11()?;
    let fid =
        file_init::open("/s11/g", O_RDWR | O_CREAT, 0o644).map_err(|e| ("file api open", e))?;
    let data = [0xC3u8; 5000];
    let wrote = file_init::write(fid, &data).map_err(|e| ("file api write", e));
    let vs = vstat("/s11/g").map_err(|e| ("vfs stat", e));
    let closed = file_init::close(fid).map_err(|e| ("file api close", e));
    let api = file_init::stat_path("/s11/g").map_err(|e| ("file api stat", e));
    let gone = file_init::unlink_path("/s11/g", false).map_err(|e| ("file api unlink", e));
    let (wrote, vs, api) = (wrote?, vs?, api?);
    closed?;
    gone?;
    if wrote != 5000 || vs.size != 5000 || api.size != 5000 {
        return Err(("size is not 5000 on both paths", FsError::Io));
    }
    if vs.ino != api.ino {
        return Err(("st_ino differs between paths", FsError::Io));
    }
    Ok(())
}

fn test_vfs_vibe_ops_mem() -> Outcome {
    if !vibefs_init::live() {
        return Outcome::Skip("no vibefs");
    }
    failed(vibe_ops_mem())
}

fn vibe_ops_mem() -> Step<()> {
    match fs_init::with(|v| v.mkdir(None, "/vibe/s11", 0o755)) {
        Ok(_) | Err(FsError::Exists) => {}
        Err(e) => return Err(("mkdir", e)),
    }
    vfs("creat", |v| v.creat(None, "/vibe/s11/f", 0o644))?;
    let fid = vfs("open", |v| v.open(None, "/vibe/s11/f", O_RDWR, 0))?;
    let mut back = [0u8; 8];
    let wrote = vfs("write", |v| v.write(fid, b"vibe ops"));
    let sought = vfs("seek", |v| v.seek(fid, 0, SEEK_SET));
    let read = vfs("read", |v| v.read(fid, &mut back));
    let closed = vfs("close", |v| v.close(fid));
    if wrote? != 8 || read? != 8 || &back != b"vibe ops" {
        return Err(("round trip", FsError::Io));
    }
    sought?;
    closed?;
    if vstat("/vibe/s11/f").map_err(|e| ("stat", e))?.size != 8 {
        return Err(("size after write", FsError::Io));
    }
    if count_names("/vibe/s11", b"f")? != (1, 1, 1) {
        return Err(("readdir names", FsError::Io));
    }
    file_init::symlink_path("/vibe/s11/l", "/vibe/s11/f").map_err(|e| ("symlink", e))?;
    let l = vstat("/vibe/s11/l").map_err(|e| ("stat link", e))?;
    let f = vstat("/vibe/s11/f").map_err(|e| ("stat file", e))?;
    let k = vfs("lstat", |v| v.lstat(None, "/vibe/s11/l"))?;
    if l.ino != f.ino || l.kind != InodeKind::Reg || k.kind != InodeKind::Lnk {
        return Err(("symlink not followed", FsError::Io));
    }
    Ok(())
}
