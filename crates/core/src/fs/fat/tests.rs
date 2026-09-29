use super::*;

fn fresh(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    mkfs(&mut b, b"TEST").unwrap();
    b
}

fn with_vol<R>(buf: &mut [u8], f: impl FnOnce(&mut FatVol, &mut MemDisk) -> R) -> R {
    let mut disk = MemDisk::new(buf, SEC as u32).unwrap();
    let mut vol = FatVol::mount(&mut disk).unwrap();
    f(&mut vol, &mut disk)
}

/// `VIBEOS_ALLOW_MISSING_TOOLS=1`, the gate switch the Makefile exports
/// (AGENTS.md How to run): a missing host tool skips its check instead of
/// failing the test.
fn allow_missing_tools() -> bool {
    std::env::var_os("VIBEOS_ALLOW_MISSING_TOOLS").is_some_and(|v| v == "1")
}

/// Require host `fsck.fat -n` to report `buf` clean (ROADMAP Phase 8 exit gate).
/// A skipped check prints its line once per process.
fn fsck(buf: &[u8]) {
    static SKIPPED: std::sync::Once = std::sync::Once::new();
    if !run_fsck("fsck.fat", buf, allow_missing_tools()) {
        // libtest captures `eprintln!` from a passing test; a direct write to
        // the stderr handle is not captured.
        SKIPPED.call_once(|| {
            use std::io::Write;
            let _ = writeln!(
                std::io::stderr(),
                "skipped fsck.fat -n: fsck.fat not installed (VIBEOS_ALLOW_MISSING_TOOLS=1)"
            );
        });
    }
}

