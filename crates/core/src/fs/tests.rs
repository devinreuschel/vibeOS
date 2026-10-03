use super::testfs::*;
use super::*;

pub(super) fn ram() -> Vfs {
    let mut v = crate::fs::host_vfs();
    v.mount_root_fs(ramfs()).unwrap();
    v
}

pub(super) fn st_ino_of(v: &Vfs, p: PathRef) -> u32 {
    let s = v.islot(p).unwrap();
    v.inodes[s as usize].ino
}

#[test]
fn root_stat_is_dir() {
    let mut v = ram();
    let s = v.stat(None, "/").unwrap();
    assert_eq!(s.kind, InodeKind::Dir);
    assert_eq!(s.nlink, 2);
    assert_eq!(s.ino, 1);
}

#[test]
fn walk_dot_and_dotdot() {
    let mut v = ram();
    v.mkdir(None, "/a", 0o755).unwrap();
    v.mkdir(None, "/a/b", 0o755).unwrap();
    let b = v.resolve(None, "/a/b", true).unwrap();
    let same = v.resolve(None, "/a/b/.", true).unwrap();
    assert_eq!(st_ino_of(&v, b), st_ino_of(&v, same));
    let a = v.resolve(None, "/a/b/..", true).unwrap();
    let a2 = v.resolve(None, "/a", true).unwrap();
    assert_eq!(st_ino_of(&v, a), st_ino_of(&v, a2));
    let root = v.resolve(None, "/a/b/../..", true).unwrap();
    assert_eq!(st_ino_of(&v, root), st_ino_of(&v, v.root().unwrap()));
    let stay = v.resolve(None, "/..", true).unwrap();
    assert_eq!(st_ino_of(&v, stay), st_ino_of(&v, v.root().unwrap()));
    let mixed = v.resolve(None, "/a/./b/../b", true).unwrap();
    assert_eq!(st_ino_of(&v, mixed), st_ino_of(&v, b));
}

#[test]
fn walk_nested_file() {
    let mut v = ram();
    v.mkdir(None, "/a", 0o755).unwrap();
    v.creat(None, "/a/f", 0o644).unwrap();
    let s = v.stat(None, "/a/f").unwrap();
    assert_eq!(s.kind, InodeKind::Reg);
    assert_eq!(s.nlink, 1);
}

#[test]
fn missing_is_not_found() {
    let mut v = ram();
    assert_eq!(v.stat(None, "/nope").unwrap_err(), FsError::NotFound);
    assert_eq!(v.stat(None, "/nope/x").unwrap_err(), FsError::NotFound);
}

#[test]
fn negative_dentry_invalidates_on_create() {
    let mut v = ram();
    assert_eq!(v.stat(None, "/foo").unwrap_err(), FsError::NotFound);
    v.creat(None, "/foo", 0o644).unwrap();
    let s = v.stat(None, "/foo").unwrap();
    assert_eq!(s.kind, InodeKind::Reg);
    assert_eq!(v.stat(None, "/bar").unwrap_err(), FsError::NotFound);
    v.creat(None, "/baz", 0o644).unwrap();
    assert_eq!(v.stat(None, "/bar").unwrap_err(), FsError::NotFound);
    let s = v.stat(None, "/baz").unwrap();
    assert_eq!(s.kind, InodeKind::Reg);
}

#[test]
fn symlink_follow_and_lstat() {
    let mut v = ram();
    v.mkdir(None, "/d", 0o755).unwrap();
    v.creat(None, "/d/f", 0o644).unwrap();
    v.symlink(None, "/l", "/d/f").unwrap();
    let followed = v.stat(None, "/l").unwrap();
    let file = v.stat(None, "/d/f").unwrap();
    assert_eq!(followed.ino, file.ino);
    assert_eq!(followed.kind, InodeKind::Reg);
    let link = v.lstat(None, "/l").unwrap();
    assert_eq!(link.kind, InodeKind::Lnk);
    assert_ne!(link.ino, file.ino);
}

#[test]
fn relative_symlink() {
    let mut v = ram();
    v.mkdir(None, "/d", 0o755).unwrap();
    v.creat(None, "/d/f", 0o644).unwrap();
    v.symlink(None, "/d/l", "f").unwrap();
    let s = v.stat(None, "/d/l").unwrap();
    let f = v.stat(None, "/d/f").unwrap();
    assert_eq!(s.ino, f.ino);
    v.symlink(None, "/d/up", "../d/f").unwrap();
    let s = v.stat(None, "/d/up").unwrap();
    assert_eq!(s.ino, f.ino);
}

#[test]
fn symlink_loop_is_error() {
    let mut v = ram();
    v.symlink(None, "/a", "/b").unwrap();
    v.symlink(None, "/b", "/a").unwrap();
    assert_eq!(v.stat(None, "/a").unwrap_err(), FsError::Loop);
    v.symlink(None, "/self", "/self").unwrap();
    assert_eq!(v.stat(None, "/self").unwrap_err(), FsError::Loop);
}

#[test]
fn symlink_depth_cap() {
    let mut v = ram();
    v.creat(None, "/end", 0o644).unwrap();
    v.symlink(None, "/s8", "/end").unwrap();
    v.symlink(None, "/s7", "/s8").unwrap();
    v.symlink(None, "/s6", "/s7").unwrap();
    v.symlink(None, "/s5", "/s6").unwrap();
    v.symlink(None, "/s4", "/s5").unwrap();
    v.symlink(None, "/s3", "/s4").unwrap();
    v.symlink(None, "/s2", "/s3").unwrap();
    v.symlink(None, "/s1", "/s2").unwrap();
    v.symlink(None, "/s0", "/s1").unwrap();
    // s0..s8 is 9 follows to /end; cap is 8.
    assert_eq!(v.stat(None, "/s0").unwrap_err(), FsError::Loop);
    assert_eq!(v.stat(None, "/s1").unwrap().kind, InodeKind::Reg);
}

#[test]
fn mount_crossing_dotdot() {
    let mut v = ram();
    v.mkdir(None, "/mnt", 0o755).unwrap();
    let mnt_before = v.stat(None, "/mnt").unwrap().ino;
    v.mount(None, "/mnt", ramfs()).unwrap();
    let mnt_after = v.stat(None, "/mnt").unwrap();
    assert_eq!(mnt_after.kind, InodeKind::Dir);
    assert_ne!(mnt_after.ino, mnt_before);
    v.creat(None, "/mnt/x", 0o644).unwrap();
    let x = v.stat(None, "/mnt/x").unwrap();
    assert_eq!(x.kind, InodeKind::Reg);
    // A file is no directory to step out of (path_resolution(7)).
    assert_eq!(v.stat(None, "/mnt/x/..").unwrap_err(), FsError::NotDir);
    let up = v.stat(None, "/mnt/.").unwrap();
    assert_eq!(up.ino, mnt_after.ino);
    let root = v.stat(None, "/mnt/./..").unwrap();
    assert_eq!(root.ino, v.stat(None, "/").unwrap().ino);
    let root2 = v.stat(None, "/mnt/..").unwrap();
    assert_eq!(root2.ino, root.ino);
    v.umount(None, "/mnt").unwrap();
    assert_eq!(v.stat(None, "/mnt/x").unwrap_err(), FsError::NotFound);
    assert_eq!(v.stat(None, "/mnt").unwrap().ino, mnt_before);
}

#[test]
fn unlinked_open_keeps_data_until_close() {
    let fs = ramfs();
    let mut v = crate::fs::host_vfs();
    v.mount_root_fs(fs).unwrap();
    let fid = v.open_path(None, "/f", O_RDWR | O_CREAT, 0o644).unwrap();
    assert_eq!(v.write(&fid, b"hello").unwrap(), 5);
    v.unlink(None, "/f").unwrap();
    assert_eq!(v.stat(None, "/f").unwrap_err(), FsError::NotFound);
    v.seek(&fid, 0, SEEK_SET).unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(v.read(&fid, &mut buf).unwrap(), 5);
    assert_eq!(&buf[..5], b"hello");
    let used = fs.with(|st| st.used());
    v.close(fid).unwrap();
    assert!(fs.with(|st| st.used()) < used);
    v.creat(None, "/f", 0o644).unwrap();
    let s = v.stat(None, "/f").unwrap();
    assert_eq!(s.size, 0);
}

