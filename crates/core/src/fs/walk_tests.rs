//! Host tests for directory references, the walk base, and the
//! namespace changes that keep a referenced dentry's name current
//! (ROADMAP §10.4).

use super::testfs::*;
use super::tests::{assert_dcache_sound, locked_vfs, mount_dev, press, ram, st_ino_of};
use super::*;

/// A base whose root is the namespace root and whose working directory
/// is `cwd`.
fn cwd_base(v: &Vfs, cwd: &DirRef) -> Option<WalkBase> {
    Some(WalkBase {
        root: v.root().unwrap(),
        cwd: cwd.at(),
    })
}

#[test]
fn dir_ref_survives_eviction() {
    let mut v = ram();
    v.mkdir(None, "/a", 0o755).unwrap();
    v.creat(None, "/a/x", 0o644).unwrap();
    v.mkdir(None, "/p", 0o755).unwrap();
    let r = v.dir_get(None, "/a").unwrap();
    let a = r.at();
    let a_ino = st_ino_of(&v, a);
    let x_ino = v.stat(None, "/a/x").unwrap().ino;
    let (d0, i0) = (v.stats.d_evicts, v.stats.i_evicts);
    // Name pressure: 48 files under /p and /q, more than the inode
    // cache holds, looked up in turn between lookups of missing names,
    // until both caches have evicted more than they hold.
    v.mkdir(None, "/q", 0o755).unwrap();
    let files = 48u32;
    let name = |i: u32| format!("/{}/f{}", if i.is_multiple_of(2) { "p" } else { "q" }, i);
    let mut seq = 0u32;
    while seq < files {
        v.creat(None, &name(seq), 0o644).unwrap();
        seq += 1;
    }
    while v.stats.d_evicts < d0 + 2 * v.dentries.len() as u32
        || v.stats.i_evicts < i0 + 2 * v.inodes.len() as u32
    {
        v.stat(None, &name(seq % files)).unwrap();
        let _ = v.stat(None, &format!("/p/n{seq}"));
        seq += 1;
        assert!(seq < 100_000, "no eviction pressure");
    }
    // The reference kept its dentry and inode: a relative stat from it
    // finds `x`, and `/a` still resolves to it.
    assert_eq!(v.stat(cwd_base(&v, &r), "x").unwrap().ino, x_ino);
    assert_eq!(v.resolve(None, "/a", true).unwrap(), a);
    assert_eq!(st_ino_of(&v, a), a_ino);
    assert_eq!(v.dentry_refs(a), u32::from(v.expected_holds(a.dslot)) + 1);
    v.dir_put(r);
    // Only the cache's own holds are left, so pressure evicts it.
    assert_eq!(v.dentry_refs(a), u32::from(v.expected_holds(a.dslot)));
    assert_dcache_sound(&v);
    let n = 2 * v.dentries.len() as u32;
    press(&mut v, "/p", n, &mut seq);
    let sb = v.sb_of(0);
    let root = v.root().unwrap().dslot;
    assert!(v.dcache_peek(sb, root, b"a").is_none(), "/a still cached");
    assert_eq!(v.stat(None, "/a/x").unwrap().ino, x_ino);
}

#[test]
fn umount_busy_counts_dir_refs() {
    let mut v = ram();
    v.mkdir(None, "/m", 0o755).unwrap();
    v.mount(None, "/m", ramfs()).unwrap();
    v.mkdir(None, "/m/d", 0o755).unwrap();
    // A working directory below the mount, and a root at its root.
    let cwd = v.dir_get(None, "/m/d").unwrap();
    assert_eq!(v.umount(None, "/m").unwrap_err(), FsError::Busy);
    v.dir_put(cwd);
    let root = v.dir_get(None, "/m").unwrap();
    assert_eq!(v.umount(None, "/m").unwrap_err(), FsError::Busy);
    // A relative umount from a reference above the mount is no user of it.
    let top = v.dir_root().unwrap();
    v.dir_put(root);
    v.umount(cwd_base(&v, &top), "m").unwrap();
    v.dir_put(top);
    assert_dcache_sound(&v);
    // Users of the same superblock through another mount do not count.
    v.mkdir(None, "/p", 0o755).unwrap();
    v.mkdir(None, "/q", 0o755).unwrap();
    let fs = ramfs();
    mount_dev(&mut v, "/p", fs, 7, false).unwrap();
    mount_dev(&mut v, "/q", fs, 7, false).unwrap();
    v.mkdir(None, "/q/d", 0o755).unwrap();
    let q = v.dir_get(None, "/q/d").unwrap();
    v.umount(None, "/p").unwrap();
    assert_eq!(v.umount(None, "/q").unwrap_err(), FsError::Busy);
    v.dir_put(q);
    v.umount(None, "/q").unwrap();
    assert_dcache_sound(&v);
}

#[test]
fn walk_dotdot_stops_at_base_root() {
    let mut v = ram();
    v.mkdir(None, "/r", 0o755).unwrap();
    v.mkdir(None, "/r/s", 0o755).unwrap();
    v.creat(None, "/r/f", 0o644).unwrap();
    v.creat(None, "/f", 0o644).unwrap();
    v.symlink(None, "/r/s/abs", "/f").unwrap();
    let root = v.dir_get(None, "/r").unwrap();
    let cwd = v.dir_get(None, "/r/s").unwrap();
    let base = Some(WalkBase {
        root: root.at(),
        cwd: cwd.at(),
    });
    let rf = v.stat(None, "/r/f").unwrap().ino;
    // `..` climbs to the base's root and stays there.
    assert_eq!(v.resolve(base, "..", true).unwrap(), root.at());
    assert_eq!(v.resolve(base, "../../..", true).unwrap(), root.at());
    assert_eq!(v.stat(base, "../../../f").unwrap().ino, rf);
    // An absolute path, and an absolute link target, start at it.
    assert_eq!(v.resolve(base, "/", true).unwrap(), root.at());
    assert_eq!(v.stat(base, "/../f").unwrap().ino, rf);
    assert_eq!(v.stat(base, "abs").unwrap().ino, rf);
    // At a mount's root below the base root, `..` still steps out.
    v.mkdir(None, "/r/s/m", 0o755).unwrap();
    v.mount(None, "/r/s/m", ramfs()).unwrap();
    assert_eq!(v.resolve(base, "m/..", true).unwrap(), cwd.at());
    v.dir_put(cwd);
    v.dir_put(root);
    v.umount(None, "/r/s/m").unwrap();
    assert_dcache_sound(&v);
}

