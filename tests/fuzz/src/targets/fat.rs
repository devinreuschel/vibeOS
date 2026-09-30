//! `fat_mount`: the FAT32 BPB, FSInfo, directory and cluster-chain code.

use vibeos::fat::{self, FatInode, FatVol, Node};

use crate::image::Sparse;

/// Units a `fat_mount` image holds at most: this bounds `count_free`'s and
/// `fats_identical`'s walks of the FAT, a harness cap (TESTING.md §8.1).
pub const MAX_UNITS: u32 = 1 << 20;
/// Directory entries the walk visits.
const MAX_NODES: usize = 256;
/// Directory depth the walk descends to.
const MAX_DEPTH: usize = 8;
/// Bytes read from each file.
const MAX_READ: usize = 64 << 10;

/// Mount, walk and read, compare the FATs, then create, write and sync a
/// file into the overlay.
pub fn mount(data: &[u8]) {
    let Some(mut d) = Sparse::parse(data, fat::SEC, MAX_UNITS) else {
        return;
    };
    let mut vol = Box::new(FatVol::new());
    if vol.mount_in(&mut d).is_err() {
        return;
    }
    walk(&mut vol, &mut d);
    let _ = vol.fats_identical(&mut d);
    let root = vol.root().clu;
    if let Ok(node) = vol.create(&mut d, root, b"fuzz-write.txt", false) {
        let mut ino = FatInode::of_node(&node);
        let _ = vol.write_ino(&mut d, &mut ino, true, 0, false, &[0x5A; 1024]);
    }
    let _ = vol.sync(&mut d);
}

/// Visit at most [`MAX_NODES`] entries, [`MAX_DEPTH`] deep, reading at most
/// [`MAX_READ`] bytes of each file.
fn walk(vol: &mut FatVol, d: &mut Sparse) {
    let mut buf = vec![0u8; MAX_READ];
    let mut stack = vec![(vol.root().clu, 0usize)];
    let mut seen = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let mut cookie = 0u64;
        loop {
            if seen >= MAX_NODES {
                return;
            }
            let mut n = Node::EMPTY;
            match vol.readdir(d, dir, cookie, &mut n) {
                Ok(Some(next)) => cookie = next,
                Ok(None) | Err(_) => break,
            }
            seen += 1;
            if n.name() == b"." || n.name() == b".." {
                continue;
            }
            if n.is_dir() {
                if depth + 1 < MAX_DEPTH {
                    stack.push((n.clu, depth + 1));
                }
            } else {
                let _ = vol.read_ino(d, &FatInode::of_node(&n), 0, &mut buf);
            }
        }
    }
}