#[test]
fn fd_dup_shares_offset() {
    let mut v = ram();
    let fid = v.open_path(None, "/f", O_RDWR | O_CREAT, 0o644).unwrap();
    v.write(&fid, b"abcd").unwrap();
    let id = fid.into_raw();
    v.api(|a| a.addref(id)).unwrap();
    let (fid, fid2) = (FileRef::from_raw(id), FileRef::from_raw(id));
    assert_eq!(v.seek(&fid, 0, SEEK_CUR).unwrap(), 4);
    v.seek(&fid2, 0, SEEK_SET).unwrap();
    assert_eq!(v.seek(&fid, 0, SEEK_CUR).unwrap(), 0, "one offset");
    v.close(fid).unwrap();
    let mut buf = [0u8; 4];
    assert_eq!(v.read(&fid2, &mut buf).unwrap(), 4);
    assert_eq!(&buf, b"abcd");
    v.close(fid2).unwrap();
    let mut t = [(false, 0, 0); SMALL.files];
    assert_eq!(v.file_table(&mut t), SMALL.files);
    assert!(!t[id.fid as usize].0, "the last close frees");
}

#[test]
fn read_write_truncate() {
    let mut v = ram();
    let fid = v.open_path(None, "/t", O_RDWR | O_CREAT, 0o644).unwrap();
    assert_eq!(v.write(&fid, b"xyz").unwrap(), 3);
    v.truncate(None, "/t", 1).unwrap();
    v.seek(&fid, 0, SEEK_SET).unwrap();
    let mut buf = [0u8; 4];
    assert_eq!(v.read(&fid, &mut buf).unwrap(), 1);
    assert_eq!(buf[0], b'x');
    assert_eq!(v.stat(None, "/t").unwrap().size, 1);
    v.close(fid).unwrap();
}

#[test]
fn readdir_dots_and_kids() {
    let mut v = ram();
    v.mkdir(None, "/a", 0o755).unwrap();
    v.creat(None, "/a/f", 0o644).unwrap();
    let dir = v.resolve(None, "/a", true).unwrap();
    let mut d = Dirent {
        ino: 0,
        kind: InodeKind::Reg,
        name: Name::EMPTY,
    };
    let c1 = v.readdir(dir, 0, &mut d).unwrap().unwrap();
    assert!(d.name.is_dot());
    let c2 = v.readdir(dir, c1, &mut d).unwrap().unwrap();
    assert!(d.name.is_dotdot());
    let c3 = v.readdir(dir, c2, &mut d).unwrap().unwrap();
    assert!(d.name.eq_bytes(b"f"));
    assert!(v.readdir(dir, c3, &mut d).unwrap().is_none());
}

#[test]
fn dentry_clock_eviction() {
    let mut v = ram();
    let mut i = 0u32;
    while v.stats.d_evicts == 0 && i < 200 {
        let mut path = [0u8; 5];
        path[0] = b'/';
        path[1] = b'n';
        path[2] = b'0' + ((i / 10) as u8 % 10);
        path[3] = b'0' + ((i % 10) as u8);
        let s = core::str::from_utf8(&path[..4]).unwrap();
        let _ = v.stat(None, s);
        i += 1;
    }
    assert!(v.stats.d_evicts >= 1);
    v.creat(None, "/real", 0o644).unwrap();
    assert_eq!(v.stat(None, "/real").unwrap().kind, InodeKind::Reg);
}

#[test]
fn inode_cache_evicts_idle() {
    let mut v = ram();
    v.mkdir(None, "/d", 0o755).unwrap();
    let mut i = 0u32;
    while i < 20 {
        let mut path = [0u8; 8];
        path[..3].copy_from_slice(b"/d/");
        path[3] = b'f';
        path[4] = b'0' + ((i / 10) as u8);
        path[5] = b'0' + ((i % 10) as u8);
        let s = core::str::from_utf8(&path[..6]).unwrap();
        v.creat(None, s, 0o644).unwrap();
        i += 1;
    }
    i = 0;
    while v.stats.d_evicts == 0 && i < 200 {
        let mut path = [0u8; 8];
        path[..3].copy_from_slice(b"/d/");
        path[3] = b'n';
        path[4] = b'0' + ((i / 10) as u8 % 10);
        path[5] = b'0' + ((i % 10) as u8);
        let s = core::str::from_utf8(&path[..6]).unwrap();
        let _ = v.stat(None, s);
        i += 1;
    }
    v.mkdir(None, "/z", 0o755).unwrap();
    let mut j = 0u32;
    while v.stats.i_evicts == 0 && j < 80 {
        let mut path = [0u8; 8];
        path[..3].copy_from_slice(b"/z/");
        path[3] = b'g';
        path[4] = b'0' + ((j % 10) as u8);
        path[5] = b'0' + (((j / 10) % 10) as u8);
        let s = core::str::from_utf8(&path[..6]).unwrap();
        let _ = v.creat(None, s, 0o644);
        j += 1;
    }
    assert!(v.stats.i_evicts >= 1);
    assert_eq!(v.stat(None, "/d/f00").unwrap().kind, InodeKind::Reg);
}

#[test]
fn cwd_relative_walk() {
    let mut v = ram();
    v.mkdir(None, "/a", 0o755).unwrap();
    v.creat(None, "/a/f", 0o644).unwrap();
    let a = v.resolve(None, "/a", true).unwrap();
    let base = Some(WalkBase {
        root: v.root().unwrap(),
        cwd: a,
    });
    let f = v.resolve(base, "f", true).unwrap();
    assert_eq!(v.stat(base, "f").unwrap().ino, st_ino_of(&v, f));
    let root = v.resolve(base, "..", true).unwrap();
    assert_eq!(st_ino_of(&v, root), st_ino_of(&v, v.root().unwrap()));
}

#[test]
fn ramfs_rename_and_link() {
    let mut v = ram();
    v.creat(None, "/a", 0o644).unwrap();
    v.mkdir(None, "/d", 0o755).unwrap();
    v.rename(None, "/a", "/d/b").unwrap();
    assert_eq!(v.stat(None, "/a").unwrap_err(), FsError::NotFound);
    assert_eq!(v.stat(None, "/d/b").unwrap().kind, InodeKind::Reg);
    v.link(None, "/d/b", "/c").unwrap();
    assert_eq!(v.stat(None, "/c").unwrap().nlink, 2);
    assert_eq!(v.stat(None, "/d/b").unwrap().nlink, 2);
}

#[test]
fn fixed_tables_match_limits() {
    use crate::limits;
    // The kernel's sizes are the limits, and a VFS built at them has
    // tables of those lengths (ROADMAP §10.4, D1).
    let k = VfsSizes::KERNEL;
    assert_eq!(k.inodes, limits::MAX_INODES);
    assert_eq!(k.dentries, limits::MAX_DENTRIES);
    assert_eq!(k.mounts, limits::MAX_MOUNTS);
    assert_eq!(k.files, limits::MAX_OPEN_FILES);
    let words = std::boxed::Box::leak(std::boxed::Box::new(words_table(k.inodes).unwrap()));
    let v = Vfs::new(&k, words).unwrap();
    assert_eq!(v.sizes(), k);
    assert_eq!(v.inodes.len(), limits::MAX_INODES);
    assert_eq!(v.dentries.len(), limits::MAX_DENTRIES);
    assert_eq!(v.supers.len(), limits::MAX_MOUNTS);
    assert_eq!(v.mounts.len(), limits::MAX_MOUNTS);
    assert_eq!(v.files.len(), limits::MAX_OPEN_FILES);
    assert_eq!(crate::fs::host_vfs().sizes(), crate::fs::SMALL);
    assert_eq!(
        FdTable::try_new(limits::MAX_FDS).unwrap().fds.len(),
        limits::MAX_FDS
    );
}

/// Negative lookups of fresh names under `dir` until the dentry cache
/// has evicted `n` more dentries.
pub(super) fn press(v: &mut Vfs, dir: &str, n: u32, seq: &mut u32) {
    let goal = v.stats.d_evicts.saturating_add(n);
    while v.stats.d_evicts < goal {
        let p = format!("{dir}/n{}", *seq);
        *seq += 1;
        assert_eq!(v.stat(None, &p).unwrap_err(), FsError::NotFound);
    }
}