#[test]
fn dir_path_crosses_mounts() {
    let mut v = ram();
    v.mkdir(None, "/a", 0o755).unwrap();
    v.mount(None, "/a", ramfs()).unwrap();
    v.mkdir(None, "/a/b", 0o755).unwrap();
    v.mkdir(None, "/a/b/c", 0o755).unwrap();
    let c = v.dir_get(None, "/a/b/c").unwrap();
    let a = v.dir_get(None, "/a").unwrap();
    let top = v.dir_root().unwrap();
    let mut out = [0u8; 64];
    let n = v.dir_path(None, c.at(), &mut out).unwrap();
    assert_eq!(&out[..n], b"/a/b/c");
    let n = v.dir_path(None, a.at(), &mut out).unwrap();
    assert_eq!(&out[..n], b"/a");
    let n = v.dir_path(None, top.at(), &mut out).unwrap();
    assert_eq!(&out[..n], b"/");
    let under_a = Some(WalkBase {
        root: a.at(),
        cwd: a.at(),
    });
    let n = v.dir_path(under_a, c.at(), &mut out).unwrap();
    assert_eq!(&out[..n], b"/b/c");
    let mut short = [0u8; 5];
    assert_eq!(
        v.dir_path(None, c.at(), &mut short).unwrap_err(),
        FsError::NameTooLong
    );
    v.dir_put(top);
    v.dir_put(a);
    v.dir_put(c);
    v.rmdir(None, "/a/b/c").unwrap();
    v.umount(None, "/a").unwrap();
    assert_dcache_sound(&v);
}

/// Write `data` to `path`, made or emptied.
fn put_file(v: &mut Vfs, base: Option<WalkBase>, path: &str, data: &[u8]) {
    let f = v
        .open_path(base, path, O_WRONLY | O_CREAT | O_TRUNC, 0o644)
        .unwrap();
    assert_eq!(v.write(&f, data).unwrap(), data.len());
    v.close(f).unwrap();
}

/// What `path` holds, up to 32 bytes.
fn get_file(v: &mut Vfs, base: Option<WalkBase>, path: &str) -> Vec<u8> {
    let f = v.open_path(base, path, O_RDONLY, 0).unwrap();
    let mut buf = [0u8; 32];
    let n = v.read(&f, &mut buf).unwrap();
    v.close(f).unwrap();
    buf[..n].to_vec()
}

#[test]
fn cwd_ref_follows_rename() {
    let mut v = ram();
    v.mkdir(None, "/d", 0o755).unwrap();
    put_file(&mut v, None, "/d/f", b"old");
    let r = v.dir_get(None, "/d").unwrap();
    v.rename(None, "/d", "/e").unwrap();
    v.mkdir(None, "/d", 0o755).unwrap();
    put_file(&mut v, None, "/d/f", b"new");
    // The reference moved with its dentry: `f` from it is `/e/f`.
    let base = cwd_base(&v, &r);
    assert_eq!(get_file(&mut v, base, "f"), b"old");
    assert_eq!(get_file(&mut v, None, "/d/f"), b"new");
    assert_eq!(get_file(&mut v, None, "/e/f"), b"old");
    assert_eq!(v.resolve(None, "/e", true).unwrap(), r.at());
    let mut out = [0u8; 16];
    let n = v.dir_path(None, r.at(), &mut out).unwrap();
    assert_eq!(&out[..n], b"/e");
    // A held directory moves under a new parent too.
    v.rename(None, "/e", "/d/g").unwrap();
    let n = v.dir_path(None, r.at(), &mut out).unwrap();
    assert_eq!(&out[..n], b"/d/g");
    let base = cwd_base(&v, &r);
    assert_eq!(get_file(&mut v, base, "../f"), b"new");
    // Never below itself.
    assert_eq!(v.rename(None, "/d", "/d/g/x").unwrap_err(), FsError::Inval);
    v.dir_put(r);
    assert_dcache_sound(&v);
}

#[test]
fn cwd_ref_rmdir_then_mkdir() {
    let mut v = ram();
    v.mkdir(None, "/r", 0o755).unwrap();
    let r = v.dir_get(None, "/r").unwrap();
    let old = st_ino_of(&v, r.at());
    v.rmdir(None, "/r").unwrap();
    v.mkdir(None, "/r", 0o755).unwrap();
    let new = v.stat(None, "/r").unwrap().ino;
    assert_ne!(new, old);
    assert_ne!(v.resolve(None, "/r", true).unwrap(), r.at());
    let base = cwd_base(&v, &r);
    assert_eq!(v.creat(base, "g", 0o644).unwrap_err(), FsError::NotFound);
    assert_eq!(v.mkdir(base, "h", 0o755).unwrap_err(), FsError::NotFound);
    assert_eq!(v.stat(base, "g").unwrap_err(), FsError::NotFound);
    assert_eq!(
        v.open_path(base, "g", O_WRONLY | O_CREAT, 0o644)
            .unwrap_err(),
        FsError::NotFound
    );
    let mut out = [0u8; 16];
    assert_eq!(
        v.dir_path(None, r.at(), &mut out).unwrap_err(),
        FsError::NotFound
    );
    // `..` still leads out of it.
    assert_eq!(v.stat(base, "../r").unwrap().ino, new);
    // A failed rmdir leaves a held directory its name.
    v.creat(None, "/r/k", 0o644).unwrap();
    let r2 = v.dir_get(None, "/r").unwrap();
    assert!(v.rmdir(None, "/r").is_err());
    assert_eq!(v.stat(cwd_base(&v, &r2), "k").unwrap().kind, InodeKind::Reg);
    v.dir_put(r2);
    let slot = r.at().dslot;
    v.dir_put(r);
    assert!(!v.dentries[slot as usize].used || v.dentries[slot as usize].name.eq_bytes(b"r"));
    assert_dcache_sound(&v);
}

#[test]
fn rename_mountpoint_busy() {
    let mut v = ram();
    v.mkdir(None, "/m", 0o755).unwrap();
    v.mkdir(None, "/n", 0o755).unwrap();
    v.creat(None, "/f", 0o644).unwrap();
    v.mount(None, "/m", ramfs()).unwrap();
    assert_eq!(v.rename(None, "/m", "/o").unwrap_err(), FsError::Busy);
    assert_eq!(v.rename(None, "/n", "/m").unwrap_err(), FsError::Busy);
    assert_eq!(v.rmdir(None, "/m").unwrap_err(), FsError::Busy);
    // A directory's kind answers before its mount: a file never replaces
    // it, and unlink never takes it.
    assert_eq!(v.rename(None, "/f", "/m").unwrap_err(), FsError::IsDir);
    assert_eq!(v.unlink(None, "/m").unwrap_err(), FsError::IsDir);
    v.umount(None, "/m").unwrap();
    v.rename(None, "/m", "/o").unwrap();
    assert_dcache_sound(&v);
}

