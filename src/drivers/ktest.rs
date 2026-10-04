//! In-guest tests for drivers (kernel_tests only). Rows: [`TESTS`].

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use vibeos::block::{BlockError, DeviceState, Op};
use vibeos::dev::{DevRef, Instance};
use vibeos::lock::RANK_DEVICE;

use crate::block_init::IoWaiter;
use crate::fat_init;
use crate::file_init;
use crate::ktest::{Outcome, Test, fid, test};
use crate::per_cpu_init;
use crate::sync_init::SpinMutex;
use crate::thread_init;
use crate::time_init;
use crate::vibefs_init::{self, VibeVolume};
use crate::virtio_blk_init::{self, VirtioBlk};

// ---- vda, the ktest disk, looked up by name: the driver keeps no list
// of its instances (DESIGN §12.1 rule 1).

/// Run `f` on vda's instance; `None` when vda is not bound.
pub(crate) fn vda<R>(f: impl FnOnce(&VirtioBlk) -> R) -> Option<R> {
    virtio_blk_init::with_disk(b"vda", f)
}

/// Whether vda is bound and live.
pub(crate) fn vda_live() -> bool {
    vda(|b| b.live()).unwrap_or(false)
}

pub(crate) fn vda_read(lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
    vda(|b| b.read(lba, buf)).unwrap_or(Err(BlockError::Gone))
}

pub(crate) fn vda_write(lba: u64, buf: &[u8]) -> Result<(), BlockError> {
    vda(|b| b.write(lba, buf)).unwrap_or(Err(BlockError::Gone))
}

pub(crate) fn vda_write_fua(lba: u64, buf: &[u8]) -> Result<(), BlockError> {
    vda(|b| b.write_fua(lba, buf)).unwrap_or(Err(BlockError::Gone))
}

fn vda_flush() -> Result<(), BlockError> {
    vda(|b| b.flush()).unwrap_or(Err(BlockError::Gone))
}

fn has_mq() -> bool {
    vda(|b| b.has_mq()).unwrap_or(false)
}

fn has_discard() -> bool {
    vda(|b| b.has_discard()).unwrap_or(false)
}

/// Runs of vda's top half (`blk_top`).
pub(crate) fn top_hits() -> u32 {
    vda(|b| b.top_hits()).unwrap_or(0)
}

fn thread_hits() -> u32 {
    vda(|b| b.thread_hits()).unwrap_or(0)
}

fn completions() -> u32 {
    vda(|b| b.completions()).unwrap_or(0)
}

/// `Flush` requests dispatched to vda, emulated-`Fua` ones and those
/// finished locally without `F_FLUSH` included.
pub(crate) fn flushes() -> u64 {
    vda(|b| b.flushes()).unwrap_or(0)
}

/// A test LBA inside vda's Linux GPT partition (which starts at 512), not
/// the GPT backup.
pub(crate) fn persist_lba() -> u64 {
    vda(|b| b.persist_lba()).unwrap_or(0)
}

fn find_blk() -> Option<DevRef> {
    crate::dev::ktest::find_id(0x1af4, 0x1042)
        .or_else(|| crate::dev::ktest::find_id(0x1af4, 0x1001))
}

