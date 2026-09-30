//! In-guest tests of the VFS lock's scope and the FAT volume lock
//! (ROADMAP §10.4, A3, F060), on a FAT volume on `vda`'s second
//! partition, `vdap2`, so `vda`'s GPT and `vdap1` stay as the block tests
//! read them.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use vibeos::fs::{FileRef, O_CREAT, O_RDONLY, O_RDWR, O_TRUNC, OpenFlags, SeekFrom};

use super::{BLK_DELAY_MS, BLK_DELAYED, FAT_READ_HELD, FAT_READ_HOLD, FAT_READ_RELEASE};
use crate::file_init;
use crate::ktest::{Outcome, second_cpu, sleep_until};
use crate::thread_init::{self, SpawnOpts};
use crate::time_init;

/// The partition the FAT volume is made on, and where it is mounted.
const DEV: &[u8] = b"vdap2";
const MNT: &[u8] = b"/s59v";
/// The stack pages the test threads get: `spawn`'s 16 KiB.
const STACK_PAGES: usize = 4;
const OK: u32 = 1;

/// Format `vdap2` with a fresh FAT image through its `BlockRef` and mount
/// it on `at` through the File API.
fn fat_vda_volume(at: &[u8]) -> Result<(), &'static str> {
    super::stack16k::fat_image_to(DEV)?;
    file_init::mkdir(at, 0o755).map_err(|_| "mkdir")?;
    file_init::mount(DEV, at, b"fat32", false).map_err(|_| "mount")
}

/// One second in nanoseconds.
fn one_s_ns() -> u64 {
    core::time::Duration::from_secs(1).as_nanos() as u64
}

/// Open `path` with `flags`, run `f` on it, and close it.
fn with_file<R>(
    path: &[u8],
    flags: u32,
    f: impl FnOnce(&FileRef) -> Result<R, &'static str>,
) -> Result<R, &'static str> {
    let fr = file_init::open(path, OpenFlags::from_bits(flags), 0o644).map_err(|_| "open")?;
    let r = f(&fr);
    let c = file_init::close(fr);
    let v = r?;
    c.map_err(|_| "close")?;
    Ok(v)
}

// ---- vfs_io_off_lock ----

static READER_RESULT: AtomicU32 = AtomicU32::new(0);
static READER_DONE: AtomicBool = AtomicBool::new(false);
const READ_FILE: &[u8] = b"/s59v/r";
const READ_DATA: &[u8] = b"s59 vda read data";

/// Read [`READ_FILE`] back; the read's FAT op waits at `fat_read_hook`.
fn reader() {
    let r = with_file(READ_FILE, O_RDONLY, |f| {
        let mut buf = [0u8; 32];
        let n = file_init::read(f, &mut buf).map_err(|_| "read")?;
        if buf.get(..n) != Some(READ_DATA) {
            return Err("read back different bytes");
        }
        Ok(())
    });
    READER_RESULT.store(if r.is_ok() { OK } else { 2 }, Ordering::Relaxed);
    READER_DONE.store(true, Ordering::Release);
}

/// `/tmp/s59` opened, written, sought and read while the FAT read is held.
fn tmpfs_ops() -> Result<(), &'static str> {
    with_file(b"/tmp/s59", O_RDWR | O_CREAT | O_TRUNC, |f| {
        file_init::write(f, b"tmpfs").map_err(|_| "tmpfs write")?;
        file_init::seek(f, SeekFrom::Start(0)).map_err(|_| "tmpfs seek")?;
        let mut buf = [0u8; 8];
        match file_init::read(f, &mut buf) {
            Ok(5) if &buf[..5] == b"tmpfs" => Ok(()),
            _ => Err("tmpfs read"),
        }
    })
}

/// A read of a `vda` FAT file held at `fat_read_hook`, which holds only
/// its volume's lock, delays neither an `open` of a tmpfs path nor a
/// tmpfs `write`, `seek` and `read` on another CPU: each finishes within
/// 1 s while the read is still held.
pub(crate) fn test_vfs_io_off_lock() -> Outcome {
    let Some(cpu) = second_cpu() else {
        return Outcome::Skip("needs a second cpu");
    };
    if let Err(e) = fat_vda_volume(MNT) {
        return Outcome::Fail(e);
    }
    let r = io_off_lock(cpu);
    FAT_READ_HOLD.store(false, Ordering::Release);
    FAT_READ_RELEASE.store(true, Ordering::Release);
    let joined = sleep_until(|| READER_DONE.load(Ordering::Acquire), 10_000);
    let _ = file_init::unlink(b"/tmp/s59");
    let _ = file_init::unlink(READ_FILE);
    let u = file_init::umount(MNT);
    if let Err(e) = r {
        return Outcome::Fail(e);
    }
    if !joined {
        return Outcome::Fail("reader did not finish after its release");
    }
    if READER_RESULT.load(Ordering::Relaxed) != OK {
        return Outcome::Fail("reader failed");
    }
    match u {
        Ok(()) => Outcome::Ok,
        Err(_) => Outcome::Fail("umount"),
    }
}

