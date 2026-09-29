//! Ramdisk + queued I/O. ROADMAP §7.1.
//!
//! Queue lock (RANK_DEVICE) is dropped before the copy and before waiter
//! wake (SCHED). Slice B can complete from a threaded IRQ with the same
//! `IoWaiter` path. Kick is inline for ramdisk; virtio-blk replaces it.

use core::cell::UnsafeCell;
#[cfg(feature = "kernel_tests")]
use core::sync::atomic::AtomicU32;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use vibeos::block::blockdev::Backing;
use vibeos::block::{
    BlockDevice, BlockError, Completion, DeviceState, DoneWord, MAX_QUEUE, Op, Queue, Ramdisk,
    Request,
};
use vibeos::kalloc::TryBox;
use vibeos::lock::RANK_DEVICE;
use vibeos::sched::FAR_DEADLINE;
use vibeos::wait::WaitQueue;

use crate::block::{blockdev_init, cache_init};
use crate::sync_init::SpinMutex;
use crate::thread_init;

pub const RAM0_NAME: &str = "ram0";
pub const RAM0_BLOCK_SIZE: u32 = 512;
pub const RAM0_SECTORS: u64 = 256;
const RAM0_BYTES: usize = RAM0_BLOCK_SIZE as usize * RAM0_SECTORS as usize;

const ST_PEND: u32 = 0;
const ST_OK: u32 = 1;
const ST_INVAL: u32 = 2;
const ST_IO: u32 = 3;
const ST_FAILED: u32 = 4;
const ST_QFULL: u32 = 5;
const _: () = assert!(ST_PEND == DoneWord::PENDING);

static Q: SpinMutex<Queue> = SpinMutex::with_rank(Queue::new(), RANK_DEVICE);
// BSS, not a heap Vec: init must not take RANK_HEAP under RANK_DEVICE.
static DATA: SpinMutex<[u8; RAM0_BYTES]> = SpinMutex::with_rank([0u8; RAM0_BYTES], RANK_DEVICE);
static STATE: AtomicU8 = AtomicU8::new(0);
#[cfg(feature = "kernel_tests")]
static FAIL_NEXT: AtomicU32 = AtomicU32::new(0);
static LIVE: AtomicBool = AtomicBool::new(false);
static IO_REQS: AtomicU64 = AtomicU64::new(0);
/// `Flush` requests dispatched to ram0; `block::ktest::flushes` reads it.
pub(super) static FLUSHES: AtomicU64 = AtomicU64::new(0);

#[allow(
    clippy::expect_used,
    reason = "ram0's geometry is the nonzero constants `block_init::RAM0_BLOCK_SIZE` and `block_init::RAM0_SECTORS`, which `Ramdisk::new` accepts"
)]
fn ram() -> Ramdisk {
    Ramdisk::new(RAM0_BLOCK_SIZE, RAM0_SECTORS).expect("ram0 geom")
}

fn pack(r: Result<(), BlockError>) -> u32 {
    match r {
        Ok(()) => ST_OK,
        Err(BlockError::Inval) => ST_INVAL,
        Err(BlockError::Io) => ST_IO,
        // A ramdisk request allocates nothing and reaches no registry, so
        // never NoMem, Gone or Exists.
        Err(BlockError::Failed | BlockError::NoMem | BlockError::Gone | BlockError::Exists) => {
            ST_FAILED
        }
        Err(BlockError::QueueFull) => ST_QFULL,
    }
}

fn unpack(v: u32) -> Result<(), BlockError> {
    match v {
        ST_OK => Ok(()),
        ST_INVAL => Err(BlockError::Inval),
        ST_IO => Err(BlockError::Io),
        ST_FAILED => Err(BlockError::Failed),
        ST_QFULL => Err(BlockError::QueueFull),
        ST_PEND => Err(BlockError::Io),
        _ => Err(BlockError::Io),
    }
}

/// Blocking/async completion. Lives on the submitter stack until `wait`
/// returns. Cookie in [`Request::waiters`] is this address.
pub struct IoWaiter {
    done: DoneWord,
    wq: UnsafeCell<WaitQueue>,
}

// SAFETY: `done` is a `DoneWord` (an atomic), and `wq` is touched only under SCHED
// (`wait` and `finish` reach it inside `with_sched`), so sharing `&IoWaiter`
// across threads races on nothing; established by `thread_init::with_sched`.
unsafe impl Sync for IoWaiter {}

impl IoWaiter {
    pub const fn new() -> Self {
        Self {
            done: DoneWord::new(),
            wq: UnsafeCell::new(WaitQueue::new()),
        }
    }

    pub fn poll(&self) -> Option<Result<(), BlockError>> {
        self.done.poll().map(unpack)
    }

