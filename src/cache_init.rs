//! Write-back block cache. ROADMAP §7.4.
//!
//! Sits above [`BlockDevice`] miss paths. Lock dropped before device
//! I/O (RANK_DEVICE + blocking wait). A slot whose write is in flight is
//! in WRITEBACK (`vibeos::cache`), and a thread that needs it sleeps on
//! the slot's wait queue. [`flush`] writes each dirty page of its device
//! and waits for it, waits for every write already in flight on the
//! device (`blk-wb`'s and eviction writes), and only then sends the device
//! `Flush` (DESIGN §10.6). Phase 12 makes this cache each block device's
//! mapping in one page cache of mappings (DESIGN §10.6).

#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::block::BlockError;
use vibeos::cache::{self, Cache, CacheKey, CacheStats, DEFAULT_PAGES, FillNeed, FlushStep, PAGE};
use vibeos::lock::RANK_DEVICE;
use vibeos::sched::FAR_DEADLINE;
use vibeos::wait::WaitQueue;

use crate::block_init;
use crate::sync_init::SpinMutex;
use crate::thread_init;
use crate::virtio_blk_init;

pub const DEV_RAM0: u32 = 0;
pub const DEV_VDA: u32 = 1;

static CACHE: SpinMutex<Cache<DEFAULT_PAGES>> = SpinMutex::with_rank(Cache::new(), RANK_DEVICE);
static LIVE: AtomicBool = AtomicBool::new(false);

/// One wait queue per cache slot, for threads waiting out its writeback.
struct SlotWaits([UnsafeCell<WaitQueue>; DEFAULT_PAGES]);

// SAFETY: each queue is touched only inside `thread_init::with_sched`,
// which serializes every access to it; established here, by
// `wait_writeback` and `end_writeback_and_wake`, the only users.
unsafe impl Sync for SlotWaits {}

static SLOT_WQ: SlotWaits = SlotWaits([const { UnsafeCell::new(WaitQueue::new()) }; DEFAULT_PAGES]);

fn geom(dev: u32) -> Result<(u32, u64), BlockError> {
    match dev {
        DEV_RAM0 => {
            if !block_init::live() {
                return Err(BlockError::Failed);
            }
            Ok((
                block_init::logical_block_size(),
                block_init::capacity_sectors(),
            ))
        }
        DEV_VDA => {
            if !virtio_blk_init::live() {
                return Err(BlockError::Failed);
            }
            Ok((
                virtio_blk_init::logical_block_size(),
                virtio_blk_init::capacity_sectors(),
            ))
        }
        _ => Err(BlockError::Inval),
    }
}

fn raw_read(dev: u32, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
    match dev {
        DEV_RAM0 => block_init::read(lba, buf),
        DEV_VDA => virtio_blk_init::read(lba, buf),
        _ => Err(BlockError::Inval),
    }
}

fn raw_write(dev: u32, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
    match dev {
        DEV_RAM0 => block_init::write(lba, buf),
        DEV_VDA => virtio_blk_init::write(lba, buf),
        _ => Err(BlockError::Inval),
    }
}

fn raw_flush(dev: u32) -> Result<(), BlockError> {
    match dev {
        DEV_RAM0 => block_init::flush(),
        DEV_VDA => virtio_blk_init::flush(),
        _ => Err(BlockError::Inval),
    }
}

fn page_io_len(dev_bytes: u64, offset: u64) -> Result<usize, BlockError> {
    if offset >= dev_bytes {
        return Err(BlockError::Inval);
    }
    let left = (dev_bytes - offset) as usize;
    Ok(left.min(PAGE))
}

fn backend_read(dev: u32, offset: u64, page: &mut [u8]) -> Result<(), BlockError> {
    if page.len() < PAGE {
        return Err(BlockError::Inval);
    }
    let (bs, cap) = geom(dev)?;
    let bs = bs as u64;
    if bs == 0 || !offset.is_multiple_of(bs) {
        return Err(BlockError::Inval);
    }
    let nbytes = (cap as u128)
        .checked_mul(bs as u128)
        .ok_or(BlockError::Inval)? as u64;
    let n = page_io_len(nbytes, offset)?;
    if n % bs as usize != 0 {
        return Err(BlockError::Inval);
    }
    page[..PAGE].fill(0);
    raw_read(dev, offset / bs, &mut page[..n])
}

fn backend_write(dev: u32, offset: u64, page: &[u8]) -> Result<(), BlockError> {
    if page.len() < PAGE {
        return Err(BlockError::Inval);
    }
    let (bs, cap) = geom(dev)?;
    let bs = bs as u64;
    if bs == 0 || !offset.is_multiple_of(bs) {
        return Err(BlockError::Inval);
    }
    let nbytes = (cap as u128)
        .checked_mul(bs as u128)
        .ok_or(BlockError::Inval)? as u64;
    let n = page_io_len(nbytes, offset)?;
    if n % bs as usize != 0 {
        return Err(BlockError::Inval);
    }
    raw_write(dev, offset / bs, &page[..n])
}

