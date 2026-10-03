//! Host tests of `Vol`'s file and name operations: truncate across the
//! inline bytes, inode numbers that run out, the times a lookup reports,
//! and rename between two names of one file.

use super::tests::{base_tree, fresh, fsck_of, new_file, slot_of, with_vol};
use super::*;

/// The `n` bytes of file `ino` from offset 0.
fn read_all(v: &mut Vol, d: &mut MemDisk, ino: u32, n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n];
    assert_eq!(v.read(d, ino, 0, &mut out).unwrap(), n);
    out
}

/// A truncate that grows an inline file past the 128 inline bytes moves
/// them to a block, as a write past byte 128 does, so the file reads its
/// bytes and then zeros, before and after a remount.
#[test]
fn truncate_past_inline_bytes_spills() {
    let mut want = b"abc".to_vec();
    want.resize(256, 0);
    let mut b = fresh(64 * BLOCK);
    let t = with_vol(&mut b, |v, d| {
        let t = new_file(v, d, b"t");
        v.write(d, t, 0, b"abc").unwrap();
        v.truncate(d, t, 256).unwrap();
        let s = v.inode_slot(t).unwrap();
        assert_eq!(v.inodes[s].flags & F_INLINE, 0);
        assert_eq!(read_all(v, d, t, 256), want);
        v.sync(d).unwrap();
        t
    });
    with_vol(&mut b, |v, d| assert_eq!(read_all(v, d, t, 256), want));
    let r = fsck_of(&mut b);
    assert_eq!(r.errors, 0, "{r:?}");
}

/// A file grown past the inline bytes by a truncate and then shrunk
/// stores no inline length past 128, so the volume still mounts and fsck
/// finds it whole.
#[test]
fn truncate_grow_then_shrink_keeps_volume_mountable() {
    let mut b = fresh(64 * BLOCK);
    let u = with_vol(&mut b, |v, d| {
        let u = new_file(v, d, b"u");
        v.truncate(d, u, 200).unwrap();
        v.truncate(d, u, 150).unwrap();
        v.sync(d).unwrap();
        u
    });
    let mut d = MemDisk::new(&mut b).unwrap();
    let mut v = Vol::new();
    mount(&mut d, &mut v).unwrap();
    assert_eq!(read_all(&mut v, &mut d, u, 150), vec![0u8; 150]);
    let r = fsck_of(&mut b);
    assert_eq!(r.errors, 0, "{r:?}");
}

/// An inline file shrunk and grown again reads zeros past the size it
/// was shrunk to, as Linux's truncate has it.
#[test]
fn inline_shrink_then_grow_reads_zeros() {
    let mut b = fresh(64 * BLOCK);
    with_vol(&mut b, |v, d| {
        let t = new_file(v, d, b"t");
        v.write(d, t, 0, &[7u8; 100]).unwrap();
        v.truncate(d, t, 40).unwrap();
        v.truncate(d, t, 100).unwrap();
        let mut want = vec![7u8; 40];
        want.resize(100, 0);
        assert_eq!(read_all(v, d, t, 100), want);
    });
}

/// An inline file whose size an image set past the inline bytes: a
/// truncate of it is `Corrupt` and changes nothing, so no inline length
/// past 128 reaches the disk and the volume still mounts.
#[test]
fn truncate_of_oversized_inline_is_corrupt() {
    let mut b = base_tree();
    with_vol(&mut b, |v, d| {
        let (_, s) = slot_of(v, d, ROOT_INO, b"g");
        v.inodes[s].size = 200;
        v.dirty = true;
        v.sync(d).unwrap();
    });
    with_vol(&mut b, |v, d| {
        let g = v.lookup(d, ROOT_INO, b"g").unwrap().ino;
        for to in [150, 300] {
            assert_eq!(v.truncate(d, g, to).unwrap_err(), Error::Corrupt);
        }
        let s = v.inode_slot(g).unwrap();
        assert_eq!((v.inodes[s].size, v.inodes[s].inline_len), (200, 5));
        v.dirty = true;
        v.sync(d).unwrap();
    });
    with_vol(&mut b, |v, d| {
        assert!(v.lookup(d, ROOT_INO, b"f").is_ok());
    });
}