pub(crate) fn test_block_vblk_rw() -> Outcome {
    if !vda_live() {
        return Outcome::Skip("no virtio-blk");
    }
    let Some(d) = find_blk() else {
        return Outcome::Fail("id missing");
    };
    match crate::dev_init::bound(&d) {
        Some("virtio-blk") => {}
        Some(_) => return Outcome::Fail("wrong driver"),
        None => return Outcome::Fail("unbound"),
    }
    // The first function binds first, so it is vda, and its instance is
    // the one its registry entry owns.
    if vda(|b| b.dev().same(&d)) != Some(true) {
        return Outcome::Fail("vda is not the first function's instance");
    }
    // vda's registry handle; its `_dev` calls reach the driver below the
    // page cache, as this test always has.
    let Some(d) = crate::block::blockdev_init::lookup(b"vda") else {
        return Outcome::Fail("no device");
    };
    if d.name().as_str() != "vda" || d.parent().is_some() {
        return Outcome::Fail("name");
    }
    let (Ok(bs), Ok(cap)) = (d.logical_block_size(), d.capacity_sectors()) else {
        return Outcome::Fail("geometry");
    };
    if bs == 0 || bs % 512 != 0 {
        return Outcome::Fail("bs");
    }
    if cap < 16 {
        return Outcome::Fail("cap");
    }
    if d.state() != DeviceState::Ready {
        return Outcome::Fail("state");
    }
    if bs != 512 {
        return Outcome::Fail("need 512");
    }
    let mut buf = [0u8; 512];
    let mut i = 0usize;
    while i < 512 {
        buf[i] = (i as u8).wrapping_add(0xA1);
        i += 1;
    }
    if d.write_dev(1, &buf).is_err() {
        return Outcome::Fail("write");
    }
    let mut out = [0u8; 512];
    if d.read_dev(1, &mut out).is_err() {
        return Outcome::Fail("read");
    }
    if out != buf {
        return Outcome::Fail("mismatch");
    }
    // unaligned multi-sector: 3 sectors not at LBA 0
    let mut multi = [0u8; 1536];
    i = 0;
    while i < 1536 {
        multi[i] = (i as u8).wrapping_add(0x5C);
        i += 1;
    }
    if d.write_dev(5, &multi).is_err() {
        return Outcome::Fail("multi write");
    }
    let mut mout = [0u8; 1536];
    if d.read_dev(5, &mut mout).is_err() || mout != multi {
        return Outcome::Fail("multi read");
    }
    if d.flush_dev().is_err() {
        return Outcome::Fail("flush");
    }
    if has_discard() && d.discard(5, 1).is_err() {
        return Outcome::Fail("discard");
    }
    match d.read_dev(0, &mut [0u8; 100]) {
        Err(BlockError::Inval) => {}
        _ => return Outcome::Fail("unaligned buf"),
    }
    match d.write_dev(cap, &buf) {
        Err(BlockError::Inval) => {}
        _ => return Outcome::Fail("past end"),
    }
    Outcome::Ok
}

pub(crate) fn test_block_vblk_irq() -> Outcome {
    if !vda_live() {
        return Outcome::Skip("no virtio-blk");
    }
    let t0 = top_hits();
    let th0 = thread_hits();
    let c0 = completions();
    let buf = [0x3Du8; 512];
    if vda_write(2, &buf).is_err() {
        return Outcome::Fail("write");
    }
    let mut out = [0u8; 512];
    if vda_read(2, &mut out).is_err() || out != buf {
        return Outcome::Fail("read");
    }
    if completions() <= c0 {
        return Outcome::Fail("no complete");
    }
    if top_hits() <= t0 {
        return Outcome::Fail("no top");
    }
    if thread_hits() <= th0 {
        return Outcome::Fail("no thread");
    }
    Outcome::Ok
}

/// Requests `vblk_deep_round` keeps in flight at once.
const VBLK_DEEP_N: usize = 8;

/// One round of `block_vblk_deep`: submit `VBLK_DEEP_N` writes at gapped
/// LBAs 10, 12, ..., so the elevator does not merge them into one VQ
/// request. The request at `bad` asks for 1 sector with 511 bytes, which
/// `virtio_blk_init::submit` refuses with `Inval`. It waits on every waiter
/// whose submit returned `Ok` before it returns, so no completion reaches
/// a waiter in a dead frame (ROADMAP §10.2, F146), and returns its outcome
/// with the number of submitted waiters still pending, counted just
/// before it returns.
fn vblk_deep_round(bad: Option<usize>) -> (Outcome, u32) {
    let waiters = [const { IoWaiter::new() }; VBLK_DEEP_N];
    let mut bufs = [[0u8; 512]; VBLK_DEEP_N];
    let mut submitted = [false; VBLK_DEEP_N];
    let mut first: Option<&'static str> = None;
    let mut i = 0usize;
    while i < VBLK_DEEP_N {
        bufs[i] = [0x10u8.wrapping_add(i as u8); 512];
        let len = if bad == Some(i) { 511 } else { 512 };
        let lba = 10 + 2 * i as u64;
        let (ptr, w) = (bufs[i].as_ptr() as usize, &waiters[i]);
        let sub = vda(|b| virtio_blk_init::submit(b, Op::Write, lba, 1, ptr, len, w))
            .unwrap_or(Err(BlockError::Gone));
        match sub {
            Ok(()) => submitted[i] = true,
            Err(_) => {
                if first.is_none() {
                    first = Some("submit");
                }
            }
        }
        i += 1;
    }
    i = 0;
    while i < VBLK_DEEP_N {
        if submitted[i] && waiters[i].wait().is_err() && first.is_none() {
            first = Some("wait");
        }
        i += 1;
    }
    let mut pending = 0u32;
    i = 0;
    while i < VBLK_DEEP_N {
        if submitted[i] && waiters[i].poll().is_none() {
            pending += 1;
        }
        i += 1;
    }
    match first {
        Some(why) => (Outcome::Fail(why), pending),
        None => (Outcome::Ok, pending),
    }
}

