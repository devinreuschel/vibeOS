//! Host tests for directory references, the walk base, and the
//! namespace changes that keep a referenced dentry's name current
//! (ROADMAP §10.4).

use super::testfs::*;
use super::tests::{assert_dcache_sound, mount_dev, press, ram, st_ino_of};
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
    assert_eq!(v.rename(None, "/f", "/m").unwrap_err(), FsError::Busy);
    assert_eq!(v.rmdir(None, "/m").unwrap_err(), FsError::Busy);
    assert_eq!(v.unlink(None, "/m").unwrap_err(), FsError::Busy);
    v.umount(None, "/m").unwrap();
    v.rename(None, "/m", "/o").unwrap();
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