#[test]
fn dcache_f065_evicted_parent_keeps_mount() {
    let mut v = ram();
    v.mkdir(None, "/a", 0o755).unwrap();
    v.mkdir(None, "/a/m", 0o755).unwrap();
    v.mount(None, "/a/m", ramfs()).unwrap();
    v.creat(None, "/a/m/marker", 0o644).unwrap();
    let marker = v.stat(None, "/a/m/marker").unwrap().ino;
    assert_dcache_sound(&v);
    let mut seq = 0u32;
    while v.stats.d_evicts < 2 * v.dentries.len() as u32 {
        press(&mut v, "", 8, &mut seq);
        assert_eq!(v.stat(None, "/a/m/marker").unwrap().ino, marker);
        assert_dcache_sound(&v);
    }
    v.umount(None, "/a/m").unwrap();
    assert_dcache_sound(&v);
    assert_eq!(v.stat(None, "/a/m/marker").unwrap_err(), FsError::NotFound);
}

#[test]
fn dcache_f065_reused_slot_never_aliases() {
    let mut v = ram();
    let mut seq = 0u32;
    let mut round = 0u32;
    while round < 24 {
        let a = format!("/a{round}");
        let ax = format!("/a{round}/x");
        let c = format!("/c{round}");
        let cx = format!("/c{round}/x");
        v.mkdir(None, &a, 0o755).unwrap();
        v.creat(None, &ax, 0o644).unwrap();
        let ino = v.stat(None, &ax).unwrap().ino;
        press(&mut v, "", round % 7 + 1, &mut seq);
        v.mkdir(None, &c, 0o755).unwrap();
        assert_eq!(v.stat(None, &cx).unwrap_err(), FsError::NotFound);
        assert_eq!(v.stat(None, &ax).unwrap().ino, ino);
        assert_dcache_sound(&v);
        v.unlink(None, &ax).unwrap();
        v.rmdir(None, &a).unwrap();
        v.rmdir(None, &c).unwrap();
        assert_dcache_sound(&v);
        round += 1;
    }
}

/// Every used dentry is held exactly by its children, the mounts on
/// it and, for a root, its superblock; a non-root dentry's parent is
/// used, positive and in the same superblock.
pub(super) fn assert_dcache_sound(v: &Vfs) {
    let mut i = 0usize;
    while i < v.dentries.len() {
        let d = &v.dentries[i];
        if d.used {
            assert_eq!(d.refs, v.expected_holds(i as u16), "dentry {i}'s holders");
            if !d.is_root(i as u16) {
                let p = &v.dentries[d.parent as usize];
                assert!(p.used && !p.negative && p.sb == d.sb, "dentry {i}'s parent");
            }
        }
        i += 1;
    }
}

/// Mount `fs` on `at` from block device `dev`.
pub(super) fn mount_dev(
    v: &mut Vfs,
    at: &str,
    fs: &'static dyn FileSystem,
    dev: u64,
    ro: bool,
) -> Result<Mounted, FsError> {
    v.mount_dev(at, fs, dev, ro)
}

#[test]
fn dcache_f065_two_mounts_one_dentry() {
    let mut v = ram();
    v.mkdir(None, "/p", 0o755).unwrap();
    v.mkdir(None, "/q", 0o755).unwrap();
    let fs = ramfs();
    mount_dev(&mut v, "/p", fs, 9, false).unwrap();
    assert!(mount_dev(&mut v, "/q", fs, 9, false).unwrap().shared);
    assert_dcache_sound(&v);
    v.creat(None, "/p/f", 0o644).unwrap();
    let pf = v.resolve(None, "/p/f", true).unwrap();
    let qf = v.resolve(None, "/q/f", true).unwrap();
    assert_ne!(pf.mount, qf.mount);
    assert_eq!(pf.dslot, qf.dslot, "one dentry for the name");
    v.creat(None, "/q/g", 0o644).unwrap();
    let pg = v.resolve(None, "/p/g", true).unwrap();
    let qg = v.resolve(None, "/q/g", true).unwrap();
    assert_eq!(pg.dslot, qg.dslot);
    let named = |v: &Vfs, n: &[u8]| {
        v.dentries
            .iter()
            .filter(|d| d.used && d.name.eq_bytes(n))
            .count()
    };
    assert_eq!(named(&v, b"f"), 1);
    assert_eq!(named(&v, b"g"), 1);
    assert_eq!(
        v.stat(None, "/p/g").unwrap().ino,
        v.stat(None, "/q/g").unwrap().ino
    );
    assert_dcache_sound(&v);
}

#[test]
fn dcache_f065_mount_pins_mountpoint_first() {
    let mut v = ram();
    v.mkdir(None, "/a", 0o755).unwrap();
    v.mkdir(None, "/a/m", 0o755).unwrap();
    let a_ino = v.stat(None, "/a").unwrap().ino;
    let mp = v.resolve(None, "/a/m", true).unwrap().dslot;
    // Fill every slot, clear every clock bit and aim the hand at the
    // mountpoint, so the mount's root dentry must evict and the
    // mountpoint is the first candidate.
    let mut seq = 0u32;
    while v.dentries.iter().any(|d| !d.used) {
        let _ = v.stat(None, &format!("/n{seq}"));
        seq += 1;
    }
    for d in v.dentries.iter_mut() {
        d.clock = false;
    }
    v.dhand = mp;
    let m = v.mount(None, "/a/m", ramfs()).unwrap();
    let mt = v.mounts[m as usize];
    assert_eq!(mt.mp_dslot, mp);
    assert_ne!(mt.mp_dslot, mt.root_dslot);
    assert_eq!(v.stat(None, "/a/m/..").unwrap().ino, a_ino);
    assert_dcache_sound(&v);
    v.umount(None, "/a/m").unwrap();
    assert_dcache_sound(&v);
}

#[test]
fn dcache_f065_umount_checks_before_state() {
    let mut v = ram();
    v.mkdir(None, "/m", 0o755).unwrap();
    v.mount(None, "/m", ramfs()).unwrap();
    v.creat(None, "/m/f", 0o644).unwrap();
    let f = v.resolve(None, "/m/f", true).unwrap();
    let held = v.iref(f).unwrap();
    v.resolve(None, "/m", true).unwrap();
    let before = (v.dentries.to_vec(), v.inodes.to_vec(), v.mounts.to_vec());
    assert_eq!(v.umount(None, "/m").unwrap_err(), FsError::Busy);
    assert_eq!(
        (v.dentries.to_vec(), v.inodes.to_vec(), v.mounts.to_vec()),
        before
    );
    v.put_ref(held);
    assert_eq!(v.stat(None, "/m/f").unwrap().kind, InodeKind::Reg);
    v.creat(None, "/m/g", 0o644).unwrap();
    let g = v.resolve(None, "/m/g", true).unwrap();
    v.dget(g.dslot).unwrap();
    v.resolve(None, "/m", true).unwrap();
    let before = (v.dentries.to_vec(), v.inodes.to_vec(), v.mounts.to_vec());
    assert_eq!(v.umount(None, "/m").unwrap_err(), FsError::Busy);
    assert_eq!(
        (v.dentries.to_vec(), v.inodes.to_vec(), v.mounts.to_vec()),
        before
    );
    assert_eq!(v.stat(None, "/m/g").unwrap().kind, InodeKind::Reg);
    v.dput(g.dslot);
    assert_dcache_sound(&v);
    v.umount(None, "/m").unwrap();
    assert_dcache_sound(&v);
    assert_eq!(v.stat(None, "/m/f").unwrap_err(), FsError::NotFound);
    assert!(v.inodes.iter().all(|i| !i.used || i.sb == 0));
}

/// `ram()` with a fresh `KeyFs` on `/k`; its store id.
pub(super) fn keyed() -> (Vfs, u64) {
    let mut v = ram();
    v.mkdir(None, "/k", 0o755).unwrap();
    let fs = keyfs_new();
    v.mount(None, "/k", fs).unwrap();
    (v, fs.id)
}

