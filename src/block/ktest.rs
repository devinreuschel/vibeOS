//! In-guest tests for block (kernel_tests only). Rows: [`TESTS`].

use alloc::vec::Vec;
use core::hint::{black_box, spin_loop};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::block::blockdev::BlockRef;
use vibeos::block::{BlockError, DeviceState, Op};
use vibeos::cache::PAGE;

use crate::block::blockdev_init;
use crate::block_init::{self, IoWaiter, testing as blk_testing};
use crate::cache_init::{self, testing};
use crate::ktest::{Outcome, Test, spawn_thread, test};
use crate::part_init;
use crate::thread_init;
use crate::time_init;

// ---- Hooks the block tests arm. Their state stays in the production
// files as `pub(in crate::block)` items.

/// `Flush` requests dispatched to ram0, emulated-`Fua` ones included.
fn flushes() -> u64 {
    block_init::FLUSHES.load(Ordering::Relaxed)
}

/// Hold the next `count` completers just before they take SCHED, each
/// until a submitter calls [`note_return`] or `max_us` passes.
fn arm_finish_stall(count: u32, max_us: u32) {
    blk_testing::STALLS_RUN.store(0, Ordering::Relaxed);
    blk_testing::STALL_MAX_US.store(max_us, Ordering::Relaxed);
    blk_testing::STALLS_LEFT.store(count, Ordering::Release);
}

fn disarm_finish_stall() {
    blk_testing::STALLS_LEFT.store(0, Ordering::Release);
}

/// A submitter's request returned to it; ends a running stall.
fn note_return() {
    blk_testing::RETURNS.fetch_add(1, Ordering::Release);
}

fn finish_stalls_run() -> u32 {
    blk_testing::STALLS_RUN.load(Ordering::Acquire)
}

/// Hold `blk-wb`'s next write of the page holding `byte_off` of `dev`.
fn hold_wb(dev: &BlockRef, byte_off: u64) {
    testing::RELEASE.store(false, Ordering::Release);
    testing::HELD.store(false, Ordering::Release);
    testing::DEV.store(dev.id(), Ordering::Release);
    testing::OFF.store(
        vibeos::cache::CacheKey::page(dev.id(), byte_off).offset,
        Ordering::Release,
    );
}

/// `blk-wb` is stopped at the hold point.
fn held() -> bool {
    testing::HELD.load(Ordering::Acquire)
}

/// Let a held write go, and disarm a hold not yet reached.
fn release() {
    testing::OFF.store(testing::UNARMED_OFF, Ordering::Release);
    testing::RELEASE.store(true, Ordering::Release);
}

/// Whether the partition scan has run.
fn part_live() -> bool {
    crate::part_init::LIVE.load(Ordering::Acquire)
}

/// ram0's registry handle.
fn ram0() -> Option<BlockRef> {
    blockdev_init::lookup(block_init::RAM0_NAME.as_bytes())
}

