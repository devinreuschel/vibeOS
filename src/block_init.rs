//! Ramdisk + queued I/O. ROADMAP §7.1.
//!
//! Queue lock (RANK_DEVICE) is dropped before the copy and before waiter
//! wake (SCHED). Slice B can complete from a threaded IRQ with the same
//! `IoWaiter` path. Kick is inline for ramdisk; virtio-blk replaces it.
#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]

use core::cell::UnsafeCell;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

use vibeos::block::{
    self, write_marker, BlockError, DeviceState, Op, Queue, Ramdisk, Request, MAX_QUEUE,
};
use vibeos::lock::RANK_DEVICE;
use vibeos::sched::FAR_DEADLINE;
use vibeos::shell::Command;
use vibeos::wait::WaitQueue;

use crate::console_init::Console;
use crate::serial::Serial;
use crate::shell_init;
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

static Q: SpinMutex<Queue> = SpinMutex::with_rank(Queue::new(), RANK_DEVICE);
// BSS, not a heap Vec: init must not take RANK_HEAP under RANK_DEVICE.
static DATA: SpinMutex<[u8; RAM0_BYTES]> =
    SpinMutex::with_rank([0u8; RAM0_BYTES], RANK_DEVICE);
static STATE: AtomicU8 = AtomicU8::new(0);
static FAIL_NEXT: AtomicU32 = AtomicU32::new(0);
static LIVE: AtomicBool = AtomicBool::new(false);
static IO_REQS: AtomicU64 = AtomicU64::new(0);

fn ram() -> Ramdisk {
    Ramdisk::new(RAM0_NAME, RAM0_BLOCK_SIZE, RAM0_SECTORS).expect("ram0 geom")
}