    pub fn wait(&self) -> Result<(), BlockError> {
        loop {
            if let Some(r) = self.poll() {
                return r;
            }
            let park = thread_init::with_sched(|s| {
                if self.done.poll().is_some() {
                    return false;
                }
                // SAFETY: `wq` is touched only under SCHED, which this
                // closure holds; established by `thread_init::with_sched`.
                s.begin_wait(unsafe { &mut *self.wq.get() }, FAR_DEADLINE);
                true
            });
            if park {
                thread_init::schedule();
            }
        }
    }

    /// Complete the waiter: under SCHED, wake its queue, then publish `done`
    /// (`DoneWord::publish`, a Release store) as the last access to it
    /// (DESIGN §2.8, §10.1). `wait`
    /// can return through the lock-free `poll` the moment that store is
    /// visible, so nothing after it may touch `*this`. A raw pointer, not
    /// `&self`: a reference argument counts as dereferenceable for the
    /// whole call, so the compiler could read `*self` after the store.
    ///
    /// # Safety
    ///
    /// `this` points to a live waiter whose `done` is pending, and it
    /// stays live until this call's `done` store and no longer.
    unsafe fn finish(this: *const IoWaiter, res: Result<(), BlockError>) {
        let st = pack(res);
        #[cfg(feature = "kernel_tests")]
        testing::finish_stall();
        thread_init::with_sched(|s| {
            // SAFETY: invariant I11 (DESIGN §2.7), established at
            // `block_init::IoWaiter::wait`: `wait` cannot return before the
            // `done` store below, so the waiter is live here, and its `wq`
            // is touched only under SCHED, which this closure holds.
            s.wake_all(unsafe { &mut *(*this).wq.get() });
            // SAFETY: invariant I11, as above (`block_init::IoWaiter::wait`):
            // the waiter is live until this store, which is the last access.
            unsafe { (*this).done.publish(st) };
        });
    }
}

/// Test hooks for the completion path (`lifetime_iowaiter_publish_last`,
/// ROADMAP §10.10). `kernel_tests` only (AGENTS.md rule 9).
#[cfg(feature = "kernel_tests")]
pub mod testing {
    use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    use crate::time_init;

    // The stall's state; `block::ktest`'s setters arm and read it.
    pub(in crate::block) static STALLS_LEFT: AtomicU32 = AtomicU32::new(0);
    pub(in crate::block) static STALL_MAX_US: AtomicU32 = AtomicU32::new(0);
    pub(in crate::block) static STALLS_RUN: AtomicU32 = AtomicU32::new(0);
    pub(in crate::block) static RETURNS: AtomicU64 = AtomicU64::new(0);

    /// Take one armed stall, if any is left, and spin until a submitter
    /// returns or the bound passes. IF is left as found.
    pub(super) fn finish_stall() {
        let mut left = STALLS_LEFT.load(Ordering::Acquire);
        loop {
            let Some(next) = left.checked_sub(1) else {
                return;
            };
            match STALLS_LEFT.compare_exchange_weak(left, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => break,
                Err(now) => left = now,
            }
        }
        STALLS_RUN.fetch_add(1, Ordering::AcqRel);
        let k = time_init::tsc_per_ms();
        if k == 0 {
            return;
        }
        let max = k.saturating_mul(u64::from(STALL_MAX_US.load(Ordering::Relaxed))) / 1000;
        let r0 = RETURNS.load(Ordering::Acquire);
        let t0 = time_init::read_tsc();
        while RETURNS.load(Ordering::Acquire) == r0 && time_init::read_tsc().wrapping_sub(t0) < max
        {
            core::hint::spin_loop();
        }
    }
}

fn complete_req(req: &Request, res: Result<(), BlockError>) {
    let mut i = 0u8;
    while i < req.nwait {
        let p = req.waiters[i as usize];
        if p != 0 {
            // SAFETY: invariant I11 (DESIGN §10.1): the cookie is a live
            // waiter registered by `block_init::submit` or
            // `virtio_blk_init::submit`, and this completer claimed the request.
            unsafe { IoWaiter::finish(p as *const IoWaiter, res) }
        }
        i += 1;
    }
}

/// Wake request cookies. Caller must not hold RANK_DEVICE (SCHED wake).
pub fn complete_waiters(req: &Request, res: Result<(), BlockError>) {
    complete_req(req, res);
}

fn execute(req: &Request) -> Result<(), BlockError> {
    IO_REQS.fetch_add(1, Ordering::Relaxed);
    if DeviceState::from_u8(STATE.load(Ordering::Acquire)) == DeviceState::Failed {
        return Err(BlockError::Failed);
    }
    #[cfg(feature = "kernel_tests")]
    loop {
        let n = FAIL_NEXT.load(Ordering::SeqCst);
        if n == 0 {
            break;
        }
        if FAIL_NEXT
            .compare_exchange(n, n - 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Err(BlockError::Io);
        }
    }
    let mut data = DATA.lock();
    // SAFETY: invariant I235: a queued request's segments stay valid and
    // untouched until its completion runs, and none aliases the ramdisk's
    // own `DATA`; established by `block_init::build`.
    unsafe { ram().apply(&mut data[..], req) }
}