/// unlink(2) of a directory is EISDIR, with a trailing slash or without,
/// and leaves it; rmdir(2) of a file is ENOTDIR.
#[test]
fn unlink_of_a_directory_is_eisdir() {
    let mut v = ram();
    v.mkdir(None, "/d", 0o755).unwrap();
    v.creat(None, "/f", 0o644).unwrap();
    assert_eq!(v.unlink(None, "/d").unwrap_err(), FsError::IsDir);
    assert_eq!(v.unlink(None, "/d/").unwrap_err(), FsError::IsDir);
    assert_eq!(v.rmdir(None, "/f").unwrap_err(), FsError::NotDir);
    assert_eq!(v.stat(None, "/d").unwrap().kind, InodeKind::Dir);
    v.rmdir(None, "/d").unwrap();
    v.unlink(None, "/f").unwrap();
    assert_dcache_sound(&v);
}

/// A namespace change whose last component is `.`, `..`, or none (a path
/// of slashes) gets Linux's errno for that change, after its parent's
/// walk, whose errors come first: unlink(2) EISDIR; rmdir(2) EINVAL for
/// `.`, ENOTEMPTY for `..` and EBUSY for the root; rename(2) EBUSY on
/// either side, after EXDEV; mkdir(2), symlink(2) and link(2)'s new name
/// EEXIST. Nothing changes.
#[test]
fn dot_dotdot_and_root_last_components_get_linux_errnos() {
    use FsError::*;
    let mut v = ram();
    v.mkdir(None, "/d", 0o755).unwrap();
    v.creat(None, "/f", 0o644).unwrap();
    v.mkdir(None, "/m", 0o755).unwrap();
    v.mount(None, "/m", ramfs()).unwrap();
    for p in ["/d/.", "/d/..", "/", "//", ".", "/d/./"] {
        assert_eq!(v.unlink(None, p), Err(IsDir), "unlink {p}");
    }
    for (p, e) in [
        ("/d/.", Inval),
        ("/d/..", NotEmpty),
        ("/", Busy),
        (".", Inval),
        ("..", NotEmpty),
    ] {
        assert_eq!(v.rmdir(None, p), Err(e), "rmdir {p}");
    }
    for (a, b) in [
        ("/d/.", "/e"),
        ("/f", "/d/.."),
        ("/", "/e"),
        ("/f", "/"),
        ("/d/..", "/d/."),
    ] {
        assert_eq!(v.rename(None, a, b), Err(Busy), "rename {a} {b}");
    }
    assert_eq!(v.rename(None, "/m/.", "/e"), Err(XDev));
    for p in ["/d/.", "/d/..", "/", "/d/./"] {
        assert_eq!(v.mkdir(None, p, 0o755), Err(Exists), "mkdir {p}");
        assert_eq!(v.symlink(None, p, "f"), Err(Exists), "symlink {p}");
        assert_eq!(v.link(None, "/f", p), Err(Exists), "link {p}");
    }
    // The parent's walk comes first.
    assert_eq!(v.unlink(None, "/none/."), Err(NotFound));
    assert_eq!(v.rmdir(None, "/f/.."), Err(NotDir));
    assert_eq!(v.mkdir(None, "/none/..", 0o755), Err(NotFound));
    assert_eq!(v.rename(None, "/none/.", "/e"), Err(NotFound));
    assert_eq!(v.rename(None, "/d/.", "/none/e"), Err(NotFound));
    assert_eq!(v.stat(None, "/d").unwrap().kind, InodeKind::Dir);
    assert_eq!(v.stat(None, "/f").unwrap().kind, InodeKind::Reg);
    assert_eq!(v.stat(None, "/e").unwrap_err(), NotFound);
    v.umount(None, "/m").unwrap();
    assert_dcache_sound(&v);
}

#[test]
fn walk_trailing_slash() {
    let mut v = ram();
    v.mkdir(None, "/a", 0o755).unwrap();
    v.mkdir(None, "/a/b", 0o755).unwrap();
    v.creat(None, "/f", 0o644).unwrap();
    v.symlink(None, "/l", "a/b").unwrap();
    v.symlink(None, "/lf", "f").unwrap();
    let b = v.resolve(None, "/a/b", true).unwrap();
    // Repeated slashes are one, `.` stays.
    assert_eq!(v.resolve(None, "//a///b", true).unwrap(), b);
    assert_eq!(v.resolve(None, "/./a/./b/.", true).unwrap(), b);
    // A component followed by `/` names a directory, following a link
    // even where the last component's would not be.
    assert_eq!(v.resolve(None, "/a/b/", true).unwrap(), b);
    assert_eq!(v.resolve(None, "/l/", false).unwrap(), b);
    assert_eq!(v.lstat(None, "/l").unwrap().kind, InodeKind::Lnk);
    assert_eq!(v.lstat(None, "/l/").unwrap().kind, InodeKind::Dir);
    assert_eq!(v.stat(None, "/f/").unwrap_err(), FsError::NotDir);
    assert_eq!(v.stat(None, "/lf/").unwrap_err(), FsError::NotDir);
    assert_eq!(v.stat(None, "/f/x").unwrap_err(), FsError::NotDir);
    assert_eq!(v.stat(None, "/f/..").unwrap_err(), FsError::NotDir);
    assert_eq!(v.stat(None, "/nope/").unwrap_err(), FsError::NotFound);
    // `..` after a link is the parent of the link's target.
    v.creat(None, "/a/x", 0o644).unwrap();
    assert_eq!(
        v.stat(None, "/l/../x").unwrap().ino,
        v.stat(None, "/a/x").unwrap().ino
    );
    // Only mkdir makes a name followed by `/`.
    v.mkdir(None, "/n/", 0o755).unwrap();
    assert_eq!(v.stat(None, "/n").unwrap().kind, InodeKind::Dir);
    assert_eq!(v.creat(None, "/m/", 0o644).unwrap_err(), FsError::NotDir);
    assert_eq!(
        v.open_path(None, "/m/", O_WRONLY | O_CREAT, 0o644)
            .unwrap_err(),
        FsError::NotDir
    );
    assert_eq!(v.symlink(None, "/s/", "f").unwrap_err(), FsError::NotDir);
    assert_eq!(v.link(None, "/f", "/h/").unwrap_err(), FsError::NotDir);
    assert_eq!(v.stat(None, "/m").unwrap_err(), FsError::NotFound);
    // A file named with `/` after it is not removed or moved.
    assert_eq!(v.unlink(None, "/f/").unwrap_err(), FsError::NotDir);
    assert_eq!(v.rename(None, "/f/", "/g").unwrap_err(), FsError::NotDir);
    assert_eq!(v.rename(None, "/f", "/g/").unwrap_err(), FsError::NotDir);
    assert_eq!(v.stat(None, "/f").unwrap().kind, InodeKind::Reg);
    v.rename(None, "/n/", "/o/").unwrap();
    v.rmdir(None, "/o/").unwrap();
    assert_dcache_sound(&v);
}

