//! In-guest tests of P10-S04, kalloc types, completion publish order, shootdown acks (DESIGN §8.2).

use core::hint::{black_box, spin_loop};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::block::{BlockError, Op};

use vibeos::thread::ThreadState;

use super::{Outcome, Test, spawn_thread, spawn_thread_on, test};
use crate::block_init::{self, IoWaiter, testing as blk_testing};
use crate::ipi_init;
use crate::kva_init;
use crate::per_cpu_init;
use crate::thread_init;
use crate::time_init;
use crate::x86;

pub(super) const TESTS: &[Test] = &[
    test(
        "lifetime_iowaiter_publish_last",
        lifetime_iowaiter_publish_last,
    )
    .deadline(60_000),
    test("lifetime_shootdown_ack_late", lifetime_shootdown_ack_late).deadline(15_000),
];

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
fn lifetime_iowaiter_publish_last() -> Outcome {
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

// ---------------------------------------------------------------------------
// lifetime_shootdown_ack_late (ROADMAP §10.10, F011)

/// How long the holder keeps IF off, in ms of TSC time.
const HOLD_MS: u64 = 3_000;
/// 0: not started; 1: IF off and spinning; 2: IF back on.
static HOLD: AtomicU32 = AtomicU32::new(0);

/// Hold IF off for [`HOLD_MS`] without polling `service_incoming`, so this
/// CPU acks no shootdown until it lets the pending `0xFC` in.
fn ack_hold() {
    let k = time_init::tsc_per_ms();
    let g = x86::InterruptGuard::enter();
    HOLD.store(1, Ordering::Release);
    let t0 = time_init::read_tsc();
    let span = k.saturating_mul(HOLD_MS);
    while time_init::read_tsc().wrapping_sub(t0) < span {
        spin_loop();
    }
    drop(g);
    HOLD.store(2, Ordering::Release);
}

fn wait_ms(pred: impl Fn() -> bool, ms: u64) -> bool {
    let t0 = time_init::now_ns();
    while !pred() {
        if time_init::now_ns().saturating_sub(t0) > ms.saturating_mul(1_000_000) {
            return false;
        }
        thread_init::yield_now();
    }
    true
}

/// ROADMAP §10.10 (F011): one CPU holds IF off for 3 s while another
/// unmaps a KVA range; `wait_acks` waits for it without panicking, logs it
/// late once a second, and both finish.
fn lifetime_shootdown_ack_late() -> Outcome {
    let k = time_init::tsc_per_ms();
    if k == 0 {
        return Outcome::Skip("no TSC");
    }
    let mask = per_cpu_init::online_mask();
    if mask.count_ones() < 2 {
        return Outcome::Skip("one CPU");
    }
    let me = per_cpu_init::current().cpu_id;
    let others = mask & !(1u64 << me);
    // Prefer an AP, so the BSP's tick keeps running.
    let pick = if others & !1 != 0 {
        others & !1
    } else {
        others
    };
    let h = pick.trailing_zeros();
    let Ok(stack) = kva_init::alloc_guarded_stack(4) else {
        return Outcome::Fail("alloc_guarded_stack");
    };
    let late0 = ipi_init::ack_late_count();
    HOLD.store(0, Ordering::Release);
    let th = spawn_thread_on("ack-hold", ack_hold, h);
    if !wait_ms(|| HOLD.load(Ordering::Acquire) != 0, 2_000) {
        kva_init::free_stack(stack);
        return Outcome::Fail("holder did not start");
    }
    let t0 = time_init::read_tsc();
    kva_init::free_stack(stack);
    let waited_ms = time_init::read_tsc().wrapping_sub(t0) / k;
    let late = ipi_init::ack_late_count().wrapping_sub(late0);
    if !wait_ms(
        || {
            HOLD.load(Ordering::Acquire) == 2
                && matches!(
                    thread_init::try_state(th.id()),
                    None | Some(ThreadState::Dead)
                )
        },
        5_000,
    ) {
        return Outcome::Fail("holder did not finish");
    }
    if waited_ms < 2_000 {
        return crate::fail_fmt!(
            "unmap did not wait for the IF-off CPU: {} ms on cpu{}",
            waited_ms,
            h
        );
    }
    if !(1..=3).contains(&late) {
        return crate::fail_fmt!("{} late lines in {} ms, want 1..=3", late, waited_ms);
    }
    Outcome::Ok
}