/// Take every queued request and every pending emulated-`Fua` `Flush`
/// off `Q` and fail their waiters, after dropping the lock each round.
fn drain_failed(q_prep: fn(&mut Queue)) {
    let mut first = true;
    loop {
        let mut dump = [None; MAX_QUEUE];
        let n = {
            let mut q = Q.lock();
            if first {
                q_prep(&mut q);
                first = false;
            }
            q.drain(&mut dump)
        };
        if n == 0 {
            return;
        }
        let mut i = 0usize;
        while i < n {
            if let Some(r) = dump[i] {
                complete_req(&r, Err(BlockError::Failed));
            }
            i += 1;
        }
    }
}

fn fail_rest() {
    STATE.store(DeviceState::Failed.as_u8(), Ordering::Release);
    drain_failed(Queue::fail);
}

/// Retire `req`'s dispatch in `Q`, then wake its waiters after the lock
/// drops, unless the queue defers the report to an emulated-`Fua` `Flush`.
fn finish(mut req: Request, res: Result<(), BlockError>) {
    let seq = u64::from(req.seq);
    let mut q = Q.lock();
    match res {
        Ok(()) => {
            let c = q.complete(seq);
            drop(q);
            if c == Completion::Report {
                complete_req(&req, Ok(()));
            }
        }
        Err(e) if e.retryable() && req.retries_left > 0 => {
            req.retries_left -= 1;
            let requeued = q.requeue(req);
            drop(q);
            if requeued.is_err() {
                complete_req(&req, Err(BlockError::Failed));
                fail_rest();
            }
        }
        Err(e) if e.retryable() => {
            q.abort(seq);
            drop(q);
            complete_req(&req, Err(BlockError::Failed));
            fail_rest();
        }
        Err(e) => {
            q.abort(seq);
            drop(q);
            complete_req(&req, Err(e));
        }
    }
}

/// Runs inline in the submitter. It loops while `pick` has work, so an
/// emulated-`Fua` write's `Flush` runs in the same pass: nothing else
/// pumps ram0.
fn pump() {
    loop {
        let req = {
            let mut q = Q.lock();
            match q.pick() {
                Some(r) => r,
                None => {
                    q.running = false;
                    return;
                }
            }
        };
        if req.bio.op == Op::Flush {
            FLUSHES.fetch_add(1, Ordering::Relaxed);
        }
        let res = execute(&req);
        finish(req, res);
    }
}

fn kick_if(start: bool) {
    if start {
        pump();
    }
}

fn submit_req(req: Request) -> Result<bool, BlockError> {
    let mut q = Q.lock();
    q.submit(req)?;
    if q.running {
        Ok(false)
    } else {
        q.running = true;
        Ok(true)
    }
}

/// Check a request and tie it to `w`. Flush and discard pass `ptr = 0`,
/// `len = 0`.
fn build(
    op: Op,
    lba: u64,
    nsect: u32,
    ptr: usize,
    len: usize,
    w: &IoWaiter,
) -> Result<Request, BlockError> {
    if !LIVE.load(Ordering::Acquire) {
        return Err(BlockError::Failed);
    }
    if DeviceState::from_u8(STATE.load(Ordering::Acquire)) == DeviceState::Failed {
        return Err(BlockError::Failed);
    }
    let bs = RAM0_BLOCK_SIZE as usize;
    let mut req = Request::new(op, lba, nsect).with_waiter(w as *const IoWaiter as usize);
    match op {
        Op::Read | Op::Write => {
            if nsect == 0 || (nsect as usize).checked_mul(bs) != Some(len) {
                return Err(BlockError::Inval);
            }
            req = req.with_seg(ptr, len);
        }
        Op::Flush => {
            if nsect != 0 || len != 0 {
                return Err(BlockError::Inval);
            }
        }
        Op::Discard => {
            if len != 0 {
                return Err(BlockError::Inval);
            }
        }
    }
    Ok(req)
}

fn start(req: Request) -> Result<(), BlockError> {
    let start = submit_req(req)?;
    kick_if(start);
    Ok(())
}

/// Async submit. `buf` must stay live until `w` completes. Flush and
/// discard pass `ptr = 0`, `len = 0`.
pub fn submit(
    op: Op,
    lba: u64,
    nsect: u32,
    ptr: usize,
    len: usize,
    w: &IoWaiter,
) -> Result<(), BlockError> {
    start(build(op, lba, nsect, ptr, len, w)?)
}