#[test]
fn ops_keyed_backend_via_super_ops() {
    let (mut v, id) = keyed();
    v.creat(None, "/k/a", 0o644).unwrap();
    let na = with_store(id, |s| {
        let n = s.names[0].2;
        s.names.push((0, b"b".to_vec(), n));
        s.nodes[n as usize].nlink += 1;
        n
    });
    let a = v.resolve(None, "/k/a", true).unwrap();
    let b = v.resolve(None, "/k/b", true).unwrap();
    assert_ne!(a.dslot, b.dslot);
    assert_eq!(
        v.islot(a).unwrap(),
        v.islot(b).unwrap(),
        "one inode per key"
    );
    assert_eq!(v.stat(None, "/k/b").unwrap().ino, na + 100);
    let fid = v.open_path(None, "/k/a", O_RDWR, 0).unwrap();
    assert_eq!(v.write(&fid, b"hello").unwrap(), 5);
    assert_eq!(v.write(&fid, b"!").unwrap(), 1);
    assert_eq!(v.stat(None, "/k/b").unwrap().size, 6);
    let r = v.iref(b).unwrap();
    let n = *v.inode(r.handle()).unwrap();
    assert_eq!(n.key, [na, 0, 0]);
    assert_eq!(
        n.private,
        [7 * u64::from(na), 2],
        "private words round-trip"
    );
    v.put_ref(r);
    v.seek(&fid, 0, SEEK_SET).unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(v.read(&fid, &mut buf).unwrap(), 6);
    assert_eq!(&buf[..6], b"hello!");
    v.unlink(None, "/k/a").unwrap();
    v.unlink(None, "/k/b").unwrap();
    assert_eq!(v.stat(None, "/k/b").unwrap_err(), FsError::NotFound);
    assert_eq!(
        with_store(id, |s| s.evicts),
        0,
        "an open file keeps its storage"
    );
    v.close(fid).unwrap();
    assert_eq!(
        with_store(id, |s| s.evicts),
        1,
        "evicted once at the last put"
    );
    assert!(!with_store(id, |s| s.nodes[na as usize].alive));
    v.umount(None, "/k").unwrap();
    assert_eq!(with_store(id, |s| s.evicts), 1);
    v.mkdir(None, "/n", 0o755).unwrap();
    v.mount(None, "/n", &NoOpsFs).unwrap();
    // A superblock without ops runs `NoOps`: each operation is missing,
    // with Linux's errno for it.
    assert_eq!(v.stat(None, "/n/x").unwrap_err(), FsError::NotDir);
    assert_eq!(v.creat(None, "/n/y", 0o644).unwrap_err(), FsError::Acces);
    assert_eq!(v.stat(None, "/n").unwrap().kind, InodeKind::Dir);
    assert_dcache_sound(&v);
}

#[test]
fn ops_inode_ref_api() {
    let (mut v, id) = keyed();
    let k = v.resolve(None, "/k", true).unwrap();
    let sb = v.sb_of_mount(k.mount).unwrap();
    assert_eq!(v.sb_private(sb).unwrap(), [id, 0]);
    let info = InodeInfo {
        key: [42, 0, 0],
        ino: 4242,
        kind: InodeKind::Reg,
        mode: S_IFREG_MODE,
        nlink: 1,
        size: 9,
        atime: 0,
        mtime: 0,
        ctime: 0,
        private: [5, 6],
    };
    let r1 = v.iget_key(sb, &info).unwrap();
    let r2 = v.iget_key(sb, &InodeInfo { size: 1, ..info }).unwrap();
    let h = r1.handle();
    assert_eq!(h, r2.handle(), "one inode per key");
    assert_eq!(v.inode(h).unwrap().size, 9, "the cached inode wins");
    let w = v.inode(h).unwrap().words().unwrap();
    assert_eq!((w.private(), w.size(), w.nlink()), ([5, 6], 9, 1));
    w.set_private([7, 8]);
    w.set_size(11);
    assert_eq!(v.inode(h).unwrap().words().unwrap().private(), [7, 8]);
    assert_eq!(v.inode(h).unwrap().stat().size, 11);
    assert_eq!(v.inodes_with_key(sb, [42, 0, 0]), 1);
    v.rekey(sb, [42, 0, 0], [43, 0, 0]).unwrap();
    assert_eq!(v.inode(h).unwrap().key, [43, 0, 0]);
    assert_eq!(v.inodes_with_key(sb, [42, 0, 0]), 0);
    assert_eq!(v.inodes_with_key(sb, [43, 0, 0]), 1);
    v.inodes[h.slot as usize].nlink = 0;
    let other = v
        .iget_key(
            sb,
            &InodeInfo {
                key: [43, 0, 0],
                ..info
            },
        )
        .unwrap();
    assert_ne!(other.handle(), h, "an unlinked inode is out of the hash");
    v.put_ref(other);
    v.put_ref(r2);
    v.put_ref(r1);
    assert!(
        v.inode(h).is_ok(),
        "its slot stays until its release returns"
    );
    assert_eq!(
        with_store(id, |s| s.evicts),
        0,
        "put_ref never calls the backend"
    );
    assert_eq!(v.stat(None, "/k").unwrap().kind, InodeKind::Dir);
    assert_eq!(
        with_store(id, |s| s.evicts),
        1,
        "the driver runs the release with the lock dropped"
    );
    assert_eq!(v.inode(h).unwrap_err(), FsError::Badf);
    let again = v.iget_key(sb, &info).unwrap();
    if again.handle() != h {
        assert_eq!(v.inode(h).unwrap_err(), FsError::Badf, "a stale generation");
    }
    v.put_ref(again);
    assert_eq!(v.stat(None, "/k/none").unwrap_err(), FsError::NotFound);
    assert!(
        v.dentries
            .iter()
            .any(|d| d.used && d.sb == sb && d.negative)
    );
    v.drop_negatives(sb);
    assert!(
        !v.dentries
            .iter()
            .any(|d| d.used && d.sb == sb && d.negative)
    );
    assert_dcache_sound(&v);
}

/// `ram()` with directories `/a` and `/b` and a `KeyFs` on block
/// device 7 mounted on `/a`.
fn dev_on_a() -> (Vfs, &'static KeyFs) {
    let mut v = ram();
    v.mkdir(None, "/a", 0o755).unwrap();
    v.mkdir(None, "/b", 0o755).unwrap();
    let fs = keyfs_new();
    let m = mount_dev(&mut v, "/a", fs, 7, false).unwrap();
    assert!(!m.shared);
    (v, fs)
}

#[test]
fn second_mount_of_device_shares_super() {
    let (mut v, fs) = dev_on_a();
    let a = v.resolve(None, "/a", true).unwrap();
    let m = mount_dev(&mut v, "/b", keyfs_new(), 7, false).unwrap();
    assert!(m.shared, "a mounted device's superblock is shared");
    assert_eq!(m.sb, v.sb_of_mount(a.mount).unwrap());
    assert_ne!(m.mount, a.mount, "two mounts");
    assert_eq!(with_store(fs.id, |s| s.fills), 1, "filled once");
    v.creat(None, "/a/f", 0o644).unwrap();
    assert_eq!(
        v.stat(None, "/b/f").unwrap().ino,
        v.stat(None, "/a/f").unwrap().ino
    );
    assert_eq!(
        v.stat(None, "/b/.").unwrap().ino,
        v.stat(None, "/a").unwrap().ino
    );
    let mut again = ram();
    again.mkdir(None, "/c", 0o755).unwrap();
    let other = mount_dev(&mut again, "/c", keyfs_new(), 8, false).unwrap();
    assert!(!other.shared, "another device gets its own superblock");
    assert_dcache_sound(&v);
}

#[test]
fn ro_mismatch_on_mounted_device_is_busy() {
    let (mut v, _) = dev_on_a();
    let before = v.mounts.to_vec();
    assert_eq!(
        mount_dev(&mut v, "/b", keyfs_new(), 7, true).unwrap_err(),
        FsError::Busy,
        "the other read-only flag"
    );
    assert_eq!(
        mount_dev(&mut v, "/b", &NoOpsFs, 7, false).unwrap_err(),
        FsError::Busy,
        "another filesystem type"
    );
    assert_eq!(v.mounts.to_vec(), before, "no mount made");
    let b = v.resolve(None, "/b", true).unwrap();
    assert_eq!(b.mount, 0, "/b stays uncovered");
    assert!(
        mount_dev(&mut v, "/b", keyfs_new(), 7, false)
            .unwrap()
            .shared
    );
    assert_dcache_sound(&v);
}

