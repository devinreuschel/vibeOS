use super::*;

fn fresh(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    {
        let mut d = MemDisk::new(&mut b).unwrap();
        let mut v = Vol::new();
        mkfs(&mut d, b"vibe", &mut v).unwrap();
    }
    b
}

fn with_vol<R>(buf: &mut [u8], f: impl FnOnce(&mut Vol, &mut MemDisk) -> R) -> R {
    let mut disk = MemDisk::new(buf).unwrap();
    let mut vol = Vol::new();
    mount(&mut disk, &mut vol).unwrap();
    f(&mut vol, &mut disk)
}

#[test]
fn version_is_one() {
    assert_eq!(VERSION, 1);
    assert_eq!(MAGIC_SUPER, u32::from_le_bytes(*b"VIBE"));
}

#[test]
fn mkfs_mount_root() {
    let mut b = fresh(256 * 1024);
    with_vol(&mut b, |v, d| {
        assert_eq!(v.generation, 1);
        let r = v.walk(d, b"/").unwrap();
        assert!(r.is_dir());
        assert_eq!(r.ino, ROOT_INO);
    });
    let mut disk = MemDisk::new(&mut b).unwrap();
    let r = fsck(&mut disk).unwrap();
    assert_eq!(r.errors, 0);
}

#[test]
fn inline_write_read() {
    let mut b = fresh(256 * 1024);
    with_vol(&mut b, |v, d| {
        v.create(d, ROOT_INO, b"a.txt", InodeKind::Reg, 0o644, None)
            .unwrap();
        let n = v.lookup(d, ROOT_INO, b"a.txt").unwrap();
        v.write(d, n.ino, 0, b"hello").unwrap();
        let mut out = [0u8; 8];
        let g = v.read(d, n.ino, 0, &mut out).unwrap();
        assert_eq!(g, 5);
        assert_eq!(&out[..5], b"hello");
        v.sync(d).unwrap();
    });
    with_vol(&mut b, |v, d| {
        let n = v.lookup(d, ROOT_INO, b"a.txt").unwrap();
        let mut out = [0u8; 8];
        v.read(d, n.ino, 0, &mut out).unwrap();
        assert_eq!(&out[..5], b"hello");
    });
}

#[test]
fn extent_past_inline() {
    let mut b = fresh(256 * 1024);
    let mut payload = [0u8; 400];
    let mut i = 0usize;
    while i < payload.len() {
        payload[i] = (i % 251) as u8;
        i += 1;
    }
    with_vol(&mut b, |v, d| {
        v.create(d, ROOT_INO, b"big.bin", InodeKind::Reg, 0o644, None)
            .unwrap();
        let n = v.lookup(d, ROOT_INO, b"big.bin").unwrap();
        v.write(d, n.ino, 0, &payload).unwrap();
        v.sync(d).unwrap();
    });
    with_vol(&mut b, |v, d| {
        let n = v.lookup(d, ROOT_INO, b"big.bin").unwrap();
        assert!(n.size as usize >= 400);
        let mut out = [0u8; 400];
        v.read(d, n.ino, 0, &mut out).unwrap();
        assert_eq!(out, payload);
    });
    let mut disk = MemDisk::new(&mut b).unwrap();
    assert_eq!(fsck(&mut disk).unwrap().errors, 0);
}

#[test]
fn mkdir_symlink_readdir() {
    let mut b = fresh(256 * 1024);
    with_vol(&mut b, |v, d| {
        v.create(d, ROOT_INO, b"sub", InodeKind::Dir, 0o755, None)
            .unwrap();
        let sub = v.lookup(d, ROOT_INO, b"sub").unwrap();
        v.create(d, sub.ino, b"f", InodeKind::Reg, 0o644, None)
            .unwrap();
        v.create(d, ROOT_INO, b"l", InodeKind::Lnk, 0o777, Some(b"/sub/f"))
            .unwrap();
        v.sync(d).unwrap();
        let mut node = Node::EMPTY;
        let mut n = 0u32;
        let mut c = 0u64;
        while let Some(next) = v.readdir(d, ROOT_INO, c, &mut node).unwrap() {
            n += 1;
            c = next;
        }
        assert_eq!(n, 2);
        let mut t = [0u8; 16];
        let ln = v.lookup(d, ROOT_INO, b"l").unwrap();
        let k = v.readlink(d, ln.ino, &mut t).unwrap();
        assert_eq!(&t[..k], b"/sub/f");
    });
}

#[test]
fn snapshot_pins_generation() {
    let mut b = fresh(256 * 1024);
    with_vol(&mut b, |v, d| {
        v.create(d, ROOT_INO, b"a", InodeKind::Reg, 0o644, None)
            .unwrap();
        let n = v.lookup(d, ROOT_INO, b"a").unwrap();
        v.write(d, n.ino, 0, b"one").unwrap();
        v.snapshot(d, b"snap0").unwrap();
        v.write(d, n.ino, 0, b"two").unwrap();
        v.sync(d).unwrap();
        assert!(v.snaps[0].used);
        assert!(v.snaps[0].generation >= 1);
    });
    let mut disk = MemDisk::new(&mut b).unwrap();
    assert_eq!(fsck(&mut disk).unwrap().errors, 0);
}

#[test]
fn corrupt_super_uses_other_slot() {
    let mut b = fresh(256 * 1024);
    b[0] ^= 0xff;
    let mut disk = MemDisk::new(&mut b).unwrap();
    let mut vol = Vol::new();
    mount(&mut disk, &mut vol).unwrap();
    assert_eq!(vol.generation, 1);
}

#[test]
fn corrupt_both_supers_fails() {
    let mut b = fresh(256 * 1024);
    b[0] ^= 0xff;
    b[BLOCK] ^= 0xff;
    let mut disk = MemDisk::new(&mut b).unwrap();
    let mut vol = Vol::new();
    assert_eq!(mount(&mut disk, &mut vol).unwrap_err(), Error::Corrupt);
    let r = fsck(&mut disk).unwrap();
    assert!(r.errors > 0);
}

#[test]
fn corrupt_inode_crc_is_reported() {
    let mut b = fresh(256 * 1024);
    // inode leaf is block 3 after mkfs
    let off = 3 * BLOCK + HDR + 10;
    b[off] ^= 0xff;
    let mut disk = MemDisk::new(&mut b).unwrap();
    let mut vol = Vol::new();
    assert_eq!(mount(&mut disk, &mut vol).unwrap_err(), Error::Corrupt);
    assert!(fsck(&mut disk).unwrap().errors > 0);
}

