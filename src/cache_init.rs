//! Write-back block cache. ROADMAP §7.6.
//!
//! Sits above [`BlockDevice`] miss paths. Lock dropped before device
//! I/O (RANK_DEVICE + blocking wait). Flush/barrier write dirty pages
//! then call down into the device. Phase 10 reuses these pages.

#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]

use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::block::BlockError;
use vibeos::cache::{self, Cache, CacheKey, CacheStats, DEFAULT_PAGES, FillNeed, PAGE};
use vibeos::lock::RANK_DEVICE;

use crate::block_init;
use crate::sync_init::SpinMutex;
use crate::thread_init;
use crate::virtio_blk_init;

pub const DEV_RAM0: u32 = 0;
pub const DEV_VDA: u32 = 1;

static CACHE: SpinMutex<Cache<DEFAULT_PAGES>> = SpinMutex::with_rank(Cache::new(), RANK_DEVICE);
static LIVE: AtomicBool = AtomicBool::new(false);

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

fn do_fill_io(fill: &cache::Fill, evict: &[u8], page: &mut [u8]) -> Result<(), BlockError> {
    match fill.need {
        FillNeed::None => Ok(()),
        FillNeed::Writeback | FillNeed::WritebackThenRead => {
            backend_write(fill.evict_key.dev, fill.evict_key.offset, evict)?;
            {
                let mut c = CACHE.lock();
                c.stats.device_writes = c.stats.device_writes.saturating_add(1);
            }
            if matches!(fill.need, FillNeed::WritebackThenRead) {
                backend_read(fill.key.dev, fill.key.offset, page)?;
                let mut c = CACHE.lock();
                c.stats.device_reads = c.stats.device_reads.saturating_add(1);
            }
            Ok(())
        }
        FillNeed::Read => {
            backend_read(fill.key.dev, fill.key.offset, page)?;
            let mut c = CACHE.lock();
            c.stats.device_reads = c.stats.device_reads.saturating_add(1);
            Ok(())
        }
    }
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
        FillNeed::None => {
            CACHE.lock().abort_fill(fill.slot);
        }
        FillNeed::Writeback => {
            if backend_write(fill.evict_key.dev, fill.evict_key.offset, evict).is_err() {
                CACHE.lock().restore_evict(fill.slot, fill.evict_key, evict);
                return;
            }
            CACHE.lock().abort_fill(fill.slot);
        }
        FillNeed::Read | FillNeed::WritebackThenRead => {
            if matches!(fill.need, FillNeed::WritebackThenRead)
                && backend_write(fill.evict_key.dev, fill.evict_key.offset, evict).is_err()
            {
                CACHE.lock().restore_evict(fill.slot, fill.evict_key, evict);
                return;
            }
            if backend_read(rk.dev, rk.offset, page).is_ok() {
                let mut one = [0u8; 1];
                let mut c = CACHE.lock();
                c.stats.device_reads = c.stats.device_reads.saturating_add(1);
                let _ = c.install_read(&fill, page, 0, &mut one);
            } else {
                CACHE.lock().abort_fill(fill.slot);
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
        match plan {
            None => {}
            Some(fill) => {
                if matches!(fill.need, FillNeed::None) {
                    spins = spins.saturating_add(1);
                    if spins > 1_000_000 {
                        return Err(BlockError::Io);
                    }
                    thread_init::yield_now();
                    continue;
                }
                match do_fill_io(&fill, &evict, &mut page) {
                    Ok(()) => {
                        CACHE
                            .lock()
                            .install_read(&fill, &page, pin, &mut buf[done..done + n])?;
                    }
                    Err(e) => {
                        let mut c = CACHE.lock();
                        if matches!(fill.need, FillNeed::Writeback | FillNeed::WritebackThenRead) {
                            c.restore_evict(fill.slot, fill.evict_key, &evict);
                        } else {
                            c.abort_fill(fill.slot);
                        }
                        return Err(e);
                    }
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
        match plan {
            None => {}
            Some(fill) => match fill.need {
                FillNeed::None => {
                    spins = spins.saturating_add(1);
                    if spins > 1_000_000 {
                        return Err(BlockError::Io);
                    }
                    thread_init::yield_now();
                    continue;
                }
                FillNeed::Writeback => {
                    if let Err(e) = backend_write(fill.evict_key.dev, fill.evict_key.offset, &evict)
                    {
                        CACHE
                            .lock()
                            .restore_evict(fill.slot, fill.evict_key, &evict);
                        return Err(e);
                    }
                    let mut c = CACHE.lock();
                    c.stats.device_writes = c.stats.device_writes.saturating_add(1);
                }
                FillNeed::Read | FillNeed::WritebackThenRead => {
                    match do_fill_io(&fill, &evict, &mut page) {
                        Ok(()) => {
                            CACHE
                                .lock()
                                .install_write(&fill, &page, pin, &buf[done..done + n])?;
                        }
                        Err(e) => {
                            let mut c = CACHE.lock();
                            if matches!(fill.need, FillNeed::WritebackThenRead) {
                                c.restore_evict(fill.slot, fill.evict_key, &evict);
                            } else {
                                c.abort_fill(fill.slot);
                            }
                            return Err(e);
                        }
                    }
                }
            },
        }
        done += n;
        spins = 0;
    }
    Ok(())
}

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
        if let Err(e) = backend_write(key.dev, key.offset, &data) {
            CACHE.lock().mark_dirty(slot, key);
            return Err(e);
        }
        {
            let mut c = CACHE.lock();
            c.stats.device_writes = c.stats.device_writes.saturating_add(1);
        }
        start = slot + 1;
    }
    Ok(())
}

pub fn flush(dev: u32) -> Result<(), BlockError> {
    writeback_dev(Some(dev))?;
    raw_flush(dev)?;
    {
        let mut c = CACHE.lock();
        c.stats.device_flushes = c.stats.device_flushes.saturating_add(1);
    }
    Ok(())
}

#[allow(dead_code)]
pub fn barrier(dev: u32) -> Result<(), BlockError> {
    writeback_dev(Some(dev))
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