fn page_vec() -> alloc::vec::Vec<u8> {
    alloc::vec![0u8; PAGE]
}

/// Sleep until `slot`'s writeback ends. Returns at once if it is not in
/// writeback. SCHED then the cache lock (RANK_DEVICE) is a legal order.
fn wait_writeback(slot: usize) {
    let Some(wq) = SLOT_WQ.0.get(slot) else {
        return;
    };
    loop {
        let park = thread_init::with_sched(|s| {
            if !CACHE.lock().in_writeback(slot) {
                return false;
            }
            // SAFETY: the slot's queue is touched only under
            // `thread_init::with_sched`, held here (see `SlotWaits`).
            s.begin_wait(unsafe { &mut *wq.get() }, FAR_DEADLINE);
            true
        });
        if !park {
            return;
        }
        thread_init::schedule();
    }
}

/// End `slot`'s writeback under the cache lock, drop it, then wake the
/// slot's waiters under SCHED (never SCHED under the cache lock).
fn end_writeback_and_wake(slot: usize, key: CacheKey, res: Result<(), BlockError>) {
    {
        let mut c = CACHE.lock();
        if res.is_ok() {
            c.stats.device_writes = c.stats.device_writes.saturating_add(1);
        }
        c.end_writeback(slot, key, res);
    }
    if let Some(wq) = SLOT_WQ.0.get(slot) {
        thread_init::with_sched(|s| {
            // SAFETY: the slot's queue is touched only under
            // `thread_init::with_sched`, held here (see `SlotWaits`).
            s.wake_all(unsafe { &mut *wq.get() });
        });
    }
}

/// Write a `Writeback` fill's victim from `evict` and end its writeback.
fn write_victim(fill: &cache::Fill, evict: &[u8]) -> Result<(), BlockError> {
    let res = backend_write(fill.evict_key.dev, fill.evict_key.offset, evict);
    end_writeback_and_wake(fill.slot, fill.evict_key, res);
    res
}

/// Read a `Read` fill's page, or abort the fill.
fn fill_read(fill: &cache::Fill, page: &mut [u8]) -> Result<(), BlockError> {
    match backend_read(fill.key.dev, fill.key.offset, page) {
        Ok(()) => {
            let mut c = CACHE.lock();
            c.stats.device_reads = c.stats.device_reads.saturating_add(1);
            Ok(())
        }
        Err(e) => {
            CACHE.lock().abort_fill(fill.slot);
            Err(e)
        }
    }
}

/// A `None` fill: sleep out a slot in writeback, or yield to a fill in
/// progress (the FILLING state is ROADMAP §12.5's).
fn wait_busy(fill: &cache::Fill, spins: &mut u32) -> Result<(), BlockError> {
    if CACHE.lock().in_writeback(fill.slot) {
        wait_writeback(fill.slot);
        return Ok(());
    }
    *spins = spins.saturating_add(1);
    if *spins > 1_000_000 {
        return Err(BlockError::Io);
    }
    thread_init::yield_now();
    Ok(())
}

fn byte_off(dev: u32, lba: u64) -> Result<u64, BlockError> {
    let (bs, cap) = geom(dev)?;
    if lba >= cap {
        return Err(BlockError::Inval);
    }
    lba.checked_mul(bs as u64).ok_or(BlockError::Inval)
}

fn bump_readahead(evict: &mut [u8], page: &mut [u8]) {
    let Some(rk) = ({ CACHE.lock().want_readahead() }) else {
        return;
    };
    if CACHE.lock().find(rk).is_some() {
        return;
    }
    let mut dummy = [0u8; 1];
    let fill = {
        let mut c = CACHE.lock();
        match c.plan_read(rk, 0, &mut dummy, evict) {
            Ok(Some(f)) => f,
            _ => return,
        }
    };
    match fill.need {
        FillNeed::None => {}
        // A readahead skips its page after a victim's writeback. A failed
        // write stays recorded: `end_writeback` leaves the page dirty for
        // the next flush to retry and report.
        FillNeed::Writeback => {
            let _kept_dirty = write_victim(&fill, evict).is_err();
        }
        FillNeed::Read => {
            if fill_read(&fill, page).is_ok() {
                let mut one = [0u8; 1];
                let mut c = CACHE.lock();
                if c.install_read(&fill, page, 0, &mut one).is_err() {
                    c.abort_fill(fill.slot);
                }
            }
        }
    }
}