pub(crate) fn test_block_vblk_deep() -> Outcome {
    if !vda_live() {
        return Outcome::Skip("no virtio-blk");
    }
    match vblk_deep_round(None) {
        (Outcome::Ok, 0) => {}
        (Outcome::Ok, n) => return crate::fail_fmt!("round: {} pending", n),
        (Outcome::Fail(why), _) => return Outcome::Fail(why),
        _ => return Outcome::Fail("round"),
    }
    let mut out = [0u8; 512];
    if vda_read(10, &mut out).is_err() || out != [0x10u8; 512] {
        return Outcome::Fail("r0");
    }
    if vda_read(18, &mut out).is_err() || out != [0x14u8; 512] {
        return Outcome::Fail("r4");
    }
    if vda_read(24, &mut out).is_err() || out != [0x17u8; 512] {
        return Outcome::Fail("r7");
    }
    match vblk_deep_round(Some(3)) {
        (Outcome::Fail("submit"), 0) => Outcome::Ok,
        (Outcome::Fail("submit"), n) => crate::fail_fmt!("bad round: {} pending", n),
        (Outcome::Ok, _) => Outcome::Fail("bad round: 511-byte request accepted"),
        (Outcome::Fail(why), _) => crate::fail_fmt!("bad round: {}, want submit", why),
        _ => Outcome::Fail("bad round"),
    }
}

const VBLK_ITERS: u32 = 40;

const VBLK_SPAN: u64 = 16;

static VBLK_WID: AtomicU32 = AtomicU32::new(0);

static VBLK_DONE: AtomicU32 = AtomicU32::new(0);

static VBLK_FAIL: AtomicU32 = AtomicU32::new(0);