fn blocking(op: Op, lba: u64, nsect: u32, ptr: usize, len: usize) -> Result<(), BlockError> {
    let w = IoWaiter::new();
    submit(op, lba, nsect, ptr, len, &w)?;
    w.wait()
}

pub fn read(lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
    let bs = RAM0_BLOCK_SIZE as usize;
    if bs == 0 || !buf.len().is_multiple_of(bs) {
        return Err(BlockError::Inval);
    }
    let nsect = (buf.len() / bs) as u32;
    blocking(Op::Read, lba, nsect, buf.as_mut_ptr() as usize, buf.len())
}

pub fn write(lba: u64, buf: &[u8]) -> Result<(), BlockError> {
    let bs = RAM0_BLOCK_SIZE as usize;
    if bs == 0 || !buf.len().is_multiple_of(bs) {
        return Err(BlockError::Inval);
    }
    let nsect = (buf.len() / bs) as u32;
    blocking(Op::Write, lba, nsect, buf.as_ptr() as usize, buf.len())
}

pub fn flush() -> Result<(), BlockError> {
    blocking(Op::Flush, 0, 0, 0, 0)
}

#[cfg(feature = "kernel_tests")]
/// Write `buf` at `lba` with `Fua`: durable when this returns `Ok`.
pub fn write_fua(lba: u64, buf: &[u8]) -> Result<(), BlockError> {
    let bs = RAM0_BLOCK_SIZE as usize;
    if bs == 0 || !buf.len().is_multiple_of(bs) {
        return Err(BlockError::Inval);
    }
    let nsect = (buf.len() / bs) as u32;
    let w = IoWaiter::new();
    let req = build(Op::Write, lba, nsect, buf.as_ptr() as usize, buf.len(), &w)?.with_fua();
    start(req)?;
    w.wait()
}

pub fn discard(lba: u64, nsectors: u64) -> Result<(), BlockError> {
    if nsectors > u32::MAX as u64 {
        return Err(BlockError::Inval);
    }
    blocking(Op::Discard, lba, nsectors as u32, 0, 0)
}

#[cfg(feature = "kernel_tests")]
pub fn live() -> bool {
    LIVE.load(Ordering::Acquire)
}

pub fn state() -> DeviceState {
    DeviceState::from_u8(STATE.load(Ordering::Acquire))
}

pub fn logical_block_size() -> u32 {
    RAM0_BLOCK_SIZE
}

pub fn capacity_sectors() -> u64 {
    RAM0_SECTORS
}

pub fn io_reqs() -> u64 {
    IO_REQS.load(Ordering::Relaxed)
}

#[cfg(feature = "kernel_tests")]
pub fn inject_io_fails(n: u32) {
    FAIL_NEXT.store(n, Ordering::SeqCst);
}

#[cfg(feature = "kernel_tests")]
pub fn reset() {
    #[cfg(feature = "kernel_tests")]
    FAIL_NEXT.store(0, Ordering::SeqCst);
    STATE.store(DeviceState::Ready.as_u8(), Ordering::Release);
    drain_failed(|q| {
        q.failed = false;
        q.running = false;
    });
}

/// ram0's driver operations, which its registry entry owns.
struct Ram0;

impl BlockDevice for Ram0 {
    fn logical_block_size(&self) -> u32 {
        logical_block_size()
    }
    fn capacity_sectors(&self) -> u64 {
        capacity_sectors()
    }
    fn state(&self) -> DeviceState {
        state()
    }
    fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        read(lba, buf)
    }
    fn write(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
        write(lba, buf)
    }
    fn flush(&self) -> Result<(), BlockError> {
        flush()
    }
    fn discard(&self, lba: u64, nsectors: u64) -> Result<(), BlockError> {
        discard(lba, nsectors)
    }
}

/// Register ram0 in the block registry, through the page cache; its
/// registration prints the `block: ram0` marker.
fn register() -> Result<(), BlockError> {
    let ops =
        TryBox::<dyn BlockDevice>::try_new_unsize(Ram0, |b| b).map_err(|_| BlockError::NoMem)?;
    blockdev_init::register(
        RAM0_NAME.as_bytes(),
        Backing::Disk {
            ops,
            cache: Some(&cache_init::PAGE_CACHE),
        },
    )
    .map(|_| ())
}

pub fn init() {
    debug_assert_eq!(ram().byte_len(), RAM0_BYTES);
    {
        let mut d = DATA.lock();
        d.fill(0);
    }
    STATE.store(DeviceState::Ready.as_u8(), Ordering::Release);
    LIVE.store(true, Ordering::Release);
    if let Err(e) = register() {
        crate::klog!(
            vibeos::log::Level::Error,
            "vibeOS: blk: ram0 not registered: {}",
            e.as_str()
        );
    }
}