/// A ramfs whose names compare without regard to ASCII case in the
/// dentry cache, as FAT's do.
struct CiFs {
    ram: &'static RamFs<std::sync::Mutex<RamState>>,
}

impl FileSystem for CiFs {
    fn name(&self) -> &'static str {
        "ci"
    }
    fn fstype(&self) -> FsType {
        FsType::Ram
    }
    fn ops(&'static self) -> Option<&'static dyn InodeOps> {
        Some(self)
    }
    fn fill_super(&self, cx: &mut OpCx<'_>) -> Result<InodeInfo, FsError> {
        self.ram.fill_super(cx)
    }
}

impl InodeOps for CiFs {
    fn name_eq(&self, cached: &[u8], asked: &[u8]) -> bool {
        cached.eq_ignore_ascii_case(asked)
    }
    fn lookup(&self, cx: &mut OpCx<'_>, dir: &Inode, name: &[u8]) -> Result<InodeInfo, FsError> {
        self.ram.lookup(cx, dir, name)
    }
    fn create(
        &self,
        cx: &mut OpCx<'_>,
        dir: &mut Inode,
        name: &[u8],
        kind: InodeKind,
        mode: u16,
        target: Option<&[u8]>,
    ) -> Result<InodeInfo, FsError> {
        self.ram.create(cx, dir, name, kind, mode, target)
    }
    fn getattr(&self, cx: &mut OpCx<'_>, ino: &mut Inode) -> Result<(), FsError> {
        self.ram.getattr(cx, ino)
    }
    fn evict(&self, cx: &mut OpCx<'_>, ino: &Inode) -> Result<(), FsError> {
        self.ram.evict(cx, ino)
    }
}

#[test]
fn walk_case_fold_finds_mount_dentry() {
    let mut v = crate::fs::host_vfs();
    let ci: &'static CiFs = std::boxed::Box::leak(std::boxed::Box::new(CiFs { ram: ramfs() }));
    v.mount_root_fs(ci).unwrap();
    v.mkdir(None, "/vibe", 0o755).unwrap();
    v.mount(None, "/vibe", ramfs()).unwrap();
    v.creat(None, "/vibe/f", 0o644).unwrap();
    let vibe = v.resolve(None, "/vibe", true).unwrap();
    assert_ne!(vibe.mount, 0, "/vibe is the mount's root");
    // Another spelling finds the dentry the mount is on, and so the mount.
    assert_eq!(v.resolve(None, "/VIBE", true).unwrap(), vibe);
    assert_eq!(v.resolve(None, "/ViBe/", true).unwrap(), vibe);
    assert_eq!(
        v.stat(None, "/VIBE/f").unwrap().ino,
        v.stat(None, "/vibe/f").unwrap().ino
    );
    // The mounted ramfs compares bytes: `/vibe/F` is another name.
    assert_eq!(v.stat(None, "/VIBE/F").unwrap_err(), FsError::NotFound);
    let named = v
        .dentries
        .iter()
        .filter(|d| d.used && d.name.as_bytes().eq_ignore_ascii_case(b"vibe"))
        .count();
    assert_eq!(named, 1, "one dentry for the name");
    v.umount(None, "/VIBE").unwrap();
    assert_dcache_sound(&v);
}

#[test]
fn open_reserves_file_slot_first() {
    let mut v = ram();
    put_file(&mut v, None, "/t", b"0123456789");
    // Fill the open-file table.
    let mut held = Vec::new();
    loop {
        match v.open_path(None, "/t", O_RDONLY, 0) {
            Ok(f) => held.push(f),
            Err(e) => {
                assert_eq!(e, FsError::NFile);
                break;
            }
        }
    }
    assert_eq!(held.len(), v.files.len());
    // Neither a truncate nor a create happens without a slot.
    assert_eq!(
        v.open_path(None, "/t", O_WRONLY | O_TRUNC, 0).unwrap_err(),
        FsError::NFile
    );
    assert_eq!(
        v.open_path(None, "/n", O_WRONLY | O_CREAT, 0o644)
            .unwrap_err(),
        FsError::NFile
    );
    assert_eq!(v.stat(None, "/t").unwrap().size, 10);
    assert_eq!(v.stat(None, "/n").unwrap_err(), FsError::NotFound);
    // A failed open gives its reservation back.
    let f = held.pop().unwrap();
    v.close(f).unwrap();
    assert_eq!(
        v.open_path(None, "/missing", O_RDONLY, 0).unwrap_err(),
        FsError::NotFound
    );
    assert_eq!(
        v.open_path(None, "/t/", O_RDONLY, 0).unwrap_err(),
        FsError::NotDir
    );
    let f = v.open_path(None, "/t", O_WRONLY | O_TRUNC, 0).unwrap();
    assert_eq!(v.stat(None, "/t").unwrap().size, 0);
    held.push(f);
    for f in held {
        v.close(f).unwrap();
    }
    assert!(v.files.iter().all(|f| !f.used && !f.reserved));
}

/// A rename onto an existing name replaces it, as rename(2) does: a file
/// replaces a file, whose inode goes once nothing holds it, and a
/// directory an empty directory; a directory never replaces a file
/// (`ENOTDIR`), nor a file a directory (`EISDIR`), and a directory with
/// entries stays (`ENOTEMPTY`); two names of one file stay as they are.
#[test]
fn rename_replaces_as_linux() {
    let mut v = ram();
    let used = |v: &Vfs| v.inodes.iter().filter(|n| n.used).count();
    let before = used(&v);
    put_file(&mut v, None, "/a", b"new");
    put_file(&mut v, None, "/b", b"old");
    v.rename(None, "/a", "/b").unwrap();
    assert_eq!(v.stat(None, "/a").unwrap_err(), FsError::NotFound);
    assert_eq!(get_file(&mut v, None, "/b"), b"new");
    assert_eq!(v.stat(None, "/b").unwrap().nlink, 1);
    v.mkdir(None, "/d", 0o755).unwrap();
    v.mkdir(None, "/e", 0o755).unwrap();
    assert_eq!(v.rename(None, "/b", "/d").unwrap_err(), FsError::IsDir);
    assert_eq!(v.rename(None, "/d", "/b").unwrap_err(), FsError::NotDir);
    put_file(&mut v, None, "/e/f", b"x");
    assert_eq!(v.rename(None, "/d", "/e").unwrap_err(), FsError::NotEmpty);
    v.unlink(None, "/e/f").unwrap();
    let links = v.stat(None, "/").unwrap().nlink;
    v.rename(None, "/d", "/e").unwrap();
    assert_eq!(v.stat(None, "/d").unwrap_err(), FsError::NotFound);
    assert_eq!(v.stat(None, "/e").unwrap().kind, InodeKind::Dir);
    // The root lost the replaced directory's `..`.
    assert_eq!(v.stat(None, "/").unwrap().nlink, links - 1);
    v.link(None, "/b", "/c").unwrap();
    v.rename(None, "/b", "/c").unwrap();
    assert_eq!(v.stat(None, "/b").unwrap().nlink, 2);
    assert_eq!(v.stat(None, "/c").unwrap().nlink, 2);
    v.unlink(None, "/b").unwrap();
    v.unlink(None, "/c").unwrap();
    v.rmdir(None, "/e").unwrap();
    assert_eq!(used(&v), before);
    assert_dcache_sound(&v);
}