fn vblk_worker() {
    let id = VBLK_WID.fetch_add(1, Ordering::SeqCst);
    let base = 32 + id as u64 * VBLK_SPAN;
    let mut i = 0u32;
    while i < VBLK_ITERS {
        let lba = base + (i as u64 % VBLK_SPAN);
        let mut buf = [0u8; 512];
        let mut j = 0usize;
        while j < 512 {
            buf[j] = (id as u8).wrapping_add(i as u8).wrapping_add(j as u8);
            j += 1;
        }
        if vda_write(lba, &buf).is_err() {
            VBLK_FAIL.fetch_add(1, Ordering::SeqCst);
            break;
        }
        let mut out = [0u8; 512];
        if vda_read(lba, &mut out).is_err() || out != buf {
            VBLK_FAIL.fetch_add(1, Ordering::SeqCst);
            break;
        }
        i += 1;
    }
    VBLK_DONE.fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn test_block_vblk_concurrent() -> Outcome {
    if !vda_live() {
        return Outcome::Skip("no virtio-blk");
    }
    VBLK_WID.store(0, Ordering::SeqCst);
    VBLK_DONE.store(0, Ordering::SeqCst);
    VBLK_FAIL.store(0, Ordering::SeqCst);
    let Ok(_a) = thread_init::spawn("vblk-a", vblk_worker) else {
        return Outcome::Fail("spawn");
    };
    let Ok(_b) = thread_init::spawn("vblk-b", vblk_worker) else {
        return Outcome::Fail("spawn");
    };
    let t0 = time_init::uptime_ms();
    loop {
        if VBLK_DONE.load(Ordering::SeqCst) == 2 {
            break;
        }
        if time_init::uptime_ms().saturating_sub(t0) > 15_000 {
            return Outcome::Fail("stall");
        }
        thread_init::yield_now();
    }
    if VBLK_FAIL.load(Ordering::SeqCst) != 0 {
        return Outcome::Fail("corrupt");
    }
    Outcome::Ok
}

pub(crate) fn test_block_vblk_mq() -> Outcome {
    if !vda_live() {
        return Outcome::Skip("no virtio-blk");
    }
    let nq = vda(|b| b.num_queues()).unwrap_or(0);
    if nq == 0 {
        return Outcome::Fail("zero queues");
    }
    let cpus = per_cpu_init::online_mask().count_ones() as u8;
    if has_mq() {
        if nq < 2 && cpus >= 2 {
            return Outcome::Fail("mq expected");
        }
        if nq > cpus {
            return Outcome::Fail("nq > cpus");
        }
    } else if nq != 1 {
        return Outcome::Fail("sq fallback");
    }
    Outcome::Ok
}

const PERSIST_MAGIC: [u8; 8] = *b"vibeOS7B";

pub(crate) fn test_block_persist() -> Outcome {
    if !vda_live() {
        return Outcome::Skip("no virtio-blk");
    }
    let lba = persist_lba();
    if lba == 0 {
        return Outcome::Fail("no persist lba");
    }
    let mut buf = [0u8; 512];
    if vda_read(lba, &mut buf).is_err() {
        return Outcome::Fail("read");
    }
    if buf[0..8] == PERSIST_MAGIC {
        let mut i = 8usize;
        while i < 512 {
            if buf[i] != 0xA5 {
                return Outcome::Fail("corrupt");
            }
            i += 1;
        }
        crate::marker!("vibeOS: persist: intact");
        return Outcome::Ok;
    }
    buf[0..8].copy_from_slice(&PERSIST_MAGIC);
    let mut i = 8usize;
    while i < 512 {
        buf[i] = 0xA5;
        i += 1;
    }
    if vda_write(lba, &buf).is_err() {
        return Outcome::Fail("write");
    }
    if vda_flush().is_err() {
        return Outcome::Fail("flush");
    }
    crate::marker!("vibeOS: persist: wrote");
    Outcome::Ok
}

// ---- block_two_disk_instances: ROADMAP §10.4's driver and volume
// instances (D2).

/// Run `f` on vdb's instance; `None` when vdb is not bound.
fn vdb<R>(f: impl FnOnce(&VirtioBlk) -> R) -> Option<R> {
    virtio_blk_init::with_disk(b"vdb", f)
}

/// The instances the device registry owns for bound virtio-blk functions,
/// and how many there are.
fn blk_instances() -> ([Option<Instance>; 2], usize) {
    let mut out: [Option<Instance>; 2] = [const { None }; 2];
    let mut n = 0usize;
    let mut i = 0usize;
    while let Some(d) = crate::dev_init::get(i) {
        i += 1;
        if crate::dev_init::bound(&d) != Some("virtio-blk") {
            continue;
        }
        if let (Some(slot), Some(inst)) = (out.get_mut(n), crate::dev_init::instance(&d)) {
            *slot = Some(inst);
        }
        n += 1;
    }
    (out, n)
}

/// 1: two functions, two instances, two disks.
fn two_instances() -> Result<(), &'static str> {
    let (insts, n) = blk_instances();
    if n != 2 {
        return Err("want exactly two bound virtio-blk functions");
    }
    let (Some(a), Some(b)) = (&insts[0], &insts[1]) else {
        return Err("a bound function without an instance");
    };
    if vibeos::dev::same_instance(a, b) {
        return Err("both functions share one instance");
    }
    let (Some(ia), Some(ib)) = (a.downcast_ref::<VirtioBlk>(), b.downcast_ref::<VirtioBlk>())
    else {
        return Err("instance type");
    };
    if (ia.name(), ib.name()) != ("vda", "vdb") || ia.dev().same(ib.dev()) {
        return Err("names not vda, vdb in bind order");
    }
    if vda(|b| b.capacity_sectors()) != Some(8192) || vdb(|b| b.capacity_sectors()) != Some(2048) {
        return Err("capacities not 8192 and 2048");
    }
    if ia.has_flush() != ib.has_flush() {
        return Err("same device model, different flush support");
    }
    let lookup = crate::block::blockdev_init::lookup;
    match (lookup(b"vda"), lookup(b"vdb")) {
        (Some(x), Some(y)) if x.id() != y.id() => Ok(()),
        (Some(_), Some(_)) => Err("vda and vdb share a block id"),
        _ => Err("vda or vdb not registered"),
    }
}

/// A disk's `(completions, thread_hits)`.
fn counters(b: &VirtioBlk) -> (u32, u32) {
    (b.completions(), b.thread_hits())
}

fn vdb_write(lba: u64, buf: &[u8]) -> Result<(), BlockError> {
    vdb(|b| b.write(lba, buf)).unwrap_or(Err(BlockError::Gone))
}

fn vdb_read(lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
    vdb(|b| b.read(lba, buf)).unwrap_or(Err(BlockError::Gone))
}

