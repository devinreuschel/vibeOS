//! In-guest tests for drivers (kernel_tests only). Rows: [`TESTS`].

use core::sync::atomic::{AtomicU32, Ordering};

use vibeos::block::{BlockError, DeviceState, Op};
use vibeos::dev::Device;
use vibeos::virtio_blk::{F_DISCARD, F_MQ};

use crate::block_init::IoWaiter;
use crate::ktest::{Outcome, Test, test};
use crate::per_cpu_init;
use crate::thread_init;
use crate::time_init;
use crate::virtio_blk_init;

// ---- Observers the drivers tests read. Their state stays in
// `virtio_blk_init` as `pub(super)` items.

fn features() -> u64 {
    virtio_blk_init::FEATURES.load(Ordering::Acquire)
}

fn has_mq() -> bool {
    features() & F_MQ != 0 && virtio_blk_init::num_queues() > 1
}

fn has_discard() -> bool {
    features() & F_DISCARD != 0
}

/// Runs of the virtio-blk top half (`blk_top`).
pub(crate) fn top_hits() -> u32 {
    virtio_blk_init::TOP_HITS.load(Ordering::Acquire)
}

fn thread_hits() -> u32 {
    virtio_blk_init::THREAD_HITS.load(Ordering::Acquire)
}

fn completions() -> u32 {
    virtio_blk_init::COMPLETIONS.load(Ordering::Acquire)
}

/// `Flush` requests dispatched to vda, emulated-`Fua` ones and those
/// finished locally without `F_FLUSH` included.
pub(crate) fn flushes() -> u64 {
    virtio_blk_init::FLUSHES.load(Ordering::Relaxed)
}

/// A test LBA inside the Linux GPT partition (which starts at 512), not
/// the GPT backup.
pub(crate) fn persist_lba() -> u64 {
    const LBA: u64 = 2048;
    let cap = virtio_blk_init::capacity_sectors();
    if cap > LBA + 1 {
        LBA
    } else {
        cap.saturating_sub(1)
    }
}

fn find_blk() -> Option<(usize, Device)> {
    crate::dev::ktest::find_id(0x1af4, 0x1042)
        .or_else(|| crate::dev::ktest::find_id(0x1af4, 0x1001))
}

pub(crate) fn test_block_vblk_rw() -> Outcome {
    if !virtio_blk_init::live() {
        return Outcome::Skip("no virtio-blk");
    }
    let Some((_, d)) = find_blk() else {
        return Outcome::Fail("id missing");
    };
    match d.bound {
        Some("virtio-blk") => {}
        Some(_) => return Outcome::Fail("wrong driver"),
        None => return Outcome::Fail("unbound"),
    }
    // vda's registry handle; its `_dev` calls reach the driver below the
    // page cache, as this test always has.
    let Some(d) = crate::block::blockdev_init::lookup(b"vda") else {
        return Outcome::Fail("no device");
    };
    if d.name().as_str() != vibeos::virtio_blk::NAME || d.parent().is_some() {
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
    if !virtio_blk_init::live() {
        return Outcome::Skip("no virtio-blk");
    }
    let t0 = top_hits();
    let th0 = thread_hits();
    let c0 = completions();
    let buf = [0x3Du8; 512];
    if virtio_blk_init::write(2, &buf).is_err() {
        return Outcome::Fail("write");
    }
    let mut out = [0u8; 512];
    if virtio_blk_init::read(2, &mut out).is_err() || out != buf {
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

pub(crate) fn test_block_vblk_deep() -> Outcome {
    if !virtio_blk_init::live() {
        return Outcome::Skip("no virtio-blk");
    }
    let w0 = IoWaiter::new();
    let w1 = IoWaiter::new();
    let w2 = IoWaiter::new();
    let w3 = IoWaiter::new();
    let w4 = IoWaiter::new();
    let w5 = IoWaiter::new();
    let w6 = IoWaiter::new();
    let w7 = IoWaiter::new();
    let b0 = [0x10u8; 512];
    let b1 = [0x11u8; 512];
    let b2 = [0x12u8; 512];
    let b3 = [0x13u8; 512];
    let b4 = [0x14u8; 512];
    let b5 = [0x15u8; 512];
    let b6 = [0x16u8; 512];
    let b7 = [0x17u8; 512];
    // gapped LBAs so the elevator does not merge them into one VQ request
    let subs = [
        virtio_blk_init::submit(Op::Write, 10, 1, b0.as_ptr() as usize, 512, &w0),
        virtio_blk_init::submit(Op::Write, 12, 1, b1.as_ptr() as usize, 512, &w1),
        virtio_blk_init::submit(Op::Write, 14, 1, b2.as_ptr() as usize, 512, &w2),
        virtio_blk_init::submit(Op::Write, 16, 1, b3.as_ptr() as usize, 512, &w3),
        virtio_blk_init::submit(Op::Write, 18, 1, b4.as_ptr() as usize, 512, &w4),
        virtio_blk_init::submit(Op::Write, 20, 1, b5.as_ptr() as usize, 512, &w5),
        virtio_blk_init::submit(Op::Write, 22, 1, b6.as_ptr() as usize, 512, &w6),
        virtio_blk_init::submit(Op::Write, 24, 1, b7.as_ptr() as usize, 512, &w7),
    ];
    let mut i = 0usize;
    while i < 8 {
        if subs[i].is_err() {
            return Outcome::Fail("submit");
        }
        i += 1;
    }
    if w0.wait().is_err()
        || w1.wait().is_err()
        || w2.wait().is_err()
        || w3.wait().is_err()
        || w4.wait().is_err()
        || w5.wait().is_err()
        || w6.wait().is_err()
        || w7.wait().is_err()
    {
        return Outcome::Fail("wait");
    }
    let mut out = [0u8; 512];
    if virtio_blk_init::read(10, &mut out).is_err() || out != b0 {
        return Outcome::Fail("r0");
    }
    if virtio_blk_init::read(18, &mut out).is_err() || out != b4 {
        return Outcome::Fail("r4");
    }
    if virtio_blk_init::read(24, &mut out).is_err() || out != b7 {
        return Outcome::Fail("r7");
    }
    Outcome::Ok
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
        if virtio_blk_init::write(lba, &buf).is_err() {
            VBLK_FAIL.fetch_add(1, Ordering::SeqCst);
            break;
        }
        let mut out = [0u8; 512];
        if virtio_blk_init::read(lba, &mut out).is_err() || out != buf {
            VBLK_FAIL.fetch_add(1, Ordering::SeqCst);
            break;
        }
        i += 1;
    }
    VBLK_DONE.fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn test_block_vblk_concurrent() -> Outcome {
    if !virtio_blk_init::live() {
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
    if !virtio_blk_init::live() {
        return Outcome::Skip("no virtio-blk");
    }
    let nq = virtio_blk_init::num_queues();
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
    if !virtio_blk_init::live() {
        return Outcome::Skip("no virtio-blk");
    }
    let lba = persist_lba();
    if lba == 0 {
        return Outcome::Fail("no persist lba");
    }
    let mut buf = [0u8; 512];
    if virtio_blk_init::read(lba, &mut buf).is_err() {
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
    if virtio_blk_init::write(lba, &buf).is_err() {
        return Outcome::Fail("write");
    }
    if virtio_blk_init::flush().is_err() {
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
