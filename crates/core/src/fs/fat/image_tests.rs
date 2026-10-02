//! Host tests of images a crafted or damaged disk holds: what FAT reads
//! from the image never aims a write elsewhere or leaves a change half
//! made.

use super::tests::{IMG, create_words, fresh, with_vol};
use super::*;

/// How a case crafts the image, and the sector it checks is kept.
type Craft = (fn(&mut [u8]), fn(&[u8]) -> usize);

/// A BPB whose FSInfo sector number names the first FAT sector, or whose
/// backup boot sector number puts the backup FSInfo on the real backup
/// boot sector: the volume writes FSInfo only over a sector that holds a
/// valid one, so a crafted image cannot aim the write-back elsewhere.
#[test]
fn fsinfo_write_back_only_over_fsinfo() {
    let cases: [Craft; 2] = [
        (
            |b| {
                let r = u16::from_le_bytes([b[14], b[15]]);
                b[48..50].copy_from_slice(&r.to_le_bytes());
            },
            |b| usize::from(u16::from_le_bytes([b[14], b[15]])) * SEC,
        ),
        (
            |b| b[50..52].copy_from_slice(&5u16.to_le_bytes()),
            |_| 6 * SEC,
        ),
    ];
    for (craft, kept) in cases {
        let mut b = fresh(IMG);
        craft(&mut b);
        let s = kept(&b);
        let before: Vec<u8> = b[s..s + SEC].to_vec();
        with_vol(&mut b, |v, d| {
            let root = v.info.root_clus;
            let mut f = create_words(v, d, root, b"f.txt");
            v.write_ino(d, &mut f, true, 0, false, b"data").unwrap();
            v.fsinfo_dirty = true;
            v.sync(d).unwrap();
        });
        assert_eq!(&b[s..s + 8], &before[..8], "sector {} kept", s / SEC);
        assert_ne!(
            &b[s..s + 4],
            &0x4161_5252u32.to_le_bytes(),
            "no FSInfo over sector {}",
            s / SEC
        );
    }
}

/// A file whose chain loops back to its first cluster: truncating it is
/// `Corrupt` before its dirent or the FAT changes, where a walk that
/// freed as it went would free the first cluster under the dirent.
#[test]
fn truncate_looped_chain_changes_nothing() {
    let mut b = fresh(IMG);
    let (cb, chain) = with_vol(&mut b, |v, d| {
        let root = v.info.root_clus;
        let mut f = create_words(v, d, root, b"f.txt");
        let cb = v.info.clus_bytes();
        v.write_ino(d, &mut f, true, 0, false, &vec![7u8; cb * 3])
            .unwrap();
        let c1 = f.first_clu;
        let c2 = v.fat_get(d, c1).unwrap();
        let c3 = v.fat_get(d, c2).unwrap();
        v.fat_set(d, c3, c1).unwrap();
        v.commit_fat(d).unwrap();
        (cb, [c1, c2, c3])
    });
    with_vol(&mut b, |v, d| {
        let root = v.info.root_clus;
        let mut f = FatInode::of_node(&v.lookup(d, root, b"f.txt").unwrap());
        let links = chain.map(|c| v.fat_get(d, c).unwrap());
        assert_eq!(
            v.truncate_ino(d, &mut f, true, 1).unwrap_err(),
            FatError::Corrupt
        );
        let n = v.lookup(d, root, b"f.txt").unwrap();
        assert_eq!((n.clu, n.size as usize), (chain[0], cb * 3));
        assert_eq!(chain.map(|c| v.fat_get(d, c).unwrap()), links);
    });
}