/// 2: I/O on each disk reaches its own device, on its own queues and
/// vectors.
fn io_on_each() -> Result<(), &'static str> {
    let (a0, b0) = (vda(counters), vdb(counters));
    let (wa, wb) = ([0xA5u8; 512], [0x5Au8; 512]);
    vda_write(2, &wa).map_err(|_| "vda write")?;
    vdb_write(2, &wb).map_err(|_| "vdb write")?;
    let (mut ra, mut rb) = ([0u8; 512], [0u8; 512]);
    vda_read(2, &mut ra).map_err(|_| "vda read")?;
    vdb_read(2, &mut rb).map_err(|_| "vdb read")?;
    if ra != wa || rb != wb {
        return Err("a disk read back the other's pattern");
    }
    let (Some(a0), Some(b0), Some(a1), Some(b1)) = (a0, b0, vda(counters), vdb(counters)) else {
        return Err("vda or vdb gone");
    };
    if a1.0 <= a0.0 || b1.0 <= b0.0 {
        return Err("a disk's completions did not advance");
    }
    if a1.1 <= a0.1 || b1.1 <= b0.1 {
        return Err("a disk's bottom half did not run");
    }
    Ok(())
}

/// 3: a failure injected on vdb fails vdb's request alone.
fn fails_alone() -> Result<(), &'static str> {
    let buf = [0x77u8; 512];
    vdb(|b| b.inject_unsupp(1)).ok_or("no vdb")?;
    match vdb_write(3, &buf) {
        Err(BlockError::Inval) => {}
        Ok(()) => return Err("injected failure not reported"),
        Err(_) => return Err("injected failure: wrong error"),
    }
    vda_write(3, &buf).map_err(|_| "vda write after vdb's failure")?;
    let mut out = [0u8; 512];
    vda_read(3, &mut out).map_err(|_| "vda read after vdb's failure")?;
    vdb_write(3, &buf).map_err(|_| "vdb write after its failure")?;
    let ready = |b: &VirtioBlk| b.state() == DeviceState::Ready;
    if vda(ready) != Some(true) || vdb(ready) != Some(true) {
        return Err("a disk not Ready after an injected failure");
    }
    Ok(())
}

const PROBE: &str = "/vdb/d2probe";
const PROBE_DATA: &[u8] = b"vdb is its own volume";

/// Write [`PROBE`], or read it back and compare.
fn probe_file(write: bool) -> Result<(), &'static str> {
    file_rw(PROBE, PROBE_DATA, write)
}

/// Write `data` to `path`, or read `path` back and compare it with `data`
/// (at most 32 bytes).
fn file_rw(path: &str, data: &[u8], write: bool) -> Result<(), &'static str> {
    use vibeos::fs::{O_CREAT, O_RDONLY, O_RDWR, O_TRUNC};
    let flags = if write {
        O_RDWR | O_CREAT | O_TRUNC
    } else {
        O_RDONLY
    };
    let f = fid::open(path, flags, 0o644).map_err(|_| "open probe")?;
    let r = if write {
        match fid::write(f, data) {
            Ok(n) if n == data.len() => Ok(()),
            _ => Err("write probe"),
        }
    } else {
        let mut buf = [0u8; 32];
        match fid::read(f, &mut buf) {
            Ok(n) if buf.get(..n) == Some(data) => Ok(()),
            _ => Err("read probe back"),
        }
    };
    let c = fid::close(f).map_err(|_| "close probe");
    r.and(c)
}

/// vdb's block entry holds a vibefs volume.
fn vdb_holds_vibefs() -> bool {
    crate::block::blockdev_init::lookup(b"vdb")
        .and_then(|r| crate::block::blockdev_init::holder(&r))
        .is_some_and(|h| h.downcast_ref::<VibeVolume>().is_some())
}

/// 4: a vibefs on vdb is its own volume instance, which vdb's entry holds.
fn volume_on_vdb() -> Result<(), &'static str> {
    vibefs_init::mkfs_dev("vdb").map_err(|_| "mkfs vdb")?;
    file_init::mkdir(b"/vdb", 0o755).map_err(|_| "mkdir /vdb")?;
    vibefs_init::mount_dev("vdb", "/vdb", false).map_err(|_| "mount vdb")?;
    let r = mounted_checks();
    let u = file_init::umount(b"/vdb").map_err(|_| "umount /vdb");
    r.and(u)?;
    if crate::block::blockdev_init::lookup(b"vdb")
        .and_then(|r| crate::block::blockdev_init::holder(&r))
        .is_some()
    {
        return Err("vdb still holds a volume after umount");
    }
    vibefs_init::mount_dev("vdb", "/vdb", false).map_err(|_| "remount vdb")?;
    let r = probe_file(false);
    let u = file_init::umount(b"/vdb").map_err(|_| "umount /vdb again");
    r.and(u)
}