pub(crate) fn test_block_ramdisk_rw() -> Outcome {
    if !block_init::live() {
        return Outcome::Fail("block not live");
    }
    block_init::reset();
    // Its `_dev` calls reach the driver below the page cache.
    let Some(d) = ram0() else {
        return Outcome::Fail("no ram0");
    };
    if d.name().as_str() != block_init::RAM0_NAME || d.parent().is_some() {
        return Outcome::Fail("name");
    }
    if d.logical_block_size() != Ok(block_init::logical_block_size()) {
        return Outcome::Fail("bs");
    }
    if d.capacity_sectors() != Ok(block_init::capacity_sectors()) {
        return Outcome::Fail("cap");
    }
    if d.state() != DeviceState::Ready {
        return Outcome::Fail("state");
    }
    let mut buf = [0u8; 512];
    let mut i = 0usize;
    while i < 512 {
        buf[i] = (i as u8).wrapping_add(0x3C);
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
    if d.flush_dev().is_err() {
        return Outcome::Fail("flush");
    }
    if d.discard(1, 1).is_err() {
        return Outcome::Fail("discard");
    }
    out = [0u8; 512];
    if d.read_dev(1, &mut out).is_err() || out != buf {
        return Outcome::Fail("discard clobber");
    }
    let mut odd = [0u8; 100];
    match d.read_dev(0, &mut odd) {
        Err(BlockError::Inval) => {}
        _ => return Outcome::Fail("unaligned"),
    }
    match d.write_dev(block_init::RAM0_SECTORS, &buf) {
        Err(BlockError::Inval) => {}
        _ => return Outcome::Fail("past end"),
    }
    match d.discard(block_init::RAM0_SECTORS, 1) {
        Err(BlockError::Inval) => {}
        _ => return Outcome::Fail("discard past"),
    }
    if !crate::shell::ktest::has_command("blk") {
        return Outcome::Fail("no blk");
    }
    if crate::shell_init::dispatch_line("blk").is_err() {
        return Outcome::Fail("blk cmd");
    }
    Outcome::Ok
}

const BLK_ITERS: u32 = 60;

const BLK_SPAN: u64 = 32;

static BLK_WID: AtomicU32 = AtomicU32::new(0);

static BLK_DONE: AtomicU32 = AtomicU32::new(0);

static BLK_FAIL: AtomicU32 = AtomicU32::new(0);

fn blk_worker() {
    let id = BLK_WID.fetch_add(1, Ordering::SeqCst);
    let base = id as u64 * BLK_SPAN;
    let mut i = 0u32;
    while i < BLK_ITERS {
        let lba = base + (i as u64 % BLK_SPAN);
        let mut buf = [0u8; 512];
        let mut j = 0usize;
        while j < 512 {
            buf[j] = (id as u8).wrapping_add(i as u8).wrapping_add(j as u8);
            j += 1;
        }
        if block_init::write(lba, &buf).is_err() {
            BLK_FAIL.fetch_add(1, Ordering::SeqCst);
            break;
        }
        let mut out = [0u8; 512];
        if block_init::read(lba, &mut out).is_err() || out != buf {
            BLK_FAIL.fetch_add(1, Ordering::SeqCst);
            break;
        }
        i += 1;
    }
    BLK_DONE.fetch_add(1, Ordering::SeqCst);
}

static TEAR_ID: AtomicU32 = AtomicU32::new(0);

static TEAR_DONE: AtomicU32 = AtomicU32::new(0);

fn tear_worker() {
    let id = TEAR_ID.fetch_add(1, Ordering::SeqCst);
    let fill = if id == 0 { 0xAAu8 } else { 0x55u8 };
    let buf = [fill; 512];
    if block_init::write(0, &buf).is_err() {
        BLK_FAIL.fetch_add(1, Ordering::SeqCst);
    }
    TEAR_DONE.fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn test_block_concurrent() -> Outcome {
    if !block_init::live() {
        return Outcome::Fail("block not live");
    }
    block_init::reset();
    BLK_WID.store(0, Ordering::SeqCst);
    BLK_DONE.store(0, Ordering::SeqCst);
    BLK_FAIL.store(0, Ordering::SeqCst);
    let Ok(_a) = thread_init::spawn("blk-a", blk_worker) else {
        return Outcome::Fail("spawn");
    };
    let Ok(_b) = thread_init::spawn("blk-b", blk_worker) else {
        return Outcome::Fail("spawn");
    };
    let t0 = time_init::uptime_ms();
    loop {
        if BLK_DONE.load(Ordering::SeqCst) == 2 {
            break;
        }
        if time_init::uptime_ms().saturating_sub(t0) > 8_000 {
            return Outcome::Fail("rw stall");
        }
        thread_init::yield_now();
    }
    if BLK_FAIL.load(Ordering::SeqCst) != 0 {
        return Outcome::Fail("rw corrupt");
    }
    TEAR_ID.store(0, Ordering::SeqCst);
    TEAR_DONE.store(0, Ordering::SeqCst);
    let Ok(_c) = thread_init::spawn("tear-a", tear_worker) else {
        return Outcome::Fail("spawn");
    };
    let Ok(_d) = thread_init::spawn("tear-b", tear_worker) else {
        return Outcome::Fail("spawn");
    };
    let t1 = time_init::uptime_ms();
    loop {
        if TEAR_DONE.load(Ordering::SeqCst) == 2 {
            break;
        }
        if time_init::uptime_ms().saturating_sub(t1) > 8_000 {
            return Outcome::Fail("tear stall");
        }
        thread_init::yield_now();
    }
    if BLK_FAIL.load(Ordering::SeqCst) != 0 {
        return Outcome::Fail("tear write");
    }
    let mut out = [0u8; 512];
    if block_init::read(0, &mut out).is_err() {
        return Outcome::Fail("tear read");
    }
    let b0 = out[0];
    if b0 != 0xAA && b0 != 0x55 {
        return Outcome::Fail("tear pattern");
    }
    let mut i = 1usize;
    while i < 512 {
        if out[i] != b0 {
            return Outcome::Fail("torn sector");
        }
        i += 1;
    }
    Outcome::Ok
}

pub(crate) fn test_block_retry() -> Outcome {
    if !block_init::live() {
        return Outcome::Fail("block not live");
    }
    block_init::reset();
    let mut buf = [0x11u8; 512];
    block_init::inject_io_fails(3);
    if block_init::write(3, &buf).is_err() {
        return Outcome::Fail("retry should pass");
    }
    let mut out = [0u8; 512];
    if block_init::read(3, &mut out).is_err() || out != buf {
        return Outcome::Fail("after retry");
    }
    block_init::inject_io_fails(4);
    match block_init::write(4, &buf) {
        Err(BlockError::Failed) => {}
        _ => return Outcome::Fail("expected failed"),
    }
    if block_init::state() != DeviceState::Failed {
        return Outcome::Fail("not marked failed");
    }
    match block_init::read(3, &mut out) {
        Err(BlockError::Failed) => {}
        _ => return Outcome::Fail("submit after fail"),
    }
    block_init::reset();
    buf[0] = 0x22;
    if block_init::write(3, &buf).is_err() {
        return Outcome::Fail("reset");
    }
    Outcome::Ok
}

pub(crate) fn test_block_part_mbr() -> Outcome {
    if !part_live() {
        return Outcome::Fail("parts not live");
    }
    let Some(disk) = ram0() else {
        return Outcome::Fail("no ram0");
    };
    let Some(d) = blockdev_init::lookup(b"ram0p1") else {
        return Outcome::Fail("no ram0p1");
    };
    let Some(d2) = blockdev_init::lookup(b"ram0p5") else {
        return Outcome::Fail("no ram0p5");
    };
    let Some(d3) = blockdev_init::lookup(b"ram0p6") else {
        return Outcome::Fail("no ram0p6");
    };
    for p in [&d, &d2, &d3] {
        if p.parent().map(BlockRef::id) != Some(disk.id()) || p.part().is_none() {
            return Outcome::Fail("parent");
        }
    }
    if d.name().as_str() != "ram0p1" {
        return Outcome::Fail("name");
    }
    if d.capacity_sectors() != Ok(32) {
        return Outcome::Fail("cap");
    }
    let mut buf = [0u8; 512];
    buf[0] = 0xC1;
    buf[511] = 0xC2;
    if d.write(0, &buf).is_err() {
        return Outcome::Fail("write");
    }
    let mut out = [0u8; 512];
    if d.read(0, &mut out).is_err() || out != buf {
        return Outcome::Fail("read");
    }
    match d.write(32, &buf) {
        Err(BlockError::Inval) => {}
        _ => return Outcome::Fail("overflow"),
    }
    match d.read(31, &mut [0u8; 1024]) {
        Err(BlockError::Inval) => {}
        _ => return Outcome::Fail("overflow 2"),
    }
    let Some(i2) = d2.part() else {
        return Outcome::Fail("info p2");
    };
    if i2.nsect != 24 {
        return Outcome::Fail("logical size");
    }
    if part_init::type_str(i2.kind) != "linux" {
        return Outcome::Fail("type");
    }
    Outcome::Ok
}

pub(crate) fn test_block_part_gpt() -> Outcome {
    if !crate::drivers::ktest::vda_live() {
        return Outcome::Skip("no virtio-blk");
    }
    let Some(disk) = blockdev_init::lookup(b"vda") else {
        return Outcome::Fail("no vda");
    };
    let Some(d) = blockdev_init::lookup(b"vdap1") else {
        return Outcome::Fail("no vdap1");
    };
    let Some(d2) = blockdev_init::lookup(b"vdap2") else {
        return Outcome::Fail("no vdap2");
    };
    for p in [&d, &d2] {
        if p.parent().map(BlockRef::id) != Some(disk.id()) {
            return Outcome::Fail("parent");
        }
    }
    let Some(i1) = d.part() else {
        return Outcome::Fail("info p1");
    };
    let Some(i2) = d2.part() else {
        return Outcome::Fail("info p2");
    };
    if part_init::type_str(i1.kind) != "efi" {
        return Outcome::Fail("efi guid");
    }
    if part_init::type_str(i2.kind) != "linux" {
        return Outcome::Fail("linux guid");
    }
    if i2.nsect < 16 {
        return Outcome::Fail("linux small");
    }
    let mut buf = [0u8; 512];
    buf[0] = 0xE1;
    buf[100] = 0xE2;
    if d.write(1, &buf).is_err() {
        return Outcome::Fail("write");
    }
    let mut out = [0u8; 512];
    if d.read(1, &mut out).is_err() || out != buf {
        return Outcome::Fail("read");
    }
    if d.flush().is_err() {
        return Outcome::Fail("flush");
    }
    match d.write(i1.nsect, &buf) {
        Err(BlockError::Inval) => {}
        _ => return Outcome::Fail("gpt overflow"),
    }
    Outcome::Ok
}

pub(crate) fn test_block_cache_hit() -> Outcome {
    if !cache_init::live() || !block_init::live() {
        return Outcome::Fail("not live");
    }
    let Some(ram) = ram0() else {
        return Outcome::Fail("no ram0");
    };
    let lba = 200u64;
    let mut buf = [0u8; 512];
    buf[3] = 0x44;
    if block_init::write(lba, &buf).is_err() {
        return Outcome::Fail("seed");
    }
    let raw0 = block_init::io_reqs();
    if block_init::read(lba, &mut [0u8; 512]).is_err() {
        return Outcome::Fail("raw1");
    }
    if block_init::read(lba, &mut [0u8; 512]).is_err() {
        return Outcome::Fail("raw2");
    }
    let raw_delta = block_init::io_reqs().saturating_sub(raw0);
    let s0 = cache_init::stats();
    let mut out = [0u8; 512];
    if ram.read(lba, &mut out).is_err() {
        return Outcome::Fail("c1");
    }
    if out != buf {
        return Outcome::Fail("data");
    }
    let s1 = cache_init::stats();
    if ram.read(lba, &mut out).is_err() || out != buf {
        return Outcome::Fail("c2");
    }
    let s2 = cache_init::stats();
    if s1.device_reads <= s0.device_reads {
        return Outcome::Fail("miss reqs");
    }
    if s2.device_reads != s1.device_reads {
        return Outcome::Fail("hit extra req");
    }
    if s2.hits <= s1.hits {
        return Outcome::Fail("no hit");
    }
    if raw_delta < 2 {
        return Outcome::Fail("raw not 2");
    }
    let cached = s2.device_reads.saturating_sub(s0.device_reads);
    if cached >= raw_delta {
        return Outcome::Fail("no reduce");
    }
    crate::marker!(
        "vibeOS: cache: hits {} misses {} device {} raw {}",
        s2.hits,
        s2.misses,
        s2.device_reqs(),
        raw_delta
    );
    Outcome::Ok
}

pub(crate) fn test_block_cache_evict() -> Outcome {
    if !cache_init::live() || !block_init::live() {
        return Outcome::Fail("not live");
    }
    let Some(ram) = ram0() else {
        return Outcome::Fail("no ram0");
    };
    let s0 = cache_init::stats();
    let mut buf = [0u8; 512];
    let mut i = 0u64;
    while i < 18 {
        let lba = i * 8;
        buf[0] = i as u8;
        if ram.read(lba, &mut buf).is_err() {
            return Outcome::Fail("fill");
        }
        i += 1;
    }
    let s1 = cache_init::stats();
    if s1.evicts <= s0.evicts {
        return Outcome::Fail("no evict");
    }
    Outcome::Ok
}

// ---------------------------------------------------------------------------
// lifetime_iowaiter_publish_last (ROADMAP §10.10, F002; Phase 7 exit gate)

/// Submitter threads; thread `t` owns LBAs `64t..64t+64` of ram0.
const IOW_THREADS: u32 = 4;

/// LBAs per thread.
const IOW_SPAN: u64 = 64;

/// Iterations per thread; each writes one block and reads one: 10,000
/// requests in all.
const IOW_ITERS: u32 = 1_250;

const IOW_REQS: u32 = IOW_THREADS * IOW_ITERS * 2;

/// Completer stalls armed, and each one's bound. The bound exceeds the
/// 10 ms quantum, so a spinner on the stalled CPU gets to run.
const IOW_STALLS: u32 = 32;

const IOW_STALL_US: u32 = 15_000;

const IOW_HANG_NS: u64 = 50_000_000_000;

const SECTOR: usize = block_init::RAM0_BLOCK_SIZE as usize;

static IOW_NEXT: AtomicU32 = AtomicU32::new(0);

static IOW_DONE: AtomicU32 = AtomicU32::new(0);

static IOW_OK: AtomicU32 = AtomicU32::new(0);

static IOW_ERRS: AtomicU32 = AtomicU32::new(0);

static IOW_BAD: AtomicBool = AtomicBool::new(false);

static IOW_BAD_T: AtomicU32 = AtomicU32::new(0);

static IOW_BAD_LBA: AtomicU64 = AtomicU64::new(0);

static IOW_BAD_SEQ: AtomicU32 = AtomicU32::new(0);

static IOW_BAD_BYTE: AtomicU32 = AtomicU32::new(0);

/// The pattern unique to (`lba`, `seq`) for thread `t`: the LBA, then the
/// sequence tagged with the thread, then bytes derived from all three.
fn iow_pattern(buf: &mut [u8; SECTOR], lba: u64, seq: u32, t: u32) {
    buf[..8].copy_from_slice(&lba.to_le_bytes());
    let tag = u64::from(seq) | (u64::from(t) << 48);
    buf[8..16].copy_from_slice(&tag.to_le_bytes());
    let mut i = 16;
    while i < SECTOR {
        buf[i] = lba
            .wrapping_mul(131)
            .wrapping_add(u64::from(seq).wrapping_mul(31))
            .wrapping_add(i as u64) as u8;
        i += 1;
    }
}

/// Submit one request on `slots[j % 2]` and wait for it: threads 0 and 2
/// poll the lock-free `poll()`, threads 1 and 3 call `wait()`. Once it
/// returns, the slot is overwritten with `0xFF`, as a returned frame's
/// reuse would, before the stalled completer is let go.
fn iow_request(
    t: u32,
    slots: &mut [IoWaiter; 2],
    j: u32,
    op: Op,
    lba: u64,
    ptr: usize,
) -> Result<(), BlockError> {
    let slot = &mut slots[(j % 2) as usize];
    // The old value may be poison; `IoWaiter` has no drop glue, so
    // assigning over it reads nothing.
    *slot = IoWaiter::new();
    let r = match block_init::submit(op, lba, 1, ptr, SECTOR, slot) {
        Err(e) => Err(e),
        Ok(()) if t.is_multiple_of(2) => loop {
            if let Some(r) = slot.poll() {
                break r;
            }
            spin_loop();
            thread_init::yield_now();
        },
        Ok(()) => slot.wait(),
    };
    // SAFETY: invariant I11 (DESIGN §2.7), established at
    // `block_init::IoWaiter::wait` and `IoWaiter::poll`: once either has
    // returned the request's result, the completer no longer owns the
    // waiter, so this thread may reuse the slot. Every field of `IoWaiter`
    // is an integer, so all-`0xFF` bytes are a valid value.
    unsafe {
        core::ptr::write_bytes(
            core::ptr::from_mut(slot).cast::<u8>(),
            0xFF,
            core::mem::size_of::<IoWaiter>(),
        );
    }
    black_box(&mut *slot);
    note_return();
    r
}

fn iow_record_bad(t: u32, lba: u64, seq: u32, byte: usize) {
    if IOW_BAD
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        IOW_BAD_T.store(t, Ordering::Relaxed);
        IOW_BAD_LBA.store(lba, Ordering::Relaxed);
        IOW_BAD_SEQ.store(seq, Ordering::Relaxed);
        IOW_BAD_BYTE.store(byte as u32, Ordering::Release);
    }
}

fn iow_worker() {
    let t = IOW_NEXT.fetch_add(1, Ordering::Relaxed);
    let base = u64::from(t) * IOW_SPAN;
    // Last sequence written per owned LBA; `u32::MAX` = not yet written.
    let mut last = [u32::MAX; IOW_SPAN as usize];
    let mut slots = [IoWaiter::new(), IoWaiter::new()];
    let mut wbuf = [0u8; SECTOR];
    let mut rbuf = [0u8; SECTOR];
    let mut expect = [0u8; SECTOR];
    let mut j = 0u32;
    let mut k = 0u32;
    while k < IOW_ITERS {
        let off = (k as u64) % IOW_SPAN;
        iow_pattern(&mut wbuf, base + off, k, t);
        let w = iow_request(
            t,
            &mut slots,
            j,
            Op::Write,
            base + off,
            wbuf.as_ptr() as usize,
        );
        j += 1;
        if w.is_err() {
            IOW_ERRS.fetch_add(1, Ordering::Relaxed);
            break;
        }
        IOW_OK.fetch_add(1, Ordering::Relaxed);
        last[off as usize] = k;

        let mut roff = (u64::from(k) * 29 + 7) % IOW_SPAN;
        if last[roff as usize] == u32::MAX {
            roff = off;
        }
        rbuf.fill(0);
        let r = iow_request(
            t,
            &mut slots,
            j,
            Op::Read,
            base + roff,
            rbuf.as_mut_ptr() as usize,
        );
        j += 1;
        if r.is_err() {
            IOW_ERRS.fetch_add(1, Ordering::Relaxed);
            break;
        }
        IOW_OK.fetch_add(1, Ordering::Relaxed);
        let seq = last[roff as usize];
        iow_pattern(&mut expect, base + roff, seq, t);
        if let Some(b) = (0..SECTOR).find(|&i| rbuf[i] != expect[i]) {
            iow_record_bad(t, base + roff, seq, b);
            break;
        }
        k += 1;
    }
    IOW_DONE.fetch_add(1, Ordering::Release);
}

/// Disarms the completer stall on every return path.
struct DisarmStall;

impl Drop for DisarmStall {
    fn drop(&mut self) {
        disarm_finish_stall();
    }
}

/// ROADMAP §10.10 (F002): with every completer held just before SCHED, and
/// every submitter reusing its waiter's memory as soon as it returns, 10,000
/// ramdisk requests complete with no fault, no hung submitter, and no
/// corrupt read (Phase 7's concurrent reads and writes line).
pub(crate) fn lifetime_iowaiter_publish_last() -> Outcome {
    if !block_init::live() {
        return Outcome::Skip("no ram0");
    }
    block_init::reset();
    IOW_NEXT.store(0, Ordering::Relaxed);
    IOW_DONE.store(0, Ordering::Relaxed);
    IOW_OK.store(0, Ordering::Relaxed);
    IOW_ERRS.store(0, Ordering::Relaxed);
    IOW_BAD.store(false, Ordering::Release);
    arm_finish_stall(IOW_STALLS, IOW_STALL_US);
    let _disarm = DisarmStall;
    let mut i = 0;
    while i < IOW_THREADS {
        spawn_thread("iow", iow_worker);
        i += 1;
    }
    let t0 = time_init::now_ns();
    loop {
        let n = IOW_DONE.load(Ordering::Acquire);
        if n == IOW_THREADS {
            break;
        }
        if time_init::now_ns().saturating_sub(t0) > IOW_HANG_NS {
            return crate::fail_fmt!("hung submitter: {} of {} done", n, IOW_THREADS);
        }
        thread_init::sleep_ms(1);
    }
    if IOW_BAD.load(Ordering::Acquire) {
        return crate::fail_fmt!(
            "corrupt read: t{} lba {} seq {} byte {}",
            IOW_BAD_T.load(Ordering::Relaxed),
            IOW_BAD_LBA.load(Ordering::Relaxed),
            IOW_BAD_SEQ.load(Ordering::Relaxed),
            IOW_BAD_BYTE.load(Ordering::Relaxed)
        );
    }
    let errs = IOW_ERRS.load(Ordering::Relaxed);
    if errs != 0 {
        return crate::fail_fmt!("{} requests returned Err", errs);
    }
    let ok = IOW_OK.load(Ordering::Relaxed);
    if ok != IOW_REQS {
        return crate::fail_fmt!("{} of {} requests completed", ok, IOW_REQS);
    }
    let stalls = finish_stalls_run();
    if stalls != IOW_STALLS {
        return crate::fail_fmt!("{} of {} completer stalls ran", stalls, IOW_STALLS);
    }
    Outcome::Ok
}

/// ram0's last ten pages (sectors 176..256): ten dirty pages put the
/// 16-page cache over its dirty ratio, so `blk-wb` writes them.
const WB_FIRST_LBA: u64 = 176;

const WB_PAGES: usize = 10;

/// How long a flush must stay blocked on the held write.
const BLOCKED_MS: u64 = 200;

const WAIT_NS: u64 = 2_000_000_000;

const FLUSH_PENDING: u32 = 0;

const FLUSH_OK: u32 = 1;

const FLUSH_ERR: u32 = 2;

static FLUSH_RES: AtomicU32 = AtomicU32::new(FLUSH_PENDING);

fn flush_ram0() {
    let res = match ram0().ok_or(BlockError::Gone).and_then(|r| r.flush()) {
        Ok(()) => FLUSH_OK,
        Err(_) => FLUSH_ERR,
    };
    FLUSH_RES.store(res, Ordering::Release);
}

/// Poll `f` until it holds or `WAIT_NS` passes.
fn wait_for(f: impl Fn() -> bool) -> bool {
    let t0 = time_init::now_ns();
    while !f() {
        if time_init::now_ns().saturating_sub(t0) > WAIT_NS {
            return false;
        }
        thread_init::sleep_ms(1);
    }
    true
}

/// Releases the hold and puts the saved pages back through the cache on
/// every path out of [`cache_flush_waits_writeback`].
struct WbGuard {
    saved: Vec<u8>,
    restored: bool,
}

impl WbGuard {
    fn restore(&mut self) -> Result<(), BlockError> {
        release();
        // A failed flush thread may still be waiting: wait out its flush.
        let _pending = wait_for(|| FLUSH_RES.load(Ordering::Acquire) != FLUSH_PENDING);
        let ram = ram0().ok_or(BlockError::Gone)?;
        ram.write(WB_FIRST_LBA, &self.saved)?;
        ram.flush()?;
        self.restored = true;
        Ok(())
    }
}

impl Drop for WbGuard {
    fn drop(&mut self) {
        if !self.restored && self.restore().is_err() {
            crate::klog!(
                vibeos::log::Level::Warn,
                "vibeOS: ktest: cache_flush_waits_writeback: ram0 restore failed"
            );
        }
    }
}

/// `PageCache::flush` sends no `Flush` while `blk-wb` holds one write of
/// the device, and the held page is on the device once it does.
pub(crate) fn cache_flush_waits_writeback() -> Outcome {
    if !cache_init::live() || !block_init::live() {
        return Outcome::Fail("no ram0 cache");
    }
    let Some(ram) = ram0() else {
        return Outcome::Fail("no ram0");
    };
    if ram.flush().is_err() {
        return Outcome::Fail("pre-flush");
    }
    let mut saved = alloc::vec![0u8; WB_PAGES * PAGE];
    if ram.read(WB_FIRST_LBA, &mut saved).is_err() {
        return Outcome::Fail("save");
    }
    FLUSH_RES.store(FLUSH_OK, Ordering::Release);
    let mut guard = WbGuard {
        saved,
        restored: false,
    };
    let mut dirty = alloc::vec![0u8; WB_PAGES * PAGE];
    let mut i = 0usize;
    while i < dirty.len() {
        dirty[i] = guard.saved[i] ^ 0xA5 ^ (i / PAGE) as u8;
        i += 1;
    }
    let held_off = WB_FIRST_LBA * 512;
    hold_wb(&ram, held_off);
    if ram.write(WB_FIRST_LBA, &dirty).is_err() {
        return Outcome::Fail("dirty");
    }
    if !wait_for(held) {
        return Outcome::Fail("blk-wb never held");
    }
    let flushes0 = flushes();
    FLUSH_RES.store(FLUSH_PENDING, Ordering::Release);
    let _flusher = spawn_thread("ktest-flush", flush_ram0);
    thread_init::sleep_ms(BLOCKED_MS);
    if FLUSH_RES.load(Ordering::Acquire) != FLUSH_PENDING {
        return Outcome::Fail("flush returned while a write was held");
    }
    if flushes() != flushes0 {
        return Outcome::Fail("Flush sent while a write was held");
    }
    release();
    if !wait_for(|| FLUSH_RES.load(Ordering::Acquire) != FLUSH_PENDING) {
        return Outcome::Fail("flush never returned");
    }
    if FLUSH_RES.load(Ordering::Acquire) != FLUSH_OK {
        return Outcome::Fail("flush failed");
    }
    if flushes() <= flushes0 {
        return Outcome::Fail("no Flush after release");
    }
    let mut page = [0u8; PAGE];
    if block_init::read(WB_FIRST_LBA, &mut page).is_err() {
        return Outcome::Fail("raw read");
    }
    if page[..] != dirty[..PAGE] {
        return Outcome::Fail("held page not on ram0");
    }
    match guard.restore() {
        Ok(()) => Outcome::Ok,
        Err(_) => Outcome::Fail("restore"),
    }
}

/// The most pages the flush-starvation test dirties during one flush.
const REDIRTY_MAX: u64 = 200;

/// Pages [`flush_redirty`] has dirtied, and the next of the [`WB_PAGES`]
/// from [`WB_FIRST_LBA`] it dirties.
static REDIRTIED: AtomicU64 = AtomicU64::new(0);
static REDIRTY_FAILED: AtomicBool = AtomicBool::new(false);

/// `cache_init`'s hook after each page a flush writes: dirty the next
/// scratch page, as a writer streaming over them beside the flush would,
/// [`REDIRTY_MAX`] times at most.
pub(crate) fn flush_redirty() {
    let n = REDIRTIED.fetch_add(1, Ordering::AcqRel);
    if n >= REDIRTY_MAX {
        return;
    }
    let sectors = (PAGE / 512) as u64;
    let lba = WB_FIRST_LBA + (n % WB_PAGES as u64) * sectors;
    let page = [n as u8 ^ 0x5A; PAGE];
    if ram0()
        .ok_or(BlockError::Gone)
        .and_then(|r| r.write(lba, &page))
        .is_err()
    {
        REDIRTY_FAILED.store(true, Ordering::Release);
    }
}

/// A flush returns while pages of its device keep being dirtied: it writes
/// each page dirty when it began once, where it once took whichever page
/// was dirty next and returned only when none was, so a writer that kept
/// ahead of it held it off for as long as it wrote (B3). The hook dirties
/// another page after each page the flush writes.
pub(crate) fn cache_flush_not_starved() -> Outcome {
    if !cache_init::live() || !block_init::live() {
        return Outcome::Fail("no ram0 cache");
    }
    let Some(ram) = ram0() else {
        return Outcome::Fail("no ram0");
    };
    let mut saved = alloc::vec![0u8; WB_PAGES * PAGE];
    if ram.read(WB_FIRST_LBA, &mut saved).is_err() {
        return Outcome::Fail("save");
    }
    // Every scratch page dirty, so the flush has some to write.
    let mut page = [0u8; PAGE];
    let sectors = (PAGE / 512) as u64;
    for i in 0..WB_PAGES {
        page.fill(i as u8 ^ 0xC3);
        if ram.write(WB_FIRST_LBA + i as u64 * sectors, &page).is_err() {
            return Outcome::Fail("dirty");
        }
    }
    REDIRTIED.store(0, Ordering::Release);
    REDIRTY_FAILED.store(false, Ordering::Release);
    let d0 = cache_init::stats().device_writes;
    testing::REDIRTY.store(true, Ordering::Release);
    let flushed = ram.flush();
    testing::REDIRTY.store(false, Ordering::Release);
    let wrote = cache_init::stats().device_writes.saturating_sub(d0);
    let dirtied = REDIRTIED.load(Ordering::Acquire);
    let restored = ram.write(WB_FIRST_LBA, &saved).and_then(|()| ram.flush());
    if flushed.is_err() {
        return Outcome::Fail("flush failed");
    }
    if REDIRTY_FAILED.load(Ordering::Acquire) {
        return Outcome::Fail("a write beside the flush failed");
    }
    // One sweep writes each slot once.
    if wrote > vibeos::cache::DEFAULT_PAGES as u64 || dirtied >= REDIRTY_MAX {
        return crate::fail_fmt!(
            "flush wrote {wrote} pages while {dirtied} were dirtied beside it"
        );
    }
    if restored.is_err() {
        return Outcome::Fail("restore");
    }
    Outcome::Ok
}

/// A ram0 page past the stamped partitions whose read the fill test holds.
const FILL_LBA: u64 = 208;

/// What each reader of the fill test read, and how it ended.
static FILL_A: AtomicU64 = AtomicU64::new(FILL_PENDING);
static FILL_B: AtomicU64 = AtomicU64::new(FILL_PENDING);
const FILL_PENDING: u64 = u64::MAX;
const FILL_ERR: u64 = u64::MAX - 1;

/// Read `FILL_LBA`'s first word into `out`, or [`FILL_ERR`].
fn read_fill_word(out: &AtomicU64) {
    let mut buf = [0u8; 512];
    let r = ram0()
        .ok_or(())
        .and_then(|d| d.read(FILL_LBA, &mut buf).map_err(|_| ()));
    let w = r.map_or(FILL_ERR, |()| {
        u64::from_le_bytes([
            buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
        ])
    });
    out.store(w, Ordering::Release);
}

fn fill_reader_a() {
    read_fill_word(&FILL_A);
}

fn fill_reader_b() {
    read_fill_word(&FILL_B);
}

/// A read of a page that another read is filling waits for that fill and
/// gets its data: the page has one slot, whose queue the second reader
/// sleeps on however long the device takes, where it once filled a second
/// copy of the page. The first reader's fill is held until the second is
/// seen waiting, or has ended.
pub(crate) fn cache_read_waits_for_fill() -> Outcome {
    if !cache_init::live() || !block_init::live() {
        return Outcome::Fail("no ram0 cache");
    }
    let Some(ram) = ram0() else {
        return Outcome::Fail("no ram0");
    };
    if ram.flush().is_err() {
        return Outcome::Fail("pre-flush");
    }
    let key = vibeos::cache::CacheKey::page(ram.id(), FILL_LBA * 512);
    testing::forget(key);
    FILL_A.store(FILL_PENDING, Ordering::Release);
    FILL_B.store(FILL_PENDING, Ordering::Release);
    testing::FILL_RELEASE.store(false, Ordering::Release);
    testing::FILL_HELD.store(false, Ordering::Release);
    testing::FILL_DEV.store(ram.id(), Ordering::Release);
    testing::FILL_OFF.store(key.offset, Ordering::Release);
    let _a = spawn_thread("ktest-fill-a", fill_reader_a);
    let held = crate::ktest::wait_for(|| testing::FILL_HELD.load(Ordering::Acquire));
    let waits0 = testing::FILL_WAITS.load(Ordering::Acquire);
    let _b = held.then(|| spawn_thread("ktest-fill-b", fill_reader_b));
    let waited = held
        && crate::ktest::sleep_for(|| {
            testing::FILL_WAITS.load(Ordering::Acquire) != waits0
                || FILL_B.load(Ordering::Acquire) != FILL_PENDING
        });
    testing::FILL_OFF.store(UNARMED_FILL, Ordering::Release);
    testing::FILL_RELEASE.store(true, Ordering::Release);
    let done = crate::ktest::sleep_for(|| {
        FILL_A.load(Ordering::Acquire) != FILL_PENDING
            && (!held || FILL_B.load(Ordering::Acquire) != FILL_PENDING)
    });
    let (a, b) = (
        FILL_A.load(Ordering::Acquire),
        FILL_B.load(Ordering::Acquire),
    );
    if !held {
        return Outcome::Fail("the first read's fill was never held");
    }
    if !waited || !done {
        return Outcome::Fail("a reader never ended");
    }
    if testing::FILL_WAITS.load(Ordering::Acquire) == waits0 {
        return Outcome::Fail("the second read filled its own copy of a page being filled");
    }
    if b == FILL_ERR {
        return Outcome::Fail("the second read failed while the first filled the page");
    }
    if a == FILL_ERR || a != b {
        return crate::fail_fmt!("readers got {a:#x} and {b:#x}");
    }
    Outcome::Ok
}

const UNARMED_FILL: u64 = testing::UNARMED_OFF;

/// A ram0 sector past the stamped MBR partitions (`part_init`); restored.
const RAM0_FUA_LBA: u64 = 200;

/// `write_fua` on one sector through `write`/`read`/`write_fua`/`flushes`:
/// the queue sends a `Flush` for it (neither driver has FUA), the data
/// reads back, and the sector is restored.
fn fua_roundtrip(
    lba: u64,
    read: fn(u64, &mut [u8]) -> Result<(), vibeos::block::BlockError>,
    write: fn(u64, &[u8]) -> Result<(), vibeos::block::BlockError>,
    write_fua: fn(u64, &[u8]) -> Result<(), vibeos::block::BlockError>,
    flushes: fn() -> u64,
) -> Outcome {
    let mut saved = [0u8; 512];
    if read(lba, &mut saved).is_err() {
        return Outcome::Fail("save read");
    }
    let mut buf = [0u8; 512];
    let mut i = 0usize;
    while i < buf.len() {
        buf[i] = (i as u8).wrapping_mul(7).wrapping_add(0x3D) ^ saved[i];
        i += 1;
    }
    let before = flushes();
    let res = write_fua(lba, &buf);
    let after = flushes();
    let mut out = [0u8; 512];
    let back = read(lba, &mut out);
    if write(lba, &saved).is_err() {
        return Outcome::Fail("restore");
    }
    if res.is_err() {
        return Outcome::Fail("write_fua");
    }
    if after <= before {
        return Outcome::Fail("no flush for fua");
    }
    if back.is_err() || out != buf {
        return Outcome::Fail("fua data");
    }
    Outcome::Ok
}

pub(crate) fn block_fua_write() -> Outcome {
    if !block_init::live() {
        return Outcome::Fail("no ram0");
    }
    let r = fua_roundtrip(
        RAM0_FUA_LBA,
        block_init::read,
        block_init::write,
        block_init::write_fua,
        flushes,
    );
    if !matches!(r, Outcome::Ok) {
        return r;
    }
    if !crate::drivers::ktest::vda_live() {
        return Outcome::Ok;
    }
    if crate::drivers::ktest::vda(|b| b.logical_block_size()) != Some(512) {
        return Outcome::Skip("vda not 512");
    }
    fua_roundtrip(
        crate::drivers::ktest::persist_lba().saturating_add(1),
        crate::drivers::ktest::vda_read,
        crate::drivers::ktest::vda_write,
        crate::drivers::ktest::vda_write_fua,
        crate::drivers::ktest::flushes,
    )
}

// ---- part_six_entries: a heap-backed disk with a six-entry table.

/// Logical blocks in the [`MemDisk`] `part_six_entries` registers.
const MEM_SECT: u64 = 256;

/// A disk of `bs`-byte logical blocks in a `TryVec`, for tests that
/// register a disk of their own.
struct MemDisk {
    bs: usize,
    data: crate::sync_init::SpinMutex<vibeos::kalloc::TryVec<u8>>,
}

impl MemDisk {
    fn new(nsect: u64, bs: usize) -> Result<Self, BlockError> {
        let n = usize::try_from(nsect)
            .ok()
            .and_then(|s| s.checked_mul(bs))
            .ok_or(BlockError::Inval)?;
        let mut v = vibeos::kalloc::TryVec::try_with_capacity(n).map_err(|_| BlockError::NoMem)?;
        let zero = [0u8; 512];
        while v.len() < n {
            v.try_extend_from_slice(&zero)
                .map_err(|_| BlockError::NoMem)?;
        }
        Ok(Self {
            bs,
            data: crate::sync_init::SpinMutex::with_rank(v, vibeos::lock::RANK_DEVICE),
        })
    }

    fn range(
        &self,
        len: usize,
        lba: u64,
        bytes: usize,
    ) -> Result<core::ops::Range<usize>, BlockError> {
        let off = usize::try_from(lba)
            .ok()
            .and_then(|l| l.checked_mul(self.bs))
            .ok_or(BlockError::Inval)?;
        let end = off.checked_add(bytes).ok_or(BlockError::Inval)?;
        if !bytes.is_multiple_of(self.bs) || end > len {
            return Err(BlockError::Inval);
        }
        Ok(off..end)
    }
}

impl vibeos::block::BlockDevice for MemDisk {
    fn logical_block_size(&self) -> u32 {
        self.bs as u32
    }
    fn capacity_sectors(&self) -> u64 {
        (self.data.lock().len() / self.bs) as u64
    }
    fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let d = self.data.lock();
        let r = self.range(d.len(), lba, buf.len())?;
        buf.copy_from_slice(d.get(r).ok_or(BlockError::Inval)?);
        Ok(())
    }
    fn write(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
        let mut d = self.data.lock();
        let r = self.range(d.len(), lba, buf.len())?;
        d.get_mut(r).ok_or(BlockError::Inval)?.copy_from_slice(buf);
        Ok(())
    }
    fn flush(&self) -> Result<(), BlockError> {
        Ok(())
    }
    fn discard(&self, lba: u64, n: u64) -> Result<(), BlockError> {
        let bytes = usize::try_from(n)
            .ok()
            .and_then(|n| n.checked_mul(self.bs))
            .ok_or(BlockError::Inval)?;
        let d = self.data.lock();
        self.range(d.len(), lba, bytes).map(|_| ())
    }
}

