//! In-guest test of `umount`'s consistency (ROADMAP §10.4, F060), on a
//! FAT volume on `vdap2` mounted twice, from a thread on `spawn`'s 16 KiB
//! stack.

use core::sync::atomic::{AtomicU32, Ordering};

use vibeos::fs::{DirRef, FileRef, FsError, O_CREAT, O_RDONLY, O_RDWR, OpenFlags, SeekFrom};

use crate::block::blockdev_init;
use crate::file_init;
use crate::fs_init;
use crate::ktest::{Outcome, sleep_until};
use crate::thread_init;

const DEV: &[u8] = b"vdap2";
const MNT_U: &[u8] = b"/s60u";
const MNT_V: &[u8] = b"/s60v";
const FILE_U: &[u8] = b"/s60u/f";
const FILE_V: &[u8] = b"/s60v/f";
const DIR_U: &[u8] = b"/s60u/d";
const DATA: &[u8] = b"s60 umount";

/// The worker's result: 0 until it finishes, then 1 for success or the
/// index + 2 of the step in [`STEPS`] that failed.
static RESULT: AtomicU32 = AtomicU32::new(0);

const STEPS: [&str; 18] = [
    "vdap2 is already mounted: another test left it",
    "format vdap2",
    "mkdir the mountpoints",
    "mount vdap2 on /s60u",
    "mount vdap2 on /s60v",
    "write /s60u/f",
    "umount /s60u with a file open there was not EBUSY",
    "read and write through the busy mount",
    "a new open through the busy mount",
    "mkdir /s60u/d",
    "a directory reference on /s60u/d",
    "umount /s60u with a directory reference there was not EBUSY",
    "open /s60v/f",
    "umount /s60u with a file open through /s60v",
    "umount /s60v",
    "mount vdap2 on /s60u again",
    "read back /s60u/f after the remount",
    "umount /s60u after the remount",
];

/// A failed step's index in [`STEPS`].
type Step = usize;

/// A failed unmount leaves the mount usable: a file or a directory
/// reference reached through `/s60u` makes `umount /s60u` `EBUSY`, and
/// reads, writes and new opens through it go on working; a file open
/// through the other mount of the same superblock, `/s60v`, does not; the
/// last unmount syncs and releases the volume, and a remount reads back
/// what was written.
pub(crate) fn test_umount_consistent() -> Outcome {
    RESULT.store(0, Ordering::Relaxed);
    if thread_init::spawn("s60umnt", worker).is_err() {
        return Outcome::Fail("spawn");
    }
    if !sleep_until(|| RESULT.load(Ordering::Acquire) != 0, 30_000) {
        return Outcome::Fail("the worker did not finish in 30 s");
    }
    match RESULT.load(Ordering::Acquire) {
        1 => Outcome::Ok,
        r => {
            let step = (r as usize).checked_sub(2).and_then(|i| STEPS.get(i));
            Outcome::Fail(step.copied().unwrap_or("unknown step"))
        }
    }
}

fn worker() {
    let r = match steps() {
        Ok(()) => 1,
        Err(i) => (i as u32).saturating_add(2),
    };
    clean();
    // Release: the last store; the test reads it with Acquire.
    RESULT.store(r, Ordering::Release);
}

/// Unmount and remove what the test made, on every path.
fn clean() {
    for _ in 0..2 {
        let _ = file_init::umount(MNT_U);
        let _ = file_init::umount(MNT_V);
    }
    let _ = file_init::rmdir(MNT_U);
    let _ = file_init::rmdir(MNT_V);
}

fn open(path: &[u8], flags: u32) -> Result<FileRef, FsError> {
    file_init::open(path, OpenFlags::from_bits(flags), 0o644)
}

/// Run `f` on a file `path` opened with `flags`, then close it.
fn with_open<R>(
    path: &[u8],
    flags: u32,
    f: impl FnOnce(&FileRef) -> Result<R, FsError>,
) -> Result<R, FsError> {
    let fr = open(path, flags)?;
    let r = f(&fr);
    let c = file_init::close(fr);
    let v = r?;
    c?;
    Ok(v)
}

fn read_all(f: &FileRef) -> Result<bool, FsError> {
    file_init::seek(f, SeekFrom::Start(0))?;
    let mut buf = [0u8; 32];
    let n = file_init::read(f, &mut buf)?;
    Ok(buf.get(..n) == Some(DATA))
}

fn busy(at: &[u8]) -> bool {
    file_init::umount(at) == Err(FsError::Busy)
}

fn steps() -> Result<(), Step> {
    let dev = blockdev_init::lookup(DEV).ok_or(1usize)?;
    if fs_init::with(|v| v.super_of_dev(dev.id())).is_some() {
        return Err(0);
    }
    super::stack16k::fat_image_to(DEV).map_err(|_| 1usize)?;
    file_init::mkdir(MNT_U, 0o755)
        .and_then(|()| file_init::mkdir(MNT_V, 0o755))
        .map_err(|_| 2usize)?;
    file_init::mount(DEV, MNT_U, b"fat32", false).map_err(|_| 3usize)?;
    file_init::mount(DEV, MNT_V, b"fat32", false).map_err(|_| 4usize)?;
    // A file open through /s60u.
    let held = open(FILE_U, O_RDWR | O_CREAT).map_err(|_| 5usize)?;
    let r = busy_with_file(&held);
    let c = file_init::close(held);
    r?;
    c.map_err(|_| 7usize)?;
    // A directory reference below /s60u.
    file_init::mkdir(DIR_U, 0o755).map_err(|_| 9usize)?;
    let d: DirRef = file_init::dir_get_at(None, DIR_U).map_err(|_| 10usize)?;
    let busy_dir = busy(MNT_U);
    file_init::dir_put(d);
    if !busy_dir {
        return Err(11);
    }
    // Users of the superblock through the other mount do not count.
    let other = open(FILE_V, O_RDONLY).map_err(|_| 12usize)?;
    let u = file_init::umount(MNT_U);
    let c = file_init::close(other);
    u.map_err(|_| 13usize)?;
    c.map_err(|_| 12usize)?;
    file_init::umount(MNT_V).map_err(|_| 14usize)?;
    // The last unmount synced the volume: a remount reads the file back.
    file_init::mount(DEV, MNT_U, b"fat32", false).map_err(|_| 15usize)?;
    match with_open(FILE_U, O_RDONLY, read_all) {
        Ok(true) => {}
        _ => return Err(16),
    }
    file_init::umount(MNT_U).map_err(|_| 17usize)
}

/// With `held` open through /s60u: the unmount is `EBUSY`, and the mount
/// still reads, writes and opens.
fn busy_with_file(held: &FileRef) -> Result<(), Step> {
    match file_init::write(held, DATA) {
        Ok(n) if n == DATA.len() => {}
        _ => return Err(5),
    }
    if !busy(MNT_U) {
        return Err(6);
    }
    match read_all(held) {
        Ok(true) => {}
        _ => return Err(7),
    }
    file_init::seek(held, SeekFrom::Start(0)).map_err(|_| 7usize)?;
    match file_init::write(held, DATA) {
        Ok(n) if n == DATA.len() => {}
        _ => return Err(7),
    }
    match with_open(FILE_U, O_RDONLY, read_all) {
        Ok(true) => Ok(()),
        _ => Err(8),
    }
}