fn mounted_checks() -> Result<(), &'static str> {
    if !vdb_holds_vibefs() {
        return Err("vdb's entry does not hold a vibefs volume");
    }
    probe_file(true)?;
    probe_file(false)?;
    if fid::stat_path("/vibe/d2probe").err() != Some(vibeos::fs::FsError::NotFound) {
        return Err("/vibe shows vdb's file");
    }
    match fat_init::mount_dev("vdb", "/vdb2", false) {
        Err(vibeos::fs::FsError::Busy) => Ok(()),
        Ok(()) => {
            let _ = file_init::umount(b"/vdb2");
            Err("FAT mounted over vdb's vibefs")
        }
        Err(_) => Err("FAT mount of vdb: want Busy"),
    }
}

/// Step 5's FAT volumes: one on `vda`'s second partition, one on `vdb`.
const FAT_DEV_A: &str = "vdap2";
const FAT_DEV_B: &str = "vdb";
const FAT_MNT_A: &str = "/d2fa";
const FAT_MNT_B: &str = "/d2fb";

/// The FAT volume instance block device `name`'s entry holds, if any.
fn fat_holder(name: &str) -> Option<Instance> {
    crate::block::blockdev_init::lookup(name.as_bytes())
        .and_then(|r| crate::block::blockdev_init::holder(&r))
        .filter(|h| h.downcast_ref::<fat_init::FatVolume>().is_some())
}

/// Format block device `dev` as FAT and mount it on `at`.
fn fat_mount(dev: &str, at: &str) -> Result<(), &'static str> {
    crate::fs::ktest::fat_image_to(dev.as_bytes())?;
    match file_init::mkdir(at.as_bytes(), 0o755) {
        Ok(()) | Err(vibeos::fs::FsError::Exists) => {}
        Err(_) => return Err("mkdir a FAT mountpoint"),
    }
    fat_init::mount_dev(dev, at, false).map_err(|_| "mount a FAT volume")
}

/// 5: one FAT module, two disks, two volume instances mounted at once:
/// each device's entry holds its own, each file stays on its own volume,
/// and each unmount drops its own device's instance alone.
fn two_fat_volumes() -> Result<(), &'static str> {
    fat_mount(FAT_DEV_A, FAT_MNT_A)?;
    let r = fat_mount(FAT_DEV_B, FAT_MNT_B).and_then(|()| fat_pair_checks());
    let ub = file_init::umount(FAT_MNT_B.as_bytes()).map_err(|_| "umount /d2fb");
    let b_alone = fat_holder(FAT_DEV_B).is_none() && fat_holder(FAT_DEV_A).is_some();
    let ua = file_init::umount(FAT_MNT_A.as_bytes()).map_err(|_| "umount /d2fa");
    let da = file_init::rmdir(FAT_MNT_A.as_bytes()).map_err(|_| "rmdir /d2fa");
    let db = file_init::rmdir(FAT_MNT_B.as_bytes()).map_err(|_| "rmdir /d2fb");
    r.and(ub).and(ua).and(da).and(db)?;
    if !b_alone {
        return Err("umount /d2fb did not drop vdb's instance alone");
    }
    if fat_holder(FAT_DEV_A).is_some() {
        return Err("vdap2 still holds a volume after umount");
    }
    Ok(())
}

fn fat_pair_checks() -> Result<(), &'static str> {
    let (Some(a), Some(b)) = (fat_holder(FAT_DEV_A), fat_holder(FAT_DEV_B)) else {
        return Err("a disk's entry does not hold a FAT volume");
    };
    if vibeos::dev::same_instance(&a, &b) {
        return Err("two disks share one FAT volume instance");
    }
    file_rw("/d2fa/f", b"fat volume on vdap2", true)?;
    file_rw("/d2fb/f", b"fat volume on vdb", true)?;
    file_rw("/d2fa/f", b"fat volume on vdap2", false)?;
    file_rw("/d2fb/f", b"fat volume on vdb", false)?;
    file_rw("/d2fb/g", b"only on vdb", true)?;
    if fid::stat_path("/d2fa/g").err() != Some(vibeos::fs::FsError::NotFound) {
        return Err("/d2fa shows vdb's file");
    }
    Ok(())
}

/// The volume thread has finished.
static TWO_DONE: AtomicBool = AtomicBool::new(false);
/// Why the volume thread failed; `None` when it passed.
static TWO_WHY: SpinMutex<Option<&'static str>> = SpinMutex::with_rank(None, RANK_DEVICE);

fn two_volume_worker() {
    *TWO_WHY.lock() = volume_on_vdb().and_then(|()| two_fat_volumes()).err();
    // Release: pairs with the test's Acquire load of `TWO_DONE`.
    TWO_DONE.store(true, Ordering::Release);
}