pub fn read(dev: u32, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
    if !LIVE.load(Ordering::Acquire) {
        return raw_read(dev, lba, buf);
    }
    let (bs, cap) = geom(dev)?;
    if bs == 0 || !buf.len().is_multiple_of(bs as usize) {
        return Err(BlockError::Inval);
    }
    let nsect = (buf.len() / bs as usize) as u64;
    if lba.checked_add(nsect).map(|e| e > cap).unwrap_or(true) {
        return Err(BlockError::Inval);
    }
    let base = byte_off(dev, lba)?;
    let mut evict = page_vec();
    let mut page = page_vec();
    let mut done = 0usize;
    let mut spins = 0u32;
    while done < buf.len() {
        let off = base.saturating_add(done as u64);
        let key = CacheKey::page(dev, off);
        let pin = (off as usize) & (PAGE - 1);
        let n = (PAGE - pin).min(buf.len() - done);
        let plan = {
            let mut c = CACHE.lock();
            c.plan_read(key, pin, &mut buf[done..done + n], &mut evict)?
        };
        if let Some(fill) = plan {
            match fill.need {
                FillNeed::None => {
                    wait_busy(&fill, &mut spins)?;
                    continue;
                }
                FillNeed::Writeback => {
                    write_victim(&fill, &evict)?;
                    continue;
                }
                FillNeed::Read => {
                    fill_read(&fill, &mut page)?;
                    CACHE
                        .lock()
                        .install_read(&fill, &page, pin, &mut buf[done..done + n])?;
                }
            }
        }
        done += n;
        spins = 0;
    }
    bump_readahead(&mut evict, &mut page);
    Ok(())
}

pub fn write(dev: u32, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
    if !LIVE.load(Ordering::Acquire) {
        return raw_write(dev, lba, buf);
    }
    let (bs, cap) = geom(dev)?;
    if bs == 0 || !buf.len().is_multiple_of(bs as usize) {
        return Err(BlockError::Inval);
    }
    let nsect = (buf.len() / bs as usize) as u64;
    if lba.checked_add(nsect).map(|e| e > cap).unwrap_or(true) {
        return Err(BlockError::Inval);
    }
    let base = byte_off(dev, lba)?;
    let mut evict = page_vec();
    let mut page = page_vec();
    let mut done = 0usize;
    let mut spins = 0u32;
    while done < buf.len() {
        let off = base.saturating_add(done as u64);
        let key = CacheKey::page(dev, off);
        let pin = (off as usize) & (PAGE - 1);
        let n = (PAGE - pin).min(buf.len() - done);
        let plan = {
            let mut c = CACHE.lock();
            c.plan_write(key, pin, &buf[done..done + n], &mut evict)?
        };
        if let Some(fill) = plan {
            match fill.need {
                FillNeed::None => {
                    wait_busy(&fill, &mut spins)?;
                    continue;
                }
                FillNeed::Writeback => {
                    write_victim(&fill, &evict)?;
                    continue;
                }
                FillNeed::Read => {
                    fill_read(&fill, &mut page)?;
                    CACHE
                        .lock()
                        .install_write(&fill, &page, pin, &buf[done..done + n])?;
                }
            }
        }
        done += n;
        spins = 0;
    }
    Ok(())
}

/// `blk-wb`'s pass: write back each dirty page not already in writeback.
fn writeback_dev(dev: Option<u32>) -> Result<(), BlockError> {
    let mut data = page_vec();
    let mut start = 0usize;
    loop {
        let next = {
            let mut c = CACHE.lock();
            c.take_dirty(start, dev, &mut data)
        };
        let Some((slot, key)) = next else {
            break;
        };
        let res = backend_write(key.dev, key.offset, &data);
        end_writeback_and_wake(slot, key, res);
        res?;
        start = slot + 1;
    }
    Ok(())
}

/// Make every write to `dev` through the cache durable: write each dirty
/// page and wait for it, wait for each write already in flight (`blk-wb`'s
/// and eviction writes), write pages dirtied meanwhile, and send the
/// device `Flush` only when `dev` has no dirty and no writeback slot.
pub fn flush(dev: u32) -> Result<(), BlockError> {
    let mut data = page_vec();
    loop {
        let step = { CACHE.lock().flush_step(Some(dev), &mut data) };
        match step {
            FlushStep::Write(slot, key) => {
                let res = backend_write(key.dev, key.offset, &data);
                end_writeback_and_wake(slot, key, res);
                res?;
            }
            FlushStep::Wait(slot, _) => wait_writeback(slot),
            FlushStep::Flush => {
                raw_flush(dev)?;
                let mut c = CACHE.lock();
                c.stats.device_flushes = c.stats.device_flushes.saturating_add(1);
                return Ok(());
            }
        }
    }
}

pub fn stats() -> CacheStats {
    CACHE.lock().stats
}

pub fn over_dirty() -> bool {
    CACHE.lock().over_dirty_ratio()
}

pub fn live() -> bool {
    LIVE.load(Ordering::Acquire)
}

fn writeback_main() {
    loop {
        thread_init::sleep_ms(50);
        if !LIVE.load(Ordering::Acquire) {
            continue;
        }
        if over_dirty() {
            let _ = writeback_dev(None);
        }
    }
}

pub fn shell_line(f: &mut impl core::fmt::Write) -> core::fmt::Result {
    if !live() {
        return Ok(());
    }
    let s = stats();
    writeln!(
        f,
        "vibeOS: cache: hits {} misses {} dirty {} device {} evicts {}",
        s.hits,
        s.misses,
        {
            let c = CACHE.lock();
            c.dirty_count()
        },
        s.device_reqs(),
        s.evicts
    )
}

pub fn init() {
    LIVE.store(true, Ordering::Release);
    let _ = thread_init::spawn("blk-wb", writeback_main);
}