/// Register a fresh [`MemDisk`] of `bs`-byte blocks named `name`, with no
/// cache in front.
fn mem_disk(name: &[u8], bs: usize) -> Result<BlockRef, BlockError> {
    use vibeos::block::blockdev::Backing;
    let ops = vibeos::kalloc::TryBox::<dyn vibeos::block::BlockDevice>::try_new_unsize(
        MemDisk::new(MEM_SECT, bs)?,
        |b| b,
    )
    .map_err(|_| BlockError::NoMem)?;
    blockdev_init::register(name, Backing::Disk { ops, cache: None })
}

/// One zeroed logical block of `d`.
fn block_buf(d: &BlockRef) -> Result<Vec<u8>, BlockError> {
    Ok(alloc::vec![0u8; d.logical_block_size()? as usize])
}

/// Write the MBR table: entry 0 extended (8, 120) with EBRs at 8, 40, 72
/// and 104, each a 16-block logical at +1; entries 1 and 2 primaries
/// after it. LBAs are `d`'s logical blocks, as on a 4 KiB-sector disk.
/// Returns the six (start, length) pairs.
fn stamp_six_mbr(d: &BlockRef) -> Result<[(u64, u64); 6], BlockError> {
    use vibeos::part::{MBR_EXTENDED, MBR_LINUX, pack_ebr, pack_mbr};
    let mut s = block_buf(d)?;
    pack_mbr(
        &mut s[..512],
        &[
            (MBR_EXTENDED, 8, 120),
            (MBR_LINUX, 136, 16),
            (MBR_LINUX, 160, 16),
            (0, 0, 0),
        ],
    );
    d.write_dev(0, &s)?;
    let ebrs = [8u32, 40, 72, 104];
    for (k, &e) in ebrs.iter().enumerate() {
        let next = ebrs.get(k + 1).map_or(0, |n| n - 8);
        let mut b = block_buf(d)?;
        pack_ebr(
            &mut b[..512],
            MBR_LINUX,
            1,
            16,
            next,
            if next == 0 { 0 } else { 17 },
        );
        d.write_dev(e as u64, &b)?;
    }
    Ok([(9, 16), (41, 16), (73, 16), (105, 16), (136, 16), (160, 16)])
}