fn io_off_lock(cpu: u32) -> Result<(), &'static str> {
    with_file(
        READ_FILE,
        O_RDWR | O_CREAT | O_TRUNC,
        |f| match file_init::write(f, READ_DATA) {
            Ok(n) if n == READ_DATA.len() => Ok(()),
            _ => Err("write the vda file"),
        },
    )?;
    READER_DONE.store(false, Ordering::Relaxed);
    READER_RESULT.store(0, Ordering::Relaxed);
    FAT_READ_HELD.store(false, Ordering::Relaxed);
    FAT_READ_RELEASE.store(false, Ordering::Relaxed);
    FAT_READ_HOLD.store(true, Ordering::Release);
    let opts = SpawnOpts {
        stack_pages: STACK_PAGES,
        cpu: Some(cpu),
    };
    thread_init::spawn_opts("s59rd", reader, opts).map_err(|_| "spawn reader")?;
    if !sleep_until(|| FAT_READ_HELD.load(Ordering::Acquire), 2_000) {
        return Err("the FAT read never reached its hook");
    }
    let t0 = time_init::now_ns();
    tmpfs_ops()?;
    let took = time_init::now_ns().saturating_sub(t0);
    if !FAT_READ_HELD.load(Ordering::Acquire) {
        return Err("the FAT read was released early");
    }
    if took >= one_s_ns() {
        return Err("tmpfs ops waited for the held FAT read");
    }
    Ok(())
}

// ---- fat_vol_wait_no_eio ----

/// Each worker's result: 0 until it finishes, then [`OK`] or its step.
static WORKER_RESULT: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];
const WORKER_FILES: [&[u8]; 2] = [b"/s59v/w0", b"/s59v/w1"];
const WORKER_BYTES: usize = 256;

fn pattern(w: usize, i: usize) -> u8 {
    (i.wrapping_mul(7) ^ (w << 5)) as u8
}

/// Write [`WORKER_BYTES`] to its file, read them back and compare; the
/// step that failed, or [`OK`].
fn worker_steps(w: usize) -> u32 {
    let Some(path) = WORKER_FILES.get(w) else {
        return 2;
    };
    let flags = OpenFlags::from_bits(O_RDWR | O_CREAT | O_TRUNC);
    let Ok(f) = file_init::open(path, flags, 0o644) else {
        return 3;
    };
    let r = write_read(&f, w);
    match file_init::close(f) {
        Ok(()) => r,
        Err(_) if r == OK => 9,
        Err(_) => r,
    }
}

fn write_read(f: &FileRef, w: usize) -> u32 {
    let mut buf = [0u8; WORKER_BYTES];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = pattern(w, i);
    }
    match file_init::write(f, &buf) {
        Ok(n) if n == WORKER_BYTES => {}
        Ok(_) => return 4,
        Err(_) => return 5,
    }
    if file_init::seek(f, SeekFrom::Start(0)).is_err() {
        return 6;
    }
    buf.fill(0);
    match file_init::read(f, &mut buf) {
        Ok(n) if n == WORKER_BYTES => {}
        Ok(_) => return 7,
        Err(_) => return 8,
    }
    if buf.iter().enumerate().any(|(i, &b)| b != pattern(w, i)) {
        return 10;
    }
    OK
}

fn worker(w: usize) {
    let r = worker_steps(w);
    if let Some(s) = WORKER_RESULT.get(w) {
        s.store(r, Ordering::Release);
    }
}

fn worker0() {
    worker(0);
}

fn worker1() {
    worker(1);
}

/// Two threads on `spawn`'s 16 KiB stacks write and read back FAT files
/// on `vda` through the File API while every block request waits 2 s for
/// the first 4 s: each waits for the volume the other holds, and neither
/// gets `EIO`.
pub(crate) fn test_fat_vol_wait_no_eio() -> Outcome {
    if let Err(e) = fat_vda_volume(MNT) {
        return Outcome::Fail(e);
    }
    for s in &WORKER_RESULT {
        s.store(0, Ordering::Relaxed);
    }
    BLK_DELAYED.store(0, Ordering::Relaxed);
    BLK_DELAY_MS.store(2_000, Ordering::Release);
    let spawned = thread_init::spawn("s59w0", worker0).is_ok()
        && thread_init::spawn("s59w1", worker1).is_ok();
    thread_init::sleep_ms(4_000);
    BLK_DELAY_MS.store(0, Ordering::Release);
    let delayed = BLK_DELAYED.load(Ordering::Relaxed);
    let done = |w: usize| {
        WORKER_RESULT
            .get(w)
            .is_some_and(|s| s.load(Ordering::Acquire) != 0)
    };
    let finished = spawned && sleep_until(|| done(0) && done(1), 45_000);
    for p in WORKER_FILES {
        let _ = file_init::unlink(p);
    }
    let u = file_init::umount(MNT);
    if !spawned {
        return Outcome::Fail("spawn");
    }
    if !finished {
        return Outcome::Fail("workers did not finish in 45 s");
    }
    crate::ktest_info!("{} block requests delayed 2 s", delayed);
    if delayed == 0 {
        return Outcome::Fail("no block request was delayed");
    }
    for (w, s) in WORKER_RESULT.iter().enumerate() {
        let r = s.load(Ordering::Acquire);
        if r != OK {
            return crate::fail_fmt!("worker {} failed at step {}", w, r);
        }
    }
    match u {
        Ok(()) => Outcome::Ok,
        Err(_) => Outcome::Fail("umount"),
    }
}