/// A rename that changes only a name's case on a filesystem that keys a
/// file by where its entry sits (FAT) moves the file to a new key: the
/// cached inode follows it, so the next file made at the freed key is a
/// new inode, not the renamed file's under another name.
#[test]
fn case_only_rename_moves_the_inode_to_its_new_key() {
    let mut v = crate::fs::host_vfs();
    v.mount_root_fs(foldfs_new()).unwrap();
    put_file(&mut v, None, "/m", b"MDATA");
    let held = v.open_path(None, "/m", O_RDWR, 0).unwrap();
    v.rename(None, "/m", "/M").unwrap();
    put_file(&mut v, None, "/n", b"NN");
    assert_eq!(get_file(&mut v, None, "/M"), b"MDATA");
    assert_eq!(get_file(&mut v, None, "/n"), b"NN");
    // A descriptor held across the rename writes the renamed file.
    assert_eq!(v.write(&held, b"m").unwrap(), 1);
    v.close(held).unwrap();
    assert_eq!(get_file(&mut v, None, "/M"), b"mDATA");
    assert_eq!(get_file(&mut v, None, "/n"), b"NN");
    assert_dcache_sound(&v);
}

/// A namespace change a hook makes, as another thread would.
#[derive(Clone, Copy)]
enum Change {
    Unlink(&'static [u8]),
    Rmdir(&'static [u8]),
    Rename(&'static [u8], &'static [u8]),
    /// Create a file and write these bytes to it.
    Create(&'static [u8], &'static [u8]),
}

/// What a hook runs once: these changes, in order, on VFS `.0`.
type Race = (&'static std::sync::Mutex<Vfs>, &'static [Change]);

/// Each test's race, so tests running in parallel keep their own.
static UNLINK_RACE: std::sync::Mutex<Option<Race>> = std::sync::Mutex::new(None);
static OVER_RACE: std::sync::Mutex<Option<Race>> = std::sync::Mutex::new(None);
static SRC_RACE: std::sync::Mutex<Option<Race>> = std::sync::Mutex::new(None);
static LINK_RACE: std::sync::Mutex<Option<Race>> = std::sync::Mutex::new(None);
static AWAY_RACE: std::sync::Mutex<Option<Race>> = std::sync::Mutex::new(None);
static SRC_AWAY_RACE: std::sync::Mutex<Option<Race>> = std::sync::Mutex::new(None);
static MADE_RACE: std::sync::Mutex<Option<Race>> = std::sync::Mutex::new(None);

fn run_race(race: &std::sync::Mutex<Option<Race>>) {
    let Some((vfs, changes)) = race.lock().unwrap().take() else {
        return;
    };
    let api = FileApi::new(vfs);
    for c in changes {
        match *c {
            Change::Unlink(a) => api.unlink(None, a).unwrap(),
            Change::Rmdir(a) => api.rmdir(None, a).unwrap(),
            Change::Rename(a, b) => api.rename(None, a, b).unwrap(),
            Change::Create(a, data) => write_new(vfs, a, data),
        }
    }
}

fn unlink_window() {
    run_race(&UNLINK_RACE);
}

fn over_window() {
    run_race(&OVER_RACE);
}

fn src_window() {
    run_race(&SRC_RACE);
}

fn link_window() {
    run_race(&LINK_RACE);
}

fn away_window() {
    run_race(&AWAY_RACE);
}

fn src_away_window() {
    run_race(&SRC_AWAY_RACE);
}

fn made_window() {
    run_race(&MADE_RACE);
}

static CALL_MADE_RACE: std::sync::Mutex<Option<Race>> = std::sync::Mutex::new(None);
static CALL_SWAP_RACE: std::sync::Mutex<Option<Race>> = std::sync::Mutex::new(None);
static CALL_SRC_RACE: std::sync::Mutex<Option<Race>> = std::sync::Mutex::new(None);
static TMP_CALL_RACE: std::sync::Mutex<Option<Race>> = std::sync::Mutex::new(None);

fn call_made_window() {
    run_race(&CALL_MADE_RACE);
}

fn call_swap_window() {
    run_race(&CALL_SWAP_RACE);
}

fn call_src_window() {
    run_race(&CALL_SRC_RACE);
}

fn tmp_call_window() {
    run_race(&TMP_CALL_RACE);
}

static RAM_RMDIR_RACE: std::sync::Mutex<Option<Race>> = std::sync::Mutex::new(None);
static TMP_RMDIR_RACE: std::sync::Mutex<Option<Race>> = std::sync::Mutex::new(None);
static TMP_RMDIR_DIR_RACE: std::sync::Mutex<Option<Race>> = std::sync::Mutex::new(None);

fn ram_rmdir_window() {
    run_race(&RAM_RMDIR_RACE);
}

fn tmp_rmdir_window() {
    run_race(&TMP_RMDIR_RACE);
}

fn tmp_rmdir_dir_window() {
    run_race(&TMP_RMDIR_DIR_RACE);
}

/// A File API on `vfs` whose namespace changes run `window` between their
/// walks and their begin steps.
fn racing(
    vfs: &'static std::sync::Mutex<Vfs>,
    window: fn(),
) -> FileApi<'static, std::sync::Mutex<Vfs>> {
    FileApi::with_hooks(
        vfs,
        Hooks {
            change_window: window,
            ..Hooks::NONE
        },
    )
}

/// A File API on `vfs` whose renames run `window` between their begin
/// steps and their backend calls.
fn racing_call(
    vfs: &'static std::sync::Mutex<Vfs>,
    window: fn(),
) -> FileApi<'static, std::sync::Mutex<Vfs>> {
    FileApi::with_hooks(
        vfs,
        Hooks {
            rename_window: window,
            ..Hooks::NONE
        },
    )
}