#[test]
fn super_released_after_last_mount() {
    let (mut v, fs) = dev_on_a();
    mount_dev(&mut v, "/b", fs, 7, false).unwrap();
    v.creat(None, "/a/f", 0o644).unwrap();
    let sb = v.super_of_dev(7).unwrap();
    v.umount(None, "/a").unwrap();
    assert_eq!(v.super_of_dev(7), Some(sb), "a mount still holds it");
    assert_eq!(v.stat(None, "/b/f").unwrap().kind, InodeKind::Reg);
    assert_eq!(v.stat(None, "/a/f").unwrap_err(), FsError::NotFound);
    assert_eq!(
        with_store(fs.id, |s| s.umounts.clone()),
        vec![(b"/a".to_vec(), false)]
    );
    v.umount(None, "/b").unwrap();
    assert_eq!(v.super_of_dev(7), None, "the last mount releases it");
    assert!(v.inodes.iter().all(|i| !i.used || i.sb != sb));
    assert!(v.dentries.iter().all(|d| !d.used || d.sb != sb));
    assert_eq!(
        with_store(fs.id, |s| s.umounts.clone()),
        vec![(b"/a".to_vec(), false), (b"/b".to_vec(), true)]
    );
    assert_eq!(
        with_store(fs.id, |s| s.mounts.clone()),
        vec![b"/a".to_vec(), b"/b".to_vec()]
    );
    let m = mount_dev(&mut v, "/a", fs, 7, false).unwrap();
    assert!(!m.shared, "a released device mounts afresh");
    assert_eq!(with_store(fs.id, |s| s.fills), 2);
    assert_dcache_sound(&v);
}

#[test]
fn two_mounts_one_dentry_per_name() {
    let (mut v, _) = dev_on_a();
    mount_dev(&mut v, "/b", keyfs_new(), 7, false).unwrap();
    v.mkdir(None, "/a/d", 0o755).unwrap();
    v.creat(None, "/b/d/x", 0o644).unwrap();
    let ax = v.resolve(None, "/a/d/x", true).unwrap();
    let bx = v.resolve(None, "/b/d/x", true).unwrap();
    assert_ne!(ax.mount, bx.mount);
    assert_eq!(ax.dslot, bx.dslot, "a dentry belongs to its superblock");
    let named = |v: &Vfs, n: &[u8]| {
        v.dentries
            .iter()
            .filter(|d| d.used && d.name.eq_bytes(n))
            .count()
    };
    assert_eq!(named(&v, b"x"), 1);
    assert_eq!(named(&v, b"d"), 1);
    assert_dcache_sound(&v);
    assert_eq!(v.umount(None, "/b/d").unwrap_err(), FsError::Inval);
    v.umount(None, "/a").unwrap();
    assert_eq!(v.resolve(None, "/b/d/x", true).unwrap().dslot, bx.dslot);
    assert_eq!(
        v.stat(None, "/b/d/../..").unwrap().ino,
        v.stat(None, "/").unwrap().ino
    );
    assert_dcache_sound(&v);
}

/// A `Vfs` behind a `std::sync::Mutex`, as the kernel's is behind its
/// sleeping lock, with a ramfs root and `/blk`.
pub(super) fn locked_vfs() -> &'static std::sync::Mutex<Vfs> {
    let vfs: &'static std::sync::Mutex<Vfs> = std::boxed::Box::leak(std::boxed::Box::new(
        std::sync::Mutex::new(crate::fs::host_vfs()),
    ));
    let api = FileApi::new(vfs);
    api.mount_root(ramfs(), None, false, None).unwrap();
    api.mkdir(None, b"/blk", 0o755).unwrap();
    vfs
}

fn names_of<L: Guarded<Vfs>>(api: &FileApi<'_, L>, dir: &[u8]) -> Vec<Vec<u8>> {
    let f = api
        .open(None, dir, OpenFlags::from_bits(O_RDONLY | O_DIRECTORY), 0)
        .unwrap();
    let mut out = Vec::new();
    api.readdir(&f, &mut |d| {
        out.push(d.name.as_bytes().to_vec());
        true
    })
    .unwrap();
    api.close(f).unwrap();
    out
}

#[test]
fn readdir_from_resumes_at_a_cookie() {
    let vfs = locked_vfs();
    let api = FileApi::new(vfs);
    for n in [b"/a".as_slice(), b"/b", b"/c"] {
        api.mkdir(None, n, 0o755).unwrap();
    }
    let f = api
        .open(None, b"/", OpenFlags::from_bits(O_RDONLY | O_DIRECTORY), 0)
        .unwrap();
    // Every entry with the cookie after it, from 0.
    let mut all = Vec::new();
    let end = api
        .readdir_from(&f, 0, &mut |d, next| {
            all.push((d.name.as_bytes().to_vec(), next));
            true
        })
        .unwrap();
    let names: Vec<&[u8]> = all.iter().map(|(n, _)| n.as_slice()).collect();
    assert_eq!(&names[..2], &[b".".as_slice(), b".."]);
    assert_eq!(all.len(), 2 + 4, "`.`, `..`, blk, a, b, c");
    assert_eq!(
        end,
        all.last().unwrap().1,
        "the end cookie is the last next"
    );
    // Refusing an entry returns its own cookie, so a resume starts there.
    let mut seen = 0;
    let at = api
        .readdir_from(&f, 0, &mut |_, _| {
            seen += 1;
            seen <= 3
        })
        .unwrap();
    assert_eq!(at, all[2].1, "the fourth entry's cookie");
    let mut rest = Vec::new();
    let end2 = api
        .readdir_from(&f, at, &mut |d, _| {
            rest.push(d.name.as_bytes().to_vec());
            true
        })
        .unwrap();
    let want: Vec<Vec<u8>> = all[3..].iter().map(|(n, _)| n.clone()).collect();
    assert_eq!(rest, want);
    assert_eq!(end2, end);
    // Past the end: nothing, and the same cookie back.
    let mut none = 0;
    assert_eq!(
        api.readdir_from(&f, end, &mut |_, _| {
            none += 1;
            true
        })
        .unwrap(),
        end
    );
    assert_eq!(none, 0);
    api.close(f).unwrap();
}

#[test]
fn backend_ops_run_with_vfs_lock_dropped() {
    let vfs = locked_vfs();
    let fs: &'static LockedFs = std::boxed::Box::leak(std::boxed::Box::new(LockedFs {
        key: keyfs_new(),
        vfs,
        calls: std::sync::atomic::AtomicU32::new(0),
    }));
    let api = FileApi::new(vfs);
    let vol = crate::dev::instance(7u32).unwrap();
    let m = api
        .mount_fs(None, b"/blk", fs, Some(3), false, Some(vol.clone()))
        .unwrap();
    assert!(!m.shared);
    // The superblock holds the volume; `/` shows none.
    let p = api.walk(None, b"/blk", true).unwrap();
    let shown = vfs.lock().unwrap().volume_of(p).unwrap();
    api.put_path(p);
    assert!(crate::dev::same_instance(&shown, &vol));
    assert_eq!(shown.downcast_ref::<u32>(), Some(&7));
    assert!(vfs.lock().unwrap().shows_volume(&vol));
    let root = vfs.lock().unwrap().root().unwrap();
    assert_eq!(
        vfs.lock().unwrap().volume_of(root).unwrap_err(),
        FsError::Inval
    );
    drop(shown);
    let rw = OpenFlags::from_bits(O_RDWR | O_CREAT);
    let f = api.open(None, b"/blk/a", rw, 0o644).unwrap();
    assert_eq!(api.write(&f, b"hello").unwrap(), 5);
    assert_eq!(api.seek(&f, SeekFrom::Start(0)).unwrap(), 0);
    let mut buf = [0u8; 8];
    assert_eq!(api.read(&f, &mut buf).unwrap(), 5);
    assert_eq!(&buf[..5], b"hello");
    assert_eq!(api.stat(&f).unwrap().size, 5);
    api.close(f).unwrap();
    api.mkdir(None, b"/blk/d", 0o755).unwrap();
    api.create(None, b"/blk/d/x", InodeKind::Reg, S_IFREG_MODE, None)
        .unwrap();
    assert_eq!(
        names_of(&api, b"/blk"),
        vec![b".".to_vec(), b"..".to_vec(), b"a".to_vec(), b"d".to_vec()]
    );
    api.rename(None, b"/blk/a", b"/blk/d/b").unwrap();
    assert_eq!(
        api.stat_path(None, b"/blk/a", true).unwrap_err(),
        FsError::NotFound
    );
    assert_eq!(api.stat_path(None, b"/blk/d/b", true).unwrap().size, 5);
    api.truncate(None, b"/blk/d/b", 2).unwrap();
    assert_eq!(api.stat_path(None, b"/blk/d/b", true).unwrap().size, 2);
    assert_eq!(api.rmdir(None, b"/blk/d").unwrap_err(), FsError::NotEmpty);
    api.unlink(None, b"/blk/d/b").unwrap();
    api.unlink(None, b"/blk/d/x").unwrap();
    api.rmdir(None, b"/blk/d").unwrap();
    assert_eq!(with_store(fs.key.id, |s| s.evicts), 3, "released unlocked");
    api.sync().unwrap();
    api.umount(None, b"/blk").unwrap();
    assert_eq!(
        with_store(fs.key.id, |s| s.umounts.clone()),
        vec![(b"/blk".to_vec(), true)]
    );
    // The released superblock dropped its count on the volume.
    assert!(!vfs.lock().unwrap().shows_volume(&vol));
    assert!(fs.calls.load(std::sync::atomic::Ordering::Relaxed) >= 20);
    assert_dcache_sound(&vfs.lock().unwrap());
}