/// Write the GPT table: six 16-block entries after the entry array, the
/// backup header at the last LBA. Returns the six (start, length) pairs.
fn stamp_six_gpt(d: &BlockRef) -> Result<[(u64, u64); 6], BlockError> {
    use vibeos::part::{
        GPT_ENTRY_SIZE, GUID_LINUX, GptHeaderInfo, entries_crc, pack_gpt_entry, pack_gpt_header,
        pack_protective_mbr,
    };
    const NENT: u32 = 128;
    const ESZ: usize = GPT_ENTRY_SIZE as usize;
    let bs = d.logical_block_size()? as usize;
    let elen = NENT as usize * ESZ;
    let nsec = (elen / bs) as u64;
    let first = 2 + nsec;
    let mut entries = alloc::vec![0u8; elen];
    let mut parts = [(0u64, 0u64); 6];
    for (k, p) in parts.iter_mut().enumerate() {
        let start = first + 16 * k as u64;
        *p = (start, 16);
        let mut uniq = [0u8; 16];
        uniq[0] = k as u8 + 1;
        pack_gpt_entry(
            &mut entries[k * ESZ..],
            &GUID_LINUX,
            &uniq,
            start,
            start + 15,
            "kt",
        );
    }
    let ecrc = entries_crc(&entries[..elen]);
    let last = MEM_SECT - 1;
    let back = last - nsec;
    let hdr = |my: u64, alt: u64, part_lba: u64| GptHeaderInfo {
        my_lba: my,
        alt_lba: alt,
        first_usable: first,
        last_usable: back - 1,
        disk_guid: [0x6B; 16],
        part_lba,
        part_count: NENT,
        part_size: GPT_ENTRY_SIZE,
        entries_crc: ecrc,
    };
    let mut s = block_buf(d)?;
    pack_protective_mbr(&mut s[..512], MEM_SECT);
    d.write_dev(0, &s)?;
    let mut h = block_buf(d)?;
    pack_gpt_header(&mut h[..512], &hdr(1, last, 2));
    d.write_dev(1, &h)?;
    for k in 0..nsec {
        let o = k as usize * bs;
        d.write_dev(2 + k, &entries[o..o + bs])?;
        d.write_dev(back + k, &entries[o..o + bs])?;
    }
    let mut b = block_buf(d)?;
    pack_gpt_header(&mut b[..512], &hdr(last, 1, back));
    d.write_dev(last, &b)?;
    Ok(parts)
}