/// `vfs` with `LockedFs` over a fresh `KeyFs` store at `/blk`, and that
/// store's id. `KeyOps::evict` fails a test that evicts a held inode.
fn keyfs_at_blk(vfs: &'static std::sync::Mutex<Vfs>) -> u64 {
    let fs: &'static LockedFs = std::boxed::Box::leak(std::boxed::Box::new(LockedFs {
        key: keyfs_new(),
        vfs,
        calls: std::sync::atomic::AtomicU32::new(0),
    }));
    let vol = crate::dev::instance(7u32).unwrap();
    FileApi::new(vfs)
        .mount_fs(None, b"/blk", fs, Some(3), false, Some(vol))
        .unwrap();
    fs.key.id
}

fn make(vfs: &'static std::sync::Mutex<Vfs>, path: &[u8]) {
    let api = FileApi::new(vfs);
    let f = api
        .open(None, path, OpenFlags::from_bits(O_RDWR | O_CREAT), 0o644)
        .unwrap();
    api.close(f).unwrap();
}

fn used_inodes(vfs: &std::sync::Mutex<Vfs>) -> usize {
    vfs.lock().unwrap().inodes.iter().filter(|n| n.used).count()
}

/// An unlink whose name a racing unlink takes between its walk and its
/// begin step finds the name gone, and holds no inode whose release the
/// racer's last put queued: `evict` runs once, on an inode nothing holds.
#[test]
fn unlink_racing_unlink_is_not_found() {
    let vfs = locked_vfs();
    let id = keyfs_at_blk(vfs);
    make(vfs, b"/blk/f");
    *UNLINK_RACE.lock().unwrap() = Some((vfs, &[Change::Unlink(b"/blk/f")]));
    let api = racing(vfs, unlink_window);
    assert_eq!(api.unlink(None, b"/blk/f").unwrap_err(), FsError::NotFound);
    assert_eq!(with_store(id, |s| s.evicts), 1);
}

/// An unlink whose name a racing rename replaces between its walk and its
/// begin step unlinks the file the name holds now, as an unlink after the
/// rename would: both names are gone and both inodes are released.
#[test]
fn unlink_racing_rename_over_takes_the_new_file() {
    let vfs = locked_vfs();
    let before = used_inodes(vfs);
    make(vfs, b"/a");
    make(vfs, b"/b");
    *OVER_RACE.lock().unwrap() = Some((vfs, &[Change::Rename(b"/a", b"/b")]));
    racing(vfs, over_window).unlink(None, b"/b").unwrap();
    let api = FileApi::new(vfs);
    for p in [b"/a" as &[u8], b"/b"] {
        assert_eq!(api.walk(None, p, true).unwrap_err(), FsError::NotFound);
    }
    assert_eq!(used_inodes(vfs), before);
}

/// A rename whose source a racing unlink takes between its walks and its
/// begin step finds the source gone, and evicts no inode it holds.
#[test]
fn rename_racing_unlink_is_not_found() {
    let vfs = locked_vfs();
    let id = keyfs_at_blk(vfs);
    make(vfs, b"/blk/a");
    *SRC_RACE.lock().unwrap() = Some((vfs, &[Change::Unlink(b"/blk/a")]));
    let api = racing(vfs, src_window);
    assert_eq!(
        api.rename(None, b"/blk/a", b"/blk/c").unwrap_err(),
        FsError::NotFound
    );
    assert_eq!(with_store(id, |s| s.evicts), 1);
    let api = FileApi::new(vfs);
    assert_eq!(
        api.walk(None, b"/blk/c", true).unwrap_err(),
        FsError::NotFound
    );
}

/// A link whose source a racing unlink takes between its walks and its
/// begin step gives the unlinked file no new name, as Linux's `link`
/// refuses a file with no links (ENOENT), and the file is released.
#[test]
fn link_racing_unlink_is_not_found() {
    let vfs = locked_vfs();
    let before = used_inodes(vfs);
    make(vfs, b"/f");
    *LINK_RACE.lock().unwrap() = Some((vfs, &[Change::Unlink(b"/f")]));
    let api = racing(vfs, link_window);
    assert_eq!(api.link(None, b"/f", b"/g").unwrap_err(), FsError::NotFound);
    let api = FileApi::new(vfs);
    assert_eq!(api.walk(None, b"/g", true).unwrap_err(), FsError::NotFound);
    assert_eq!(used_inodes(vfs), before);
}

/// Create `path` on `vfs`, or truncate it, and write `data` to it.
fn write_new(vfs: &'static std::sync::Mutex<Vfs>, path: &[u8], data: &[u8]) {
    let api = FileApi::new(vfs);
    let flags = OpenFlags::from_bits(O_RDWR | O_CREAT | O_TRUNC);
    let f = api.open(None, path, flags, 0o644).unwrap();
    assert_eq!(api.write(&f, data).unwrap(), data.len());
    api.close(f).unwrap();
}

/// The first bytes of the file `path` names on `vfs`.
fn read_all(vfs: &'static std::sync::Mutex<Vfs>, path: &[u8]) -> Vec<u8> {
    let api = FileApi::new(vfs);
    let f = api
        .open(None, path, OpenFlags::from_bits(O_RDONLY), 0)
        .unwrap();
    let mut buf = [0u8; 64];
    let n = api.read(&f, &mut buf).unwrap();
    api.close(f).unwrap();
    buf[..n].to_vec()
}

/// A `Vfs` behind a lock with a ramfs root, and that ramfs, whose store
/// counts the nodes in use.
fn ram_vfs() -> (
    &'static std::sync::Mutex<Vfs>,
    &'static RamFs<std::sync::Mutex<RamState>>,
) {
    let vfs: &'static std::sync::Mutex<Vfs> = std::boxed::Box::leak(std::boxed::Box::new(
        std::sync::Mutex::new(crate::fs::host_vfs()),
    ));
    let fs = ramfs();
    FileApi::new(vfs).mount_root(fs, None, false, None).unwrap();
    (vfs, fs)
}

/// An unlink whose victim a racing rename moves away, and whose name a
/// second racing rename gives another file, between its walk and its
/// begin step, unlinks the file the name holds now, as an unlink after
/// the renames would: the moved file keeps its one link, and the
/// unlinked one's node is freed.
#[test]
fn unlink_racing_rename_away_takes_the_new_file() {
    let (vfs, fs) = ram_vfs();
    let base = fs.with(|s| s.used());
    write_new(vfs, b"/a", b"A");
    write_new(vfs, b"/b", b"B");
    *AWAY_RACE.lock().unwrap() = Some((
        vfs,
        &[Change::Rename(b"/a", b"/c"), Change::Rename(b"/b", b"/a")],
    ));
    racing(vfs, away_window).unlink(None, b"/a").unwrap();
    let api = FileApi::new(vfs);
    assert_eq!(api.walk(None, b"/a", true).unwrap_err(), FsError::NotFound);
    assert_eq!(api.stat_path(None, b"/c", true).unwrap().nlink, 1);
    assert_eq!(read_all(vfs, b"/c"), b"A");
    api.unlink(None, b"/c").unwrap();
    assert_eq!(fs.with(|s| s.used()), base);
}