static STALE_VFS: std::sync::Mutex<Option<&'static std::sync::Mutex<Vfs>>> =
    std::sync::Mutex::new(None);
static STALE_ID: std::sync::Mutex<Option<FileId>> = std::sync::Mutex::new(None);
static STALE_B: std::sync::Mutex<Option<FileId>> = std::sync::Mutex::new(None);

/// Between a write's backend call and its commit: close the writer's
/// file and open another into its slot, as another thread would.
fn stale_window() {
    let Some(id) = STALE_ID.lock().unwrap().take() else {
        return;
    };
    let vfs = STALE_VFS.lock().unwrap().unwrap();
    let api = FileApi::new(vfs);
    api.close(FileRef::from_raw(id)).unwrap();
    let b = api
        .open(None, b"/t", OpenFlags::from_bits(O_RDWR), 0)
        .unwrap();
    *STALE_B.lock().unwrap() = Some(b.into_raw());
}

/// `open` on Linux's terms (open(2), POSIX `open`): `O_NOFOLLOW` on a
/// symlink is `ELOOP`, or `ENOTDIR` with `O_DIRECTORY`; `O_CREAT` on a
/// directory is `EISDIR`; `O_CREAT | O_DIRECTORY` is `EINVAL` and creates
/// nothing.
#[test]
fn open_links_and_dirs_as_linux() {
    let vfs = locked_vfs();
    let api = FileApi::new(vfs);
    let open = |p: &[u8], fl: u32| {
        api.open(None, p, OpenFlags::from_bits(fl), 0o644)
            .map(|f| api.close(f).unwrap())
    };
    assert_eq!(open(b"/f", O_RDWR | O_CREAT), Ok(()));
    api.symlink(None, b"/l", b"/f").unwrap();
    assert_eq!(open(b"/l", O_RDONLY | O_NOFOLLOW), Err(FsError::Loop));
    assert_eq!(open(b"/l", O_RDWR | O_NOFOLLOW), Err(FsError::Loop));
    assert_eq!(
        open(b"/l", O_RDONLY | O_NOFOLLOW | O_DIRECTORY),
        Err(FsError::NotDir)
    );
    assert_eq!(open(b"/l", O_RDONLY), Ok(()));
    assert_eq!(open(b"/blk", O_RDONLY | O_CREAT), Err(FsError::IsDir));
    assert_eq!(open(b"/blk", O_RDONLY | O_DIRECTORY), Ok(()));
    assert_eq!(
        open(b"/new", O_RDONLY | O_CREAT | O_DIRECTORY),
        Err(FsError::Inval)
    );
    assert_eq!(
        api.stat_path(None, b"/new", false).unwrap_err(),
        FsError::NotFound
    );
}

static APPEND_VFS: std::sync::Mutex<Option<&'static std::sync::Mutex<Vfs>>> =
    std::sync::Mutex::new(None);
static APPEND_PATH: std::sync::Mutex<Option<&'static [u8]>> = std::sync::Mutex::new(None);

/// Between the first appender's backend write and its commit: a second
/// `O_APPEND` writer appends `BBBB`, as another thread would.
fn append_window() {
    let Some(path) = APPEND_PATH.lock().unwrap().take() else {
        return;
    };
    let api = FileApi::new(APPEND_VFS.lock().unwrap().unwrap());
    let f = api
        .open(None, path, OpenFlags::from_bits(O_WRONLY | O_APPEND), 0)
        .unwrap();
    assert_eq!(api.write(&f, b"BBBB").unwrap(), 4);
    api.close(f).unwrap();
}

/// `O_APPEND` on ramfs and tmpfs: a second appender that writes in the
/// first's window, after its backend write and before its commit, lands
/// after it, and the file's size is both writes', whichever commit
/// merges last (open(2): the seek to the end and the write are one
/// atomic step).
#[test]
fn append_lands_after_an_overlapping_append() {
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
    *APPEND_VFS.lock().unwrap() = Some(vfs);
    let hooked = FileApi::with_hooks(
        vfs,
        Hooks {
            write_window: append_window,
            ..Hooks::NONE
        },
    );
    for path in [b"/app" as &'static [u8], b"/tmp/app"] {
        let flags = OpenFlags::from_bits(O_WRONLY | O_CREAT | O_APPEND);
        let f = api.open(None, path, flags, 0o644).unwrap();
        *APPEND_PATH.lock().unwrap() = Some(path);
        assert_eq!(hooked.write(&f, b"AAAA").unwrap(), 4);
        api.close(f).unwrap();
        let r = api
            .open(None, path, OpenFlags::from_bits(O_RDONLY), 0)
            .unwrap();
        let mut buf = [0u8; 16];
        let n = api.read(&r, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"AAAABBBB", "{:?}", path);
        assert_eq!(api.stat(&r).unwrap().size, 8, "{:?}", path);
        assert_eq!(api.seek(&r, SeekFrom::End(0)).unwrap(), 8, "{:?}", path);
        api.close(r).unwrap();
    }
}