/// Inode numbers run out rather than repeat: the last one goes to one
/// file, every later create is `NoSpace` and takes nothing, after a
/// remount too, and two files never share a record.
#[test]
fn inode_numbers_run_out_without_reuse() {
    let mut b = fresh(64 * BLOCK);
    let last = u32::MAX - 1;
    with_vol(&mut b, |v, d| {
        v.next_ino = last;
        let a = new_file(v, d, b"a");
        assert_eq!(a, last);
        let used = v.inodes.iter().filter(|i| i.used).count();
        assert_eq!(
            v.create(d, ROOT_INO, b"c", InodeKind::Reg, 0o644, None)
                .unwrap_err(),
            Error::NoSpace
        );
        assert_eq!(v.inodes.iter().filter(|i| i.used).count(), used);
        assert_eq!(v.lookup(d, ROOT_INO, b"c").unwrap_err(), Error::NotFound);
        v.write(d, a, 0, b"AAAA").unwrap();
        v.sync(d).unwrap();
    });
    with_vol(&mut b, |v, d| {
        assert_eq!(
            v.create(d, ROOT_INO, b"c", InodeKind::Dir, 0o755, None)
                .unwrap_err(),
            Error::NoSpace
        );
        let a = v.lookup(d, ROOT_INO, b"a").unwrap().ino;
        assert_eq!(read_all(v, d, a, 4), b"AAAA");
    });
    let r = fsck_of(&mut b);
    assert_eq!(r.errors, 0, "{r:?}");
}

/// A lookup reports the record's three times, as `stat` shows them: a
/// rename stamps the moved file's ctime and leaves its mtime, before and
/// after a remount, and an unset (0) atime reads as the mtime.
#[test]
fn lookup_reports_the_records_times() {
    const T0: u64 = 1_700_000_000;
    let mut b = fresh(64 * BLOCK);
    let check = |v: &mut Vol, d: &mut MemDisk| {
        let g = v.lookup(d, ROOT_INO, b"g").unwrap();
        assert_eq!((g.atime, g.mtime, g.ctime), (T0, T0, T0 + 100));
        assert_eq!(v.attr(g.ino).unwrap().ctime, T0 + 100);
        let r = v.walk(d, b"/").unwrap();
        assert_eq!((r.atime, r.mtime, r.ctime), (T0 + 100, T0 + 100, T0 + 100));
    };
    with_vol(&mut b, |v, d| {
        v.now = T0;
        new_file(v, d, b"f");
        v.now = T0 + 100;
        v.rename(d, ROOT_INO, b"f", ROOT_INO, b"g").unwrap();
        check(v, d);
        v.sync(d).unwrap();
    });
    with_vol(&mut b, check);
}

/// A rename between two names of one file, as an image with a hard link
/// holds them, changes nothing: both names stay, with the link count 2,
/// as rename(2) has it.
#[test]
fn rename_between_links_of_one_file_keeps_both() {
    let mut b = fresh(64 * BLOCK);
    let a = with_vol(&mut b, |v, d| {
        let a = new_file(v, d, b"a");
        let (e, s) = slot_of(v, d, ROOT_INO, b"a");
        let mut link = v.dents[e];
        link.nlen = 1;
        link.name = [0; MAX_NAME];
        link.name[0] = b'b';
        let de = v.alloc_dent().unwrap();
        v.dents[de] = link;
        v.inodes[s].nlink = 2;
        v.dirty = true;
        v.sync(d).unwrap();
        a
    });
    let r = fsck_of(&mut b);
    assert_eq!(r.errors, 0, "the planted link: {r:?}");
    with_vol(&mut b, |v, d| {
        v.rename(d, ROOT_INO, b"a", ROOT_INO, b"b").unwrap();
        for n in [b"a", b"b"] {
            assert_eq!(v.lookup(d, ROOT_INO, n).unwrap().ino, a);
        }
        assert_eq!(v.inodes[v.inode_slot(a).unwrap()].nlink, 2);
        v.sync(d).unwrap();
    });
    let r = fsck_of(&mut b);
    assert_eq!(r.errors, 0, "{r:?}");
}