/// A rename whose source a racing rename moves away, and whose old name
/// a second racing rename gives another file, between its walks and its
/// begin step, moves the file the name holds now, and the old name is
/// gone from the dentry cache too.
#[test]
fn rename_racing_rename_away_moves_the_new_file() {
    let (vfs, fs) = ram_vfs();
    let base = fs.with(|s| s.used());
    write_new(vfs, b"/a", b"A");
    write_new(vfs, b"/b", b"B");
    *SRC_AWAY_RACE.lock().unwrap() = Some((
        vfs,
        &[Change::Rename(b"/a", b"/c"), Change::Rename(b"/b", b"/a")],
    ));
    racing(vfs, src_away_window)
        .rename(None, b"/a", b"/d")
        .unwrap();
    let api = FileApi::new(vfs);
    assert_eq!(api.walk(None, b"/a", true).unwrap_err(), FsError::NotFound);
    assert_eq!(read_all(vfs, b"/c"), b"A");
    assert_eq!(read_all(vfs, b"/d"), b"B");
    api.unlink(None, b"/c").unwrap();
    api.unlink(None, b"/d").unwrap();
    assert_eq!(fs.with(|s| s.used()), base);
    assert_dcache_sound(&vfs.lock().unwrap());
}

/// A rename whose target a racing create makes between its walks, which
/// found the name free, and its begin step replaces the new file, as
/// rename(2) does, and that file's node is freed.
#[test]
fn rename_racing_create_of_its_target_replaces_it() {
    let (vfs, fs) = ram_vfs();
    let base = fs.with(|s| s.used());
    write_new(vfs, b"/a", b"A");
    *MADE_RACE.lock().unwrap() = Some((vfs, &[Change::Create(b"/b", b"B")]));
    racing(vfs, made_window).rename(None, b"/a", b"/b").unwrap();
    let api = FileApi::new(vfs);
    assert_eq!(api.walk(None, b"/a", true).unwrap_err(), FsError::NotFound);
    assert_eq!(read_all(vfs, b"/b"), b"A");
    assert_eq!(api.stat_path(None, b"/b", true).unwrap().nlink, 1);
    api.unlink(None, b"/b").unwrap();
    assert_eq!(fs.with(|s| s.used()), base);
}

/// A rename whose target a racing create makes after its begin step, with
/// the VFS lock dropped before the backend call, replaces the new file,
/// whose node is freed: the backend refuses a name the walks did not find
/// (`RenameSeen`), and the rename walks again.
#[test]
fn rename_racing_create_before_its_backend_call_replaces_it() {
    let (vfs, fs) = ram_vfs();
    let base = fs.with(|s| s.used());
    write_new(vfs, b"/a", b"A");
    *CALL_MADE_RACE.lock().unwrap() = Some((vfs, &[Change::Create(b"/b", b"B")]));
    racing_call(vfs, call_made_window)
        .rename(None, b"/a", b"/b")
        .unwrap();
    let api = FileApi::new(vfs);
    assert_eq!(api.walk(None, b"/a", true).unwrap_err(), FsError::NotFound);
    assert_eq!(read_all(vfs, b"/b"), b"A");
    assert_eq!(api.stat_path(None, b"/b", true).unwrap().nlink, 1);
    api.unlink(None, b"/b").unwrap();
    assert_eq!(fs.with(|s| s.used()), base);
}

/// A rename whose target a racing unlink and create replace with another
/// file after its begin step replaces that new file, and both the file
/// the walk found and the new one are freed.
#[test]
fn rename_racing_swap_of_its_target_replaces_the_new_file() {
    let (vfs, fs) = ram_vfs();
    let base = fs.with(|s| s.used());
    write_new(vfs, b"/a", b"A");
    write_new(vfs, b"/b", b"B");
    *CALL_SWAP_RACE.lock().unwrap() =
        Some((vfs, &[Change::Unlink(b"/b"), Change::Create(b"/b", b"C")]));
    racing_call(vfs, call_swap_window)
        .rename(None, b"/a", b"/b")
        .unwrap();
    assert_eq!(read_all(vfs, b"/b"), b"A");
    FileApi::new(vfs).unlink(None, b"/b").unwrap();
    assert_eq!(fs.with(|s| s.used()), base);
}

/// A rename whose source a racing rename moves away, and whose old name a
/// racing create reuses, after its begin step moves the new file, and the
/// old name is gone from the dentry cache too.
#[test]
fn rename_racing_swap_of_its_source_moves_the_new_file() {
    let (vfs, fs) = ram_vfs();
    let base = fs.with(|s| s.used());
    write_new(vfs, b"/a", b"A");
    *CALL_SRC_RACE.lock().unwrap() = Some((
        vfs,
        &[Change::Rename(b"/a", b"/c"), Change::Create(b"/a", b"N")],
    ));
    racing_call(vfs, call_src_window)
        .rename(None, b"/a", b"/b")
        .unwrap();
    let api = FileApi::new(vfs);
    assert_eq!(api.walk(None, b"/a", true).unwrap_err(), FsError::NotFound);
    assert_eq!(read_all(vfs, b"/b"), b"N");
    assert_eq!(read_all(vfs, b"/c"), b"A");
    api.unlink(None, b"/b").unwrap();
    api.unlink(None, b"/c").unwrap();
    assert_eq!(fs.with(|s| s.used()), base);
    assert_dcache_sound(&vfs.lock().unwrap());
}

/// `vfs` with tmpfs at `/tmp`, and the kernfs store it is a skin of.
fn tmp_at(
    vfs: &'static std::sync::Mutex<Vfs>,
) -> &'static crate::fs::kernfs::KernFs<std::sync::Mutex<crate::fs::kernfs::KernState>> {
    use crate::fs::kernfs::{KernFs, KernSkin, KernState};
    let kfs: &'static KernFs<std::sync::Mutex<KernState>> = std::boxed::Box::leak(
        std::boxed::Box::new(KernFs::new(std::sync::Mutex::new(KernState::new()))),
    );
    let tmp: &'static KernSkin<std::sync::Mutex<KernState>> =
        std::boxed::Box::leak(std::boxed::Box::new(KernSkin::new(kfs, FsType::Tmp)));
    let api = FileApi::new(vfs);
    api.mkdir(None, b"/tmp", 0o755).unwrap();
    api.mount_fs(None, b"/tmp", tmp, None, false, None).unwrap();
    kfs
}