#[test]
fn file_ref_generation_rejects_stale_id() {
    let vfs = locked_vfs();
    let api = FileApi::new(vfs);
    let rw = OpenFlags::from_bits(O_RDWR | O_CREAT);
    let a = api.open(None, b"/s", rw, 0o644).unwrap();
    let stale = a.id();
    api.close(a).unwrap();
    let b = api.open(None, b"/t", rw, 0o644).unwrap();
    assert_eq!(b.id().fid, stale.fid, "the slot is reused");
    assert_ne!(b.id().r#gen, stale.r#gen, "under a new generation");
    let a = FileRef::from_raw(stale);
    let mut buf = [0u8; 4];
    assert_eq!(api.write(&a, b"x").unwrap_err(), FsError::Badf);
    assert_eq!(api.read(&a, &mut buf).unwrap_err(), FsError::Badf);
    assert_eq!(api.seek(&a, SeekFrom::Start(5)).unwrap_err(), FsError::Badf);
    assert_eq!(api.stat(&a).unwrap_err(), FsError::Badf);
    assert_eq!(api.addref(stale).unwrap_err(), FsError::Badf);
    assert_eq!(api.fget(stale).unwrap_err(), FsError::Badf);
    assert_eq!(api.close(a).unwrap_err(), FsError::Badf);
    assert_eq!(
        api.seek(&b, SeekFrom::Current(0)).unwrap(),
        0,
        "b untouched"
    );
    api.close(b).unwrap();
    // A write whose file is closed, and its slot reused, between its
    // backend call and its commit fails with Badf and leaves the other
    // file's offset alone.
    *STALE_VFS.lock().unwrap() = Some(vfs);
    let a = api.open(None, b"/s", rw, 0o644).unwrap();
    *STALE_ID.lock().unwrap() = Some(a.id());
    let hooked = FileApi::with_hooks(
        vfs,
        Hooks {
            write_window: stale_window,
            ..Hooks::NONE
        },
    );
    assert_eq!(hooked.write(&a, b"x").unwrap_err(), FsError::Badf);
    let b = FileRef::from_raw(STALE_B.lock().unwrap().take().unwrap());
    assert_eq!(b.id().fid, a.id().fid);
    assert_eq!(api.seek(&b, SeekFrom::Current(0)).unwrap(), 0);
    api.close(b).unwrap();
    let mut t = [(false, 0, 0); SMALL.files];
    vfs.lock().unwrap().file_table(&mut t);
    assert!(t.iter().all(|f| !f.0));
}

/// FAT's and vibefs's errors are `FsError` itself (E2, F083): each binding
/// compiles only while the aliases name the one type, and both convert to
/// `KError` through its one `From`.
#[test]
fn disk_errors_are_fs_error() {
    let f: FsError = crate::fs::fat::FatError::Corrupt;
    let v: FsError = crate::fs::vibefs::Error::Corrupt;
    assert_eq!(f, v);
    assert_eq!(f.as_str(), "corrupt");
    assert_eq!(
        crate::kerror::KError::from(f),
        crate::kerror::KError::from(FsError::Corrupt)
    );
    assert_eq!(
        crate::kerror::KError::from(v).errno(),
        crate::kerror::KError::from(FsError::Corrupt).errno()
    );
}

/// One test per `FsError` variant: its errno through the E2 table is
/// Linux's for the condition (ROADMAP §10.4, E2, F083), and it names
/// itself.
mod fs_error_errno_per_variant {
    use crate::fs::FsError;
    use crate::kerror::KError;

    macro_rules! per_variant {
        ($($name:ident: $v:ident => $errno:literal,)*) => {
            $(
                #[test]
                fn $name() {
                    assert_eq!(KError::from(FsError::$v).errno(), $errno);
                    assert!(!FsError::$v.as_str().is_empty());
                }
            )*

            /// Each variant and Linux's errno for it, for
            /// [`super::fs_error_errno_per_variant`].
            pub(super) const ROWS: &[(FsError, i32)] = &[$((FsError::$v, $errno)),*];

            /// Every variant has its test above: a new variant fails to
            /// compile here until it gets one.
            #[test]
            fn every_variant_listed() {
                for e in [$(FsError::$v),*] {
                    match e {
                        $(FsError::$v => {})*
                    }
                }
            }
        };
    }

    per_variant! {
        not_found: NotFound => 2,
        exists: Exists => 17,
        not_dir: NotDir => 20,
        is_dir: IsDir => 21,
        inval: Inval => 22,
        no_space: NoSpace => 28,
        loop_: Loop => 40,
        name_too_long: NameTooLong => 36,
        not_empty: NotEmpty => 39,
        busy: Busy => 16,
        badf: Badf => 9,
        not_supp: NotSupp => 95,
        io: Io => 5,
        file_too_big: FileTooBig => 27,
        no_mem: NoMem => 12,
        again: Again => 11,
        corrupt: Corrupt => 5,
        nfile: NFile => 23,
        perm: Perm => 1,
        spipe: SPipe => 29,
        xdev: XDev => 18,
        stale: Stale => 2,
        acces: Acces => 13,
    }
}

/// The whole table at once: every `FsError` variant's errno is Linux's
/// (the per-variant tests above name each one).
#[test]
fn fs_error_errno_per_variant() {
    use crate::kerror::KError;
    for &(e, want) in fs_error_errno_per_variant::ROWS {
        assert_eq!(KError::from(e).errno(), want, "{e:?}");
    }
}

/// Each E2 condition, driven through a real backend, returns Linux's errno
/// (ROADMAP §9.3, §10.4, F052, F057, F083).
#[test]
fn fs_error_conditions_errno() {
    use crate::fs::{fat, vibefs};
    use crate::kerror::KError;
    let errno = |e: FsError| KError::from(e).errno();

    // A small FAT volume, written until a write fails: ENOSPC.
    let mut img = vec![0u8; 64 * 1024];
    fat::mkfs(&mut img, b"FULL").unwrap();
    {
        let mut d = fat::MemDisk::new(&mut img, fat::SEC as u32).unwrap();
        let mut v = fat::FatVol::mount(&mut d).unwrap();
        let node = v.create(&mut d, v.info.root_clus, b"fill", false).unwrap();
        let mut n = fat::FatInode::of_node(&node);
        let chunk = [0x5au8; 4096];
        let mut off = 0u64;
        let e = loop {
            match v.write_ino(&mut d, &mut n, true, off, false, &chunk) {
                Ok((k, _)) => off += k as u64,
                Err(e) => break e,
            }
            assert!(off <= 64 * 1024, "the volume never filled");
        };
        assert_eq!(errno(e), 28, "{e:?}");
        // A write that starts at FAT's 4 GiB file limit: EFBIG (one that
        // crosses it is cut there, `fat::limit_tests`).
        let e = v
            .write_ino(&mut d, &mut n, true, fat::MAX_FILE_SIZE, false, b"xy")
            .unwrap_err();
        assert_eq!(errno(e), 27, "{e:?}");
    }

    // The same on vibefs: ENOSPC.
    let mut img = vec![0u8; 256 * 1024];
    {
        let mut d = vibefs::MemDisk::new(&mut img).unwrap();
        let mut v = vibefs::Vol::new();
        vibefs::mkfs(&mut d, b"full", &mut v).unwrap();
        v.create(
            &mut d,
            vibefs::ROOT_INO,
            b"fill",
            InodeKind::Reg,
            0o644,
            None,
        )
        .unwrap();
        let ino = v.lookup(&mut d, vibefs::ROOT_INO, b"fill").unwrap().ino;
        let chunk = [0xa5u8; 4096];
        let mut off = 0u64;
        let e = loop {
            match v.write(&mut d, ino, off, &chunk) {
                Ok(k) => off += k as u64,
                Err(e) => break e,
            }
            assert!(off <= 256 * 1024, "the volume never filled");
        };
        assert_eq!(errno(e), 28, "{e:?}");
    }

    // `open` on a host `Vfs` whose open-file table is full: ENFILE.
    let mut v = ram();
    let mut open = Vec::new();
    let e = loop {
        match v.open_path(None, "/f", O_RDWR | O_CREAT, 0o644) {
            Ok(f) => open.push(f),
            Err(e) => break e,
        }
        assert!(open.len() <= crate::fs::MAX_FILES, "the table never filled");
    };
    assert_eq!(errno(e), 23, "{e:?}");
    for f in open {
        v.close(f).unwrap();
    }

    // A vibefs read across an extent whose CRC fails: EIO.
    let mut img = vec![0u8; 256 * 1024];
    {
        let mut d = vibefs::MemDisk::new(&mut img).unwrap();
        let mut v = vibefs::Vol::new();
        vibefs::mkfs(&mut d, b"crc", &mut v).unwrap();
        v.create(&mut d, vibefs::ROOT_INO, b"x", InodeKind::Reg, 0o644, None)
            .unwrap();
        let ino = v.lookup(&mut d, vibefs::ROOT_INO, b"x").unwrap().ino;
        v.write(&mut d, ino, 0, &[7u8; 200]).unwrap();
        v.sync(&mut d).unwrap();
    }
    let (block, meta, sup) = (vibefs::BLOCK, vibefs::MAGIC_META, vibefs::MAGIC_SUPER);
    let blk = (4..img.len() / block)
        .find(|b| {
            let m = u32::from_le_bytes(img[b * block..b * block + 4].try_into().unwrap());
            m != meta && m != sup
        })
        .unwrap();
    img[blk * block] ^= 0xff;
    {
        let mut d = vibefs::MemDisk::new(&mut img).unwrap();
        let mut v = vibefs::Vol::new();
        vibefs::mount(&mut d, &mut v).unwrap();
        let ino = v.lookup(&mut d, vibefs::ROOT_INO, b"x").unwrap().ino;
        let mut out = [0u8; 200];
        let e = v.read(&mut d, ino, 0, &mut out).unwrap_err();
        assert_eq!(errno(e), 5, "{e:?}");
    }
}

/// An operation a filesystem or an object does not support returns the
/// errno Linux returns for that operation (ROADMAP §10.4, A3, E2, F083):
/// each `InodeOps` default, on `NoOps`; FAT's `symlink` and `link`; a
/// `rename` and a `link` across mounts; a write of an object that cannot be
/// written; `readlink` of a regular file; and `lseek` of `/dev/tty`.
#[test]
fn inode_ops_unsupported_errno() {
    use crate::kerror::KError;
    let errno = |e: FsError| KError::from(e).errno();

    // Each default, on a filesystem that overrides none.
    let ops: &dyn InodeOps = &NoOps;
    let mut private = [0u64; 2];
    let mut cx = OpCx {
        sb: 0,
        fstype: FsType::Ram,
        private: &mut private,
        now: 0,
        vol: None,
    };
    let (mut a, mut b) = (Inode::EMPTY, Inode::EMPTY);
    let mut buf = [0u8; 4];
    let mut de = Dirent::EMPTY;
    assert_eq!(errno(ops.lookup(&mut cx, &a, b"x").unwrap_err()), 20);
    for (kind, want) in [
        (InodeKind::Reg, 13),
        (InodeKind::Dir, 1),
        (InodeKind::Lnk, 1),
        (InodeKind::Chr, 1),
        (InodeKind::Blk, 1),
    ] {
        let e = ops
            .create(&mut cx, &mut a, b"x", kind, 0o644, Some(b"t"))
            .unwrap_err();
        assert_eq!(errno(e), want, "{kind:?}");
    }
    assert_eq!(errno(ops.unlink(&mut cx, &mut a, b"x").unwrap_err()), 1);
    assert_eq!(errno(ops.rmdir(&mut cx, &mut a, b"x").unwrap_err()), 1);
    assert_eq!(
        errno(ops.link(&mut cx, &mut a, b"x", &mut b).unwrap_err()),
        1
    );
    let seen = RenameSeen {
        src: [1, 0, 0],
        tgt: None,
    };
    let e = ops
        .rename(&mut cx, &mut a, b"x", &mut b, b"y", seen)
        .unwrap_err();
    assert_eq!(errno(e), 1);
    assert_eq!(
        errno(ops.read(&mut cx, &mut a, 0, &mut buf).unwrap_err()),
        22
    );
    assert_eq!(errno(ops.write(&mut cx, &mut a, 0, b"x").unwrap_err()), 22);
    assert_eq!(
        errno(ops.write_append(&mut cx, &mut a, b"x").unwrap_err()),
        22
    );
    assert_eq!(errno(ops.truncate(&mut cx, &mut a, 0).unwrap_err()), 22);
    assert_eq!(errno(ops.readdir(&mut cx, &a, 0, &mut de).unwrap_err()), 20);
    assert_eq!(errno(ops.readlink(&mut cx, &a, &mut buf).unwrap_err()), 22);
    assert_eq!(ops.getattr(&mut cx, &mut a), Ok(()));
    assert_eq!(ops.sync(&mut cx), Ok(()));
    assert_eq!(ops.evict(&mut cx, &a), Ok(()));
    assert_eq!(ops.check_seek(&mut cx, &a), Ok(()));
    ops.release(&mut cx);

    // FAT's `symlink` and `link`, through the core adapter: `EPERM`, as on
    // Linux's vfat.
    let mut img = vec![0u8; 64 * 1024];
    crate::fs::fat::mkfs(&mut img, b"PERM").unwrap();
    {
        let mut d = crate::fs::fat::MemDisk::new(&mut img, crate::fs::fat::SEC as u32).unwrap();
        let mut fv = crate::fs::fat::FatVol::mount(&mut d).unwrap();
        fv.create(&mut d, fv.info.root_clus, b"a", false).unwrap();
        fv.sync(&mut d).unwrap();
    }
    let mut v = ram();
    v.mkdir(None, "/fat", 0o755).unwrap();
    v.mount(None, "/fat", crate::fs::fat::tests::FatHostOps::new(img))
        .unwrap();
    assert_eq!(errno(v.symlink(None, "/fat/s", "/x").unwrap_err()), 1);
    assert_eq!(errno(v.link(None, "/fat/a", "/fat/b").unwrap_err()), 1);

    // Across mounts, ramfs to tmpfs and back: `EXDEV`.
    let (mut v, _k) = crate::fs::kernfs::tests::boot();
    v.creat(None, "/r", 0o644).unwrap();
    v.creat(None, "/tmp/t", 0o644).unwrap();
    assert_eq!(errno(v.rename(None, "/r", "/tmp/r2").unwrap_err()), 18);
    assert_eq!(errno(v.link(None, "/r", "/tmp/r3").unwrap_err()), 18);
    assert_eq!(errno(v.rename(None, "/tmp/t", "/t2").unwrap_err()), 18);

    // A write of an object that cannot be written: `EINVAL`.
    let f = v.open_path(None, "/proc/1/cmdline", O_RDWR, 0).unwrap();
    assert_eq!(errno(v.write(&f, b"x").unwrap_err()), 22);
    v.close(f).unwrap();

    // `readlink` of a regular file, on ramfs and on tmpfs: `EINVAL`.
    for path in ["/r", "/tmp/t"] {
        let p = v.resolve(None, path, true).unwrap();
        let mut c = v.call(v.islot(p).unwrap()).unwrap();
        let r = c.run(|o, cx, n| o.readlink(cx, n, &mut buf));
        v.finish(c, false);
        assert_eq!(errno(r.unwrap_err()), 22, "{path}");
    }

    // `lseek` of `/dev/tty`: `ESPIPE`; `/dev/null` seeks.
    let f = v.open_path(None, "/dev/tty", O_RDWR, 0).unwrap();
    assert_eq!(errno(v.seek(&f, 0, SEEK_CUR).unwrap_err()), 29);
    v.close(f).unwrap();
    let f = v.open_path(None, "/dev/null", O_RDWR, 0).unwrap();
    assert_eq!(v.seek(&f, 0, SEEK_CUR), Ok(0));
    v.close(f).unwrap();
}

/// More names than inode slots, each cached by a positive dentry that
/// nothing holds: a lookup that needs an inode evicts dentries until one
/// releases its inode, rather than failing `NoSpace`.
#[test]
fn stat_more_names_than_inode_slots() {
    let mut v = ram();
    v.mkdir(None, "/p", 0o755).unwrap();
    v.mkdir(None, "/q", 0o755).unwrap();
    // ramfs holds MAX_DIR_ENTS names a directory, so split them.
    let n = SMALL.inodes + 12;
    let name = |i: usize| format!("/{}/f{i}", if i.is_multiple_of(2) { "p" } else { "q" });
    for i in 0..n {
        v.creat(None, &name(i), 0o644).unwrap();
    }
    for _ in 0..2 {
        for i in 0..n {
            let s = v.stat(None, &name(i)).unwrap();
            assert_eq!(s.kind, InodeKind::Reg, "{}", name(i));
        }
    }
    assert!(v.stats.i_evicts >= (n - SMALL.inodes) as u32);
}

/// A ramfs create or link into a directory a racing rmdir removed, which
/// the VFS still holds, fails with `NotFound`, as Linux refuses a dead
/// directory, and makes nothing: a node made there would be unreachable,
/// and never freed.
#[test]
fn ramfs_make_in_a_removed_directory_is_not_found() {
    let fs = ramfs();
    let mut v = crate::fs::host_vfs();
    v.mount_root_fs(fs).unwrap();
    v.mkdir(None, "/d", 0o755).unwrap();
    v.creat(None, "/f", 0o644).unwrap();
    let used = fs.with(|s| s.used());
    let r = make_in_removed_dir(&mut v, fs, "/d", "/f");
    assert_eq!(r, [Err(FsError::NotFound); 3]);
    // The removed directory went at its last put; nothing else changed.
    v.stat(None, "/").unwrap();
    assert_eq!(fs.with(|s| s.used()), used - 1);
    assert_eq!(v.stat(None, "/f").unwrap().nlink, 1);
}