/// Writes a six-entry table and returns each entry's (start, length).
type Stamp = fn(&BlockRef) -> Result<[(u64, u64); 6], BlockError>;

/// Check one six-entry table on disk `name`, then unregister it.
fn six_entries(name: &str, bs: usize, stamp: Stamp, nums: [u32; 6]) -> Outcome {
    let disk = match mem_disk(name.as_bytes(), bs) {
        Ok(d) => d,
        Err(e) => return crate::fail_fmt!("{name}: register: {}", e.as_str()),
    };
    let r = six_entries_on(&disk, name, stamp, nums);
    if let Err(e) = blockdev_init::unregister(&disk) {
        return crate::fail_fmt!("{name}: unregister: {}", e.as_str());
    }
    r
}

fn six_entries_on(disk: &BlockRef, name: &str, stamp: Stamp, nums: [u32; 6]) -> Outcome {
    let parts = match stamp(disk) {
        Ok(p) => p,
        Err(e) => return crate::fail_fmt!("{name}: stamp: {}", e.as_str()),
    };
    // A distinct byte at each entry's first LBA.
    let tag = |k: usize| 0xA0u8 + k as u8;
    for (k, &(start, _)) in parts.iter().enumerate() {
        let Ok(mut s) = block_buf(disk) else {
            return crate::fail_fmt!("{name}: block size");
        };
        s[0] = tag(k);
        if disk.write_dev(start, &s).is_err() {
            return crate::fail_fmt!("{name}: tag {k}");
        }
    }
    match part_init::scan(disk) {
        Ok(r) if r.added == 6 && r.dropped == 0 => {}
        Ok(r) => return crate::fail_fmt!("{name}: added {} dropped {}", r.added, r.dropped),
        Err(e) => return crate::fail_fmt!("{name}: scan: {}", e.as_str()),
    }
    let mut seen = [false; 6];
    for n in nums {
        let Ok(cname) = vibeos::block::blockdev::BlockName::child(disk.name(), n) else {
            return crate::fail_fmt!("{name}: child name {n}");
        };
        let Some(c) = blockdev_init::lookup(cname.as_bytes()) else {
            return crate::fail_fmt!("{name}: no {}", cname.as_str());
        };
        if c.parent().map(BlockRef::id) != Some(disk.id()) {
            return crate::fail_fmt!("{}: parent", cname.as_str());
        }
        let Ok(mut s) = block_buf(&c) else {
            return crate::fail_fmt!("{}: block size", cname.as_str());
        };
        if c.read(0, &mut s).is_err() {
            return crate::fail_fmt!("{}: read", cname.as_str());
        }
        let cap = c.capacity_sectors().unwrap_or(0);
        let hit = parts
            .iter()
            .enumerate()
            .position(|(k, &(_, len))| s[0] == tag(k) && cap == len);
        match hit.and_then(|k| seen.get_mut(k)) {
            Some(f) if !*f => *f = true,
            _ => {
                return crate::fail_fmt!(
                    "{}: cap {cap} byte {:#x} matches no entry",
                    cname.as_str(),
                    s[0]
                );
            }
        }
    }
    // The names are taken now, so a second scan registers nothing.
    match part_init::scan(disk) {
        Ok(r) if r.added == 0 && r.dropped == 6 => Outcome::Ok,
        Ok(r) => crate::fail_fmt!("{name}: rescan added {} dropped {}", r.added, r.dropped),
        Err(e) => crate::fail_fmt!("{name}: rescan: {}", e.as_str()),
    }
}

