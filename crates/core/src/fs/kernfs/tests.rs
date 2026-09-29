use super::*;
use crate::fs::{O_CREAT, O_RDWR, SEEK_SET, Vfs};
use std::boxed::Box;
use std::sync::Mutex;

type Store = Mutex<KernState>;

/// A kernfs store of its own, which the test leaks, and its skins.
struct Kfs {
    fs: &'static KernFs<Store>,
    skins: [&'static KernSkin<Store>; 4],
}

fn kernfs() -> Kfs {
    let fs: &'static KernFs<Store> = Box::leak(Box::new(KernFs::new(Mutex::new(KernState::new()))));
    let skin = |ty| -> &'static KernSkin<Store> { Box::leak(Box::new(KernSkin::new(fs, ty))) };
    Kfs {
        fs,
        skins: [
            skin(FsType::Dev),
            skin(FsType::Proc),
            skin(FsType::Tmp),
            skin(FsType::Sys),
        ],
    }
}

/// Mount the four skins on `/dev`, `/proc`, `/tmp` and `/sys`, making
/// each mountpoint through the root's ops.
fn pseudo(v: &mut Vfs, k: &Kfs) {
    for (at, skin) in ["/dev", "/proc", "/tmp", "/sys"].into_iter().zip(k.skins) {
        match v.mkdir(None, at, 0o755) {
            Ok(_) | Err(FsError::Exists) => {}
            Err(e) => panic!("mkdir {at}: {e:?}"),
        }
        v.mount(None, at, skin).unwrap();
    }
}

fn boot() -> (Vfs, Kfs) {
    let mut v = Vfs::new();
    v.mount_root_fs(crate::fs::testfs::ramfs()).unwrap();
    let k = kernfs();
    pseudo(&mut v, &k);
    (v, k)
}

#[test]
fn mount_pseudo_on_keyed_root() {
    let mut v = Vfs::new();
    v.mount_root_fs(crate::fs::testfs::keyfs_new()).unwrap();
    pseudo(&mut v, &kernfs());
    assert_eq!(v.stat(None, "/dev").unwrap().kind, InodeKind::Dir);
    assert_eq!(v.stat(None, "/proc").unwrap().kind, InodeKind::Dir);
    assert_eq!(v.stat(None, "/tmp").unwrap().kind, InodeKind::Dir);
    assert_eq!(v.stat(None, "/sys").unwrap().kind, InodeKind::Dir);
    assert!(has_name(&mut v, "/", b"dev"));
    let fid = v.open_path(None, "/dev/null", O_RDWR, 0).unwrap();
    assert_eq!(v.write(&fid, b"x").unwrap(), 1);
    v.close(fid).unwrap();
}

fn readdir_names(v: &mut Vfs, path: &str, out: &mut [[u8; 16]; 32]) -> usize {
    let dir = v.resolve(None, path, true).unwrap();
    let mut d = Dirent {
        ino: 0,
        kind: InodeKind::Reg,
        name: Name::EMPTY,
    };
    let mut cookie = 0u64;
    let mut n = 0usize;
    loop {
        match v.readdir(dir, cookie, &mut d).unwrap() {
            None => break,
            Some(next) => {
                if n < 32 {
                    let b = d.name.as_bytes();
                    let k = b.len().min(16);
                    out[n][..k].copy_from_slice(&b[..k]);
                    out[n][k..].fill(0);
                }
                n += 1;
                cookie = next;
            }
        }
    }
    n
}

fn has_name(v: &mut Vfs, path: &str, want: &[u8]) -> bool {
    let mut names = [[0u8; 16]; 32];
    let n = readdir_names(v, path, &mut names);
    let mut i = 0usize;
    while i < n {
        let mut l = 0usize;
        while l < 16 && names[i][l] != 0 {
            l += 1;
        }
        if &names[i][..l] == want {
            return true;
        }
        i += 1;
    }
    false
}

#[test]
fn mounts_exist() {
    let (mut v, _k) = boot();
    assert_eq!(v.stat(None, "/dev").unwrap().kind, InodeKind::Dir);
    assert_eq!(v.stat(None, "/proc").unwrap().kind, InodeKind::Dir);
    assert_eq!(v.stat(None, "/tmp").unwrap().kind, InodeKind::Dir);
    assert_eq!(v.stat(None, "/sys").unwrap().kind, InodeKind::Dir);
}

#[test]
fn devfs_char_nodes() {
    let (mut v, _k) = boot();
    assert!(has_name(&mut v, "/dev", b"null"));
    assert!(has_name(&mut v, "/dev", b"zero"));
    assert!(has_name(&mut v, "/dev", b"random"));
    assert!(has_name(&mut v, "/dev", b"console"));
    assert!(has_name(&mut v, "/dev", b"tty"));
    assert_eq!(v.stat(None, "/dev/null").unwrap().kind, InodeKind::Chr);
    assert_eq!(v.stat(None, "/dev/zero").unwrap().kind, InodeKind::Chr);
    let fid = v.open_path(None, "/dev/null", O_RDWR, 0).unwrap();
    assert_eq!(v.write(&fid, b"drop").unwrap(), 4);
    let mut buf = [0xFFu8; 8];
    assert_eq!(v.read(&fid, &mut buf).unwrap(), 0);
    v.close(fid).unwrap();
    let z = v.open_path(None, "/dev/zero", O_RDWR, 0).unwrap();
    let mut buf = [0xFFu8; 8];
    assert_eq!(v.read(&z, &mut buf).unwrap(), 8);
    assert_eq!(buf, [0u8; 8]);
    v.close(z).unwrap();
}

#[test]
fn devfs_random_does_not_block() {
    let (mut v, _k) = boot();
    v.now = 0x1234_5678;
    let fid = v.open_path(None, "/dev/random", O_RDWR, 0).unwrap();
    let mut a = [0u8; 16];
    let mut b = [0u8; 16];
    assert_eq!(v.read(&fid, &mut a).unwrap(), 16);
    assert_eq!(v.read(&fid, &mut b).unwrap(), 16);
    assert_ne!(a, b);
    v.close(fid).unwrap();
    let u = v.open_path(None, "/dev/urandom", O_RDWR, 0).unwrap();
    assert_eq!(v.read(&u, &mut a).unwrap(), 16);
    v.close(u).unwrap();
}

#[test]
fn devfs_block_names() {
    let (mut v, k) = boot();
    k.fs.devfs_add_block(b"ram0", 256 * 512).unwrap();
    k.fs.devfs_add_block(b"vda", 1024 * 512).unwrap();
    k.fs.devfs_add_block(b"ram0p1", 32 * 512).unwrap();
    assert!(has_name(&mut v, "/dev", b"ram0"));
    assert!(has_name(&mut v, "/dev", b"vda"));
    assert!(has_name(&mut v, "/dev", b"ram0p1"));
    let s = v.stat(None, "/dev/ram0").unwrap();
    assert_eq!(s.kind, InodeKind::Blk);
    assert_eq!(s.size, 256 * 512);
    let fid = v.open_path(None, "/dev/ram0", O_RDWR, 0).unwrap();
    let mut buf = [0u8; 4];
    assert_eq!(v.read(&fid, &mut buf).unwrap_err(), FsError::NotSupp);
    v.close(fid).unwrap();
}

#[test]
fn tmpfs_uses_cache_and_evicts() {
    let (mut v, k) = boot();
    let fid = v
        .open_path(None, "/tmp/big", O_RDWR | O_CREAT, 0o644)
        .unwrap();
    let one = [0x5Au8; 1];
    let mut i = 0u64;
    while i < 6 {
        v.seek(&fid, (i * PAGE as u64) as i64, SEEK_SET).unwrap();
        assert_eq!(v.write(&fid, &one).unwrap(), 1);
        i += 1;
    }
    assert!(
        k.fs.tmp_cache_stats().evicts >= 1,
        "tmpfs must evict through the Phase 7 cache, not pin a Vec"
    );
    v.seek(&fid, 0, SEEK_SET).unwrap();
    let mut out = [0u8; 1];
    assert_eq!(v.read(&fid, &mut out).unwrap(), 1);
    assert_eq!(out[0], 0x5A);
    v.close(fid).unwrap();
    assert_eq!(v.stat(None, "/tmp/big").unwrap().size, 5 * PAGE as u64 + 1);
}

#[test]
fn tmpfs_mkdir_and_unlink() {
    let (mut v, _k) = boot();
    v.mkdir(None, "/tmp/a", 0o755).unwrap();
    v.creat(None, "/tmp/a/f", 0o644).unwrap();
    let fid = v.open_path(None, "/tmp/a/f", O_RDWR, 0).unwrap();
    assert_eq!(v.write(&fid, b"hi").unwrap(), 2);
    v.close(fid).unwrap();
    v.unlink(None, "/tmp/a/f").unwrap();
    assert_eq!(v.stat(None, "/tmp/a/f").unwrap_err(), FsError::NotFound);
}

#[test]
fn procfs_stubs_with_only_kernel_thread() {
    let (mut v, _k) = boot();
    assert!(has_name(&mut v, "/proc", b"1"));
    assert!(has_name(&mut v, "/proc", b"self"));
    let s = v.stat(None, "/proc/self").unwrap();
    assert_eq!(s.kind, InodeKind::Dir);
    assert!(has_name(&mut v, "/proc/1", b"cmdline"));
    assert!(has_name(&mut v, "/proc/1", b"status"));
    assert!(has_name(&mut v, "/proc/1", b"maps"));
    assert!(has_name(&mut v, "/proc/1", b"fd"));
    let fid = v.open_path(None, "/proc/1/cmdline", O_RDWR, 0).unwrap();
    let mut buf = [0u8; 16];
    let n = v.read(&fid, &mut buf).unwrap();
    assert!(n > 0);
    assert_eq!(&buf[..6], b"vibeos");
    v.close(fid).unwrap();
    let st = v.open_path(None, "/proc/1/status", O_RDWR, 0).unwrap();
    let n = v.read(&st, &mut buf).unwrap();
    assert!(n > 0);
    v.close(st).unwrap();
    assert_eq!(v.stat(None, "/proc/1/fd").unwrap().kind, InodeKind::Dir);
    assert_eq!(
        v.mkdir(None, "/proc/nope", 0o755).unwrap_err(),
        FsError::NotSupp
    );
}

#[test]
fn sysfs_device_tree() {
    let (mut v, k) = boot();
    k.fs.sysfs_add_device(b"00:01.0", 0x1af4, 0x1042, 0x01, Some(b"virtio-blk"))
        .unwrap();
    k.fs.sysfs_add_device(b"00:02.0", 0x8086, 0x100e, 0x02, None)
        .unwrap();
    assert!(has_name(&mut v, "/sys/devices", b"00:01.0"));
    let fid = v
        .open_path(None, "/sys/devices/00:01.0/vendor", O_RDWR, 0)
        .unwrap();
    let mut buf = [0u8; 16];
    let n = v.read(&fid, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"0x1af4\n");
    v.close(fid).unwrap();
    let d = v
        .open_path(None, "/sys/devices/00:01.0/driver", O_RDWR, 0)
        .unwrap();
    let n = v.read(&d, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"virtio-blk\n");
    v.close(d).unwrap();
    let unbound = v
        .open_path(None, "/sys/devices/00:02.0/driver", O_RDWR, 0)
        .unwrap();
    let n = v.read(&unbound, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"-\n");
    v.close(unbound).unwrap();
    assert!(has_name(&mut v, "/sys/bus/pci/drivers", b"virtio-blk"));
    let lnk = v.lstat(None, "/sys/bus/pci/devices/00:01.0").unwrap();
    assert_eq!(lnk.kind, InodeKind::Lnk);
}

#[test]
fn console_write_captured() {
    let (mut v, k) = boot();
    let fid = v.open_path(None, "/dev/console", O_RDWR, 0).unwrap();
    assert_eq!(v.write(&fid, b"hi").unwrap(), 2);
    let mut out = [0u8; 8];
    let n = k.fs.cons_captured(&mut out);
    assert_eq!(&out[..n], b"hi");
    v.close(fid).unwrap();
}

#[test]
fn fixed_tables_match_limits() {
    let k = std::boxed::Box::new(KernState::new());
    assert_eq!(k.nodes.len(), crate::limits::MAX_KERN_NODES);
}
