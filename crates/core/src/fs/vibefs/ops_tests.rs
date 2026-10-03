//! Host tests of `Vol`'s file and name operations: truncate across the
//! inline bytes.

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
