//! `vdap1_vfs_read` (kernel_tests only), re-exported from `fs::ktest`: a
//! FAT32 volume made on `vda`'s first GPT partition is mounted by its
//! partition name, and `/dev/vdap1` is read through `Vfs`, all from a
//! thread started with `spawn`'s 16 KiB stack, never the registry's
//! (ROADMAP §10.4's `BlockRef` box, DESIGN §12.1). The partition holds no
//! table and no other test's data: `block_part_gpt` writes its sector 1
//! and reads it straight back, so a fresh volume here leaves it nothing
//! to miss.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use vibeos::fs::{FileRef, InodeKind, O_CREAT, O_RDONLY, O_RDWR, O_TRUNC, OpenFlags};
use vibeos::kalloc::TryVec;

use crate::block::blockdev_init;
use crate::file_init;
use crate::ktest::{Outcome, sleep_for};
use crate::thread_init;

/// The partition, its devfs node, and where it is mounted.
const PART: &[u8] = b"vdap1";
const NODE: &[u8] = b"/dev/vdap1";
const MNT: &[u8] = b"/s59p1";
const FILE: &[u8] = b"/s59p1/by_name.txt";
/// What the worker writes through the mount, then finds in `/dev/vdap1`.
const DATA: &[u8] = b"vdap1 FAT32 mounted by its partition name\n";
const SEC: usize = 512;
/// `fat::mkfs`'s boot sector: the FAT32 type string at byte 82 and the
/// signature at 510.
const FAT32_TYPE: &[u8] = b"FAT32   ";

/// The worker's result: 0 until it finishes, then [`OK`] or the step that
/// failed.
static RESULT: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicBool = AtomicBool::new(false);
const OK: u32 = 1;

/// The steps the worker runs: format, mount by name, write and read back
/// a file, read the node, unmount. Each failure is a step number.
fn worker_steps() -> u32 {
    if super::stack16k::fat_image_to(PART).is_err() {
        return 2;
    }
    if file_init::mkdir(MNT, 0o755).is_err() {
        return 3;
    }
    let r = mounted_steps();
    let u = file_init::umount(MNT);
    let d = file_init::rmdir(MNT);
    if r != OK {
        return r;
    }
    if u.is_err() {
        return 20;
    }
    if d.is_err() {
        return 21;
    }
    OK
}

fn mounted_steps() -> u32 {
    if file_init::mount(PART, MNT, b"fat32", false).is_err() {
        return 4;
    }
    let flags = OpenFlags::from_bits(O_CREAT | O_RDWR | O_TRUNC);
    let Ok(f) = file_init::open(FILE, flags, 0o644) else {
        return 5;
    };
    let w = file_init::write(&f, DATA);
    if file_init::close(f).is_err() || !matches!(w, Ok(n) if n == DATA.len()) {
        return 6;
    }
    let Ok(f) = file_init::open(FILE, OpenFlags::from_bits(O_RDONLY), 0) else {
        return 7;
    };
    let mut back = [0u8; 64];
    let r = file_init::read(&f, &mut back);
    if file_init::close(f).is_err() || back.get(..DATA.len()) != Some(DATA) {
        return 8;
    }
    if !matches!(r, Ok(n) if n == DATA.len()) {
        return 8;
    }
    node_steps()
}

/// Read all of `/dev/vdap1` through `Vfs` and check it against the
/// partition read through its `BlockRef`: the same bytes, a FAT32 boot
/// sector, and the file the mount wrote.
fn node_steps() -> u32 {
    let Some(part) = blockdev_init::lookup(PART) else {
        return 9;
    };
    let Ok(cap) = part.capacity_sectors() else {
        return 9;
    };
    let Some(len) = usize::try_from(cap).ok().and_then(|c| c.checked_mul(SEC)) else {
        return 9;
    };
    let (Ok(mut by_node), Ok(mut by_ref)) = (zeroed(len), zeroed(len)) else {
        return 10;
    };
    let Ok(f) = file_init::open(NODE, OpenFlags::from_bits(O_RDONLY), 0) else {
        return 11;
    };
    let r = node_read(&f, &mut by_node, len as u64);
    if file_init::close(f).is_err() {
        return 12;
    }
    if r != OK {
        return r;
    }
    if part.read(0, &mut by_ref).is_err() {
        return 15;
    }
    if by_node[..] != by_ref[..] {
        return 16;
    }
    if by_node.get(82..90) != Some(FAT32_TYPE) || by_node.get(510..512) != Some(&[0x55, 0xAA]) {
        return 17;
    }
    if !by_node.windows(DATA.len()).any(|w| w == DATA) {
        return 18;
    }
    OK
}

/// The node is a block device of `len` bytes; read it whole into `buf`.
fn node_read(f: &FileRef, buf: &mut [u8], len: u64) -> u32 {
    match file_init::stat(f) {
        Ok(st) if st.kind == InodeKind::Blk && st.size == len => {}
        _ => return 13,
    }
    let mut done = 0usize;
    while let Some(rest) = buf.get_mut(done..) {
        if rest.is_empty() {
            break;
        }
        match file_init::read(f, rest) {
            Ok(0) | Err(_) => return 14,
            Ok(n) => done += n,
        }
    }
    OK
}

fn zeroed(n: usize) -> Result<TryVec<u8>, ()> {
    let mut v = TryVec::try_with_capacity(n).map_err(|_| ())?;
    let zero = [0u8; SEC];
    while v.len() < n {
        let take = (n - v.len()).min(SEC);
        v.try_extend_from_slice(zero.get(..take).ok_or(())?)
            .map_err(|_| ())?;
    }
    Ok(v)
}

fn vdap1_worker() {
    let r = worker_steps();
    RESULT.store(r, Ordering::Relaxed);
    // Release: pairs with the registry's Acquire load of `DONE`.
    DONE.store(true, Ordering::Release);
}

/// A FAT32 volume made on `vdap1` and mounted by its partition name takes
/// a file's write and read through `Vfs`, and `/dev/vdap1`, read through
/// `Vfs`, is a block node of the partition's size whose bytes are the ones
/// its `BlockRef` reads: the volume's boot sector and the file's data. The
/// worker runs on `spawn`'s 16 KiB stack; the registry only waits.
pub(crate) fn test_vdap1_vfs_read() -> Outcome {
    if !crate::drivers::ktest::vda_live() {
        return Outcome::Fail("no virtio-blk");
    }
    RESULT.store(0, Ordering::Relaxed);
    DONE.store(false, Ordering::Relaxed);
    let h = match thread_init::spawn("vdap1rd", vdap1_worker) {
        Ok(h) => h,
        Err(_) => return Outcome::Fail("spawn"),
    };
    if !sleep_for(|| DONE.load(Ordering::Acquire)) {
        return Outcome::Fail("worker did not finish by the run's deadline");
    }
    let depth = crate::sched::ktest::wait_exit_depth(h.id().0);
    crate::ktest_info!("vdap1 worker stack depth {:?}", depth);
    match RESULT.load(Ordering::Relaxed) {
        OK => Outcome::Ok,
        r => crate::fail_fmt!("worker step {} failed", r),
    }
}