/// ROADMAP §10.12 (F117): an MBR table with an extended entry, four
/// logicals and two primaries after it, and a six-entry GPT, each get six
/// children, numbered as Linux numbers them: the MBR's `<disk>p2`, `p3`
/// and `p5` to `p8`, the GPT's `<disk>p1` to `p6`. Each runs on a disk of
/// 512-byte blocks and on one of 4 KiB blocks, whose tables count in
/// 4 KiB blocks.
pub(crate) fn part_six_entries() -> Outcome {
    // Linux's numbers: the MBR's primaries keep their slots, 2 and 3,
    // and its four logical partitions are 5 to 8; the GPT's six entries
    // are 1 to 6.
    let cases: [(&str, usize, Stamp, [u32; 6]); 4] = [
        ("ktmbr", 512, stamp_six_mbr, [2, 3, 5, 6, 7, 8]),
        ("ktgpt", 512, stamp_six_gpt, [1, 2, 3, 4, 5, 6]),
        ("ktmbr4k", 4096, stamp_six_mbr, [2, 3, 5, 6, 7, 8]),
        ("ktgpt4k", 4096, stamp_six_gpt, [1, 2, 3, 4, 5, 6]),
    ];
    for (name, bs, stamp, nums) in cases {
        let r = six_entries(name, bs, stamp, nums);
        if !matches!(r, Outcome::Ok) {
            return r;
        }
    }
    Outcome::Ok
}

