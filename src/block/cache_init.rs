//! Write-back block cache. ROADMAP §7.4.
//!
//! [`PAGE_CACHE`] is every disk's `BlockCache` (`vibeos::block::blockdev`):
//! a disk's `BlockRef` reads, writes and flushes through it, and its miss
//! path reaches the driver through the same handle's `read_dev`,
//! `write_dev` and `flush_dev`. Pages are keyed by (`BlockRef` id, page
//! offset). Lock dropped before device I/O (RANK_DEVICE + blocking wait).
//! A slot whose write is in flight is in WRITEBACK (`vibeos::cache`), and
//! a thread that needs it sleeps on the slot's wait queue. A page whose
//! device is gone is dropped, never retried (DEVICES.md §12.4 rule 9).
//! [`PageCache::flush`] sweeps the slots once: it writes each page of its
//! device dirty when it began and waits for it, waits for every write in
//! flight on the device (`blk-wb`'s and eviction writes), writes again a
//! page whose write it waited on failed, and only then sends the device
//! `Flush` (DESIGN §10.6). Phase 12 makes this cache each block device's
//! mapping in one page cache of mappings (DESIGN §10.6).

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::block::BlockError;
use vibeos::block::blockdev::{BlockCache, BlockRef};
use vibeos::cache::{self, Cache, CacheKey, CacheStats, DEFAULT_PAGES, FillNeed, FlushStep, PAGE};
use vibeos::kalloc::TryVec;
use vibeos::lock::RANK_DEVICE;
use vibeos::sched::FAR_DEADLINE;
use vibeos::wait::WaitQueue;

use crate::block::blockdev_init;
use crate::sync_init::SpinMutex;
use crate::thread_init;

static CACHE: SpinMutex<Cache<DEFAULT_PAGES>> = SpinMutex::with_rank(Cache::new(), RANK_DEVICE);
static LIVE: AtomicBool = AtomicBool::new(false);
/// Whether `init` started `blk-wb`, the writeback thread.
static WRITEBACK: AtomicBool = AtomicBool::new(false);

/// One wait queue per cache slot, for threads waiting out its writeback.
struct SlotWaits([UnsafeCell<WaitQueue>; DEFAULT_PAGES]);

// SAFETY: each queue is touched only inside `thread_init::with_sched`,
// which serializes every access to it; established here, by
// `wait_writeback`, `wait_writeback_end`, `wait_fill` and `wake_slot`,
// the only functions that dereference `SLOT_WQ`'s cells.
unsafe impl Sync for SlotWaits {}

static SLOT_WQ: SlotWaits = SlotWaits([const { UnsafeCell::new(WaitQueue::new()) }; DEFAULT_PAGES]);

/// Block size and capacity of `dev`, in logical blocks.
fn geom(dev: &BlockRef) -> Result<(u32, u64), BlockError> {
    Ok((dev.logical_block_size()?, dev.capacity_sectors()?))
}

fn page_io_len(dev_bytes: u64, offset: u64) -> Result<usize, BlockError> {
    if offset >= dev_bytes {
        return Err(BlockError::Inval);
    }
    let left = (dev_bytes - offset) as usize;
    Ok(left.min(PAGE))
}

fn backend_read(dev: &BlockRef, offset: u64, page: &mut [u8]) -> Result<(), BlockError> {
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
    dev.read_dev(offset / bs, &mut page[..n])
}

fn backend_write(dev: &BlockRef, offset: u64, page: &[u8]) -> Result<(), BlockError> {
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
    dev.write_dev(offset / bs, &page[..n])
}

/// The device a page key names: `dev` when the ids match, else a lookup
/// by id, made with no cache lock held. `Gone` when the id is no longer
/// registered.
fn key_dev(dev: Option<&BlockRef>, key: CacheKey) -> Result<BlockRef, BlockError> {
    match dev {
        Some(d) if d.id() == key.dev => Ok(d.clone()),
        _ => blockdev_init::lookup_id(key.dev).ok_or(BlockError::Gone),
    }
}