#[test]
fn corrupt_data_crc_not_returned() {
    let mut b = fresh(256 * 1024);
    with_vol(&mut b, |v, d| {
        v.create(d, ROOT_INO, b"x", InodeKind::Reg, 0o644, None)
            .unwrap();
        let n = v.lookup(d, ROOT_INO, b"x").unwrap();
        let big = [7u8; 200];
        v.write(d, n.ino, 0, &big).unwrap();
        v.sync(d).unwrap();
    });
    // smash a data block (search for non-meta by flipping a late block)
    let mut flipped = false;
    let mut blk = 4usize;
    while blk < b.len() / BLOCK {
        let o = blk * BLOCK;
        if le32(&b[o..], 0) != MAGIC_META && le32(&b[o..], 0) != MAGIC_SUPER {
            b[o] ^= 0xff;
            flipped = true;
            break;
        }
        blk += 1;
    }
    assert!(flipped);
    with_vol(&mut b, |v, d| {
        let n = v.lookup(d, ROOT_INO, b"x").unwrap();
        let mut out = [0u8; 200];
        assert_eq!(v.read(d, n.ino, 0, &mut out).unwrap_err(), Error::Corrupt);
    });
    let mut disk = MemDisk::new(&mut b).unwrap();
    assert!(fsck(&mut disk).unwrap().errors > 0);
}

#[test]
fn truncated_mkfs() {
    let mut b = vec![0u8; 4096];
    assert!(MemDisk::new(&mut b).is_err());
}

#[test]
fn btree_many_dirents() {
    let mut b = fresh(256 * 1024);
    with_vol(&mut b, |v, d| {
        let mut i = 0u32;
        while i < 40 {
            let mut name = [0u8; 8];
            name[0] = b'f';
            name[1] = b'0' + (i / 10) as u8;
            name[2] = b'0' + (i % 10) as u8;
            v.create(d, ROOT_INO, &name[..3], InodeKind::Reg, 0o644, None)
                .unwrap();
            i += 1;
        }
        v.sync(d).unwrap();
        assert_eq!(v.dir_count(ROOT_INO), 40);
    });
    with_vol(&mut b, |v, d| {
        assert_eq!(v.dir_count(ROOT_INO), 40);
        v.lookup(d, ROOT_INO, b"f39").unwrap();
    });
    let mut disk = MemDisk::new(&mut b).unwrap();
    assert_eq!(fsck(&mut disk).unwrap().errors, 0);
}

#[test]
fn crash_at_each_write_is_consistent() {
    let base = fresh(256 * 1024);
    // count ops on a full run
    let mut probe = base.clone();
    {
        let mut c = CrashDisk::new(&mut probe, u64::MAX).unwrap();
        let mut v = Vol::new();
        mount(&mut c, &mut v).unwrap();
        v.create(&mut c, ROOT_INO, b"a", InodeKind::Reg, 0o644, None)
            .unwrap();
        let a = v.lookup(&mut c, ROOT_INO, b"a").unwrap();
        v.write(&mut c, a.ino, 0, b"alpha").unwrap();
        v.sync(&mut c).unwrap();
        let payload = [9u8; 300];
        v.write(&mut c, a.ino, 0, &payload).unwrap();
        v.sync(&mut c).unwrap();
        v.create(&mut c, ROOT_INO, b"d", InodeKind::Dir, 0o755, None)
            .unwrap();
        let dir = v.lookup(&mut c, ROOT_INO, b"d").unwrap();
        v.create(&mut c, dir.ino, b"b", InodeKind::Reg, 0o644, None)
            .unwrap();
        let bb = v.lookup(&mut c, dir.ino, b"b").unwrap();
        v.write(&mut c, bb.ino, 0, b"beta-data").unwrap();
        v.sync(&mut c).unwrap();
        v.unlink(&mut c, ROOT_INO, b"a", false).unwrap();
        v.sync(&mut c).unwrap();
        assert!(c.ops > 4);
        let total = c.ops;
        // Under Miri each crash point's replay, fsck and mount take tens of
        // seconds; three points spread over the run keep every step's code.
        let stride = if cfg!(miri) { total.div_ceil(3) } else { 1 };
        let mut i = 1u64;
        while i <= total {
            let mut img = base.clone();
            {
                let mut c = CrashDisk::new(&mut img, i).unwrap();
                let mut v = Vol::new();
                let _ = (|| {
                    mount(&mut c, &mut v)?;
                    v.create(&mut c, ROOT_INO, b"a", InodeKind::Reg, 0o644, None)?;
                    let a = v.lookup(&mut c, ROOT_INO, b"a")?;
                    v.write(&mut c, a.ino, 0, b"alpha")?;
                    v.sync(&mut c)?;
                    let payload = [9u8; 300];
                    v.write(&mut c, a.ino, 0, &payload)?;
                    v.sync(&mut c)?;
                    v.create(&mut c, ROOT_INO, b"d", InodeKind::Dir, 0o755, None)?;
                    let dir = v.lookup(&mut c, ROOT_INO, b"d")?;
                    v.create(&mut c, dir.ino, b"b", InodeKind::Reg, 0o644, None)?;
                    let bb = v.lookup(&mut c, dir.ino, b"b")?;
                    v.write(&mut c, bb.ino, 0, b"beta-data")?;
                    v.sync(&mut c)?;
                    v.unlink(&mut c, ROOT_INO, b"a", false)?;
                    v.sync(&mut c)?;
                    Ok::<(), Error>(())
                })();
            }
            let mut disk = MemDisk::new(&mut img).unwrap();
            let r = fsck(&mut disk).expect("fsck runs");
            assert_eq!(r.errors, 0, "crash at op {i} left a corrupt live tree");
            let mut vol = Vol::new();
            mount(&mut disk, &mut vol).expect("mount after crash");
            i += stride;
        }
    }
}

