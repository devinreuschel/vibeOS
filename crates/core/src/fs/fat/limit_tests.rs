//! Host tests of the limit FAT's format sets on a file: a 32-bit size
//! (`MAX_FILE_SIZE`), at which Linux's vfat stops a file.

use super::tests::{IMG, create_words, fresh, with_vol};
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
