//! Host tests of images a crafted or damaged disk holds, and of a disk
//! that fails a write: what FAT reads from the image never aims a write
//! elsewhere or leaves a change half made.

use super::tests::{IMG, create_words, fresh, fsck, with_vol};
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

/// FSInfo's free count is a hint, which a mount does not take: one of 0
/// on a volume with free clusters refuses no write, and the volume's count
/// is the FAT's own.
#[test]
fn fsinfo_free_count_is_not_trusted() {
    let mut b = fresh(IMG);
    let at = usize::from(u16::from_le_bytes([b[48], b[49]])) * SEC + 488;
    b[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
    with_vol(&mut b, |v, d| {
        let counted = v.count_free(d).unwrap();
        assert_eq!(v.free, counted);
        let root = v.info.root_clus;
        let mut f = create_words(v, d, root, b"f.txt");
        v.write_ino(d, &mut f, true, 0, false, b"data").unwrap();
        v.sync(d).unwrap();
    });
}

/// A disk whose `fail`-th write or flush since the count was reset,
/// counted from 1, fails with `Io` and changes nothing; every other one
/// goes through. A read of sector `bad_read` fails with `Io`.
struct FailDisk<'a> {
    inner: MemDisk<'a>,
    ops: u32,
    fail: u32,
    bad_read: Option<u32>,
}

impl FailDisk<'_> {
    fn tick(&mut self) -> Result<(), FatError> {
        self.ops += 1;
        if self.ops == self.fail {
            return Err(FatError::Io);
        }
        Ok(())
    }
}

impl Disk for FailDisk<'_> {
    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }

    fn nsectors(&self) -> u32 {
        self.inner.nsectors()
    }

    fn read(&mut self, lba: u32, buf: &mut [u8]) -> Result<(), FatError> {
        if self.bad_read == Some(lba) {
            return Err(FatError::Io);
        }
        self.inner.read(lba, buf)
    }

    fn write(&mut self, lba: u32, buf: &[u8]) -> Result<(), FatError> {
        self.tick()?;
        self.inner.write(lba, buf)
    }

    fn flush(&mut self) -> Result<(), FatError> {
        self.tick()?;
        self.inner.flush()
    }
}

/// The `..` entry of directory `dir`, as 32 raw bytes.
fn dotdot_raw<D: Disk>(v: &mut FatVol, d: &mut D, dir: u32) -> [u8; ENT] {
    let mut ent = [0u8; ENT];
    assert!(v.read_dir_raw(d, dir, ENT as u32, &mut ent).unwrap());
    ent
}

/// `X/S`, a directory holding `IN.TXT`, with the empty directory `Q` and
/// the files `A.TXT` and `B.TXT` in the root, synced; the clusters of
/// `X`, `S`, `Q`, `A.TXT` and `B.TXT`.
fn rename_image(b: &mut [u8]) -> [u32; 5] {
    with_vol(b, |v, d| {
        let root = v.info.root_clus;
        let x = v.create(d, root, b"X", true).unwrap();
        let s = v.create(d, x.clu, b"S", true).unwrap();
        let mut f = create_words(v, d, s.clu, b"IN.TXT");
        v.write_ino(d, &mut f, true, 0, false, b"inside").unwrap();
        let q = v.create(d, root, b"Q", true).unwrap();
        let mut a = create_words(v, d, root, b"A.TXT");
        v.write_ino(d, &mut a, true, 0, false, b"aaaa").unwrap();
        let mut bb = create_words(v, d, root, b"B.TXT");
        v.write_ino(d, &mut bb, true, 0, false, b"bb").unwrap();
        v.sync(d).unwrap();
        [x.clu, s.clu, q.clu, a.first_clu, bb.first_clu]
    })
}

/// A moved directory whose `..` entry is not one: the rename is `Corrupt`
/// before anything is written, over an existing name or to a new one, so
/// the target keeps its entry and no name shares the source's clusters.
#[test]
fn rename_corrupt_dotdot_changes_nothing() {
    let mut b = fresh(IMG);
    let [x, s, q, ..] = rename_image(&mut b);
    with_vol(&mut b, |v, d| {
        let root = v.info.root_clus;
        let good = dotdot_raw(v, d, s);
        let mut bad = good;
        bad[..11].copy_from_slice(b"NOTDOTDOT  ");
        v.write_dir_raw(d, s, ENT as u32, &bad).unwrap();
        let free = v.count_free(d).unwrap();
        for to in [&b"Q"[..], b"NEW"] {
            assert_eq!(
                v.rename(d, x, b"S", root, to).unwrap_err(),
                FatError::Corrupt
            );
        }
        assert_eq!(v.lookup(d, root, b"Q").unwrap().clu, q);
        assert_eq!(v.lookup(d, x, b"S").unwrap().clu, s);
        assert_eq!(v.lookup(d, root, b"NEW").unwrap_err(), FatError::NotFound);
        assert_eq!(dotdot_raw(v, d, s), bad);
        assert_eq!(v.count_free(d).unwrap(), free);
        v.write_dir_raw(d, s, ENT as u32, &good).unwrap();
        v.sync(d).unwrap();
    });
    fsck(&b);
}