fn xorshift64(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

/// Iterations the crash workload commits; `run_vibefs_crash.KILL_COMMIT_MAX`.
const CRASH_ITERS: u32 = 200;
const CRASH_ITER_BYTES: usize = 300;

fn crash_payload(n: u32) -> [u8; CRASH_ITER_BYTES] {
    let mut p = [0u8; CRASH_ITER_BYTES];
    for (k, b) in p.iter_mut().enumerate() {
        *b = (n as usize + k) as u8;
    }
    p
}

/// One iteration of the guest's `crash_loop`: open `/w` with `O_TRUNC`,
/// write 300 bytes, sync.
fn crash_iter<D: Disk>(v: &mut Vol, d: &mut D, n: u32) -> Result<(), Error> {
    let w = match v.lookup(d, ROOT_INO, b"w") {
        Ok(w) => w,
        Err(Error::NotFound) => {
            v.create(d, ROOT_INO, b"w", InodeKind::Reg, 0o644, None)?;
            v.lookup(d, ROOT_INO, b"w")?
        }
        Err(e) => return Err(e),
    };
    v.truncate(d, w.ino, 0)?;
    let wrote = v.write(d, w.ino, 0, &crash_payload(n))?;
    assert_eq!(wrote, CRASH_ITER_BYTES);
    v.sync(d)
}

fn crash_decode(b: &[u8]) -> Option<u32> {
    let n = u32::from(*b.first()?);
    (b.len() == CRASH_ITER_BYTES && b == crash_payload(n)).then_some(n)
}

#[test]
fn crash_workload_seeded_points() {
    // Under Miri one commit takes seconds: 3 commits and 2 crash points
    // keep the path (the 200-commit fit is the native run's claim).
    let iters = if cfg!(miri) { 3 } else { CRASH_ITERS };
    let points = if cfg!(miri) { 2 } else { 1000 };
    // 256 KiB: `run_vibefs_crash.IMAGE_BYTES`, the guest's image.
    let mut base = fresh(256 * 1024);
    {
        let mut d = MemDisk::new(&mut base).unwrap();
        let mut v = Vol::new();
        mount(&mut d, &mut v).unwrap();
        crash_iter(&mut v, &mut d, 0).unwrap();
    }
    // Probe: 200 commits fit the image, and T counts their device ops.
    let total = {
        let mut img = base.clone();
        let mut c = CrashDisk::seeded(&mut img, u64::MAX, 1).unwrap();
        let mut v = Vol::new();
        mount(&mut c, &mut v).unwrap();
        for n in 1..=iters {
            crash_iter(&mut v, &mut c, n).unwrap_or_else(|e| panic!("probe iteration {n}: {e:?}"));
        }
        c.ops
    };
    let mut rng = 0x5eed_c0de_u64;
    for _ in 0..points {
        let p = 1 + xorshift64(&mut rng) % total;
        let seed_p = xorshift64(&mut rng);
        let mut img = base.clone();
        let mut last = 0u32;
        {
            let mut c = CrashDisk::seeded(&mut img, p, seed_p).unwrap();
            let mut v = Vol::new();
            mount(&mut c, &mut v).unwrap();
            for n in 1..=iters {
                if c.ops >= p {
                    break;
                }
                last = n;
                if crash_iter(&mut v, &mut c, n).is_err() {
                    break;
                }
            }
            c.crash();
        }
        let ctx = format!("seed {seed_p:#x} p {p} N {last}");
        let mut disk = MemDisk::new(&mut img).unwrap();
        let r = fsck(&mut disk).unwrap_or_else(|e| panic!("{ctx}: fsck {e:?}"));
        assert_eq!((r.errors, r.warnings), (0, 0), "{ctx}: fsck");
        let mut vol = Vol::new();
        mount(&mut disk, &mut vol).unwrap_or_else(|e| panic!("{ctx}: mount {e:?}"));
        let w = vol
            .lookup(&mut disk, ROOT_INO, b"w")
            .unwrap_or_else(|e| panic!("{ctx}: /w {e:?}"));
        let mut buf = [0u8; 512];
        let got = vol.read(&mut disk, w.ino, 0, &mut buf).unwrap();
        let i = crash_decode(&buf[..got]);
        assert!(
            i == Some(last) || (last > 0 && i == Some(last - 1)),
            "{ctx}: /w holds {i:?}"
        );
    }
}

/// A regular file `name` in the root; its inode number.
fn new_file(v: &mut Vol, d: &mut MemDisk, name: &[u8]) -> u32 {
    v.create(d, ROOT_INO, name, InodeKind::Reg, 0o644, None)
        .unwrap();
    v.lookup(d, ROOT_INO, name).unwrap().ino
}

fn n_ext(v: &Vol, ino: u32) -> u8 {
    v.inodes[v.inode_slot(ino).unwrap()].n_ext
}

#[test]
fn write_past_size_limit() {
    assert_eq!(MAX_FILE_SIZE, (1u64 << 44) - 4096);
    let mut b = fresh(256 * 1024);
    with_vol(&mut b, |v, d| {
        let ino = new_file(v, d, b"big");
        assert_eq!(v.write(d, ino, 0, b"abc").unwrap(), 3);
        let df = v.df();
        let ext = n_ext(v, ino);
        for off in [MAX_FILE_SIZE, 1u64 << 44] {
            assert_eq!(v.write(d, ino, off, b"x").unwrap_err(), Error::FileTooBig);
        }
        assert_eq!(v.write(d, ino, MAX_FILE_SIZE, b"").unwrap(), 0);
        assert_eq!(v.file_size(ino).unwrap(), 3);
        let mut out = [0u8; 8];
        assert_eq!(v.read(d, ino, 0, &mut out).unwrap(), 3);
        assert_eq!(&out[..3], b"abc");
        assert_eq!(v.df(), df);
        assert_eq!(n_ext(v, ino), ext);
    });
}

#[test]
fn write_crossing_size_limit_is_short() {
    let mut b = fresh(256 * 1024);
    with_vol(&mut b, |v, d| {
        let ino = new_file(v, d, b"edge");
        assert_eq!(v.write(d, ino, MAX_FILE_SIZE - 1, b"xy").unwrap(), 1);
        assert_eq!(v.file_size(ino).unwrap(), MAX_FILE_SIZE);
        let mut out = [0u8; 4];
        assert_eq!(v.read(d, ino, MAX_FILE_SIZE - 1, &mut out).unwrap(), 1);
        assert_eq!(out[0], b'x');
    });
}

#[test]
fn truncate_past_size_limit() {
    let mut b = fresh(256 * 1024);
    with_vol(&mut b, |v, d| {
        let ino = new_file(v, d, b"t");
        assert_eq!(
            v.truncate(d, ino, MAX_FILE_SIZE + 1).unwrap_err(),
            Error::FileTooBig
        );
        v.truncate(d, ino, MAX_FILE_SIZE).unwrap();
        assert_eq!(v.file_size(ino).unwrap(), MAX_FILE_SIZE);
    });
}

#[test]
fn node_size_above_4gib() {
    let mut b = fresh(256 * 1024);
    with_vol(&mut b, |v, d| {
        let ino = new_file(v, d, b"five");
        assert_eq!(v.write(d, ino, 5 << 30, b"x").unwrap(), 1);
        let n = v.lookup(d, ROOT_INO, b"five").unwrap();
        assert_eq!(n.size, (5u64 << 30) + 1);
    });
}

#[test]
fn block_math_near_u32_limit() {
    let mut b = fresh(256 * 1024);
    with_vol(&mut b, |v, d| {
        let ino = new_file(v, d, b"top");
        let start = MAX_FILE_SIZE - 2 * BLOCK as u64;
        let mut two = vec![0u8; 2 * BLOCK];
        let mut i = 0usize;
        while i < two.len() {
            two[i] = (i % 253) as u8;
            i += 1;
        }
        assert_eq!(v.write(d, ino, start, &two).unwrap(), two.len());
        assert_eq!(v.file_size(ino).unwrap(), MAX_FILE_SIZE);
        // Overwrite part of the first block: the split path.
        two[10..20].fill(0xAB);
        assert_eq!(v.write(d, ino, start + 10, &[0xAB; 10]).unwrap(), 10);
        let mut out = vec![0u8; 2 * BLOCK];
        assert_eq!(v.read(d, ino, start, &mut out).unwrap(), out.len());
        assert_eq!(out, two);
        v.truncate(d, ino, MAX_FILE_SIZE - BLOCK as u64).unwrap();
        assert_eq!(v.file_size(ino).unwrap(), MAX_FILE_SIZE - BLOCK as u64);
        let mut out = vec![0u8; 2 * BLOCK];
        assert_eq!(v.read(d, ino, start, &mut out).unwrap(), BLOCK);
        assert_eq!(out[..BLOCK], two[..BLOCK]);
    });
}

/// `fsck` over the image in `b`.
fn fsck_of(b: &mut [u8]) -> FsckReport {
    let mut d = MemDisk::new(b).unwrap();
    fsck(&mut d).unwrap()
}

fn payload(seed: u32) -> [u8; 300] {
    let mut p = [0u8; 300];
    let mut i = 0usize;
    while i < p.len() {
        p[i] = (seed as usize * 31 + i) as u8;
        i += 1;
    }
    p
}

#[test]
fn sessions_64_no_leak() {
    let mut b = fresh(64 * BLOCK);
    with_vol(&mut b, |v, d| {
        let ino = new_file(v, d, b"f");
        assert_eq!(v.write(d, ino, 0, &payload(0)).unwrap(), 300);
        v.sync(d).unwrap();
    });
    let free = with_vol(&mut b, |v, _| v.df().1);
    let warn0 = fsck_of(&mut b).warnings;
    // Under Miri a session's mount, commit and fsck take tens of seconds.
    let sessions = if cfg!(miri) { 2 } else { 64 };
    let mut s = 1u32;
    while s <= sessions {
        with_vol(&mut b, |v, d| {
            assert_eq!(v.df().1, free, "free bytes at session {s}");
            let ino = v.lookup(d, ROOT_INO, b"f").unwrap().ino;
            assert_eq!(v.write(d, ino, 0, &payload(s)).unwrap(), 300);
            v.sync(d)
                .unwrap_or_else(|e| panic!("sync at session {s}: {e:?}"));
        });
        let r = fsck_of(&mut b);
        assert_eq!(r.errors, 0, "fsck errors at session {s}");
        assert_eq!(r.warnings, warn0, "fsck warnings at session {s}");
        s += 1;
    }
    with_vol(&mut b, |v, d| {
        let ino = v.lookup(d, ROOT_INO, b"f").unwrap().ino;
        let mut out = [0u8; 300];
        assert_eq!(v.read(d, ino, 0, &mut out).unwrap(), 300);
        assert_eq!(out, payload(sessions));
    });
}

#[test]
fn commit_free_count_constant() {
    let mut b = fresh(64 * BLOCK);
    let free = with_vol(&mut b, |v, d| {
        let ino = new_file(v, d, b"f");
        assert_eq!(v.write(d, ino, 0, &payload(0)).unwrap(), 300);
        v.sync(d).unwrap();
        let free = v.free_count();
        // Under Miri a commit takes seconds.
        let commits = if cfg!(miri) { 4 } else { 200 };
        let mut i = 1u32;
        while i <= commits {
            assert_eq!(v.write(d, ino, 0, &payload(i)).unwrap(), 300);
            v.sync(d)
                .unwrap_or_else(|e| panic!("sync at commit {i}: {e:?}"));
            assert_eq!(v.free_count(), free, "free count after commit {i}");
            i += 1;
        }
        free
    });
    with_vol(&mut b, |v, _| {
        assert_eq!(v.free_count(), free, "after remount")
    });
    let r = fsck_of(&mut b);
    assert_eq!((r.errors, r.warnings), (0, 0));
}

#[test]
#[cfg_attr(miri, ignore = "63 nested mkdirs run past 10 minutes under Miri")]
fn nested_dirs_63_commit_remount() {
    let mut b = fresh(256 * BLOCK);
    with_vol(&mut b, |v, d| {
        let mut dir = ROOT_INO;
        let mut n = 0usize;
        while n < 63 {
            v.create(d, dir, b"d", InodeKind::Dir, 0o755, None)
                .unwrap_or_else(|e| panic!("mkdir {n}: {e:?}"));
            dir = v.lookup(d, dir, b"d").unwrap().ino;
            n += 1;
        }
        v.sync(d).unwrap();
        assert_eq!(v.nmeta, 70);
    });
    let mut path = Vec::new();
    let mut n = 0usize;
    while n < 63 {
        if n > 0 {
            path.push(b'/');
        }
        path.push(b'd');
        n += 1;
    }
    with_vol(&mut b, |v, d| {
        assert!(v.walk(d, &path).unwrap().is_dir());
        v.dirty = true;
        v.sync(d).unwrap();
        assert_eq!(v.nmeta, 70);
    });
    with_vol(&mut b, |v, d| {
        assert!(v.walk(d, &path).unwrap().is_dir());
    });
    let r = fsck_of(&mut b);
    assert_eq!((r.errors, r.warnings), (0, 0));
}

/// `f` (300 bytes, one extent), `g` (5 bytes, inline) and `p/c` on a
/// 64-block image that fsck finds clean.
fn base_tree() -> Vec<u8> {
    let mut b = fresh(64 * BLOCK);
    with_vol(&mut b, |v, d| {
        let f = new_file(v, d, b"f");
        assert_eq!(v.write(d, f, 0, &payload(1)).unwrap(), 300);
        let g = new_file(v, d, b"g");
        assert_eq!(v.write(d, g, 0, b"hello").unwrap(), 5);
        v.create(d, ROOT_INO, b"p", InodeKind::Dir, 0o755, None)
            .unwrap();
        let p = v.lookup(d, ROOT_INO, b"p").unwrap().ino;
        v.create(d, p, b"c", InodeKind::Dir, 0o755, None).unwrap();
        v.sync(d).unwrap();
    });
    let r = fsck_of(&mut b);
    assert_eq!((r.errors, r.warnings), (0, 0));
    b
}

/// The base tree with `plant` applied in memory and committed.
fn planted(plant: impl FnOnce(&mut Vol, &mut MemDisk)) -> FsckReport {
    let mut b = base_tree();
    with_vol(&mut b, |v, d| {
        plant(v, d);
        v.dirty = true;
        v.sync(d).unwrap();
    });
    fsck_of(&mut b)
}

/// The base tree with `plant(bitmap, refc, f's data block)` applied to
/// the on-disk `ALLOC` block, re-sealed in place.
fn planted_alloc(plant: impl FnOnce(&mut [u8], &mut [u8], u32)) -> FsckReport {
    let mut b = base_tree();
    let (root, n, fblk) = with_vol(&mut b, |v, d| {
        let f = v.lookup(d, ROOT_INO, b"f").unwrap().ino;
        let s = v.inode_slot(f).unwrap();
        assert_eq!(v.inodes[s].n_ext, 1);
        (v.alloc_root, v.nblocks, v.inodes[s].extents[0].phys)
    });
    {
        let mut d = MemDisk::new(&mut b).unwrap();
        let mut blk = [0u8; BLOCK];
        d.read_block(root, &mut blk).unwrap();
        let nbytes = (n as usize).div_ceil(8);
        {
            let (head, rest) = blk.split_at_mut(HDR + nbytes);
            plant(&mut head[HDR..], &mut rest[..n as usize], fblk);
        }
        finish_meta(&mut blk);
        d.write_block(root, &blk).unwrap();
    }
    fsck_of(&mut b)
}

fn slot_of(v: &mut Vol, d: &mut MemDisk, dir: u32, name: &[u8]) -> (usize, usize) {
    let e = v.find_dent(dir, name).unwrap();
    let ino = v.lookup(d, dir, name).unwrap().ino;
    (e, v.inode_slot(ino).unwrap())
}

#[test]
#[cfg_attr(miri, ignore = "one image per defect runs past 10 minutes under Miri")]
fn fsck_reports_each_planted_defect() {
    let expect = |what: &str, r: FsckReport, class: Defect, n: Option<u32>| {
        assert!(r.count(class) > 0, "{what}: no {} in {r:?}", class.as_str());
        if let Some(n) = n {
            assert_eq!(r.count(class), n, "{what}: {r:?}");
        }
        if class == Defect::Leak {
            assert_eq!(r.errors, 0, "{what}: {r:?}");
        } else {
            assert!(r.errors > 0, "{what}: {r:?}");
        }
    };
    let r = planted(|v, d| {
        let (e, _) = slot_of(v, d, ROOT_INO, b"f");
        v.dents[e].kind = KIND_DIR;
    });
    expect("dirent kind", r, Defect::Kind, None);
    let r = planted(|v, d| {
        let (_, s) = slot_of(v, d, ROOT_INO, b"f");
        v.inodes[s].mode = crate::fs::S_IFDIR | 0o644;
    });
    expect("mode", r, Defect::Mode, None);
    let r = planted(|v, d| {
        let (_, s) = slot_of(v, d, ROOT_INO, b"g");
        assert!(v.inodes[s].flags & F_INLINE != 0);
        v.inodes[s].size = 200;
    });
    expect("inline", r, Defect::Inline, None);
    let r = planted(|v, d| {
        let (e, _) = slot_of(v, d, ROOT_INO, b"g");
        v.dents[e].name[0] = b'f';
    });
    expect("dup name", r, Defect::DupName, Some(1));
    let r = planted(|v, d| {
        let (_, s) = slot_of(v, d, ROOT_INO, b"f");
        v.inodes[s].nlink = 2;
    });
    expect("nlink", r, Defect::Nlink, Some(1));
    let r = planted(|v, d| {
        let (e, _) = slot_of(v, d, ROOT_INO, b"g");
        v.dents[e].used = false;
    });
    expect("dirent emptied", r, Defect::Unreachable, Some(1));
    let r = planted(|v, d| {
        let p = v.lookup(d, ROOT_INO, b"p").unwrap().ino;
        let c = v.lookup(d, p, b"c").unwrap().ino;
        let e = v.find_dent(ROOT_INO, b"p").unwrap();
        v.dents[e].parent = c;
    });
    expect("dir in own subtree", r, Defect::Unreachable, Some(2));
    let r = planted(|v, d| {
        let (e, _) = slot_of(v, d, ROOT_INO, b"g");
        v.dents[e].ino = 999;
    });
    expect("dangling", r, Defect::Dangling, Some(1));
    let r = planted(|v, d| {
        let (_, s) = slot_of(v, d, ROOT_INO, b"g");
        v.inodes[s].kind = 9;
    });
    expect("kind out of range", r, Defect::Mount, Some(1));
    let r = planted_alloc(|bm, _, b| bit_set(bm, b, false));
    expect("bit clear", r, Defect::BitFree, Some(1));
    let r = planted_alloc(|bm, rc, b| {
        bit_set(bm, b, false);
        rc[b as usize] = 0;
    });
    expect("refcount clear", r, Defect::RefFree, Some(1));
    let r = planted_alloc(|bm, rc, _| {
        let last = rc.len() as u32 - 1;
        assert_eq!(rc[last as usize], 0);
        bit_set(bm, last, true);
    });
    expect("bit on a free block", r, Defect::Leak, Some(1));
}

#[test]
fn fsck_dir_int_57_entries() {
    let name = |i: usize| [b'f', b'0' + (i / 10) as u8, b'0' + (i % 10) as u8];
    let mut b = fresh(64 * BLOCK);
    with_vol(&mut b, |v, d| {
        let mut i = 0usize;
        while i < 57 {
            new_file(v, d, &name(i));
            i += 1;
        }
        v.sync(d).unwrap();
    });
    let r = fsck_of(&mut b);
    assert_eq!((r.errors, r.warnings), (0, 0));
    let root_kind = |v: &mut Vol, d: &mut MemDisk| {
        let s = v.inode_slot(ROOT_INO).unwrap();
        let mut blk = [0u8; BLOCK];
        d.read_block(v.inodes[s].dir_root, &mut blk).unwrap();
        blk[4]
    };
    with_vol(&mut b, |v, d| {
        assert_eq!(root_kind(v, d), META_DIR_INT);
        assert_eq!(v.dir_count(ROOT_INO), 57);
        v.lookup(d, ROOT_INO, &name(56)).unwrap();
        // Two entries now share a name; fsck must read both leaves.
        let e = v.find_dent(ROOT_INO, &name(3)).unwrap();
        v.dents[e].name[..3].copy_from_slice(&name(55));
        v.dirty = true;
        v.sync(d).unwrap();
    });
    with_vol(&mut b, |v, d| assert_eq!(root_kind(v, d), META_DIR_INT));
    let r = fsck_of(&mut b);
    assert_eq!(r.count(Defect::DupName), 1, "{r:?}");
}

#[test]
fn rename_dir_into_own_subtree_einval() {
    let mut b = fresh(64 * BLOCK);
    with_vol(&mut b, |v, d| {
        v.create(d, ROOT_INO, b"p", InodeKind::Dir, 0o755, None)
            .unwrap();
        let p = v.lookup(d, ROOT_INO, b"p").unwrap().ino;
        v.create(d, p, b"c", InodeKind::Dir, 0o755, None).unwrap();
        let c = v.lookup(d, p, b"c").unwrap().ino;
        assert_eq!(
            v.rename(d, ROOT_INO, b"p", c, b"q").unwrap_err(),
            Error::Inval
        );
        assert_eq!(
            v.rename(d, ROOT_INO, b"p", p, b"q").unwrap_err(),
            Error::Inval
        );
        let x = new_file(v, d, b"tmp");
        v.rename(d, ROOT_INO, b"tmp", c, b"x").unwrap();
        assert_eq!(
            v.rename(d, ROOT_INO, b"p", c, b"x").unwrap_err(),
            Error::Inval
        );
        assert_eq!(v.lookup(d, c, b"x").unwrap().ino, x);
        assert_eq!(v.walk(d, b"/p/c").unwrap().ino, c);
        v.sync(d).unwrap();
    });
    let r = fsck_of(&mut b);
    assert_eq!(r.errors, 0, "{r:?}");
    assert_eq!(r.count(Defect::Unreachable), 0);
}

/// A rename onto an existing name replaces it as rename(2) does: a
/// directory replaces an empty directory and not one with entries, nor a
/// file, and a file never replaces a directory; fsck finds the volume
/// whole after.
#[test]
fn rename_replaces_as_linux() {
    let mut b = fresh(64 * BLOCK);
    with_vol(&mut b, |v, d| {
        for n in [b"p" as &[u8], b"q", b"r"] {
            v.create(d, ROOT_INO, n, InodeKind::Dir, 0o755, None)
                .unwrap();
        }
        let p = v.lookup(d, ROOT_INO, b"p").unwrap().ino;
        let r = v.lookup(d, ROOT_INO, b"r").unwrap().ino;
        new_file(v, d, b"f");
        new_file(v, d, b"g");
        v.create(d, r, b"in", InodeKind::Reg, 0o644, None).unwrap();
        assert_eq!(
            v.rename(d, ROOT_INO, b"p", ROOT_INO, b"f").unwrap_err(),
            Error::NotDir
        );
        assert_eq!(
            v.rename(d, ROOT_INO, b"f", ROOT_INO, b"p").unwrap_err(),
            Error::IsDir
        );
        assert_eq!(
            v.rename(d, ROOT_INO, b"p", ROOT_INO, b"r").unwrap_err(),
            Error::NotEmpty
        );
        v.rename(d, ROOT_INO, b"p", ROOT_INO, b"q").unwrap();
        assert_eq!(v.lookup(d, ROOT_INO, b"q").unwrap().ino, p);
        assert_eq!(v.lookup(d, ROOT_INO, b"p").unwrap_err(), Error::NotFound);
        v.rename(d, ROOT_INO, b"f", ROOT_INO, b"g").unwrap();
        assert_eq!(v.lookup(d, ROOT_INO, b"f").unwrap_err(), Error::NotFound);
        v.sync(d).unwrap();
    });
    let r = fsck_of(&mut b);
    assert_eq!(r.errors, 0, "{r:?}");
    assert_eq!(r.count(Defect::Unreachable), 0);
}

/// Block `i` of the crafted test data: every byte `i + 1`, the first
/// eight the block's number.
fn craft_block(i: u32) -> [u8; BLOCK] {
    let mut blk = [(i + 1) as u8; BLOCK];
    blk[..4].copy_from_slice(&i.to_le_bytes());
    blk
}

/// Hold `ino`'s first `blocks` file blocks in one multi-block extent, which
/// the kernel never writes itself: contiguous blocks on a fresh volume,
/// written with [`craft_block`] and committed.
fn craft_extent(v: &mut Vol, d: &mut MemDisk, ino: u32, blocks: u32) {
    let first = v.alloc_block().unwrap();
    for i in 1..blocks {
        assert_eq!(v.alloc_block().unwrap(), first + i, "contiguous blocks");
    }
    for i in 0..blocks {
        d.write_block(first + i, &craft_block(i)).unwrap();
    }
    let crc = v.extent_crc(d, first, blocks).unwrap();
    let is = v.inode_slot(ino).unwrap();
    v.inodes[is].extents[0] = Extent {
        log: 0,
        phys: first,
        len: blocks,
        crc,
    };
    v.inodes[is].n_ext = 1;
    v.inodes[is].size = u64::from(blocks) * BLOCK as u64;
    v.inodes[is].flags &= !F_INLINE;
    v.inodes[is].inline_len = 0;
    v.dirty = true;
    v.sync(d).unwrap();
}

/// `ino`'s extents, each `(log, phys, len, crc)`.
fn extents(v: &Vol, ino: u32) -> Vec<(u32, u32, u32, u32)> {
    let r = &v.inodes[v.inode_slot(ino).unwrap()];
    r.extents[..r.n_ext as usize]
        .iter()
        .map(|e| (e.log, e.phys, e.len, e.crc))
        .collect()
}

#[test]
fn write_past_16k_retry_no_leak() {
    let mut b = fresh(256 * 1024);
    with_vol(&mut b, |v, d| {
        let ino = new_file(v, d, b"f");
        for k in 0..4u64 {
            let blk = craft_block(k as u32);
            assert_eq!(v.write(d, ino, k * BLOCK as u64, &blk).unwrap(), BLOCK);
        }
        assert_eq!(n_ext(v, ino), MAX_EXT as u8);
        v.sync(d).unwrap();
        let df = v.df();
        let ex = extents(v, ino);
        for i in 0..100 {
            assert_eq!(
                v.write(d, ino, 16 * 1024, b"x").unwrap_err(),
                Error::NoSpace,
                "retry {i}"
            );
            assert_eq!(v.df(), df, "free space after retry {i}");
        }
        assert_eq!(extents(v, ino), ex);
        assert_eq!(v.file_size(ino).unwrap(), 16 * 1024);
        v.sync(d).unwrap();
        assert_eq!(v.df(), df);
    });
    let r = fsck_of(&mut b);
    assert_eq!((r.errors, r.warnings), (0, 0));
}

#[test]
fn write_short_count_updates_size() {
    let mut b = fresh(256 * 1024);
    with_vol(&mut b, |v, d| {
        let ino = new_file(v, d, b"f");
        for k in 0..3u64 {
            let blk = craft_block(k as u32);
            assert_eq!(v.write(d, ino, k * BLOCK as u64, &blk).unwrap(), BLOCK);
        }
        v.sync(d).unwrap();
        let (_, free, _) = v.df();
        let two = [0x5Au8; 2 * BLOCK];
        assert_eq!(v.write(d, ino, 12 * 1024, &two).unwrap(), BLOCK);
        assert_eq!(v.file_size(ino).unwrap(), 16 * 1024);
        assert_eq!(v.df().1, free - BLOCK as u64);
        let mut out = [0u8; 2 * BLOCK];
        assert_eq!(v.read(d, ino, 12 * 1024, &mut out).unwrap(), BLOCK);
        assert_eq!(out[..BLOCK], two[..BLOCK]);
        v.sync(d).unwrap();
        assert_eq!(v.df().1, free - BLOCK as u64);
    });
    let r = fsck_of(&mut b);
    assert_eq!((r.errors, r.warnings), (0, 0));
}

#[test]
fn cow_split_without_slots_leaks_nothing() {
    let mut b = fresh(256 * 1024);
    with_vol(&mut b, |v, d| {
        let ino = new_file(v, d, b"f");
        craft_extent(v, d, ino, 3);
        for k in 3..5u64 {
            let blk = craft_block(k as u32);
            assert_eq!(v.write(d, ino, k * BLOCK as u64, &blk).unwrap(), BLOCK);
        }
        assert_eq!(n_ext(v, ino), 3);
        let df = v.df();
        let ex = extents(v, ino);
        assert_eq!(
            v.write(d, ino, BLOCK as u64 + 7, b"x").unwrap_err(),
            Error::NoSpace
        );
        assert_eq!(v.df(), df);
        assert_eq!(extents(v, ino), ex);
        v.sync(d).unwrap();
        assert_eq!(v.df(), df);
    });
    let r = fsck_of(&mut b);
    assert_eq!((r.errors, r.warnings), (0, 0));
}

/// Require every extent of `ino` to match its CRC.
fn assert_crcs_valid(v: &mut Vol, d: &mut MemDisk, ino: u32) {
    let is = v.inode_slot(ino).unwrap();
    let r = v.inodes[is];
    for e in &r.extents[..r.n_ext as usize] {
        v.check_extent(d, *e)
            .unwrap_or_else(|err| panic!("extent at {}: {err:?}", e.log));
    }
}

/// `ino`'s bytes from `off`, `len` of them, which must all read.
fn read_all(v: &mut Vol, d: &mut MemDisk, ino: u32, off: u64, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    assert_eq!(v.read(d, ino, off, &mut out).unwrap(), len);
    out
}

#[test]
fn overwrite_corrupt_block_reports_corrupt() {
    let mut b = fresh(256 * 1024);
    let (ino, phys) = with_vol(&mut b, |v, d| {
        let ino = new_file(v, d, b"f");
        assert_eq!(v.write(d, ino, 0, &payload(1)).unwrap(), 300);
        v.sync(d).unwrap();
        let ex = extents(v, ino);
        assert_eq!(ex.len(), 1);
        (ino, ex[0].1)
    });
    b[phys as usize * BLOCK + 17] ^= 0x40;
    let reads_corrupt = |v: &mut Vol, d: &mut MemDisk| {
        for _ in 0..3 {
            let mut out = [0u8; 300];
            assert_eq!(v.read(d, ino, 0, &mut out).unwrap_err(), Error::Corrupt);
        }
    };
    with_vol(&mut b, |v, d| {
        let df = v.df();
        let ex = extents(v, ino);
        assert_eq!(v.write(d, ino, 100, b"z").unwrap_err(), Error::Corrupt);
        reads_corrupt(v, d);
        assert_eq!(v.df(), df);
        assert_eq!(extents(v, ino), ex);
        v.sync(d).unwrap();
        reads_corrupt(v, d);
        assert_eq!(extents(v, ino), ex);
    });
    with_vol(&mut b, |v, d| reads_corrupt(v, d));
}

#[test]
fn cow_block0_of_two_block_extent() {
    let mut b = fresh(256 * 1024);
    with_vol(&mut b, |v, d| {
        let ino = new_file(v, d, b"f");
        craft_extent(v, d, ino, 2);
        assert_eq!(v.write(d, ino, 5, b"new").unwrap(), 3);
        assert_eq!(n_ext(v, ino), 2);
        assert_crcs_valid(v, d, ino);
        let mut blk0 = craft_block(0);
        blk0[5..8].copy_from_slice(b"new");
        assert_eq!(read_all(v, d, ino, 0, BLOCK), blk0);
        assert_eq!(read_all(v, d, ino, BLOCK as u64, BLOCK), craft_block(1));
        v.sync(d).unwrap();
    });
    let r = fsck_of(&mut b);
    assert_eq!((r.errors, r.warnings), (0, 0));
}

#[test]
fn cow_middle_of_three_block_extent() {
    let mut b = fresh(256 * 1024);
    with_vol(&mut b, |v, d| {
        let ino = new_file(v, d, b"f");
        craft_extent(v, d, ino, 3);
        assert_eq!(v.write(d, ino, BLOCK as u64 + 9, b"mid").unwrap(), 3);
        assert_eq!(n_ext(v, ino), 3);
        assert_crcs_valid(v, d, ino);
        let mut blk1 = craft_block(1);
        blk1[9..12].copy_from_slice(b"mid");
        assert_eq!(read_all(v, d, ino, 0, BLOCK), craft_block(0));
        assert_eq!(read_all(v, d, ino, BLOCK as u64, BLOCK), blk1);
        assert_eq!(read_all(v, d, ino, 2 * BLOCK as u64, BLOCK), craft_block(2));
        v.sync(d).unwrap();
    });
    let r = fsck_of(&mut b);
    assert_eq!((r.errors, r.warnings), (0, 0));
}

/// Truncate a file held in one crafted 2-block extent to `new` bytes;
/// every CRC stays valid and the kept bytes unchanged.
fn truncate_two_block_extent(new: u64) {
    let mut b = fresh(256 * 1024);
    with_vol(&mut b, |v, d| {
        let ino = new_file(v, d, b"f");
        craft_extent(v, d, ino, 2);
        let mut want = craft_block(0).to_vec();
        want.extend_from_slice(&craft_block(1));
        want.truncate(new as usize);
        v.truncate(d, ino, new).unwrap();
        assert_eq!(v.file_size(ino).unwrap(), new);
        assert_crcs_valid(v, d, ino);
        assert_eq!(read_all(v, d, ino, 0, new as usize), want);
        v.sync(d).unwrap();
    });
    let r = fsck_of(&mut b);
    assert_eq!((r.errors, r.warnings), (0, 0));
}

#[test]
fn truncate_two_block_extent_to_1_5_blocks() {
    truncate_two_block_extent(BLOCK as u64 + BLOCK as u64 / 2);
}

#[test]
fn truncate_two_block_extent_to_half_block() {
    truncate_two_block_extent(BLOCK as u64 / 2);
}

#[test]
fn read_corrupt_leaves_buffer_untouched() {
    let mut b = fresh(256 * 1024);
    let (ino, phys) = with_vol(&mut b, |v, d| {
        let ino = new_file(v, d, b"f");
        for k in 0..2u64 {
            let blk = craft_block(k as u32);
            assert_eq!(v.write(d, ino, k * BLOCK as u64, &blk).unwrap(), BLOCK);
        }
        v.sync(d).unwrap();
        (ino, extents(v, ino)[1].1)
    });
    b[phys as usize * BLOCK + 100] ^= 1;
    with_vol(&mut b, |v, d| {
        let mut out = vec![0xAAu8; 2 * BLOCK];
        assert_eq!(v.read(d, ino, 0, &mut out).unwrap_err(), Error::Corrupt);
        assert!(out.iter().all(|&c| c == 0xAA));
        assert_eq!(read_all(v, d, ino, 0, BLOCK), craft_block(0));
    });
}

/// Commit `n` times from a fresh volume with `plant` set, one new file
/// and a write per commit; `fsck` of the result.
fn planted_commits(plant: Plant, n: u32) -> FsckReport {
    let mut b = fresh(64 * BLOCK);
    with_vol(&mut b, |v, d| {
        v.set_plant(plant);
        for i in 0..n {
            let ino = new_file(v, d, format!("f{i}").as_bytes());
            assert_eq!(v.write(d, ino, 0, &payload(i)).unwrap(), 300);
            v.sync(d).unwrap();
        }
    });
    fsck_of(&mut b)
}

#[test]
fn plant_leak_shows_leak_warning() {
    let r = planted_commits(Plant::None, 3);
    assert_eq!((r.errors, r.warnings), (0, 0));
    let r = planted_commits(Plant::Leak, 3);
    assert_eq!(r.errors, 0);
    assert!(r.warnings >= 2, "warnings {}", r.warnings);
}

/// A device with a volatile cache that keeps every write until the next
/// flush. At each superblock write it captures the image a power loss
/// right after that write could leave: the durable blocks plus the super.
struct PendingDisk {
    durable: Vec<u8>,
    pending: Vec<(u32, [u8; BLOCK])>,
    captured: Vec<Vec<u8>>,
}

impl Disk for PendingDisk {
    fn nblocks(&self) -> u32 {
        (self.durable.len() / BLOCK) as u32
    }

    fn read_block(&mut self, bno: u32, buf: &mut [u8; BLOCK]) -> Result<(), Error> {
        let o = bno as usize * BLOCK;
        buf.copy_from_slice(&self.durable[o..o + BLOCK]);
        for (p, data) in &self.pending {
            if *p == bno {
                buf.copy_from_slice(data);
            }
        }
        Ok(())
    }

    fn write_block(&mut self, bno: u32, buf: &[u8; BLOCK]) -> Result<(), Error> {
        if bno < 2 {
            let mut img = self.durable.clone();
            let o = bno as usize * BLOCK;
            img[o..o + BLOCK].copy_from_slice(buf);
            self.captured.push(img);
        }
        self.pending.push((bno, *buf));
        Ok(())
    }

    fn flush(&mut self) -> Result<(), Error> {
        for (bno, data) in self.pending.drain(..) {
            let o = bno as usize * BLOCK;
            self.durable[o..o + BLOCK].copy_from_slice(&data);
        }
        Ok(())
    }
}

/// Run commits with `plant` on a [`PendingDisk`]; for each image captured
/// at a super write, whether it mounts and `fsck` finds it clean.
fn early_super_images(plant: Plant) -> Vec<bool> {
    let mut d = PendingDisk {
        durable: fresh(64 * BLOCK),
        pending: Vec::new(),
        captured: Vec::new(),
    };
    let mut v = Vol::new();
    mount(&mut d, &mut v).unwrap();
    v.set_plant(plant);
    for i in 0..6u32 {
        v.create(
            &mut d,
            ROOT_INO,
            format!("f{i}").as_bytes(),
            InodeKind::Reg,
            0o644,
            None,
        )
        .unwrap();
        let ino = v
            .lookup(&mut d, ROOT_INO, format!("f{i}").as_bytes())
            .unwrap()
            .ino;
        assert_eq!(v.write(&mut d, ino, 0, &payload(i)).unwrap(), 300);
        v.sync(&mut d).unwrap();
    }
    assert_eq!(d.captured.len(), 6);
    d.captured
        .iter_mut()
        .map(|img| {
            let mut md = MemDisk::new(img).unwrap();
            let mut mv = Vol::new();
            mount(&mut md, &mut mv).is_ok() && fsck(&mut md).is_ok_and(|r| r.errors == 0)
        })
        .collect()
}

#[test]
fn plant_early_super_breaks_rebuilt_image() {
    assert!(early_super_images(Plant::None).iter().all(|&ok| ok));
    let early = early_super_images(Plant::EarlySuper);
    assert!(early.iter().any(|&ok| !ok), "{early:?}");
}

/// A zero-length read inside a file returns 0: the last byte it would
/// read is not computed (`off + len - 1` underflows at `len` 0).
#[test]
fn read_into_empty_buffer_is_zero() {
    let mut b = fresh(256 * 1024);
    with_vol(&mut b, |v, d| {
        v.create(d, ROOT_INO, b"big.bin", InodeKind::Reg, 0o644, None)
            .unwrap();
        let n = v.lookup(d, ROOT_INO, b"big.bin").unwrap();
        v.write(d, n.ino, 0, &[9u8; 400]).unwrap();
        assert_eq!(v.read(d, n.ino, 10, &mut []).unwrap(), 0);
    });
}

/// An inline file whose size an image set past the 128 inline bytes, as
/// fsck's `inline` defect plants it: a read of it, and a write that would
/// move its bytes to a block, are `Corrupt` rather than an index past
/// `inline_data`.
#[test]
fn inline_size_past_inline_bytes_is_corrupt() {
    let mut b = base_tree();
    with_vol(&mut b, |v, d| {
        let (_, s) = slot_of(v, d, ROOT_INO, b"g");
        assert!(v.inodes[s].flags & F_INLINE != 0);
        v.inodes[s].size = 200;
        v.dirty = true;
        v.sync(d).unwrap();
    });
    with_vol(&mut b, |v, d| {
        let g = v.lookup(d, ROOT_INO, b"g").unwrap().ino;
        let mut out = [0u8; 256];
        assert_eq!(v.read(d, g, 0, &mut out).unwrap_err(), Error::Corrupt);
        assert_eq!(v.write(d, g, 300, b"x").unwrap_err(), Error::Corrupt);
    });
}

/// A volume whose generation is at its last value fails its next commit
/// with `Corrupt`, where the increment would overflow.
#[test]
fn commit_at_last_generation_is_corrupt() {
    let mut b = fresh(256 * 1024);
    with_vol(&mut b, |v, d| {
        v.generation = u64::MAX;
        v.create(d, ROOT_INO, b"a", InodeKind::Reg, 0o644, None)
            .unwrap();
        assert_eq!(v.sync(d).unwrap_err(), Error::Corrupt);
    });
}

/// A [`MemDisk`] whose block `bad` cannot be read.
struct BadRead<'a> {
    d: MemDisk<'a>,
    bad: u32,
}