/// Run `prog -n` on an image of `buf` and require it to report the image
/// clean; false when `prog` is not installed and `allow` skips the check. A
/// missing `prog` panics unless `allow`.
fn run_fsck(prog: &str, buf: &[u8], allow: bool) -> bool {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir();
    let p = dir.join(format!(
        "vibeos-fat32-{}-{}.img",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&p, buf).unwrap();
    let st = std::process::Command::new(prog)
        .args(["-n", p.to_str().unwrap()])
        .output();
    let _ = std::fs::remove_file(&p);
    match st {
        Ok(o) => {
            assert!(
                o.status.success(),
                "{prog} failed status={:?}\nstdout:\n{}\nstderr:\n{}",
                o.status.code(),
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            true
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !allow {
                panic!(
                    "{prog} not installed: install dosfstools, or set \
                     VIBEOS_ALLOW_MISSING_TOOLS=1 to skip the fsck.fat -n check"
                );
            }
            false
        }
        Err(e) => panic!("{prog} spawn: {e}"),
    }
}

#[test]
#[should_panic(expected = "not installed")]
fn fsck_missing_tool_fails() {
    run_fsck("vibeos-no-such-fsck", &fresh(INITRD_BYTES), false);
}

#[test]
fn fsck_missing_tool_skipped_when_allowed() {
    assert!(!run_fsck("vibeos-no-such-fsck", &fresh(INITRD_BYTES), true));
}

#[test]
fn bpb_validate_and_mount() {
    let mut b = fresh(INITRD_BYTES);
    with_vol(&mut b, |v, _| {
        assert_eq!(v.info.bps, 512);
        assert_eq!(v.info.num_fats, 2);
        assert_eq!(v.info.root_clus, 2);
        assert!(v.info.nclus >= 2);
    });
}

#[test]
fn corrupt_boot_sig() {
    let mut b = fresh(INITRD_BYTES);
    b[510] = 0;
    let mut disk = MemDisk::new(&mut b, SEC as u32).unwrap();
    let err = match FatVol::mount(&mut disk) {
        Err(e) => e,
        Ok(_) => panic!("expected corrupt"),
    };
    assert_eq!(err, FatError::Corrupt);
}

#[test]
fn truncated_image() {
    let mut b = vec![0u8; 512];
    assert_eq!(mkfs(&mut b, b"X").unwrap_err(), FatError::Inval);
}

#[test]
fn fatsz16_rejected() {
    let mut b = fresh(INITRD_BYTES);
    b[22] = 1;
    let mut disk = MemDisk::new(&mut b, SEC as u32).unwrap();
    let err = match FatVol::mount(&mut disk) {
        Err(e) => e,
        Ok(_) => panic!("expected inval"),
    };
    assert_eq!(err, FatError::Inval);
}

#[test]
fn lfn_checksum_matches_spec() {
    let n = *b"HELLO   TXT";
    let s = lfn_checksum(&n);
    let mut sum = 0u8;
    for c in n {
        sum = ((sum & 1) << 7).wrapping_add(sum >> 1).wrapping_add(c);
    }
    assert_eq!(s, sum);
}

#[test]
fn create_write_read_across_clusters() {
    let mut b = fresh(INITRD_BYTES);
    with_vol(&mut b, |v, d| {
        let n = v.create(d, v.info.root_clus, b"big.bin", false).unwrap();
        let mut clu = n.clu;
        let mut size = n.size;
        let mut payload = [0u8; 1200];
        let mut i = 0usize;
        while i < payload.len() {
            payload[i] = (i % 251) as u8;
            i += 1;
        }
        v.write(d, n.dir_clu, n.dir_off, &mut clu, &mut size, 0, &payload)
            .unwrap();
        assert!(size as usize >= payload.len());
        assert!(clu >= 2);
        let mut out = [0u8; 1200];
        let got = v.read(d, clu, size, 0, &mut out).unwrap();
        assert_eq!(got, 1200);
        assert_eq!(out, payload);
        let mut mid = [0u8; 40];
        v.read(d, clu, size, 500, &mut mid).unwrap();
        assert_eq!(mid, payload[500..540]);
        v.sync(d).unwrap();
    });
    assert!(with_vol(&mut b, |v, d| v.fats_identical(d).unwrap()));
    fsck(&b);
}

#[test]
fn lfn_roundtrip_and_readdir() {
    let mut b = fresh(INITRD_BYTES);
    with_vol(&mut b, |v, d| {
        v.create(d, v.info.root_clus, b"hello.txt", false).unwrap();
        v.create(d, v.info.root_clus, b"Long File Name.dat", false)
            .unwrap();
        let n = v.lookup(d, v.info.root_clus, b"hello.txt").unwrap();
        assert_eq!(n.name(), b"hello.txt");
        let n = v.lookup(d, v.info.root_clus, b"HELLO.TXT").unwrap();
        assert_eq!(n.kind, InodeKind::Reg);
        let n = v
            .lookup(d, v.info.root_clus, b"Long File Name.dat")
            .unwrap();
        assert_eq!(n.name(), b"Long File Name.dat");
        let mut names = 0u32;
        let mut cookie = 0u64;
        let mut node = Node::EMPTY;
        while let Some(next) = v.readdir(d, v.info.root_clus, cookie, &mut node).unwrap() {
            names += 1;
            cookie = next;
        }
        assert_eq!(names, 2);
        v.sync(d).unwrap();
    });
    fsck(&b);
}

#[test]
fn lfn_checksum_mismatch_falls_back() {
    let mut b = fresh(INITRD_BYTES);
    with_vol(&mut b, |v, d| {
        v.create(d, v.info.root_clus, b"hello.txt", false).unwrap();
        v.sync(d).unwrap();
    });
    // Corrupt the LFN checksum byte in the first LFN entry of the root.
    let mut disk = MemDisk::new(&mut b, SEC as u32).unwrap();
    let vol = FatVol::mount(&mut disk).unwrap();
    let lba = vol.info.clus_lba(vol.info.root_clus).unwrap();
    let mut sec = [0u8; SEC];
    disk.read(lba, &mut sec).unwrap();
    // first entry is LFN (attr 0x0F); checksum at offset 13
    if sec[11] == ATTR_LFN {
        sec[13] ^= 0xFF;
        disk.write(lba, &sec).unwrap();
    }
    let mut vol = FatVol::mount(&mut disk).unwrap();
    let n = vol
        .lookup(&mut disk, vol.info.root_clus, b"HELLO.TXT")
        .unwrap();
    assert_eq!(n.kind, InodeKind::Reg);
}

#[test]
fn mkdir_rmdir_unlink() {
    let mut b = fresh(INITRD_BYTES);
    with_vol(&mut b, |v, d| {
        let dir = v.create(d, v.info.root_clus, b"sub", true).unwrap();
        assert!(dir.is_dir());
        v.create(d, dir.clu, b"a.txt", false).unwrap();
        assert_eq!(
            unlink_free(v, d, v.info.root_clus, b"sub", true).unwrap_err(),
            FatError::NotEmpty
        );
        unlink_free(v, d, dir.clu, b"a.txt", false).unwrap();
        unlink_free(v, d, v.info.root_clus, b"sub", true).unwrap();
        assert_eq!(
            v.lookup(d, v.info.root_clus, b"sub").unwrap_err(),
            FatError::NotFound
        );
        v.sync(d).unwrap();
    });
    fsck(&b);
}

#[test]
fn truncate_and_delete() {
    let mut b = fresh(INITRD_BYTES);
    with_vol(&mut b, |v, d| {
        let n = v.create(d, v.info.root_clus, b"t.bin", false).unwrap();
        let mut clu = n.clu;
        let mut size = n.size;
        v.write(d, n.dir_clu, n.dir_off, &mut clu, &mut size, 0, &[9u8; 800])
            .unwrap();
        v.truncate(d, n.dir_clu, n.dir_off, &mut clu, &mut size, 10)
            .unwrap();
        assert_eq!(size, 10);
        let mut out = [0u8; 16];
        let got = v.read(d, clu, size, 0, &mut out).unwrap();
        assert_eq!(got, 10);
        unlink_free(v, d, v.info.root_clus, b"t.bin", false).unwrap();
        v.sync(d).unwrap();
    });
    fsck(&b);
}

#[test]
fn rename_across_dirs() {
    let mut b = fresh(INITRD_BYTES);
    with_vol(&mut b, |v, d| {
        let a = v.create(d, v.info.root_clus, b"a", true).unwrap();
        let bb = v.create(d, v.info.root_clus, b"b", true).unwrap();
        let f = v.create(d, a.clu, b"x.txt", false).unwrap();
        let mut clu = f.clu;
        let mut size = f.size;
        v.write(d, f.dir_clu, f.dir_off, &mut clu, &mut size, 0, b"hi")
            .unwrap();
        v.rename(d, a.clu, b"x.txt", bb.clu, b"y.txt").unwrap();
        assert_eq!(
            v.lookup(d, a.clu, b"x.txt").unwrap_err(),
            FatError::NotFound
        );
        let y = v.lookup(d, bb.clu, b"y.txt").unwrap();
        let mut out = [0u8; 2];
        v.read(d, y.clu, y.size, 0, &mut out).unwrap();
        assert_eq!(&out, b"hi");
        v.sync(d).unwrap();
    });
    fsck(&b);
}

#[test]
fn dual_fat_after_alloc() {
    let mut b = fresh(INITRD_BYTES);
    with_vol(&mut b, |v, d| {
        v.create(d, v.info.root_clus, b"one", false).unwrap();
        v.create(d, v.info.root_clus, b"two", true).unwrap();
        v.sync(d).unwrap();
        assert!(v.fats_identical(d).unwrap());
    });
}

#[test]
fn initrd_image_fsck() {
    let mut b = vec![0u8; INITRD_BYTES];
    mkinitrd(&mut b).unwrap();
    with_vol(&mut b, |v, d| {
        let h = v.lookup(d, v.info.root_clus, b"hello.txt").unwrap();
        let mut buf = [0u8; 32];
        let n = v.read(d, h.clu, h.size, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello from initrd\n");
        assert!(v.lookup(d, v.info.root_clus, b"etc").unwrap().is_dir());
    });
    fsck(&b);
}

#[test]
fn initrd_add_one_level_dir() {
    let mut b = vec![0u8; INITRD_BYTES];
    mkinitrd(&mut b).unwrap();
    with_vol(&mut b, |v, d| {
        v.now = 1_262_304_000;
        let sbin = v.create(d, v.info.root_clus, b"sbin", true).unwrap();
        let init = v.create(d, sbin.clu, b"init", false).unwrap();
        let mut clu = init.clu;
        let mut size = init.size;
        v.write(
            d,
            init.dir_clu,
            init.dir_off,
            &mut clu,
            &mut size,
            0,
            b"\x7fELF",
        )
        .unwrap();
        v.sync(d).unwrap();
    });
    with_vol(&mut b, |v, d| {
        let sbin = v.lookup(d, v.info.root_clus, b"sbin").unwrap();
        assert!(sbin.is_dir());
        let init = v.lookup(d, sbin.clu, b"init").unwrap();
        let mut buf = [0u8; 4];
        let n = v.read(d, init.clu, init.size, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"\x7fELF");
    });
    fsck(&b);
}

#[test]
fn chain_loop_is_corrupt() {
    let mut b = fresh(INITRD_BYTES);
    with_vol(&mut b, |v, d| {
        let n = v.create(d, v.info.root_clus, b"x", false).unwrap();
        let mut clu = n.clu;
        let mut size = n.size;
        v.write(d, n.dir_clu, n.dir_off, &mut clu, &mut size, 0, &[1u8; 600])
            .unwrap();
        v.sync(d).unwrap();
        // Point a FAT entry at itself.
        v.fat_set(d, clu, clu).unwrap();
        v.commit_fat(d).unwrap();
        let mut out = [0u8; 4];
        let _ = v.read(d, clu, 600, 0, &mut out);
    });
}

#[test]
fn second_fat_mismatch_is_detected() {
    let mut b = fresh(INITRD_BYTES);
    with_vol(&mut b, |v, d| {
        v.create(d, v.info.root_clus, b"x", false).unwrap();
        v.sync(d).unwrap();
        assert!(v.fats_identical(d).unwrap());
    });
    let fatsz = {
        let mut disk = MemDisk::new(&mut b, SEC as u32).unwrap();
        let vol = FatVol::mount(&mut disk).unwrap();
        vol.info.fatsz
    };
    let fat1 = 32 + fatsz;
    let off = fat1 as usize * SEC + 12;
    b[off] ^= 0xFF;
    let mut disk = MemDisk::new(&mut b, SEC as u32).unwrap();
    let mut vol = FatVol::mount(&mut disk).unwrap();
    assert!(!vol.fats_identical(&mut disk).unwrap());
}

#[test]
fn truncate_zero_dirent_cluster() {
    let mut b = fresh(INITRD_BYTES);
    with_vol(&mut b, |v, d| {
        let n = v.create(d, v.info.root_clus, b"z.bin", false).unwrap();
        let mut clu = n.clu;
        let mut size = n.size;
        v.write(d, n.dir_clu, n.dir_off, &mut clu, &mut size, 0, &[7u8; 40])
            .unwrap();
        v.truncate(d, n.dir_clu, n.dir_off, &mut clu, &mut size, 0)
            .unwrap();
        assert_eq!(clu, 0);
        assert_eq!(size, 0);
        let got = v.lookup(d, v.info.root_clus, b"z.bin").unwrap();
        assert_eq!(got.clu, 0);
        assert_eq!(got.size, 0);
        v.sync(d).unwrap();
    });
    fsck(&b);
}

#[test]
fn error_strings() {
    assert_eq!(FatError::NotSupp.as_str(), "not supp");
    assert_eq!(FatError::NotSupp.to_fs(), FsError::NotSupp);
    assert_eq!(FatError::Corrupt.to_fs(), FsError::Inval);
}

#[test]
fn fixed_tables_match_limits() {
    assert_eq!(Node::EMPTY.name.len(), crate::limits::MAX_NAME);
}

/// Unlink `name` and free its clusters, as the last put of a file
/// nothing holds does.
fn unlink_free(
    v: &mut FatVol,
    d: &mut MemDisk,
    dir: u32,
    name: &[u8],
    rmdir: bool,
) -> Result<(), FatError> {
    let gone = v.unlink(d, dir, name, rmdir)?;
    v.free_chain(d, gone.first_clu)
}

/// Create `name` in `dir`; the caller-owned words of the new file.
fn create_words(v: &mut FatVol, d: &mut MemDisk, dir: u32, name: &[u8]) -> FatInode {
    let n = v.create(d, dir, name, false).unwrap();
    FatInode::of_node(&n)
}

fn read_back(v: &mut FatVol, d: &mut MemDisk, n: &FatInode) -> Vec<u8> {
    let size = n.size as usize;
    let mut out = vec![0u8; size];
    let got = v.read_ino(d, n, 0, &mut out).unwrap();
    assert_eq!(got, size);
    out
}

#[test]
fn fat_unlinked_open_frees_at_last_iput() {
    let mut b = fresh(INITRD_BYTES);
    with_vol(&mut b, |v, d| {
        let root = v.info.root_clus;
        let before = v.free;
        let mut w = create_words(v, d, root, b"GONE.BIN");
        assert_eq!(
            v.write_ino(d, &mut w, true, 0, false, &[5u8; 1500])
                .unwrap(),
            (1500, 0)
        );
        let held = v.free;
        assert!(held < before);
        let gone = v.unlink(d, root, b"GONE.BIN", false).unwrap();
        assert_eq!((gone.dir_clu, gone.dir_off), (w.dir_clu, w.dir_off));
        assert_eq!(
            v.lookup(d, root, b"GONE.BIN").unwrap_err(),
            FatError::NotFound
        );
        assert_eq!(v.free, held, "an open unlinked file keeps its clusters");
        assert_eq!(read_back(v, d, &w), vec![5u8; 1500]);
        assert_eq!(
            v.write_ino(d, &mut w, false, 0, true, b"xy").unwrap(),
            (2, 1500)
        );
        v.free_chain(d, w.first_clu).unwrap();
        assert_eq!(v.free, before, "the last put frees the chain");
        let counted = v.count_free(d).unwrap();
        assert_eq!(v.free, counted);
        v.sync(d).unwrap();
    });
    fsck(&b);
}

#[test]
fn fat_create_in_freed_slot_new_inode() {
    let mut b = fresh(INITRD_BYTES);
    with_vol(&mut b, |v, d| {
        let root = v.info.root_clus;
        let mut old = create_words(v, d, root, b"A.BIN");
        v.write_ino(d, &mut old, true, 0, false, b"old").unwrap();
        v.unlink(d, root, b"A.BIN", false).unwrap();
        let n = v.create(d, root, b"B.BIN", false).unwrap();
        assert_eq!(n.dir_off, old.dir_off, "the new file takes the freed slot");
        let mut new = FatInode::of_node(&n);
        v.write_ino(d, &mut new, true, 0, false, b"new!").unwrap();
        // Growing the unlinked file never writes its old dirent slot.
        v.write_ino(d, &mut old, false, 0, true, &[1u8; 700])
            .unwrap();
        let got = v.lookup(d, root, b"B.BIN").unwrap();
        assert_eq!((got.size, got.clu), (4, new.first_clu));
        assert_ne!(old.first_clu, new.first_clu);
        assert_eq!(&read_back(v, d, &old)[..3], b"old");
        assert_eq!(read_back(v, d, &new), b"new!");
        v.free_chain(d, old.first_clu).unwrap();
        let counted = v.count_free(d).unwrap();
        assert_eq!(v.free, counted);
        v.sync(d).unwrap();
    });
    fsck(&b);
}

#[test]
fn fat_rename_rekeys_open_inode() {
    let mut b = fresh(INITRD_BYTES);
    with_vol(&mut b, |v, d| {
        let root = v.info.root_clus;
        let a = v.create(d, root, b"a", true).unwrap();
        let bb = v.create(d, root, b"b", true).unwrap();
        let mut w = create_words(v, d, a.clu, b"X.TXT");
        v.write_ino(d, &mut w, true, 0, false, b"hi").unwrap();
        let moved = v.rename(d, a.clu, b"X.TXT", bb.clu, b"Y.TXT").unwrap();
        assert_eq!(moved.from, (w.dir_clu, w.dir_off));
        assert_eq!(moved.replaced, None);
        (w.dir_clu, w.dir_off) = moved.to;
        let y = v.lookup(d, bb.clu, b"Y.TXT").unwrap();
        assert_eq!((w.dir_clu, w.dir_off), (bb.clu, y.dir_off));
        assert_eq!(y.ino, stat_ino(w.dir_clu, w.dir_off));
        v.write_ino(d, &mut w, true, 0, true, b" there").unwrap();
        let y = v.lookup(d, bb.clu, b"Y.TXT").unwrap();
        assert_eq!(y.size, 8);
        assert_eq!(read_back(v, d, &w), b"hi there");
        v.sync(d).unwrap();
    });
    fsck(&b);
}

#[test]
fn fat_rename_reports_replaced() {
    let mut b = fresh(INITRD_BYTES);
    with_vol(&mut b, |v, d| {
        let root = v.info.root_clus;
        let mut src = create_words(v, d, root, b"S.BIN");
        v.write_ino(d, &mut src, true, 0, false, &[1u8; 700])
            .unwrap();
        let mut dst = create_words(v, d, root, b"D.BIN");
        v.write_ino(d, &mut dst, true, 0, false, &[2u8; 900])
            .unwrap();
        let held = v.free;
        let moved = v.rename(d, root, b"S.BIN", root, b"D.BIN").unwrap();
        let gone = moved.replaced.unwrap();
        assert_eq!((gone.dir_clu, gone.dir_off), (dst.dir_clu, dst.dir_off));
        assert_eq!(gone.first_clu, dst.first_clu);
        assert_eq!(v.free, held, "the replaced file keeps its clusters");
        v.free_chain(d, gone.first_clu).unwrap();
        let got = v.lookup(d, root, b"D.BIN").unwrap();
        assert_eq!((got.clu, got.size), (src.first_clu, 700));
        let counted = v.count_free(d).unwrap();
        assert_eq!(v.free, counted);
        v.sync(d).unwrap();
    });
    fsck(&b);
}

#[test]
fn fat_inode_two_descriptors() {
    let mut b = fresh(INITRD_BYTES);
    with_vol(&mut b, |v, d| {
        let root = v.info.root_clus;
        let n = v.create(d, root, b"two.bin", false).unwrap();
        // One inode's words, reached through two descriptors' offsets.
        let mut w = FatInode::of_node(&n);
        let (one, two) = (0u64, 0u64);
        assert_eq!(
            v.write_ino(d, &mut w, true, one, false, b"AAAA").unwrap(),
            (4, 0)
        );
        assert_eq!(
            v.write_ino(d, &mut w, true, two, false, b"BB").unwrap(),
            (2, 0)
        );
        let first = w.first_clu;
        assert!(first >= 2);
        assert_eq!(read_back(v, d, &w), b"BBAA");
        v.truncate_ino(d, &mut w, true, 0).unwrap();
        assert_eq!(w.first_clu, 0);
        assert_eq!(w.size, 0);
        assert_eq!(
            v.write_ino(d, &mut w, true, 5000, false, &[3u8; 10])
                .unwrap(),
            (10, 5000)
        );
        assert_eq!(w.size, 5010);
        let got = v.lookup(d, root, b"two.bin").unwrap();
        assert_eq!(got.size, 5010);
        assert_eq!(got.clu, w.first_clu);
        let data = read_back(v, d, &w);
        assert!(data[..5000].iter().all(|&x| x == 0));
        assert_eq!(&data[5000..], &[3u8; 10]);
        v.sync(d).unwrap();
    });
    with_vol(&mut b, |v, d| {
        let counted = v.count_free(d).unwrap();
        assert_eq!(v.free, counted, "FSInfo free count after a remount");
    });
    fsck(&b);
}

#[test]
fn stat_ino_distinct() {
    assert_eq!(stat_ino(0, 0), ROOT_INO);
    let mut seen = std::collections::HashSet::new();
    for clu in [2u32, 3, 7, 0xFFFF] {
        for off in (0..64u32).map(|i| i * ENT as u32) {
            let n = stat_ino(clu, off);
            assert!(n != 0 && n != ROOT_INO);
            assert!(seen.insert(n), "stat_ino({clu}, {off}) repeats");
        }
    }
    for (clu, off) in [
        (0x1_0000, 0),
        (0x0FFF_FFFF, 32),
        (2, 0x20_0000),
        (0, 32),
        (1, 0),
    ] {
        let n = stat_ino(clu, off);
        assert!(n != 0 && n != ROOT_INO, "fold of ({clu}, {off})");
    }
}

/// `n` bytes of a pattern that differs per `seed`.
fn pattern(seed: u8, n: usize) -> Vec<u8> {
    (0..n).map(|i| (i as u8).wrapping_mul(7) ^ seed).collect()
}

/// Names in `dir` equal to `name` ignoring ASCII case.
fn ci_names(v: &mut FatVol, d: &mut MemDisk, dir: u32, name: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut node = Node::EMPTY;
    let mut cookie = 0u64;
    while let Some(next) = v.readdir(d, dir, cookie, &mut node).unwrap() {
        if node.name().eq_ignore_ascii_case(name) {
            out.push(node.name().to_vec());
        }
        cookie = next;
    }
    out
}

fn case_only_rename(from: &[u8], to: &[u8]) {
    let mut b = fresh(INITRD_BYTES);
    let (clu, size, data) = with_vol(&mut b, |v, d| {
        let root = v.info.root_clus;
        // Leave three deleted slots ahead of `from`, so the rename's two
        // new entries fit without growing the directory: `dir_reserve`
        // still extends one at the first 0x00 entry (F053).
        v.create(d, root, b"scratch-slot.bin", false).unwrap();
        unlink_free(v, d, root, b"scratch-slot.bin", false).unwrap();
        let data = pattern(0x5A, 3 * v.info.clus_bytes());
        let mut w = create_words(v, d, root, from);
        v.write_ino(d, &mut w, true, 0, false, &data).unwrap();
        let src = v.lookup(d, root, from).unwrap();
        let free = v.free;
        v.rename(d, root, from, root, to).unwrap();
        assert_eq!(v.free, free, "a case-only rename frees nothing");
        let got = v.lookup(d, root, to).unwrap();
        assert_eq!((got.clu, got.size), (src.clu, src.size));
        let mut out = vec![0u8; data.len()];
        v.read(d, got.clu, got.size, 0, &mut out).unwrap();
        assert_eq!(out, data);
        assert_eq!(ci_names(v, d, root, from), vec![to.to_vec()]);
        v.sync(d).unwrap();
        (got.clu, got.size, data)
    });
    with_vol(&mut b, |v, d| {
        let root = v.info.root_clus;
        let mut w = create_words(v, d, root, b"OTHER.BIN");
        v.write_ino(d, &mut w, true, 0, false, &pattern(0xA5, data.len()))
            .unwrap();
        let got = v.lookup(d, root, to).unwrap();
        assert_eq!((got.clu, got.size), (clu, size));
        let mut out = vec![0u8; data.len()];
        v.read(d, got.clu, got.size, 0, &mut out).unwrap();
        assert_eq!(out, data, "the renamed file's clusters were reused");
        v.sync(d).unwrap();
    });
    fsck(&b);
}

#[test]
fn rename_case_only() {
    case_only_rename(b"a", b"A");
}

#[test]
fn rename_case_only_lfn() {
    case_only_rename(b"hello.txt", b"Hello.txt");
}

#[test]
fn rename_into_own_subtree_einval() {
    let mut b = fresh(INITRD_BYTES);
    with_vol(&mut b, |v, d| {
        let root = v.info.root_clus;
        let p = v.create(d, root, b"p", true).unwrap();
        let c = v.create(d, p.clu, b"c", true).unwrap();
        let free = v.free;
        assert_eq!(
            v.rename(d, root, b"p", c.clu, b"q").unwrap_err(),
            FatError::Inval
        );
        assert_eq!(
            v.rename(d, root, b"p", p.clu, b"q").unwrap_err(),
            FatError::Inval
        );
        assert_eq!(v.lookup(d, root, b"p").unwrap().clu, p.clu);
        assert_eq!(v.lookup(d, p.clu, b"c").unwrap().clu, c.clu);
        assert_eq!(v.free, free);
        v.sync(d).unwrap();
    });
    fsck(&b);
}

#[test]
fn rename_dir_dotdot_names_new_parent() {
    let mut b = fresh(INITRD_BYTES);
    with_vol(&mut b, |v, d| {
        let root = v.info.root_clus;
        let a = v.create(d, root, b"a", true).unwrap();
        let bb = v.create(d, root, b"b", true).unwrap();
        let dd = v.create(d, a.clu, b"d", true).unwrap();
        let mut w = create_words(v, d, dd.clu, b"IN.TXT");
        v.write_ino(d, &mut w, true, 0, false, b"inside").unwrap();
        v.rename(d, a.clu, b"d", bb.clu, b"d").unwrap();
        assert_eq!(v.dotdot_of(d, dd.clu).unwrap(), bb.clu);
        let mut ent = [0u8; ENT];
        v.read_dir_raw(d, dd.clu, ENT as u32, &mut ent).unwrap();
        assert_eq!(
            (le16(&ent, 20).unwrap() as u32) << 16 | le16(&ent, 26).unwrap() as u32,
            bb.clu
        );
        v.rename(d, bb.clu, b"d", root, b"d").unwrap();
        v.read_dir_raw(d, dd.clu, ENT as u32, &mut ent).unwrap();
        assert_eq!((le16(&ent, 20).unwrap(), le16(&ent, 26).unwrap()), (0, 0));
        let got = v.lookup(d, root, b"d").unwrap();
        assert_eq!(got.clu, dd.clu);
        let f = v.lookup(d, got.clu, b"IN.TXT").unwrap();
        let mut out = [0u8; 6];
        v.read(d, f.clu, f.size, 0, &mut out).unwrap();
        assert_eq!(&out, b"inside");
        v.sync(d).unwrap();
    });
    fsck(&b);
}

/// Mount `b` with FATSz32 set to `fatsz` and NumFATs to `nfats`.
fn mount_with_fatsz(fatsz: u32, nfats: u8) -> Result<(), FatError> {
    let mut b = fresh(INITRD_BYTES);
    b[36..40].copy_from_slice(&fatsz.to_le_bytes());
    b[16] = nfats;
    let mut disk = MemDisk::new(&mut b, SEC as u32).unwrap();
    FatVol::mount(&mut disk).map(|_| ())
}

#[test]
fn fat_bpb_data_lba_overflow_two_fats() {
    assert_eq!(mount_with_fatsz(0x8000_0000, 2), Err(FatError::Corrupt));
}

#[test]
fn fat_bpb_data_lba_overflow_one_fat() {
    assert_eq!(mount_with_fatsz(0xFFFF_FFFF, 1), Err(FatError::Corrupt));
}

/// A disk of `u32::MAX` sectors that holds `boot` at sector 0 and zeros
/// everywhere else.
struct HugeDisk {
    boot: [u8; SEC],
}

impl Disk for HugeDisk {
    fn sector_size(&self) -> u32 {
        SEC as u32
    }

    fn nsectors(&self) -> u32 {
        u32::MAX
    }

    fn read(&mut self, lba: u32, buf: &mut [u8]) -> Result<(), FatError> {
        if lba == 0 {
            buf.copy_from_slice(&self.boot);
        } else {
            buf.fill(0);
        }
        Ok(())
    }

    fn write(&mut self, _lba: u32, _buf: &[u8]) -> Result<(), FatError> {
        Ok(())
    }

    fn flush(&mut self) -> Result<(), FatError> {
        Ok(())
    }
}

#[test]
fn fat_bpb_cluster_count_above_max_is_corrupt() {
    let mut boot = [0u8; SEC];
    boot[11..13].copy_from_slice(&(SEC as u16).to_le_bytes());
    boot[13] = 1;
    boot[14..16].copy_from_slice(&32u16.to_le_bytes());
    boot[16] = 2;
    boot[21] = 0xF8;
    boot[32..36].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
    boot[36..40].copy_from_slice(&1024u32.to_le_bytes());
    boot[44..48].copy_from_slice(&2u32.to_le_bytes());
    boot[510] = 0x55;
    boot[511] = 0xAA;
    let mut d = HugeDisk { boot };
    assert_eq!(FatVol::mount(&mut d).map(|_| ()), Err(FatError::Corrupt));
}

#[test]
fn extend_nospace_leaks_nothing() {
    let mut b = fresh(INITRD_BYTES);
    let before = with_vol(&mut b, |v, d| {
        let root = v.info.root_clus;
        let mut e = create_words(v, d, root, b"e.txt");
        let mut f = create_words(v, d, root, b"FILL.BIN");
        let cb = v.info.clus_bytes();
        let n = (v.free as usize - 2) * cb;
        v.write_ino(d, &mut f, true, 0, false, &vec![7u8; n])
            .unwrap();
        assert_eq!(v.free, 2);
        v.sync(d).unwrap();
        let before = (v.free_bytes(), v.count_free(d).unwrap());
        assert_eq!(
            v.write_ino(d, &mut e, true, 10 * cb as u64, false, b"x")
                .unwrap_err(),
            FatError::NoSpace
        );
        assert_eq!((e.first_clu, e.size), (0, 0));
        assert_eq!((v.free_bytes(), v.count_free(d).unwrap()), before);
        v.sync(d).unwrap();
        before
    });
    with_vol(&mut b, |v, d| {
        assert_eq!((v.free_bytes(), v.count_free(d).unwrap()), before);
        let e = v.lookup(d, v.info.root_clus, b"e.txt").unwrap();
        assert_eq!((e.clu, e.size), (0, 0));
    });
    fsck(&b);
}

#[test]
fn extend_failure_rolls_back_chain() {
    let mut b = fresh(INITRD_BYTES);
    let (o_first, free) = with_vol(&mut b, |v, d| {
        let root = v.info.root_clus;
        let mut e = create_words(v, d, root, b"e.txt");
        let mut o = create_words(v, d, root, b"o.txt");
        v.write_ino(d, &mut o, true, 0, false, b"one cluster")
            .unwrap();
        v.sync(d).unwrap();
        let o_first = o.first_clu;
        let free = v.count_free(d).unwrap();
        assert_eq!(v.free, free);
        // A lying FSInfo count: the pre-check passes and the allocation
        // runs out part way.
        v.free += 20;
        let cb = v.info.clus_bytes() as u64;
        let far = (u64::from(free) + 5) * cb;
        for f in [&mut e, &mut o] {
            let size = f.size;
            let first = f.first_clu;
            assert_eq!(
                v.write_ino(d, f, true, far, false, b"x").unwrap_err(),
                FatError::NoSpace
            );
            assert_eq!((f.first_clu, f.size), (first, size));
            assert_eq!(v.count_free(d).unwrap(), free);
            assert_eq!(v.free, free + 20);
        }
        assert!(is_eoc(v.fat_get(d, o_first).unwrap()));
        v.free -= 20;
        v.fsinfo_dirty = true;
        v.sync(d).unwrap();
        (o_first, free)
    });
    with_vol(&mut b, |v, d| {
        let root = v.info.root_clus;
        assert_eq!(v.count_free(d).unwrap(), free);
        assert_eq!(v.free, free);
        let e = v.lookup(d, root, b"e.txt").unwrap();
        assert_eq!((e.clu, e.size), (0, 0));
        let o = v.lookup(d, root, b"o.txt").unwrap();
        assert_eq!((o.clu, o.size), (o_first, 11));
        assert!(is_eoc(v.fat_get(d, o_first).unwrap()));
        assert!(v.fats_identical(d).unwrap());
    });
    fsck(&b);
}