/// ROADMAP §10.4 (D2): the two ktest disks are two driver instances, owned
/// by their PCI entries, each with its own name, I/O, interrupts and
/// failure; a vibefs on vdb is a volume instance its block entry holds,
/// and so are two FAT volumes on vdap2 and vdb mounted at once, on a
/// thread with `spawn`'s default 16 KiB stack.
pub(crate) fn test_block_two_disk_instances() -> Outcome {
    if let Err(why) = two_instances()
        .and_then(|()| io_on_each())
        .and_then(|()| fails_alone())
    {
        return Outcome::Fail(why);
    }
    TWO_DONE.store(false, Ordering::Relaxed);
    *TWO_WHY.lock() = None;
    let Ok(_t) = thread_init::spawn("d2-vol", two_volume_worker) else {
        return Outcome::Fail("spawn");
    };
    let t0 = time_init::uptime_ms();
    while !TWO_DONE.load(Ordering::Acquire) {
        if time_init::uptime_ms().saturating_sub(t0) > 20_000 {
            return Outcome::Fail("volume thread stalled");
        }
        thread_init::yield_now();
    }
    match *TWO_WHY.lock() {
        Some(why) => Outcome::Fail(why),
        None => Outcome::Ok,
    }
}

/// ROADMAP §10.12 (F116): a virtio probe that fails after `QENABLE`
/// resets its device and turns bus mastering off before its frames go
/// back. The virtio-blk function at `PROBE_BLK_BDF` failed at boot; the
/// rng half removes and re-probes the bound rng.
pub(crate) fn test_virtio_probe_fail_quiesces() -> Outcome {
    use crate::dev::ktest::{PROBE_BLK_BDF, probe_fails_quiesced, rng_fail_after_qenable_case};
    use vibeos::dev::Driver;

    let Some(d) = crate::dev_init::find_bdf(PROBE_BLK_BDF) else {
        return Outcome::Fail("no virtio-blk at 00:1e.0");
    };
    if crate::dev_init::bound(&d).is_some() {
        return Outcome::Fail("reserved virtio-blk bound");
    }
    let probe = || virtio_blk_init::BLK_DRV.probe(&d).is_ok();
    if let Err(why) = probe_fails_quiesced(PROBE_BLK_BDF, probe) {
        return Outcome::Fail(why);
    }
    rng_fail_after_qenable_case()
}

// ---- vda as a pattern image in its own boot (`run_ktest.py`'s
// `_vblk_readonly_boot` and `_vblk_bad_sector_boot`).

/// Every byte of sector n of the pattern image is `(n & 0xFF) ^
/// PATTERN_XOR`. Harness twin: `harness.VBLK_PATTERN_XOR`.
const PATTERN_XOR: u8 = 0xA5;
/// The sector the read-only test reads and tries to write. The harness
/// has no twin: any sector of the pattern image serves.
const RO_SECTOR: u64 = 100;
/// The sector whose reads QEMU's `blkdebug` fails; `fs::ktest`'s
/// `fat_bad_sector_eio` puts files on it. Harness twin:
/// `harness.VBLK_BAD_SECTOR`.
pub(crate) const BAD_SECTOR: u64 = 4096;

/// vda's registry handle; its `_dev` calls reach the driver below the
/// page cache, as `block_vblk_rw`'s do.
fn vda_ref() -> Result<vibeos::block::blockdev::BlockRef, &'static str> {
    crate::block::blockdev_init::lookup(b"vda").ok_or("no vda")
}

/// Read sector `lba` of the pattern image and check it.
fn pattern_read(d: &vibeos::block::blockdev::BlockRef, lba: u64) -> Result<(), &'static str> {
    let mut buf = [0u8; 512];
    d.read_dev(lba, &mut buf)
        .map_err(|_| "pattern read failed")?;
    let want = (lba as u8) ^ PATTERN_XOR;
    if buf.iter().any(|&b| b != want) {
        return Err("sector does not hold the pattern: vda is not the pattern image");
    }
    Ok(())
}

fn io_reqs() -> u64 {
    vda(|b| b.io_reqs()).unwrap_or(0)
}

fn vda_ready() -> bool {
    vda(|b| b.state() == DeviceState::Ready).unwrap_or(false)
}