/// On tmpfs too, a rename whose target a racing create makes after its
/// begin step replaces the new file, whose node and data are freed.
#[test]
fn tmpfs_rename_racing_create_before_its_backend_call_replaces_it() {
    let (vfs, _) = ram_vfs();
    let kfs = tmp_at(vfs);
    let base = kfs.tmp_nodes().unwrap().0;
    write_new(vfs, b"/tmp/a", b"A");
    *TMP_CALL_RACE.lock().unwrap() = Some((vfs, &[Change::Create(b"/tmp/b", b"BBBB")]));
    racing_call(vfs, tmp_call_window)
        .rename(None, b"/tmp/a", b"/tmp/b")
        .unwrap();
    assert_eq!(read_all(vfs, b"/tmp/b"), b"A");
    FileApi::new(vfs).unlink(None, b"/tmp/b").unwrap();
    assert_eq!(kfs.tmp_nodes().unwrap().0, base);
}

/// A rename into a directory a racing rmdir removes after the rename's
/// begin step fails with `NotFound`, as Linux refuses a dead directory,
/// and leaves its source where it was, on ramfs and on tmpfs: a node
/// moved into the removed directory would be unreachable and never freed.
#[test]
fn rename_into_a_directory_removed_before_its_backend_call_is_not_found() {
    let (vfs, fs) = ram_vfs();
    let kfs = tmp_at(vfs);
    let api = FileApi::new(vfs);
    let ram_base = fs.with(|s| s.used());
    let tmp_base = kfs.tmp_nodes().unwrap().0;
    // The directory, the source, its new name, whether the source is a
    // directory, and the race that removes the directory.
    type Case = (
        &'static [u8],
        &'static [u8],
        &'static [u8],
        bool,
        &'static std::sync::Mutex<Option<Race>>,
        &'static [Change],
        fn(),
    );
    let cases: [Case; 3] = [
        (
            b"/d",
            b"/f",
            b"/d/f",
            false,
            &RAM_RMDIR_RACE,
            &[Change::Rmdir(b"/d")],
            ram_rmdir_window,
        ),
        (
            b"/tmp/d",
            b"/tmp/f",
            b"/tmp/d/f",
            false,
            &TMP_RMDIR_RACE,
            &[Change::Rmdir(b"/tmp/d")],
            tmp_rmdir_window,
        ),
        (
            b"/tmp/e",
            b"/tmp/g",
            b"/tmp/e/g",
            true,
            &TMP_RMDIR_DIR_RACE,
            &[Change::Rmdir(b"/tmp/e")],
            tmp_rmdir_dir_window,
        ),
    ];
    for (d, f, to, dir, race, rmdir, window) in cases {
        api.mkdir(None, d, 0o755).unwrap();
        if dir {
            api.mkdir(None, f, 0o755).unwrap();
        } else {
            write_new(vfs, f, b"F");
        }
        *race.lock().unwrap() = Some((vfs, rmdir));
        let r = racing_call(vfs, window).rename(None, f, to);
        assert_eq!(r, Err(FsError::NotFound), "{to:?}");
        assert_eq!(api.walk(None, d, true).unwrap_err(), FsError::NotFound);
        if dir {
            assert_eq!(api.stat_path(None, f, true).unwrap().nlink, 2);
            api.rmdir(None, f).unwrap();
        } else {
            assert_eq!(read_all(vfs, f), b"F");
            api.unlink(None, f).unwrap();
        }
    }
    assert_eq!(fs.with(|s| s.used()), ram_base);
    assert_eq!(kfs.tmp_nodes().unwrap().0, tmp_base);
}

/// `..` from the root of mounts stacked on one directory leads to that
/// directory's parent, not into a mount below, from the top of the stack
/// and from the lower mount once the top one goes.
#[test]
fn dotdot_climbs_stacked_mounts() {
    let mut v = ram();
    v.mkdir(None, "/m", 0o755).unwrap();
    v.mkdir(None, "/x", 0o755).unwrap();
    v.mount(None, "/m", ramfs()).unwrap();
    v.mkdir(None, "/m/low", 0o755).unwrap();
    v.mount(None, "/m", ramfs()).unwrap();
    v.mkdir(None, "/m/in", 0o755).unwrap();
    let x = v.stat(None, "/x").unwrap().ino;
    for p in ["/m/../x", "/m/in/../../x"] {
        assert_eq!(v.stat(None, p).unwrap().ino, x, "{p}");
    }
    assert_eq!(v.stat(None, "/m/low").unwrap_err(), FsError::NotFound);
    v.umount(None, "/m").unwrap();
    assert_eq!(v.stat(None, "/m/low/../../x").unwrap().ino, x);
    v.umount(None, "/m").unwrap();
    assert_dcache_sound(&v);
}

/// A scan that unlinks each entry `readdir` gives it meets every entry
/// once, on ramfs and on tmpfs: a removed entry moves no other entry
/// behind the scan's cookie.
#[test]
fn readdir_unlinking_each_entry_meets_all() {
    use crate::fs::kernfs::{KernFs, KernSkin, KernState};
    let vfs = locked_vfs();
    let api = FileApi::new(vfs);
    let kfs: &'static KernFs<std::sync::Mutex<KernState>> = std::boxed::Box::leak(
        std::boxed::Box::new(KernFs::new(std::sync::Mutex::new(KernState::new()))),
    );
    let tmp: &'static KernSkin<std::sync::Mutex<KernState>> =
        std::boxed::Box::leak(std::boxed::Box::new(KernSkin::new(kfs, FsType::Tmp)));
    api.mkdir(None, b"/tmp", 0o755).unwrap();
    api.mount_fs(None, b"/tmp", tmp, None, false, None).unwrap();
    for dir in ["/d", "/tmp/d"] {
        api.mkdir(None, dir.as_bytes(), 0o755).unwrap();
        for i in 0..10 {
            make(vfs, format!("{dir}/f{i}").as_bytes());
        }
        let flags = OpenFlags::from_bits(O_RDONLY | O_DIRECTORY);
        let f = api.open(None, dir.as_bytes(), flags, 0).unwrap();
        let mut seen = Vec::new();
        api.readdir(&f, &mut |d| {
            let n = d.name.as_bytes().to_vec();
            if n != b"." && n != b".." {
                let p = format!("{dir}/{}", core::str::from_utf8(&n).unwrap());
                api.unlink(None, p.as_bytes()).unwrap();
                seen.push(n);
            }
            true
        })
        .unwrap();
        api.close(f).unwrap();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 10, "{dir}: {seen:?}");
        api.rmdir(None, dir.as_bytes()).unwrap();
    }
}