fn pack(r: Result<(), BlockError>) -> u32 {
    match r {
        Ok(()) => ST_OK,
        Err(BlockError::Inval) => ST_INVAL,
        Err(BlockError::Io) => ST_IO,
        Err(BlockError::Failed) => ST_FAILED,
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
    done: AtomicU32,
    wq: UnsafeCell<WaitQueue>,
}

unsafe impl Sync for IoWaiter {}

impl IoWaiter {
    pub const fn new() -> Self {
        Self {
            done: AtomicU32::new(ST_PEND),
            wq: UnsafeCell::new(WaitQueue::new()),
        }
    }

    pub fn poll(&self) -> Option<Result<(), BlockError>> {
        let st = self.done.load(Ordering::Acquire);
        if st == ST_PEND {
            None
        } else {
            Some(unpack(st))
        }
    }

    pub fn wait(&self) -> Result<(), BlockError> {
        loop {
            if let Some(r) = self.poll() {
                return r;
            }
            let park = thread_init::with_sched(|s| {
                if self.done.load(Ordering::Acquire) != ST_PEND {
                    return false;
                }
                s.begin_wait(unsafe { &mut *self.wq.get() }, FAR_DEADLINE);
                true
            });
            if park {
                thread_init::schedule();
            }
        }
    }

    fn finish(&self, res: Result<(), BlockError>) {
        self.done.store(pack(res), Ordering::Release);
        thread_init::with_sched(|s| {
            s.wake_all(unsafe { &mut *self.wq.get() });
        });
    }
}

fn complete_req(req: &Request, res: Result<(), BlockError>) {
    let mut i = 0u8;
    while i < req.nwait {
        let p = req.waiters[i as usize];
        if p != 0 {
            unsafe { &*(p as *const IoWaiter) }.finish(res);
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
    ram().apply(&mut data[..], req)
}

fn fail_rest() {
    STATE.store(DeviceState::Failed.as_u8(), Ordering::Release);
    let mut dump = [None; MAX_QUEUE];
    let n = {
        let mut q = Q.lock();
        q.fail();
        q.drain(&mut dump)
    };
    let mut i = 0usize;
    while i < n {
        if let Some(r) = dump[i] {
            complete_req(&r, Err(BlockError::Failed));
        }
        i += 1;
    }
}

fn finish(mut req: Request, res: Result<(), BlockError>) {
    match res {
        Ok(()) => complete_req(&req, Ok(())),
        Err(e) if e.retryable() && req.retries_left > 0 => {
            req.retries_left -= 1;
            let mut q = Q.lock();
            if q.requeue(req).is_err() {
                drop(q);
                complete_req(&req, Err(BlockError::Failed));
                fail_rest();
            }
        }
        Err(e) if e.retryable() => {
            complete_req(&req, Err(BlockError::Failed));
            fail_rest();
        }
        Err(e) => complete_req(&req, Err(e)),
    }
}

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

/// Async submit. `buf` must stay live until `w` completes. Flush/barrier
/// / discard pass `ptr = 0`, `len = 0`.
pub fn submit(
    op: Op,
    lba: u64,
    nsect: u32,
    ptr: usize,
    len: usize,
    w: &IoWaiter,
) -> Result<(), BlockError> {
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
        Op::Flush | Op::Barrier => {
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
    let start = submit_req(req)?;
    kick_if(start);
    Ok(())
}

fn blocking(op: Op, lba: u64, nsect: u32, ptr: usize, len: usize) -> Result<(), BlockError> {
    let w = IoWaiter::new();
    submit(op, lba, nsect, ptr, len, &w)?;
    w.wait()
}

pub fn read(lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
    let bs = RAM0_BLOCK_SIZE as usize;
    if bs == 0 || buf.len() % bs != 0 {
        return Err(BlockError::Inval);
    }
    let nsect = (buf.len() / bs) as u32;
    blocking(
        Op::Read,
        lba,
        nsect,
        buf.as_mut_ptr() as usize,
        buf.len(),
    )
}

pub fn write(lba: u64, buf: &[u8]) -> Result<(), BlockError> {
    let bs = RAM0_BLOCK_SIZE as usize;
    if bs == 0 || buf.len() % bs != 0 {
        return Err(BlockError::Inval);
    }
    let nsect = (buf.len() / bs) as u32;
    blocking(Op::Write, lba, nsect, buf.as_ptr() as usize, buf.len())
}

pub fn flush() -> Result<(), BlockError> {
    blocking(Op::Flush, 0, 0, 0, 0)
}

pub fn discard(lba: u64, nsectors: u64) -> Result<(), BlockError> {
    if nsectors > u32::MAX as u64 {
        return Err(BlockError::Inval);
    }
    blocking(Op::Discard, lba, nsectors as u32, 0, 0)
}

pub fn barrier() -> Result<(), BlockError> {
    blocking(Op::Barrier, 0, 0, 0, 0)
}

pub fn live() -> bool {
    LIVE.load(Ordering::Acquire)
}

pub fn state() -> DeviceState {
    DeviceState::from_u8(STATE.load(Ordering::Acquire))
}

pub fn name() -> &'static str {
    RAM0_NAME
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

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn inject_io_fails(n: u32) {
    FAIL_NEXT.store(n, Ordering::SeqCst);
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn reset() {
    FAIL_NEXT.store(0, Ordering::SeqCst);
    STATE.store(DeviceState::Ready.as_u8(), Ordering::Release);
    let mut dump = [None; MAX_QUEUE];
    let n = {
        let mut q = Q.lock();
        q.failed = false;
        q.running = false;
        q.drain(&mut dump)
    };
    let mut i = 0usize;
    while i < n {
        if let Some(r) = dump[i] {
            complete_req(&r, Err(BlockError::Failed));
        }
        i += 1;
    }
}

struct Ram0;

impl block::BlockDevice for Ram0 {
    fn name(&self) -> &'static str {
        RAM0_NAME
    }
    fn logical_block_size(&self) -> u32 {
        RAM0_BLOCK_SIZE
    }
    fn capacity_sectors(&self) -> u64 {
        RAM0_SECTORS
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

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn device() -> &'static dyn block::BlockDevice {
    &Ram0
}

fn cmd_blk(_args: &[&str]) {
    let st = state().as_str();
    let _ = writeln!(
        Console,
        "vibeOS: blk: {} {} {} sectors {st} io {}",
        RAM0_NAME,
        RAM0_BLOCK_SIZE,
        RAM0_SECTORS,
        io_reqs()
    );
    let _ = crate::virtio_blk_init::shell_line(&mut Console);
    let _ = crate::part_init::shell_lines(&mut Console);
    let _ = crate::cache_init::shell_line(&mut Console);
}

pub fn init() {
    debug_assert_eq!(ram().byte_len(), RAM0_BYTES);
    {
        let mut d = DATA.lock();
        d.fill(0);
    }
    STATE.store(DeviceState::Ready.as_u8(), Ordering::Release);
    LIVE.store(true, Ordering::Release);
    let _ = write_marker(&mut Serial, RAM0_NAME, RAM0_SECTORS);
    let _ = writeln!(Serial);
    let _ = shell_init::register(Command {
        name: "blk",
        help: "block devices",
        run: cmd_blk,
    });
}