impl Disk for BadRead<'_> {
    fn nblocks(&self) -> u32 {
        self.d.nblocks()
    }
    fn read_block(&mut self, bno: u32, buf: &mut [u8; BLOCK]) -> Result<(), Error> {
        if bno == self.bad {
            return Err(Error::Io);
        }
        self.d.read_block(bno, buf)
    }
    fn write_block(&mut self, bno: u32, buf: &[u8; BLOCK]) -> Result<(), Error> {
        self.d.write_block(bno, buf)
    }
    fn flush(&mut self) -> Result<(), Error> {
        self.d.flush()
    }
}

/// A superblock slot that cannot be read fails the mount with `Io`, for
/// either slot: it may hold the newer generation, and mounting the other
/// would roll the volume back.
#[test]
fn unreadable_super_slot_fails_mount() {
    let mut b = fresh(256 * 1024);
    with_vol(&mut b, |v, d| {
        for name in [b"a" as &[u8], b"b"] {
            v.create(d, ROOT_INO, name, InodeKind::Reg, 0o644, None)
                .unwrap();
            v.sync(d).unwrap();
        }
    });
    for bad in 0..2 {
        let mut disk = BadRead {
            d: MemDisk::new(&mut b).unwrap(),
            bad,
        };
        let mut v = Vol::new();
        assert_eq!(
            mount(&mut disk, &mut v).unwrap_err(),
            Error::Io,
            "slot {bad}"
        );
    }
    with_vol(&mut b, |v, d| {
        assert!(v.lookup(d, ROOT_INO, b"b").is_ok());
    });
}
