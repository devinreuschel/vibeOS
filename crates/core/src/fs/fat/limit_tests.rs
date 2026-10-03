//! Host tests at FAT's limits: a file's 32-bit size (`MAX_FILE_SIZE`),
//! at which Linux's vfat stops a file, and a volume with no free cluster.

use super::tests::{IMG, create_words, fresh, fsck, with_vol};
use super::*;

/// A write that starts at the limit is EFBIG; one that crosses it is cut
/// at the limit and goes on to need clusters, which a small volume lacks
/// (ENOSPC), where the whole write was refused; a truncate past the
/// limit is EFBIG, not EINVAL.
#[test]
fn writes_and_truncates_stop_at_4_gib() {
    let mut b = fresh(IMG);
    with_vol(&mut b, |v, d| {
        let root = v.info.root_clus;
        let mut f = create_words(v, d, root, b"big");
        for (off, e) in [
            (MAX_FILE_SIZE, FatError::FileTooBig),
            (MAX_FILE_SIZE + 9, FatError::FileTooBig),
            (MAX_FILE_SIZE - 4, FatError::NoSpace),
        ] {
            let r = v.write_ino(d, &mut f, true, off, false, &[1u8; 8]);
            assert_eq!(r.unwrap_err(), e, "write at {off:#x}");
        }
        assert_eq!(
            v.truncate_ino(d, &mut f, true, MAX_FILE_SIZE + 1)
                .unwrap_err(),
            FatError::FileTooBig
        );
        assert_eq!(f.size, 0);
        v.sync(d).unwrap();
    });
}

/// A rename onto an existing name in a full directory on a full volume
/// replaces it in its own slot, so it needs no room: here the new name,
/// `a.txt`, needs a long-name entry that the old `A.TXT` lacks, and a
/// rename that removed the target first and then made the new name lost
/// `A.TXT` when the directory could not grow.
#[test]
fn rename_over_in_a_full_directory() {
    let mut b = fresh(IMG);
    with_vol(&mut b, |v, d| {
        let root = v.info.root_clus;
        let dir = v.create(d, root, b"D", true).unwrap().clu;
        let old = create_words(v, d, dir, b"A.TXT");
        let mut src = create_words(v, d, dir, b"SRC");
        v.write_ino(d, &mut src, true, 0, false, b"new").unwrap();
        let mut fill = create_words(v, d, root, b"FILL");
        let mut off = 0u64;
        while v
            .write_ino(d, &mut fill, true, off, false, &[0u8; 512])
            .is_ok()
        {
            off += 512;
        }
        let mut i = 0u32;
        while v.create(d, dir, format!("F{i}").as_bytes(), false).is_ok() {
            i += 1;
        }
        let moved = v.rename(d, dir, b"SRC", dir, b"a.txt").unwrap();
        assert_eq!(v.lookup(d, dir, b"SRC").unwrap_err(), FatError::NotFound);
        let got = v.lookup(d, dir, b"A.TXT").unwrap();
        assert_eq!((got.clu, got.size), (src.first_clu, 3));
        let gone = moved.replaced.unwrap();
        assert_eq!((gone.dir_clu, gone.dir_off), (old.dir_clu, old.dir_off));
        v.free_chain(d, gone.first_clu).unwrap();
        v.sync(d).unwrap();
    });
    fsck(&b);
}
