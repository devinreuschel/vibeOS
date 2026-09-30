//! `vibefs_mount`: vibefs's superblock pick, mount, B-tree and extent code,
//! and `fsck`. No checksum fix-up: vibefs v1 trusts checksum-valid blocks
//! (F061, ROADMAP §14.8), so the target reaches only what a checksum lets
//! through (TESTING.md §8.1).

use vibeos::vibefs::{self, Node, Vol};

use crate::image::Sparse;

/// Blocks a `vibefs_mount` image holds at most.
pub const MAX_UNITS: u32 = 4096;
/// Directory entries the walk visits.
const MAX_NODES: usize = 256;
/// Directory depth the walk descends to.
const MAX_DEPTH: usize = 8;
/// Bytes read from each file.
const MAX_READ: usize = 64 << 10;

/// Probe and mount, walk and read, then `fsck`, which must not call a disk
/// clean that `mount` refused.
pub fn mount(data: &[u8]) {
    let Some(mut d) = Sparse::parse(data, vibefs::BLOCK, MAX_UNITS) else {
        return;
    };
    let _ = vibefs::probe(&mut d);
    let mut vol = Box::new(Vol::new());
    let mounted = vibefs::mount(&mut d, &mut vol).is_ok();
    if mounted {
        walk(&mut vol, &mut d);
    }
    if let Ok(r) = vibefs::fsck(&mut d) {
        assert!(
            r.errors != 0 || mounted,
            "fsck reported 0 errors on a disk mount refused"
        );
    }
}

/// Visit at most [`MAX_NODES`] entries, [`MAX_DEPTH`] deep, reading at most
/// [`MAX_READ`] bytes of each file.
fn walk(vol: &mut Vol, d: &mut Sparse) {
    let mut buf = vec![0u8; MAX_READ];
    let mut stack = vec![(vol.root_ino, 0usize)];
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
            if n.is_dir() {
                if depth + 1 < MAX_DEPTH {
                    stack.push((n.ino, depth + 1));
                }
            } else {
                let _ = vol.read(d, n.ino, 0, &mut buf);
            }
        }
    }
}