fn readonly_steps() -> Result<(), &'static str> {
    if vda(|b| b.features() & vibeos::virtio_blk::F_RO) != Some(vibeos::virtio_blk::F_RO) {
        return Err("F_RO not negotiated: vda is not a readonly=on disk");
    }
    let d = vda_ref()?;
    pattern_read(&d, RO_SECTOR)?;
    let before = io_reqs();
    let buf = [0x5Au8; 512];
    match d.write_dev(RO_SECTOR, &buf) {
        Err(BlockError::ReadOnly) => {}
        Ok(()) => return Err("write to a read-only device succeeded"),
        Err(_) => return Err("write to a read-only device: wrong error"),
    }
    if io_reqs() != before {
        return Err("the refused write reached the device");
    }
    match d.discard(RO_SECTOR, 1) {
        Err(BlockError::ReadOnly) => {}
        Ok(()) => return Err("discard on a read-only device succeeded"),
        Err(_) => return Err("discard on a read-only device: wrong error"),
    }
    if io_reqs() != before {
        return Err("the refused discard reached the device");
    }
    // Through the page cache too: refused before a page is dirtied that
    // could never be written back.
    match d.write(RO_SECTOR, &buf) {
        Err(BlockError::ReadOnly) => {}
        Ok(()) => return Err("a cached write to a read-only device succeeded"),
        Err(_) => return Err("a cached write to a read-only device: wrong error"),
    }
    let mut back = [0u8; 512];
    if d.read(RO_SECTOR, &mut back).is_err() || back == buf {
        return Err("a read after the refused cached write failed or got its bytes");
    }
    pattern_read(&d, RO_SECTOR)?;
    if !vda_ready() {
        return Err("vda not Ready after a refused write");
    }
    Ok(())
}

/// virtio-blk `F_RO` (ROADMAP §10.11, F046): a write and a discard fail
/// at once with `ReadOnly`, below the page cache and through it, and reads
/// go on. Opt-in: its boot's vda is a `readonly=on` pattern image.
pub(crate) fn vblk_readonly() -> Outcome {
    if !vda_live() {
        return Outcome::Fail("no virtio-blk");
    }
    match readonly_steps() {
        Ok(()) => Outcome::Ok,
        Err(why) => Outcome::Fail(why),
    }
}

fn bad_sector_steps() -> Result<(), &'static str> {
    let d = vda_ref()?;
    if d.logical_block_size() != Ok(512) {
        return Err("vda's logical block size is not 512");
    }
    pattern_read(&d, BAD_SECTOR - 1)?;
    pattern_read(&d, BAD_SECTOR + 1)?;
    let before = io_reqs();
    let mut buf = [0u8; 512];
    match d.read_dev(BAD_SECTOR, &mut buf) {
        Err(BlockError::Io) => {}
        Ok(()) => return Err("bad sector read succeeded: vda has no blkdebug fault"),
        Err(_) => return Err("bad sector read: wrong error"),
    }
    let tries = io_reqs().wrapping_sub(before);
    if tries < 1 + u64::from(vibeos::block::DEFAULT_RETRY_BUDGET) {
        return Err("bad sector read failed before its retry budget was spent");
    }
    if !vda_ready() {
        return Err("vda not Ready after one sector's read failed");
    }
    for lba in [0, BAD_SECTOR - 1, BAD_SECTOR + 1] {
        pattern_read(&d, lba)?;
    }
    Ok(())
}

/// A virtio-blk request that exhausts its retries fails alone (ROADMAP
/// §10.11, F046). Opt-in: its boot's vda fails every read of
/// [`BAD_SECTOR`] through QEMU's `blkdebug`.
pub(crate) fn vblk_bad_sector() -> Outcome {
    if !vda_live() {
        return Outcome::Fail("no virtio-blk");
    }
    match bad_sector_steps() {
        Ok(()) => Outcome::Ok,
        Err(why) => Outcome::Fail(why),
    }
}

/// This subsystem's in-guest tests, in run order; `crate::ktest::GROUPS`
/// runs them (DESIGN §8.2).
pub(crate) const TESTS: &[Test] = &[
    test("block_vblk_rw", test_block_vblk_rw),
    test("block_vblk_irq", test_block_vblk_irq),
    test("block_vblk_deep", test_block_vblk_deep),
    test("block_vblk_concurrent", test_block_vblk_concurrent),
    test("block_vblk_mq", test_block_vblk_mq),
    test("block_persist", test_block_persist),
    test("block_two_disk_instances", test_block_two_disk_instances).deadline(30_000),
    test(
        "virtio_probe_fail_quiesces",
        test_virtio_probe_fail_quiesces,
    ),
    test("vblk_readonly", vblk_readonly).opt_in(),
    test("vblk_bad_sector", vblk_bad_sector).opt_in(),
];