/// A zeroed page buffer; `NoMem` when the heap refuses it (DESIGN §4.4).
fn page_vec() -> Result<TryVec<u8>, BlockError> {
    let mut v = TryVec::try_with_capacity(PAGE).map_err(|_| BlockError::NoMem)?;
    let zero = [0u8; 256];
    while v.len() < PAGE {
        // Within the reserved capacity: never reallocates.
        v.try_extend_from_slice(&zero)
            .map_err(|_| BlockError::NoMem)?;
    }
    Ok(v)
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

/// Sleep until `slot`'s writeback `start` ends ([`Cache::writeback_start`]).
/// Returns at once if that one is not in flight, so a writeback begun
/// after it is not waited for. As [`wait_writeback`], the check and the
/// wait share one SCHED section.
fn wait_writeback_end(slot: usize, start: u64) {
    let Some(wq) = SLOT_WQ.0.get(slot) else {
        return;
    };
    loop {
        let park = thread_init::with_sched(|s| {
            if CACHE.lock().writeback_start(slot) != Some(start) {
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

/// Sleep until a read's fill of `slot` ends, which wakes the slot's queue
/// ([`end_fill`], [`fill_read`]). Returns at once if none is filling it.
/// As [`wait_writeback`], the check and the wait share one SCHED section.
fn wait_fill(slot: usize) {
    let Some(wq) = SLOT_WQ.0.get(slot) else {
        return;
    };
    // AcqRel: pairs with the Acquire loads in `block::ktest::cache_read_waits_for_fill`.
    #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
    testing::FILL_WAITS.fetch_add(1, Ordering::AcqRel);
    loop {
        let park = thread_init::with_sched(|s| {
            if !CACHE.lock().filling(slot) {
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

/// Wake `slot`'s waiters under SCHED, with the cache lock dropped (never
/// SCHED under the cache lock).
fn wake_slot(slot: usize) {
    if let Some(wq) = SLOT_WQ.0.get(slot) {
        thread_init::with_sched(|s| {
            // SAFETY: the slot's queue is touched only under
            // `thread_init::with_sched`, held here (see `SlotWaits`).
            s.wake_all(unsafe { &mut *wq.get() });
        });
    }
}

/// End `slot`'s writeback under the cache lock, drop it, then wake the
/// slot's waiters.
fn end_writeback_and_wake(slot: usize, key: CacheKey, res: Result<(), BlockError>) {
    {
        let mut c = CACHE.lock();
        if res.is_ok() {
            c.stats.device_writes = c.stats.device_writes.saturating_add(1);
        }
        c.end_writeback(slot, key, res);
    }
    wake_slot(slot);
}

/// End `fill` with `install`, an `install_*` call, under the cache lock;
/// abort the fill when it fails, so the slot is never left filling; then
/// wake the slot's waiters.
fn end_fill<T>(
    fill: &cache::Fill,
    install: impl FnOnce(&mut Cache<DEFAULT_PAGES>) -> Result<T, BlockError>,
) -> Result<T, BlockError> {
    let r = {
        let mut c = CACHE.lock();
        let r = install(&mut c);
        if r.is_err() {
            c.abort_fill(fill);
        }
        r
    };
    wake_slot(fill.slot);
    r
}

/// Write back `key`'s page from `data` through its device, then end the
/// slot's writeback. A page whose device is gone is dropped with every
/// other page of that id, and the write counts as done: it can never
/// succeed, and retrying it would stall its caller for good
/// (INVARIANTS.md §2.5's recorded state).
fn write_page(
    dev: Option<&BlockRef>,
    slot: usize,
    key: CacheKey,
    data: &[u8],
) -> Result<(), BlockError> {
    let res = key_dev(dev, key).and_then(|d| backend_write(&d, key.offset, data));
    end_writeback_and_wake(slot, key, res);
    match res {
        Err(BlockError::Gone) => {
            CACHE.lock().drop_dev(key.dev);
            Ok(())
        }
        r => r,
    }
}

/// Write a `Writeback` fill's victim from `evict` and end its writeback.
/// The victim may belong to another device than `dev`.
fn write_victim(dev: &BlockRef, fill: &cache::Fill, evict: &[u8]) -> Result<(), BlockError> {
    write_page(Some(dev), fill.slot, fill.evict_key, evict)
}

/// Read a `Read` fill's page, or abort the fill and wake its waiters.
fn fill_read(dev: &BlockRef, fill: &cache::Fill, page: &mut [u8]) -> Result<(), BlockError> {
    #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
    testing::fill_hold_point(fill.key);
    match backend_read(dev, fill.key.offset, page) {
        Ok(()) => {
            let mut c = CACHE.lock();
            c.stats.device_reads = c.stats.device_reads.saturating_add(1);
            Ok(())
        }
        Err(e) => {
            CACHE.lock().abort_fill(fill);
            wake_slot(fill.slot);
            Err(e)
        }
    }
}

/// A `None` fill: sleep out the slot's writeback, or the read filling
/// it. Either ends, by the device request's own deadline at worst (BLOCK.md
/// §10.3); a slot that is neither by now is planned again.
fn wait_busy(fill: &cache::Fill) {
    let (wb, filling) = {
        let c = CACHE.lock();
        (c.in_writeback(fill.slot), c.filling(fill.slot))
    };
    if wb {
        wait_writeback(fill.slot);
    } else if filling {
        wait_fill(fill.slot);
    }
}

fn byte_off(dev: &BlockRef, lba: u64) -> Result<u64, BlockError> {
    let (bs, cap) = geom(dev)?;
    if lba >= cap {
        return Err(BlockError::Inval);
    }
    lba.checked_mul(bs as u64).ok_or(BlockError::Inval)
}

fn bump_readahead(dev: &BlockRef, evict: &mut [u8], page: &mut [u8]) {
    let Some(rk) = ({ CACHE.lock().want_readahead() }) else {
        return;
    };
    // Readahead follows `dev`'s own sequential run only.
    if rk.dev != dev.id() {
        return;
    }
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
            let _kept_dirty = write_victim(dev, &fill, evict).is_err();
        }
        FillNeed::Read => {
            if fill_read(dev, &fill, page).is_ok() {
                let mut one = [0u8; 1];
                let _skipped =
                    end_fill(&fill, |c| c.install_read(&fill, page, 0, &mut one)).is_err();
            }
        }
    }
}

fn read(dev: &BlockRef, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
    // Acquire: pairs with the Release store in `init`.
    if !LIVE.load(Ordering::Acquire) {
        return dev.read_dev(lba, buf);
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
    let mut evict = page_vec()?;
    let mut page = page_vec()?;
    let mut done = 0usize;
    while done < buf.len() {
        let off = base.saturating_add(done as u64);
        let key = CacheKey::page(dev.id(), off);
        let pin = (off as usize) & (PAGE - 1);
        let n = (PAGE - pin).min(buf.len() - done);
        let plan = {
            let mut c = CACHE.lock();
            c.plan_read(key, pin, &mut buf[done..done + n], &mut evict)?
        };
        if let Some(fill) = plan {
            match fill.need {
                FillNeed::None => {
                    wait_busy(&fill);
                    continue;
                }
                FillNeed::Writeback => {
                    write_victim(dev, &fill, &evict)?;
                    continue;
                }
                FillNeed::Read => {
                    fill_read(dev, &fill, &mut page)?;
                    end_fill(&fill, |c| {
                        c.install_read(&fill, &page, pin, &mut buf[done..done + n])
                    })?;
                }
            }
        }
        done += n;
    }
    bump_readahead(dev, &mut evict, &mut page);
    Ok(())
}

fn write(dev: &BlockRef, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
    // Acquire: pairs with the Release store in `init`.
    if !LIVE.load(Ordering::Acquire) {
        return dev.write_dev(lba, buf);
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
    let mut evict = page_vec()?;
    let mut page = page_vec()?;
    let mut done = 0usize;
    while done < buf.len() {
        let off = base.saturating_add(done as u64);
        let key = CacheKey::page(dev.id(), off);
        let pin = (off as usize) & (PAGE - 1);
        let n = (PAGE - pin).min(buf.len() - done);
        let plan = {
            let mut c = CACHE.lock();
            c.plan_write(key, pin, &buf[done..done + n], &mut evict)?
        };
        if let Some(fill) = plan {
            match fill.need {
                FillNeed::None => {
                    wait_busy(&fill);
                    continue;
                }
                FillNeed::Writeback => {
                    write_victim(dev, &fill, &evict)?;
                    continue;
                }
                FillNeed::Read => {
                    fill_read(dev, &fill, &mut page)?;
                    // A slot taken back during the read installs nothing:
                    // the write is planned again.
                    if !end_fill(&fill, |c| {
                        c.install_write(&fill, &page, pin, &buf[done..done + n])
                    })? {
                        continue;
                    }
                }
            }
        }
        done += n;
    }
    Ok(())
}

/// `blk-wb`'s pass: write back each dirty page not already in writeback,
/// of device id `dev` or of every device. Each page's device is looked up
/// by its id; a gone device's pages are dropped ([`write_page`]).
fn writeback_dev(dev: Option<u64>) -> Result<(), BlockError> {
    let mut data = page_vec()?;
    let mut start = 0usize;
    loop {
        let next = {
            let mut c = CACHE.lock();
            c.take_dirty(start, dev, &mut data)
        };
        let Some((slot, key)) = next else {
            break;
        };
        #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
        testing::hold_point(key);
        write_page(None, slot, key, &data)?;
        start = slot + 1;
    }
    Ok(())
}

/// The page cache every disk's `BlockRef` reads and writes through
/// (`vibeos::block::blockdev::BlockCache`). Until [`init`] each call goes
/// straight to the device.
pub struct PageCache;

pub static PAGE_CACHE: PageCache = PageCache;

impl BlockCache for PageCache {
    fn read(&self, dev: &BlockRef, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        read(dev, lba, buf)
    }

    fn write(&self, dev: &BlockRef, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
        write(dev, lba, buf)
    }

    /// Make every write to `dev` through the cache that returned before
    /// the flush began durable: one sweep of the slots writes each page of
    /// `dev` dirty then and waits for it, and waits for each write of `dev`
    /// in flight (`blk-wb`'s and eviction writes), before the device
    /// `Flush` ([`Cache::flush_slot`]). A page whose write it waited on
    /// failed it writes itself, and returns that write's error
    /// ([`Cache::flush_waited`]). A writer that keeps dirtying pages cannot
    /// hold it off, since the sweep visits each slot once.
    fn flush(&self, dev: &BlockRef) -> Result<(), BlockError> {
        // Acquire: pairs with the Release store in `init`.
        if !LIVE.load(Ordering::Acquire) {
            return dev.flush_dev();
        }
        let mut data = page_vec()?;
        let mut sweep = CACHE.lock().flush_sweep(Some(dev.id()));
        loop {
            let step = { CACHE.lock().flush_next(&mut sweep, &mut data) };
            match step {
                None => break,
                Some(FlushStep::Write(s, key)) => {
                    write_page(Some(dev), s, key, &data)?;
                    #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
                    testing::after_flush_write(dev.id());
                }
                Some(wait @ FlushStep::Wait { slot: s, start, .. }) => {
                    wait_writeback_end(s, start);
                    CACHE.lock().flush_waited(&mut sweep, wait)?;
                }
                // `flush_next` never returns it.
                Some(FlushStep::Done) => {}
            }
        }
        dev.flush_dev()?;
        let mut c = CACHE.lock();
        c.stats.device_flushes = c.stats.device_flushes.saturating_add(1);
        Ok(())
    }
}

pub fn stats() -> CacheStats {
    CACHE.lock().stats
}

pub fn over_dirty() -> bool {
    CACHE.lock().over_dirty_ratio()
}

pub fn live() -> bool {
    // Acquire: pairs with the Release store in `init`.
    LIVE.load(Ordering::Acquire)
}

fn writeback_main() {
    loop {
        thread_init::sleep_ms(50);
        // Acquire: pairs with the Release store in `init`.
        if !LIVE.load(Ordering::Acquire) {
            continue;
        }
        #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
        if !testing::wb_pass_begin() {
            continue;
        }
        if over_dirty()
            && let Err(e) = writeback_dev(None)
        {
            crate::klog_ratelimited!(
                1000,
                vibeos::log::Level::Warn,
                "vibeOS: cache: background writeback failed: {}",
                e.as_str()
            );
        }
        #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
        testing::wb_pass_end();
    }
}

pub fn shell_line(f: &mut impl core::fmt::Write) -> core::fmt::Result {
    if !live() {
        return Ok(());
    }
    let s = stats();
    writeln!(
        f,
        "vibeOS: cache: hits {} misses {} dirty {} device {} evicts {}{}",
        s.hits,
        s.misses,
        {
            let c = CACHE.lock();
            c.dirty_count()
        },
        s.device_reqs(),
        s.evicts,
        // Acquire: pairs with the Release store in `init`.
        if WRITEBACK.load(Ordering::Acquire) {
            ""
        } else {
            " writeback none"
        }
    )
}

pub fn init() {
    // Release: pairs with every Acquire load of `LIVE` above.
    LIVE.store(true, Ordering::Release);
    match thread_init::spawn("blk-wb", writeback_main) {
        // Release: pairs with the Acquire load in `shell_line`.
        Ok(_) => WRITEBACK.store(true, Ordering::Release),
        // The cache runs without it: flushes and evictions still write
        // dirty pages, and the shell line says there is no writer thread
        // (MEMORY.md §4.4).
        Err(e) => crate::klog!(
            vibeos::log::Level::Error,
            "cache: blk-wb spawn failed: {}",
            e.as_str()
        ),
    }
}

/// A one-shot hold of `blk-wb`'s write of one page, so a test can find
/// [`PageCache::flush`] waiting for it (ROADMAP §10.11).
#[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
pub mod testing {
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use vibeos::cache::CacheKey;

    const UNARMED: u64 = u64::MAX;

    // The hold's state; `block::ktest`'s setters arm and read it.
    pub(in crate::block) const UNARMED_OFF: u64 = UNARMED;
    pub(in crate::block) static DEV: AtomicU64 = AtomicU64::new(0);
    pub(in crate::block) static OFF: AtomicU64 = AtomicU64::new(UNARMED);
    pub(in crate::block) static HELD: AtomicBool = AtomicBool::new(false);
    pub(in crate::block) static RELEASE: AtomicBool = AtomicBool::new(false);

    /// Drop the cached page `key` without writing it back: a test that
    /// flushed it first makes its next read a miss.
    pub(in crate::block) fn forget(key: CacheKey) {
        super::CACHE.lock().invalidate(key);
    }

    /// Times a reader has waited on a slot another read was filling.
    pub(in crate::block) static FILL_WAITS: AtomicU64 = AtomicU64::new(0);
    // The fill hold's state, as the writeback hold's above.
    pub(in crate::block) static FILL_DEV: AtomicU64 = AtomicU64::new(0);
    pub(in crate::block) static FILL_OFF: AtomicU64 = AtomicU64::new(UNARMED);
    pub(in crate::block) static FILL_HELD: AtomicBool = AtomicBool::new(false);
    pub(in crate::block) static FILL_RELEASE: AtomicBool = AtomicBool::new(false);
    /// The fill hold ended at the running row's deadline, not at the
    /// test's release.
    pub(in crate::block) static FILL_TIMED_OUT: AtomicBool = AtomicBool::new(false);

    /// `fill_read`'s hold point, before the device read, with no lock
    /// held: it holds one read's fill of the armed page until
    /// `block::ktest`'s release. Only the running row's deadline bounds it
    /// (`ktest::sleep_for`), so a slow host cannot end it before the test
    /// has seen what it waits for.
    pub(super) fn fill_hold_point(key: CacheKey) {
        // Acquire, AcqRel: pairs with the Release stores in `cache_read_waits_for_fill`.
        if FILL_DEV.load(Ordering::Acquire) != key.dev
            || FILL_OFF
                .compare_exchange(key.offset, UNARMED, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        // Release: pairs with the Acquire load in `cache_read_waits_for_fill`.
        FILL_HELD.store(true, Ordering::Release);
        // Acquire: pairs with the Release stores in `cache_read_waits_for_fill`.
        if !crate::ktest::sleep_for(|| FILL_RELEASE.load(Ordering::Acquire)) {
            // Release: pairs with the Acquire load in `cache_read_waits_for_fill`.
            FILL_TIMED_OUT.store(true, Ordering::Release);
        }
        // Release: pairs with the Acquire load in `cache_read_waits_for_fill`.
        FILL_HELD.store(false, Ordering::Release);
    }

    /// The device whose flush calls `block::ktest::flush_redirty` after
    /// each page it writes, as a writer racing the flush would dirty
    /// another page then; [`UNARMED`] for none.
    pub(in crate::block) static REDIRTY_DEV: AtomicU64 = AtomicU64::new(UNARMED);

    /// `PageCache::flush`'s point after each page it writes of `dev`, with
    /// no lock held.
    pub(super) fn after_flush_write(dev: u64) {
        // Acquire: pairs with the Release stores in `block::ktest::cache_flush_not_starved`.
        if REDIRTY_DEV.load(Ordering::Acquire) == dev {
            crate::block::ktest::flush_redirty();
        }
    }

    /// While set, `blk-wb` starts no pass.
    static WB_PAUSE: AtomicBool = AtomicBool::new(false);
    /// `blk-wb` is in a pass, or deciding whether to start one.
    static WB_IN_PASS: AtomicBool = AtomicBool::new(false);

    /// Holds `blk-wb` between passes until dropped.
    pub(in crate::block) struct WbPause;

    impl WbPause {
        /// Stop `blk-wb` starting a pass and wait out one in progress;
        /// `None` when that pass does not end by the row's deadline.
        pub(in crate::block) fn new() -> Option<Self> {
            WB_PAUSE.store(true, Ordering::SeqCst);
            let pause = Self;
            crate::ktest::sleep_for(|| !WB_IN_PASS.load(Ordering::SeqCst)).then_some(pause)
        }
    }

    impl Drop for WbPause {
        fn drop(&mut self) {
            // Release: pairs with the SeqCst load in `wb_pass_begin`.
            WB_PAUSE.store(false, Ordering::Release);
        }
    }

    /// Whether `blk-wb` may start a pass. SeqCst with
    /// [`WbPause::new`], which stores [`WB_PAUSE`] and then reads
    /// [`WB_IN_PASS`]: either this load sees the pause, or the pause sees
    /// this pass and waits for [`wb_pass_end`].
    pub(super) fn wb_pass_begin() -> bool {
        WB_IN_PASS.store(true, Ordering::SeqCst);
        if WB_PAUSE.load(Ordering::SeqCst) {
            // Release: pairs with the SeqCst load in `WbPause::new`.
            WB_IN_PASS.store(false, Ordering::Release);
            return false;
        }
        true
    }

    pub(super) fn wb_pass_end() {
        // Release: pairs with the SeqCst load in `WbPause::new`.
        WB_IN_PASS.store(false, Ordering::Release);
    }

    /// `writeback_dev`'s hold point, reached with no lock held. It sleeps
    /// until `block::ktest::release`; only the running row's deadline
    /// bounds it, as [`fill_hold_point`]'s.
    pub(super) fn hold_point(key: CacheKey) {
        // Acquire, AcqRel: pairs with the Release stores in `block::ktest::hold_wb` and `release`.
        if DEV.load(Ordering::Acquire) != key.dev
            || OFF
                .compare_exchange(key.offset, UNARMED, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        // Release: pairs with the Acquire load in `block::ktest::held`.
        HELD.store(true, Ordering::Release);
        // Acquire: pairs with the Release stores in `block::ktest::hold_wb` and `release`.
        let _released = crate::ktest::sleep_for(|| RELEASE.load(Ordering::Acquire));
        // Release: pairs with the Acquire load in `block::ktest::held`.
        HELD.store(false, Ordering::Release);
    }
}
