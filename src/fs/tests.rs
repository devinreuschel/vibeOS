use super::*;

/// A ramfs over a store of its own, which the test leaks.
pub(super) fn ramfs() -> &'static RamFs<std::sync::Mutex<RamState>> {
    std::boxed::Box::leak(std::boxed::Box::new(RamFs::new(std::sync::Mutex::new(
        RamState::new(),
    ))))
}

fn ram() -> Vfs {
    let mut v = Vfs::new();
    v.mount_root_fs(ramfs()).unwrap();
    v
}

fn st_ino_of(v: &Vfs, p: PathRef) -> u32 {
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
    let up = v.stat(None, "/mnt/x/..").unwrap();
    assert_eq!(up.ino, mnt_after.ino);
    let root = v.stat(None, "/mnt/x/../..").unwrap();
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
    let mut v = Vfs::new();
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
    assert!(!v.file_table()[id.fid as usize].0, "the last close frees");
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
    let f = v.resolve(Some(a), "f", true).unwrap();
    assert_eq!(v.stat(Some(a), "f").unwrap().ino, st_ino_of(&v, f));
    let root = v.resolve(Some(a), "..", true).unwrap();
    assert_eq!(st_ino_of(&v, root), st_ino_of(&v, v.root().unwrap()));
}

#[test]
fn ram_rename_and_link() {
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
    let v = Vfs::new();
    assert_eq!(v.inodes.len(), limits::MAX_INODES);
    assert_eq!(v.dentries.len(), limits::MAX_DENTRIES);
    assert_eq!(v.supers.len(), limits::MAX_MOUNTS);
    assert_eq!(v.mounts.len(), limits::MAX_MOUNTS);
    assert_eq!(v.files.len(), limits::MAX_OPEN_FILES);
    assert_eq!(FdTable::new().fds.len(), limits::MAX_FDS);
}

