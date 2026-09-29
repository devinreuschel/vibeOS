//! In-guest tests for block (kernel_tests only). Rows: the list in crate::ktest.

use alloc::vec::Vec;
use core::hint::{black_box, spin_loop};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::block::{BlockError, DeviceState, Op};
use vibeos::cache::PAGE;

use crate::block_init::{self, IoWaiter, testing as blk_testing};
use crate::cache_init::{self, DEV_RAM0, testing};
use crate::ktest::{Outcome, spawn_thread};
use crate::part_init;
use crate::thread_init;
use crate::time_init;
use crate::virtio_blk_init;

pub(crate) fn test_block_ramdisk_rw() -> Outcome {
    if !block_init::live() {
        return Outcome::Fail("block not live");
    }
    block_init::reset();
    let d = block_init::device();
    if d.name() != block_init::name() {
        return Outcome::Fail("name");
    }
    if d.logical_block_size() != block_init::logical_block_size() {
        return Outcome::Fail("bs");
    }
    if d.capacity_sectors() != block_init::capacity_sectors() {
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
    if d.write(1, &buf).is_err() {
        return Outcome::Fail("write");
    }
    let mut out = [0u8; 512];
    if d.read(1, &mut out).is_err() {
        return Outcome::Fail("read");
    }
    if out != buf {
        return Outcome::Fail("mismatch");
    }
    if d.flush().is_err() {
        return Outcome::Fail("flush");
    }
    if d.discard(1, 1).is_err() {
        return Outcome::Fail("discard");
    }
    out = [0u8; 512];
    if d.read(1, &mut out).is_err() || out != buf {
        return Outcome::Fail("discard clobber");
    }
    let mut odd = [0u8; 100];
    match d.read(0, &mut odd) {
        Err(BlockError::Inval) => {}
        _ => return Outcome::Fail("unaligned"),
    }
    match d.write(block_init::RAM0_SECTORS, &buf) {
        Err(BlockError::Inval) => {}
        _ => return Outcome::Fail("past end"),
    }
    match d.discard(block_init::RAM0_SECTORS, 1) {
        Err(BlockError::Inval) => {}
        _ => return Outcome::Fail("discard past"),
    }
    if !crate::shell_init::has_command("blk") {
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
    if !part_init::live() {
        return Outcome::Fail("parts not live");
    }
    let Some(i) = part_init::find_name("ram0p1") else {
        return Outcome::Fail("no ram0p1");
    };
    let Some(i2) = part_init::find_name("ram0p2") else {
        return Outcome::Fail("no ram0p2");
    };
    if part_init::find_name("ram0p3").is_none() {
        return Outcome::Fail("no ram0p3");
    }
    let Some(d) = part_init::device(i) else {
        return Outcome::Fail("no device");
    };
    if d.name() != "ram0p1" {
        return Outcome::Fail("name");
    }
    if d.capacity_sectors() != 32 {
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
    let Some((_, n2, _, k2)) = part_init::info(i2) else {
        return Outcome::Fail("info p2");
    };
    if n2 != 24 {
        return Outcome::Fail("logical size");
    }
    if part_init::type_str(k2) != "linux" {
        return Outcome::Fail("type");
    }
    Outcome::Ok
}

pub(crate) fn test_block_part_gpt() -> Outcome {
    if !virtio_blk_init::live() {
        return Outcome::Skip("no virtio-blk");
    }
    let Some(i1) = part_init::find_name("vdap1") else {
        return Outcome::Fail("no vdap1");
    };
    let Some(i2) = part_init::find_name("vdap2") else {
        return Outcome::Fail("no vdap2");
    };
    let Some((_, _, _, k1)) = part_init::info(i1) else {
        return Outcome::Fail("info p1");
    };
    let Some((_, n2, _, k2)) = part_init::info(i2) else {
        return Outcome::Fail("info p2");
    };
    if part_init::type_str(k1) != "efi" {
        return Outcome::Fail("efi guid");
    }
    if part_init::type_str(k2) != "linux" {
        return Outcome::Fail("linux guid");
    }
    if n2 < 16 {
        return Outcome::Fail("linux small");
    }
    let Some(d) = part_init::device(i1) else {
        return Outcome::Fail("no d1");
    };
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
    match d.write(d.capacity_sectors(), &buf) {
        Err(BlockError::Inval) => {}
        _ => return Outcome::Fail("gpt overflow"),
    }
    Outcome::Ok
}

pub(crate) fn test_block_cache_hit() -> Outcome {
    if !cache_init::live() || !block_init::live() {
        return Outcome::Fail("not live");
    }
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
    if cache_init::read(cache_init::DEV_RAM0, lba, &mut out).is_err() {
        return Outcome::Fail("c1");
    }
    if out != buf {
        return Outcome::Fail("data");
    }
    let s1 = cache_init::stats();
    if cache_init::read(cache_init::DEV_RAM0, lba, &mut out).is_err() || out != buf {
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
    let s0 = cache_init::stats();
    let mut buf = [0u8; 512];
    let mut i = 0u64;
    while i < 18 {
        let lba = i * 8;
        buf[0] = i as u8;
        if cache_init::read(cache_init::DEV_RAM0, lba, &mut buf).is_err() {
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
    blk_testing::note_return();
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
        blk_testing::disarm_finish_stall();
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
    blk_testing::arm_finish_stall(IOW_STALLS, IOW_STALL_US);
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
    let stalls = blk_testing::finish_stalls_run();
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
    let res = match cache_init::flush(DEV_RAM0) {
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
        testing::release();
        // A failed flush thread may still be waiting: wait out its flush.
        let _pending = wait_for(|| FLUSH_RES.load(Ordering::Acquire) != FLUSH_PENDING);
        cache_init::write(DEV_RAM0, WB_FIRST_LBA, &self.saved)?;
        cache_init::flush(DEV_RAM0)?;
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

/// `cache_init::flush` sends no `Flush` while `blk-wb` holds one write of
/// the device, and the held page is on the device once it does.
pub(crate) fn cache_flush_waits_writeback() -> Outcome {
    if !cache_init::live() || !block_init::live() {
        return Outcome::Fail("no ram0 cache");
    }
    if cache_init::flush(DEV_RAM0).is_err() {
        return Outcome::Fail("pre-flush");
    }
    let mut saved = alloc::vec![0u8; WB_PAGES * PAGE];
    if cache_init::read(DEV_RAM0, WB_FIRST_LBA, &mut saved).is_err() {
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
    testing::hold_wb(DEV_RAM0, held_off);
    if cache_init::write(DEV_RAM0, WB_FIRST_LBA, &dirty).is_err() {
        return Outcome::Fail("dirty");
    }
    if !wait_for(testing::held) {
        return Outcome::Fail("blk-wb never held");
    }
    let flushes0 = block_init::flushes();
    FLUSH_RES.store(FLUSH_PENDING, Ordering::Release);
    let _flusher = spawn_thread("ktest-flush", flush_ram0);
    thread_init::sleep_ms(BLOCKED_MS);
    if FLUSH_RES.load(Ordering::Acquire) != FLUSH_PENDING {
        return Outcome::Fail("flush returned while a write was held");
    }
    if block_init::flushes() != flushes0 {
        return Outcome::Fail("Flush sent while a write was held");
    }
    testing::release();
    if !wait_for(|| FLUSH_RES.load(Ordering::Acquire) != FLUSH_PENDING) {
        return Outcome::Fail("flush never returned");
    }
    if FLUSH_RES.load(Ordering::Acquire) != FLUSH_OK {
        return Outcome::Fail("flush failed");
    }
    if block_init::flushes() <= flushes0 {
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
        block_init::flushes,
    );
    if !matches!(r, Outcome::Ok) {
        return r;
    }
    if !virtio_blk_init::live() {
        return Outcome::Ok;
    }
    if virtio_blk_init::logical_block_size() != 512 {
        return Outcome::Skip("vda not 512");
    }
    fua_roundtrip(
        crate::drivers::ktest::persist_lba().saturating_add(1),
        virtio_blk_init::read,
        virtio_blk_init::write,
        virtio_blk_init::write_fua,
        crate::drivers::ktest::flushes,
    )
}