/// A rename whose every write or flush in turn fails: each failure
/// returns the error with every name naming the clusters it named before
/// and the moved directory's `..` as it was, and the image is clean, as
/// rename(2) leaves both names on an error. The cases replace an empty
/// directory in another parent, replace a file, and move a directory to a
/// new name in another parent.
#[test]
fn rename_failed_write_keeps_both_names() {
    let cases: [(&[u8], &[u8]); 3] = [(b"S", b"Q"), (b"A.TXT", b"B.TXT"), (b"S", b"NEW")];
    for (from, to) in cases {
        let mut fail = 1u32;
        loop {
            let mut b = fresh(IMG);
            let [x, s, q, a, bb] = rename_image(&mut b);
            let done = {
                let mut d = FailDisk {
                    inner: MemDisk::new(&mut b, SEC as u32).unwrap(),
                    ops: 0,
                    fail: 0,
                    bad_read: None,
                };
                let mut v = FatVol::mount(&mut d).unwrap();
                let root = v.info.root_clus;
                let sd = if from == b"S" { x } else { root };
                let dd = dotdot_raw(&mut v, &mut d, s);
                let free = v.count_free(&mut d).unwrap();
                (d.ops, d.fail) = (0, fail);
                let r = v.rename(&mut d, sd, from, root, to);
                let fired = d.ops >= fail;
                d.fail = 0;
                match r {
                    Ok(m) => {
                        assert!(!fired, "op {fail} failed unreported");
                        if let Some(gone) = m.replaced {
                            v.free_chain(&mut d, gone.first_clu).unwrap();
                        }
                        v.sync(&mut d).unwrap();
                    }
                    Err(e) => {
                        let name = |n: &[u8]| String::from_utf8_lossy(n).into_owned();
                        let at = format!("{} -> {}, op {fail}", name(from), name(to));
                        assert!(fired, "{at}: {e:?}");
                        assert_eq!(e, FatError::Io, "{at}");
                        let src = v.lookup(&mut d, sd, from).unwrap().clu;
                        assert_eq!(src, if from == b"S" { s } else { a }, "{at}");
                        match v.lookup(&mut d, root, to) {
                            Ok(n) => assert_eq!(n.clu, if to == b"Q" { q } else { bb }, "{at}"),
                            Err(e) => assert_eq!((to, e), (&b"NEW"[..], FatError::NotFound)),
                        }
                        assert_eq!(dotdot_raw(&mut v, &mut d, s), dd, "{at}");
                        assert_eq!(v.count_free(&mut d).unwrap(), free, "{at}");
                        v.sync(&mut d).unwrap();
                    }
                }
                !fired
            };
            fsck(&b);
            if done {
                break;
            }
            fail += 1;
        }
        assert!(fail > 2, "the rename wrote");
    }
}

/// A directory whose cluster the disk fails to read: a lookup in it, a
/// listing of it and its rmdir each return `Io`, never `NotFound`, the
/// listing's end, or an empty directory removed, so `open` gets `EIO`
/// (ROADMAP §10.5's errno box); once the sector reads, the name is there.
#[test]
fn dir_read_error_is_io() {
    let mut b = fresh(IMG);
    let (x, lba) = with_vol(&mut b, |v, d| {
        let root = v.info.root_clus;
        let x = v.create(d, root, b"X", true).unwrap();
        let mut f = create_words(v, d, x.clu, b"IN.TXT");
        v.write_ino(d, &mut f, true, 0, false, b"inside").unwrap();
        v.sync(d).unwrap();
        (x.clu, v.info.clus_lba(x.clu).unwrap())
    });
    let mut d = FailDisk {
        inner: MemDisk::new(&mut b, SEC as u32).unwrap(),
        ops: 0,
        fail: 0,
        bad_read: Some(lba),
    };
    let mut v = FatVol::mount(&mut d).unwrap();
    let root = v.info.root_clus;
    assert_eq!(v.lookup(&mut d, x, b"IN.TXT").unwrap_err(), FatError::Io);
    let mut out = Node::EMPTY;
    assert_eq!(v.readdir(&mut d, x, 0, &mut out).unwrap_err(), FatError::Io);
    assert_eq!(
        v.unlink(&mut d, root, b"X", true).unwrap_err(),
        FatError::Io
    );
    d.bad_read = None;
    assert_eq!(v.lookup(&mut d, x, b"IN.TXT").unwrap().size, 6);
    assert_eq!(v.lookup(&mut d, root, b"X").unwrap().clu, x);
}

/// A directory whose cluster chain leaves the volume fails its walk with
/// `Corrupt` (EIO) past its first cluster, rather than ending it there.
#[test]
fn dir_damaged_chain_is_corrupt() {
    let mut b = fresh(IMG);
    with_vol(&mut b, |v, d| {
        let root = v.info.root_clus;
        let x = v.create(d, root, b"X", true).unwrap().clu;
        // `.`, `..` and 14 names fill one 512-byte cluster; a 15th grows
        // the chain to a second.
        for i in 0..15u8 {
            let name = [b'F', b'0' + i / 10, b'0' + i % 10];
            v.create(d, x, &name, false).unwrap();
        }
        assert!(v.lookup(d, x, b"F14").is_ok());
        let past = v.info.nclus.checked_add(10).unwrap();
        v.fat_set(d, x, past).unwrap();
        assert_eq!(v.lookup(d, x, b"NOPE").unwrap_err(), FatError::Corrupt);
        let mut out = Node::EMPTY;
        let mut off = 0u64;
        let err = loop {
            match v.readdir(d, x, off, &mut out) {
                Ok(Some(next)) => off = next,
                Ok(None) => panic!("the walk ended at the damaged chain"),
                Err(e) => break e,
            }
        };
        assert_eq!(err, FatError::Corrupt);
    });
}