/// Negative lookups of fresh names under `dir` until the dentry cache
/// has evicted `n` more dentries.
fn press(v: &mut Vfs, dir: &str, n: u32, seq: &mut u32) {
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
    while v.stats.d_evicts < 2 * MAX_DENTRIES as u32 {
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
fn assert_dcache_sound(v: &Vfs) {
    let mut i = 0usize;
    while i < MAX_DENTRIES {
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
fn mount_dev(
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
    let before = (v.dentries, v.inodes, v.mounts);
    assert_eq!(v.umount(None, "/m").unwrap_err(), FsError::Busy);
    assert_eq!((v.dentries, v.inodes, v.mounts), before);
    v.put_ref(held);
    assert_eq!(v.stat(None, "/m/f").unwrap().kind, InodeKind::Reg);
    v.creat(None, "/m/g", 0o644).unwrap();
    let g = v.resolve(None, "/m/g", true).unwrap();
    v.dget(g.dslot).unwrap();
    v.resolve(None, "/m", true).unwrap();
    let before = (v.dentries, v.inodes, v.mounts);
    assert_eq!(v.umount(None, "/m").unwrap_err(), FsError::Busy);
    assert_eq!((v.dentries, v.inodes, v.mounts), before);
    assert_eq!(v.stat(None, "/m/g").unwrap().kind, InodeKind::Reg);
    v.dput(g.dslot);
    assert_dcache_sound(&v);
    v.umount(None, "/m").unwrap();
    assert_dcache_sound(&v);
    assert_eq!(v.stat(None, "/m/f").unwrap_err(), FsError::NotFound);
    assert!(v.inodes.iter().all(|i| !i.used || i.sb == 0));
}

/// A test backend whose storage lives outside `Vfs`, in `KEYFS`, found
/// by the store id in its superblock's private word 0. Node `n` has
/// key `[n, 0, 0]`, `st_ino` `n + 100` and private words `[7 * n, w]`,
/// where `w` counts the writes made through the inode.
pub(super) struct KeyFs {
    id: u64,
}

struct KeyOps;

#[derive(Clone)]
struct KNode {
    kind: InodeKind,
    nlink: u32,
    data: Vec<u8>,
    alive: bool,
}

#[derive(Default)]
struct Store {
    nodes: Vec<KNode>,
    names: Vec<(u32, Vec<u8>, u32)>,
    evicts: u32,
    fills: u32,
    mounts: Vec<Vec<u8>>,
    umounts: Vec<(Vec<u8>, bool)>,
}

static KEYFS: std::sync::Mutex<Vec<Store>> = std::sync::Mutex::new(Vec::new());

pub(super) fn keyfs_new() -> &'static KeyFs {
    let mut g = KEYFS.lock().unwrap();
    g.push(Store::default());
    std::boxed::Box::leak(std::boxed::Box::new(KeyFs {
        id: (g.len() - 1) as u64,
    }))
}

fn with_store<R>(id: u64, f: impl FnOnce(&mut Store) -> R) -> R {
    f(&mut KEYFS.lock().unwrap()[id as usize])
}

fn knode_info(s: &Store, n: u32) -> InodeInfo {
    let k = &s.nodes[n as usize];
    InodeInfo {
        key: [n, 0, 0],
        ino: n + 100,
        kind: k.kind,
        mode: k.kind.ifmt() | 0o644,
        nlink: k.nlink,
        size: k.data.len() as u64,
        atime: 0,
        mtime: 0,
        ctime: 0,
        private: [7 * u64::from(n), 0],
    }
}

impl FileSystem for KeyFs {
    fn name(&self) -> &'static str {
        "keyfs"
    }
    fn fstype(&self) -> FsType {
        FsType::Ram
    }
    fn ops(&'static self) -> Option<&'static dyn InodeOps> {
        Some(&KeyOps)
    }
    fn fill_super(&self, cx: &mut OpCx<'_>) -> Result<InodeInfo, FsError> {
        *cx.private = [self.id, 0];
        with_store(self.id, |s| {
            s.fills += 1;
            if s.nodes.is_empty() {
                s.nodes.push(KNode {
                    kind: InodeKind::Dir,
                    nlink: 2,
                    data: Vec::new(),
                    alive: true,
                });
            }
            Ok(knode_info(s, 0))
        })
    }
    fn on_mount(&self, _cx: &mut OpCx<'_>, at: &[u8]) {
        with_store(self.id, |s| s.mounts.push(at.to_vec()));
    }
    fn on_umount(&self, _cx: &mut OpCx<'_>, at: &[u8], last: bool) {
        with_store(self.id, |s| s.umounts.push((at.to_vec(), last)));
    }
}

impl InodeOps for KeyOps {
    fn lookup(&self, cx: &mut OpCx<'_>, dir: &Inode, name: &[u8]) -> Result<InodeInfo, FsError> {
        with_store(cx.private[0], |s| {
            let n = s
                .names
                .iter()
                .find(|e| e.0 == dir.key[0] && e.1 == name)
                .ok_or(FsError::NotFound)?
                .2;
            Ok(knode_info(s, n))
        })
    }
    fn create(
        &self,
        cx: &mut OpCx<'_>,
        dir: &mut Inode,
        name: &[u8],
        kind: InodeKind,
        _mode: u16,
        _target: Option<&[u8]>,
    ) -> Result<InodeInfo, FsError> {
        with_store(cx.private[0], |s| {
            if s.names.iter().any(|e| e.0 == dir.key[0] && e.1 == name) {
                return Err(FsError::Exists);
            }
            s.nodes.push(KNode {
                kind,
                nlink: 1,
                data: Vec::new(),
                alive: true,
            });
            let n = (s.nodes.len() - 1) as u32;
            s.names.push((dir.key[0], name.to_vec(), n));
            Ok(knode_info(s, n))
        })
    }
    fn unlink(&self, cx: &mut OpCx<'_>, dir: &mut Inode, name: &[u8]) -> Result<(), FsError> {
        with_store(cx.private[0], |s| {
            let i = s
                .names
                .iter()
                .position(|e| e.0 == dir.key[0] && e.1 == name)
                .ok_or(FsError::NotFound)?;
            let n = s.names.remove(i).2 as usize;
            s.nodes[n].nlink -= 1;
            Ok(())
        })
    }
    fn read(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        with_store(cx.private[0], |s| {
            let d = &s.nodes[ino.key[0] as usize].data;
            let off = (off as usize).min(d.len());
            let n = buf.len().min(d.len() - off);
            buf[..n].copy_from_slice(&d[off..off + n]);
            Ok(n)
        })
    }
    fn write(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &[u8],
    ) -> Result<usize, FsError> {
        with_store(cx.private[0], |s| {
            let d = &mut s.nodes[ino.key[0] as usize].data;
            let end = off as usize + buf.len();
            if d.len() < end {
                d.resize(end, 0);
            }
            d[off as usize..end].copy_from_slice(buf);
            ino.size = ino.size.max(end as u64);
            ino.private[1] += 1;
            Ok(buf.len())
        })
    }
    fn truncate(&self, cx: &mut OpCx<'_>, ino: &mut Inode, size: u64) -> Result<(), FsError> {
        with_store(cx.private[0], |s| {
            s.nodes[ino.key[0] as usize].data.resize(size as usize, 0);
            ino.size = size;
            Ok(())
        })
    }
    fn readdir(
        &self,
        cx: &mut OpCx<'_>,
        dir: &Inode,
        cookie: u64,
        out: &mut Dirent,
    ) -> Result<Option<u64>, FsError> {
        with_store(cx.private[0], |s| {
            let mut kids = s.names.iter().filter(|e| e.0 == dir.key[0]);
            let Some(e) = kids.nth(cookie as usize) else {
                return Ok(None);
            };
            out.ino = e.2 + 100;
            out.kind = s.nodes[e.2 as usize].kind;
            out.name = Name::from_bytes(&e.1)?;
            Ok(Some(cookie + 1))
        })
    }
    fn getattr(&self, cx: &mut OpCx<'_>, ino: &mut Inode) -> Result<(), FsError> {
        with_store(cx.private[0], |s| {
            if let Some(k) = s.nodes.get(ino.key[0] as usize) {
                ino.nlink = k.nlink;
            }
            Ok(())
        })
    }
    fn evict(&self, cx: &mut OpCx<'_>, ino: &Inode) -> Result<(), FsError> {
        with_store(cx.private[0], |s| {
            if let Some(n) = s.nodes.get_mut(ino.key[0] as usize) {
                n.alive = false;
            }
            s.evicts += 1;
            Ok(())
        })
    }
}

/// A filesystem with no ops: its root and nothing else.
struct NoOpsFs;

impl FileSystem for NoOpsFs {
    fn name(&self) -> &'static str {
        "noops"
    }
    fn fstype(&self) -> FsType {
        FsType::Fat
    }
    fn ops(&'static self) -> Option<&'static dyn InodeOps> {
        None
    }
    fn fill_super(&self, cx: &mut OpCx<'_>) -> Result<InodeInfo, FsError> {
        Ok(InodeInfo {
            key: [0, 0, 0],
            ino: 1,
            kind: InodeKind::Dir,
            mode: S_IFDIR_MODE,
            nlink: 2,
            size: 0,
            atime: cx.now,
            mtime: cx.now,
            ctime: cx.now,
            private: [2, 0],
        })
    }
}

/// `ram()` with a fresh `KeyFs` on `/k`; its store id.
fn keyed() -> (Vfs, u64) {
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
    assert_eq!(v.stat(None, "/n/x").unwrap_err(), FsError::NotSupp);
    assert_eq!(v.creat(None, "/n/y", 0o644).unwrap_err(), FsError::NotSupp);
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
    let w = v.inode_words(h).unwrap();
    assert_eq!((w.key, w.private, w.size), ([42, 0, 0], [5, 6], 9));
    v.set_inode_words(h, [7, 8], 11).unwrap();
    assert_eq!(v.inode(h).unwrap().private, [7, 8]);
    assert_eq!(v.inode(h).unwrap().size, 11);
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
        v.stat(None, "/b/f/..").unwrap().ino,
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
    let before = v.mounts;
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
    assert_eq!(v.mounts, before, "no mount made");
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
        v.stat(None, "/b/d/x/../../..").unwrap().ino,
        v.stat(None, "/").unwrap().ino
    );
    assert_dcache_sound(&v);
}

/// A block-device filesystem over `KeyFs`'s store whose every hook and op
/// checks that the VFS lock, `vfs`, is not held: `try_lock` succeeds.
struct LockedFs {
    key: &'static KeyFs,
    vfs: &'static std::sync::Mutex<Vfs>,
    calls: std::sync::atomic::AtomicU32,
}

impl LockedFs {
    fn unlocked(&self) {
        assert!(
            self.vfs.try_lock().is_ok(),
            "backend called under the VFS lock"
        );
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl FileSystem for LockedFs {
    fn name(&self) -> &'static str {
        "locked"
    }
    fn fstype(&self) -> FsType {
        self.key.fstype()
    }
    fn ops(&'static self) -> Option<&'static dyn InodeOps> {
        Some(self)
    }
    fn fill_super(&self, cx: &mut OpCx<'_>) -> Result<InodeInfo, FsError> {
        self.unlocked();
        self.key.fill_super(cx)
    }
    fn on_mount(&self, cx: &mut OpCx<'_>, at: &[u8]) {
        self.unlocked();
        self.key.on_mount(cx, at);
    }
    fn on_umount(&self, cx: &mut OpCx<'_>, at: &[u8], last: bool) {
        self.unlocked();
        self.key.on_umount(cx, at, last);
    }
}

impl InodeOps for LockedFs {
    fn lookup(&self, cx: &mut OpCx<'_>, dir: &Inode, name: &[u8]) -> Result<InodeInfo, FsError> {
        self.unlocked();
        KeyOps.lookup(cx, dir, name)
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
        self.unlocked();
        KeyOps.create(cx, dir, name, kind, mode, target)
    }
    fn unlink(&self, cx: &mut OpCx<'_>, dir: &mut Inode, name: &[u8]) -> Result<(), FsError> {
        self.unlocked();
        KeyOps.unlink(cx, dir, name)
    }
    fn rmdir(&self, cx: &mut OpCx<'_>, dir: &mut Inode, name: &[u8]) -> Result<(), FsError> {
        self.unlocked();
        with_store(cx.private[0], |s| {
            let n = s
                .names
                .iter()
                .find(|e| e.0 == dir.key[0] && e.1 == name)
                .ok_or(FsError::NotFound)?
                .2;
            if s.names.iter().any(|e| e.0 == n) {
                return Err(FsError::NotEmpty);
            }
            Ok(())
        })?;
        KeyOps.unlink(cx, dir, name)
    }
    fn rename(
        &self,
        cx: &mut OpCx<'_>,
        odir: &mut Inode,
        oname: &[u8],
        ndir: &mut Inode,
        nname: &[u8],
    ) -> Result<Option<Key>, FsError> {
        self.unlocked();
        with_store(cx.private[0], |s| {
            if s.names.iter().any(|e| e.0 == ndir.key[0] && e.1 == nname) {
                return Err(FsError::Exists);
            }
            let e = s
                .names
                .iter_mut()
                .find(|e| e.0 == odir.key[0] && e.1 == oname)
                .ok_or(FsError::NotFound)?;
            *e = (ndir.key[0], nname.to_vec(), e.2);
            Ok(None)
        })
    }
    fn read(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        self.unlocked();
        KeyOps.read(cx, ino, off, buf)
    }
    fn write(
        &self,
        cx: &mut OpCx<'_>,
        ino: &mut Inode,
        off: u64,
        buf: &[u8],
    ) -> Result<usize, FsError> {
        self.unlocked();
        KeyOps.write(cx, ino, off, buf)
    }
    fn truncate(&self, cx: &mut OpCx<'_>, ino: &mut Inode, size: u64) -> Result<(), FsError> {
        self.unlocked();
        KeyOps.truncate(cx, ino, size)
    }
    fn readdir(
        &self,
        cx: &mut OpCx<'_>,
        dir: &Inode,
        cookie: u64,
        out: &mut Dirent,
    ) -> Result<Option<u64>, FsError> {
        self.unlocked();
        KeyOps.readdir(cx, dir, cookie, out)
    }
    fn getattr(&self, cx: &mut OpCx<'_>, ino: &mut Inode) -> Result<(), FsError> {
        self.unlocked();
        KeyOps.getattr(cx, ino)
    }
    fn sync(&self, _cx: &mut OpCx<'_>) -> Result<(), FsError> {
        self.unlocked();
        Ok(())
    }
    fn evict(&self, cx: &mut OpCx<'_>, ino: &Inode) -> Result<(), FsError> {
        self.unlocked();
        KeyOps.evict(cx, ino)
    }
    fn kill_sb(&self, _cx: &mut OpCx<'_>) {
        self.unlocked();
    }
}

/// A `Vfs` behind a `std::sync::Mutex`, as the kernel's is behind its
/// spinlock, with a ramfs root and `/blk`.
fn locked_vfs() -> &'static std::sync::Mutex<Vfs> {
    let vfs: &'static std::sync::Mutex<Vfs> =
        std::boxed::Box::leak(std::boxed::Box::new(std::sync::Mutex::new(Vfs::new())));
    let api = FileApi::new(vfs);
    api.mount_root(ramfs(), None, false).unwrap();
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
fn backend_ops_run_with_vfs_lock_dropped() {
    let vfs = locked_vfs();
    let fs: &'static LockedFs = std::boxed::Box::leak(std::boxed::Box::new(LockedFs {
        key: keyfs_new(),
        vfs,
        calls: std::sync::atomic::AtomicU32::new(0),
    }));
    let api = FileApi::new(vfs);
    let m = api.mount_fs(None, b"/blk", fs, Some(3), false).unwrap();
    assert!(!m.shared);
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
            open_race: Hooks::NONE.open_race,
        },
    );
    assert_eq!(hooked.write(&a, b"x").unwrap_err(), FsError::Badf);
    let b = FileRef::from_raw(STALE_B.lock().unwrap().take().unwrap());
    assert_eq!(b.id().fid, a.id().fid);
    assert_eq!(api.seek(&b, SeekFrom::Current(0)).unwrap(), 0);
    api.close(b).unwrap();
    assert!(vfs.lock().unwrap().file_table().iter().all(|f| !f.0));
}
