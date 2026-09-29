//! In-guest tests for drivers (kernel_tests only). Rows: [`TESTS`].

use core::sync::atomic::{AtomicU32, Ordering};

use vibeos::block::{BlockError, DeviceState, Op};
use vibeos::dev::DevRef;

use crate::block_init::IoWaiter;
use crate::ktest::{Outcome, Test, test};
use crate::per_cpu_init;
use crate::thread_init;
use crate::time_init;
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

/// This subsystem's in-guest tests, in run order; `crate::ktest::GROUPS`
/// runs them (DESIGN §8.2).
pub(crate) const TESTS: &[Test] = &[
    test("block_vblk_rw", test_block_vblk_rw),
    test("block_vblk_irq", test_block_vblk_irq),
    test("block_vblk_deep", test_block_vblk_deep),
    test("block_vblk_concurrent", test_block_vblk_concurrent),
    test("block_vblk_mq", test_block_vblk_mq),
    test("block_persist", test_block_persist),
];