/// A FAT mount on a disk of 4 KiB blocks is refused with `EINVAL`, as
/// Linux refuses a volume whose sectors are smaller than the device's,
/// where its first 512-byte read once failed with `EIO`.
pub(crate) fn fat_mount_4k_disk_inval() -> Outcome {
    let disk = match mem_disk(b"ktfat4k", 4096) {
        Ok(d) => d,
        Err(e) => return crate::fail_fmt!("register: {}", e.as_str()),
    };
    let r = crate::fat_init::mount_dev("ktfat4k", "/tmp", true);
    if let Err(e) = blockdev_init::unregister(&disk) {
        return crate::fail_fmt!("unregister: {}", e.as_str());
    }
    match r {
        Err(vibeos::fs::FsError::Inval) => Outcome::Ok,
        Err(e) => crate::fail_fmt!("mount: {:?}, not Inval", e),
        Ok(()) => Outcome::Fail("mounted"),
    }
}

/// This subsystem's in-guest tests, in run order; `crate::ktest::GROUPS`
/// runs them (DESIGN §8.2).
pub(crate) const TESTS: &[Test] = &[
    test("block_ramdisk_rw", test_block_ramdisk_rw),
    test("block_concurrent", test_block_concurrent),
    test("block_retry", test_block_retry),
    test("block_part_mbr", test_block_part_mbr),
    test("block_part_gpt", test_block_part_gpt),
    test("block_cache_hit", test_block_cache_hit).once(),
    test("block_cache_evict", test_block_cache_evict),
    test("part_six_entries", part_six_entries),
    test("fat_mount_4k_disk_inval", fat_mount_4k_disk_inval),
    test(
        "lifetime_iowaiter_publish_last",
        lifetime_iowaiter_publish_last,
    )
    .deadline(60_000),
    test("cache_flush_waits_writeback", cache_flush_waits_writeback),
    test("cache_read_waits_for_fill", cache_read_waits_for_fill).deadline(60_000),
    test("cache_flush_not_starved", cache_flush_not_starved),
    test("block_fua_write", block_fua_write),
];
