use super::*;
use crate::block::blockdev::testing as blk_testing;
use crate::block::blockdev::{DiskSeq, Registry};
use crate::fs::{O_CREAT, O_RDWR, SEEK_SET, Vfs};
use std::boxed::Box;
use std::sync::Mutex;

type Store = Mutex<KernState>;

/// A kernfs store of its own, which the test leaks, and its skins.
pub(crate) struct Kfs {
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

pub(crate) fn boot() -> (Vfs, Kfs) {
    let mut v = crate::fs::host_vfs();
    v.mount_root_fs(crate::fs::testfs::ramfs()).unwrap();
    let k = kernfs();
    pseudo(&mut v, &k);
    (v, k)
}

#[test]
fn mount_pseudo_on_keyed_root() {
    let mut v = crate::fs::host_vfs();
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

/// A hardware source that has 5 bytes, from `RDRAND`.
fn five_bytes(buf: &mut [u8]) -> (usize, Option<crate::entropy::Source>) {
    let n = buf.len().min(5);
    for (i, b) in buf.iter_mut().take(n).enumerate() {
        *b = 0xE0 + i as u8;
    }
    (n, Some(crate::entropy::Source::RdRand))
}

/// Hardware sources with no byte.
fn no_bytes(_buf: &mut [u8]) -> (usize, Option<crate::entropy::Source>) {
    (0, None)
}

/// ROADMAP §10.12 (F134): `/dev/random` and `/dev/urandom` return only what
/// the hardware hook supplies: a short count, or `Again` for none.
#[test]
fn devfs_random_hardware_only() {
    use crate::entropy::{Source, last_source, test_hook};
    let (mut v, _k) = boot();
    for path in ["/dev/random", "/dev/urandom"] {
        let fid = v.open_path(None, path, O_RDWR, 0).unwrap();
        {
            let _g = test_hook(Some(five_bytes));
            let mut buf = [0u8; 64];
            assert_eq!(v.read(&fid, &mut buf), Ok(5), "{path}");
            assert_eq!(buf[..5], [0xE0, 0xE1, 0xE2, 0xE3, 0xE4], "{path}");
            assert!(buf[5..].iter().all(|&b| b == 0), "{path}");
            assert_eq!(last_source(), Some(Source::RdRand), "{path}");
            assert_eq!(v.read(&fid, &mut []), Ok(0), "{path}");
        }
        {
            let _g = test_hook(Some(no_bytes));
            let mut buf = [0u8; 64];
            assert_eq!(v.read(&fid, &mut buf), Err(FsError::Again), "{path}");
            assert_eq!(last_source(), None, "{path}");
            assert_eq!(v.read(&fid, &mut []), Ok(0), "{path}");
        }
        {
            // No hook at all, as on the host and before `entropy_init`.
            let _g = test_hook(None);
            let mut buf = [0u8; 8];
            assert_eq!(v.read(&fid, &mut buf), Err(FsError::Again), "{path}");
        }
        v.close(fid).unwrap();
    }
    // The `KError` table maps `Again` to Linux's EAGAIN.
    assert_eq!(FsError::Again.as_str(), "again");
    assert_eq!(crate::kerror::KError::from(FsError::Again).errno(), 11);
}

/// A registry of the fake disk `fake` (64 sectors) and its partition
/// `fakep1` (sectors 8 to 23), and handles to both.
fn fake_disk() -> (Registry, BlockRef, BlockRef) {
    let seq = DiskSeq::new();
    let mut reg = Registry::new();
    let d = blk_testing::disk(&mut reg, &seq, b"fake", 64);
    let p = blk_testing::part(&mut reg, &seq, &d, b"fakep1", 8, 16);
    (reg, d, p)
}

#[test]
fn devfs_block_names() {
    let (mut v, k) = boot();
    let seq = DiskSeq::new();
    let mut reg = Registry::new();
    let ram0 = blk_testing::disk(&mut reg, &seq, b"ram0", 256);
    let vda = blk_testing::disk(&mut reg, &seq, b"vda", 1024);
    let p1 = blk_testing::part(&mut reg, &seq, &ram0, b"ram0p1", 80, 32);
    for d in [&ram0, &vda, &p1] {
        k.fs.devfs_add_block(d).unwrap();
    }
    assert!(has_name(&mut v, "/dev", b"ram0"));
    assert!(has_name(&mut v, "/dev", b"vda"));
    assert!(has_name(&mut v, "/dev", b"ram0p1"));
    let s = v.stat(None, "/dev/ram0").unwrap();
    assert_eq!(s.kind, InodeKind::Blk);
    assert_eq!(s.size, 256 * 512);
    assert_eq!(v.stat(None, "/dev/ram0p1").unwrap().size, 32 * 512);
    // A second add of the same device keeps its one node.
    let a = k.fs.devfs_add_block(&vda).unwrap();
    assert_eq!(k.fs.devfs_add_block(&vda).unwrap(), a);
}

#[test]
fn devfs_block_rw_through_blockref() {
    let (mut v, k) = boot();
    let (mut reg, d, p) = fake_disk();
    k.fs.devfs_add_block(&d).unwrap();
    k.fs.devfs_add_block(&p).unwrap();
    assert_eq!(v.stat(None, "/dev/fakep1").unwrap().size, 16 * 512);
    let mut disk = [0u8; 512];
    d.read(8, &mut disk).unwrap();
    let fid = v.open_path(None, "/dev/fakep1", O_RDWR, 0).unwrap();
    let mut first = [0u8; 512];
    assert_eq!(v.read(&fid, &mut first).unwrap(), 512);
    assert_eq!(first, disk);
    v.seek(&fid, 3, SEEK_SET).unwrap();
    let mut five = [0u8; 5];
    assert_eq!(v.read(&fid, &mut five).unwrap(), 5);
    assert_eq!(five, disk[3..8]);
    // A read across a block boundary and one crossing the end.
    let mut mid = [0u8; 700];
    v.seek(&fid, 300, SEEK_SET).unwrap();
    assert_eq!(v.read(&fid, &mut mid).unwrap(), 700);
    let mut two = [0u8; 1024];
    d.read(8, &mut two).unwrap();
    assert_eq!(mid[..], two[300..1000]);
    v.seek(&fid, 16 * 512 - 10, SEEK_SET).unwrap();
    assert_eq!(v.read(&fid, &mut mid).unwrap(), 10);
    v.seek(&fid, 16 * 512, SEEK_SET).unwrap();
    assert_eq!(v.read(&fid, &mut five).unwrap(), 0);
    assert_eq!(v.write(&fid, b"x").unwrap_err(), FsError::NoSpace);
    // Writes land in the disk at the partition's start.
    v.seek(&fid, 0, SEEK_SET).unwrap();
    assert_eq!(v.write(&fid, &[0xA5u8; 512]).unwrap(), 512);
    d.read(8, &mut disk).unwrap();
    assert_eq!(disk, [0xA5u8; 512]);
    v.seek(&fid, 510, SEEK_SET).unwrap();
    assert_eq!(v.write(&fid, b"\x55\xAA\x01\x02").unwrap(), 4);
    d.read(8, &mut two).unwrap();
    // The read-modify-write kept the bytes around the write.
    assert_eq!(two[508..510], [0xA5, 0xA5]);
    assert_eq!(two[510..514], [0x55, 0xAA, 0x01, 0x02]);
    assert_eq!(usize::from(two[514]), (9 * 512 + 2) % 251);
    // After unpublish and kill, I/O through the node is EIO.
    blk_testing::remove(&mut reg, &p);
    v.seek(&fid, 0, SEEK_SET).unwrap();
    assert_eq!(v.read(&fid, &mut five).unwrap_err(), FsError::Io);
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
        FsError::Perm
    );
    assert_eq!(
        v.creat(None, "/proc/nope", 0o644).unwrap_err(),
        FsError::Acces
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
    assert_eq!(k.skins.len(), crate::limits::MAX_KERN_MOUNTS);
}

/// Whether `path` names a regular file.
fn is_reg(v: &mut Vfs, path: &str) -> bool {
    v.stat(None, path).is_ok_and(|s| s.kind == InodeKind::Reg)
}

/// Nodes in use and the table's length and capacity.
fn node_counts(k: &Kfs) -> (usize, usize, usize) {
    k.fs.with(|s| {
        let used = s.nodes.iter().filter(|n| n.used).count();
        assert_eq!(used + s.free_len, s.nodes.len());
        (used, s.nodes.len(), s.nodes.capacity())
    })
}

#[test]
fn node_table_starts_empty_and_grows_past_the_old_pool() {
    let k = std::boxed::Box::new(KernState::new());
    assert_eq!(k.nodes.capacity(), 0);
    let (mut v, k) = boot();
    let (boot_used, _, _) = node_counts(&k);
    // Under Miri, past the first capacity only; else well past the 128
    // nodes the table held as a fixed pool.
    let n = if cfg!(miri) { NODES_FIRST } else { 3 * 128 };
    for i in 0..n {
        v.creat(None, &format!("/tmp/f{i}"), 0o644).unwrap();
    }
    let (used, len, cap) = node_counts(&k);
    assert_eq!(used, boot_used + n);
    assert!(len > NODES_FIRST && cap >= len);
    assert!(cfg!(miri) || len > 128);
    // Every name is still linked after the moves, and the other skins'
    // nodes made before them still work.
    for i in 0..n {
        assert!(is_reg(&mut v, &format!("/tmp/f{i}")));
    }
    let fid = v.open_path(None, "/dev/zero", O_RDWR, 0).unwrap();
    let mut b = [1u8; 4];
    assert_eq!(v.read(&fid, &mut b).unwrap(), 4);
    assert_eq!(b, [0; 4]);
    v.close(fid).unwrap();
    assert!(has_name(&mut v, "/proc/1", b"cmdline"));
    assert_eq!(v.stat(None, "/sys/bus/pci").unwrap().kind, InodeKind::Dir);
}

#[test]
fn node_table_reuses_freed_nodes() {
    let (mut v, k) = boot();
    let (made, again, rounds) = if cfg!(miri) {
        (80, 60, 2)
    } else {
        (200, 150, 20)
    };
    for i in 0..made {
        v.creat(None, &format!("/tmp/f{i}"), 0o644).unwrap();
    }
    for i in 0..made {
        v.unlink(None, &format!("/tmp/f{i}")).unwrap();
    }
    let (used, len, cap) = node_counts(&k);
    for round in 0..rounds {
        for i in 0..again {
            v.creat(None, &format!("/tmp/r{round}_{i}"), 0o644).unwrap();
        }
        for i in 0..again {
            v.unlink(None, &format!("/tmp/r{round}_{i}")).unwrap();
        }
        assert_eq!(node_counts(&k), (used, len, cap));
    }
}

#[test]
fn node_table_grow_failure_is_nomem_and_changes_nothing() {
    let (mut v, k) = boot();
    v.creat(None, "/tmp/keep", 0o644).unwrap();
    // Fill the table to its capacity, so the next node needs a grow.
    let mut i = 0usize;
    while k
        .fs
        .with(|s| s.free_len + (s.nodes.capacity() - s.nodes.len()))
        > 0
    {
        v.creat(None, &format!("/tmp/f{i}"), 0o644).unwrap();
        i += 1;
    }
    let before = node_counts(&k);
    crate::kalloc::tests::fail_in(0);
    let r = k.fs.with_room(1, |s| kern_alloc(s, 1));
    crate::kalloc::tests::disarm();
    assert_eq!(r, Err(FsError::NoMem));
    assert_eq!(node_counts(&k), before);
    // The tree is whole, and the next create grows the table.
    assert!(is_reg(&mut v, "/tmp/keep"));
    for j in 0..i {
        assert!(is_reg(&mut v, &format!("/tmp/f{j}")));
    }
    v.creat(None, "/tmp/after", 0o644).unwrap();
    assert!(is_reg(&mut v, "/tmp/after"));
    assert!(node_counts(&k).2 > before.2);
}

/// Read `n` bytes of `path` at `off`.
fn read_at(v: &mut Vfs, path: &str, off: u64, n: usize) -> Vec<u8> {
    let f = v.open_path(None, path, O_RDWR, 0).unwrap();
    v.seek(&f, off as i64, SEEK_SET).unwrap();
    let mut out = vec![0u8; n];
    assert_eq!(v.read(&f, &mut out).unwrap(), n);
    v.close(f).unwrap();
    out
}

/// Write `data` to `path` at `off`, creating it.
fn write_at(v: &mut Vfs, path: &str, off: u64, data: &[u8]) {
    let f = v.open_path(None, path, O_RDWR | O_CREAT, 0o644).unwrap();
    v.seek(&f, off as i64, SEEK_SET).unwrap();
    assert_eq!(v.write(&f, data).unwrap(), data.len());
    v.close(f).unwrap();
}

#[test]
fn tmpfs_extent_move_keeps_data() {
    let (mut v, _k) = boot();
    let head = *b"AAAAaaaa";
    let pg = PAGE as u64;
    write_at(&mut v, "/tmp/a", 0, &head);
    // B takes the page after A's, so A's growth moves its extent.
    write_at(&mut v, "/tmp/b", 0, b"B");
    write_at(&mut v, "/tmp/a", pg + 3, b"grow");
    assert_eq!(read_at(&mut v, "/tmp/a", 0, 8), head);
    assert_eq!(read_at(&mut v, "/tmp/a", pg + 3, 4), b"grow");
    assert_eq!(read_at(&mut v, "/tmp/a", 8, 64), vec![0u8; 64]);
    assert_eq!(read_at(&mut v, "/tmp/b", 0, 1), b"B");
    // C takes the page after A's new run; a truncate grows A past it.
    write_at(&mut v, "/tmp/c", 0, b"C");
    v.truncate(None, "/tmp/a", 3 * pg + 1).unwrap();
    assert_eq!(v.stat(None, "/tmp/a").unwrap().size, 3 * pg + 1);
    assert_eq!(read_at(&mut v, "/tmp/a", 0, 8), head);
    assert_eq!(read_at(&mut v, "/tmp/a", pg + 3, 4), b"grow");
    assert_eq!(read_at(&mut v, "/tmp/a", pg + 7, 64), vec![0u8; 64]);
    assert_eq!(read_at(&mut v, "/tmp/a", 3 * pg, 1), [0u8]);
    assert_eq!(read_at(&mut v, "/tmp/b", 0, 1), b"B");
    assert_eq!(read_at(&mut v, "/tmp/c", 0, 1), b"C");
}
