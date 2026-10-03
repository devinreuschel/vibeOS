//! Page-granular block cache. ROADMAP §7.4.
//!
//! Keyed by (`BlockRef` id, page offset): the page-aligned byte offset on
//! the device whose id `blockdev::DiskSeq` handed out, never reused. Read-through, write-back,
//! clock (second-chance) eviction, sequential readahead, dirty-ratio cap.
//!
//! Phase 12 makes this each block device's mapping in one page cache of
//! mappings (DESIGN §10.6): the same frames, clock, and writeback as the
//! file mappings, which key file pages by page index, never by device
//! location. Do not add a second private cache beside it.

use crate::block::BlockError;

pub const PAGE: usize = 4096;
pub const DEFAULT_PAGES: usize = 16;
pub const DIRTY_RATIO_PCT: u32 = 50;
pub const READAHEAD_PAGES: u32 = 1;

const F_VALID: u8 = 1;
const F_DIRTY: u8 = 2;
const F_REF: u8 = 4;
const F_FILL: u8 = 8;
/// WRITEBACK: the slot's device write is in flight. The slot keeps its
/// key, stays readable and writable (a write dirties it again), is never
/// picked by the clock or re-keyed, and gets no second write until the
/// first completes ([`Cache::end_writeback`]). Every cache write sets it:
/// `blk-wb`'s, `flush`'s, and an eviction's, so a dirty victim is written
/// back in place before it is re-keyed. A kernel waiter sleeps on the
/// slot's wait queue (`cache_init`).
const F_WB: u8 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheKey {
    /// A `BlockRef` id (`blockdev::DiskSeq`).
    pub dev: u64,
    pub offset: u64,
}

impl CacheKey {
    pub const fn page(dev: u64, byte_off: u64) -> Self {
        Self {
            dev,
            offset: byte_off & !((PAGE as u64) - 1),
        }
    }

    pub const fn next_page(self) -> Self {
        Self {
            dev: self.dev,
            offset: self.offset.saturating_add(PAGE as u64),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub evicts: u64,
    pub device_reads: u64,
    pub device_writes: u64,
    pub device_flushes: u64,
}

impl CacheStats {
    pub fn device_reqs(self) -> u64 {
        self.device_reads
            .saturating_add(self.device_writes)
            .saturating_add(self.device_flushes)
    }
}

#[derive(Clone, Copy)]
struct Meta {
    key: CacheKey,
    flags: u8,
    /// While [`F_WB`] is set, which writeback this is: the cache's
    /// [`Cache::wb_seq`] when it began.
    wb_start: u64,
}

impl Meta {
    const EMPTY: Self = Self {
        key: CacheKey {
            dev: u64::MAX,
            offset: 0,
        },
        flags: 0,
        wb_start: 0,
    };
}

/// I/O the caller must run with the cache lock dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillNeed {
    /// Busy slot: a FILL slot of this key, or an [`F_WB`] slot when nothing
    /// else is evictable. Wait, then plan again.
    None,
    /// Read `key` into `slot`, then `install_read` or `install_write`.
    Read,
    /// `slot` is the dirty victim `evict_key`, now in writeback with its
    /// page in `evict_out`. Write it, [`Cache::end_writeback`], then plan
    /// again.
    Writeback,
}

/// What a flush does next with one slot, from [`Cache::flush_slot`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlushStep {
    /// The page is in `dst` and the slot in writeback: write it, then
    /// [`Cache::end_writeback`]. The slot is then done.
    Write(usize, CacheKey),
    /// The slot's writeback `start` is in flight: wait until it ends. When
    /// `done`, it began after the flush did, so it carries every byte
    /// dirtied before, and the slot is then done; else ask again.
    Wait {
        slot: usize,
        key: CacheKey,
        start: u64,
        done: bool,
    },
    /// Nothing the flush must write is left in the slot.
    Done,
}

pub struct Fill {
    pub slot: usize,
    pub key: CacheKey,
    pub need: FillNeed,
    pub evict_key: CacheKey,
}

pub trait Backend {
    fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError>;
    fn write(&self, offset: u64, buf: &[u8]) -> Result<(), BlockError>;
    fn flush(&self) -> Result<(), BlockError>;
}

pub struct Cache<const N: usize> {
    meta: [Meta; N],
    data: [[u8; PAGE]; N],
    hand: usize,
    last: CacheKey,
    /// Sequential page reads in a row, for readahead.
    run: u32,
    /// Writebacks begun so far; each takes the next number
    /// ([`Meta::wb_start`]).
    wb_seq: u64,
    pub stats: CacheStats,
}

impl<const N: usize> Cache<N> {
    pub const fn new() -> Self {
        Self {
            meta: [Meta::EMPTY; N],
            data: [[0u8; PAGE]; N],
            hand: 0,
            last: CacheKey {
                dev: u64::MAX,
                offset: u64::MAX,
            },
            run: 0,
            wb_seq: 0,
            stats: CacheStats {
                hits: 0,
                misses: 0,
                evicts: 0,
                device_reads: 0,
                device_writes: 0,
                device_flushes: 0,
            },
        }
    }

    pub fn n_pages(&self) -> usize {
        N
    }

    pub fn dirty_count(&self) -> usize {
        let mut n = 0usize;
        let mut i = 0usize;
        while i < N {
            if self.meta[i].flags & (F_VALID | F_DIRTY) == F_VALID | F_DIRTY {
                n += 1;
            }
            i += 1;
        }
        n
    }

    pub fn over_dirty_ratio(&self) -> bool {
        if N == 0 {
            return false;
        }
        (self.dirty_count() as u64) * 100 > (N as u64) * (DIRTY_RATIO_PCT as u64)
    }

    /// The slot that holds `key`, or that a read is filling with it: a
    /// page has one slot, so a second reader of a page being filled waits
    /// for that fill rather than filling a second copy, which a later read
    /// could find stale after a write to the first.
    pub fn find(&self, key: CacheKey) -> Option<usize> {
        let mut i = 0usize;
        while i < N {
            let m = self.meta[i];
            if m.flags & (F_VALID | F_FILL) != 0 && m.key == key {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// Clock (second chance) over slots neither filling nor in writeback.
    /// A clean victim first; a dirty one only when no clean one is
    /// evictable.
    fn clock_slot(&mut self) -> Option<usize> {
        let mut dirty = None;
        let mut steps = 0usize;
        while steps < N * 2 {
            let i = self.hand;
            self.hand = (self.hand + 1) % N;
            steps += 1;
            let f = self.meta[i].flags;
            if f & (F_FILL | F_WB) != 0 {
                continue;
            }
            if f & F_VALID == 0 {
                return Some(i);
            }
            if f & F_REF != 0 {
                self.meta[i].flags = f & !F_REF;
                continue;
            }
            if f & F_DIRTY == 0 {
                return Some(i);
            }
            if dirty.is_none() {
                dirty = Some(i);
            }
        }
        dirty
    }

    fn any_writeback(&self, dev: Option<u64>) -> Option<usize> {
        let mut i = 0usize;
        while i < N {
            let m = self.meta[i];
            if m.flags & F_WB != 0 && dev.is_none_or(|d| m.key.dev == d) {
                return Some(i);
            }
            i += 1;
        }
        None
    }

    /// A victim for `key`'s miss. `Ok(Err(fill))` when the caller must do
    /// I/O or wait first; `Ok(Ok(slot))` for a slot free to re-key.
    fn victim(
        &mut self,
        key: CacheKey,
        evict_out: &mut [u8],
    ) -> Result<Result<usize, Fill>, BlockError> {
        let Some(slot) = self.clock_slot() else {
            return match self.any_writeback(None) {
                Some(wb) => Ok(Err(Fill {
                    slot: wb,
                    key,
                    need: FillNeed::None,
                    evict_key: self.meta[wb].key,
                })),
                None => Err(BlockError::Failed),
            };
        };
        let f = self.meta[slot].flags;
        if f & (F_VALID | F_DIRTY) == F_VALID | F_DIRTY {
            self.copy_page(slot, evict_out)?;
            self.begin_writeback(slot);
            return Ok(Err(Fill {
                slot,
                key,
                need: FillNeed::Writeback,
                evict_key: self.meta[slot].key,
            }));
        }
        self.stats.misses = self.stats.misses.saturating_add(1);
        if f & F_VALID != 0 {
            self.stats.evicts = self.stats.evicts.saturating_add(1);
        }
        Ok(Ok(slot))
    }

    fn note_seq(&mut self, key: CacheKey) -> bool {
        let seq =
            self.last.dev == key.dev && self.last.offset.saturating_add(PAGE as u64) == key.offset;
        self.last = key;
        if seq {
            self.run = self.run.saturating_add(1);
        } else {
            self.run = 0;
        }
        seq && self.run >= 1
    }

    pub fn copy_page(&self, slot: usize, dst: &mut [u8]) -> Result<(), BlockError> {
        if slot >= N || dst.len() < PAGE {
            return Err(BlockError::Inval);
        }
        dst[..PAGE].copy_from_slice(&self.data[slot]);
        Ok(())
    }

    /// Copy a hit into `out`; a hit on a slot in writeback reads too. On a
    /// miss, re-key a clean victim and return a `Read` fill, or start a
    /// dirty victim's writeback in place (`evict_out` ≥ PAGE gets its page)
    /// and return a `Writeback` fill without planning the miss.
    pub fn plan_read(
        &mut self,
        key: CacheKey,
        off: usize,
        out: &mut [u8],
        evict_out: &mut [u8],
    ) -> Result<Option<Fill>, BlockError> {
        if off.checked_add(out.len()).map(|e| e > PAGE).unwrap_or(true) {
            return Err(BlockError::Inval);
        }
        if let Some(i) = self.find(key) {
            if self.meta[i].flags & F_FILL != 0 {
                return Ok(Some(Fill {
                    slot: i,
                    key,
                    need: FillNeed::None,
                    evict_key: key,
                }));
            }
            self.meta[i].flags |= F_REF;
            self.stats.hits = self.stats.hits.saturating_add(1);
            out.copy_from_slice(&self.data[i][off..off + out.len()]);
            let _ = self.note_seq(key);
            return Ok(None);
        }
        let slot = match self.victim(key, evict_out)? {
            Ok(slot) => slot,
            Err(fill) => return Ok(Some(fill)),
        };
        self.meta[slot].key = key;
        self.meta[slot].flags = F_FILL;
        let _ = self.note_seq(key);
        Ok(Some(Fill {
            slot,
            key,
            need: FillNeed::Read,
            evict_key: key,
        }))
    }

    /// As [`Cache::plan_read`]. A hit, on a slot in writeback too, copies
    /// `src` in and dirties it; a whole-page miss installs into a clean
    /// victim at once.
    pub fn plan_write(
        &mut self,
        key: CacheKey,
        off: usize,
        src: &[u8],
        evict_out: &mut [u8],
    ) -> Result<Option<Fill>, BlockError> {
        if off.checked_add(src.len()).map(|e| e > PAGE).unwrap_or(true) {
            return Err(BlockError::Inval);
        }
        if let Some(i) = self.find(key) {
            if self.meta[i].flags & F_FILL != 0 {
                return Ok(Some(Fill {
                    slot: i,
                    key,
                    need: FillNeed::None,
                    evict_key: key,
                }));
            }
            self.data[i][off..off + src.len()].copy_from_slice(src);
            self.meta[i].flags |= F_DIRTY | F_REF | F_VALID;
            self.stats.hits = self.stats.hits.saturating_add(1);
            let _ = self.note_seq(key);
            return Ok(None);
        }
        let slot = match self.victim(key, evict_out)? {
            Ok(slot) => slot,
            Err(fill) => return Ok(Some(fill)),
        };
        let _ = self.note_seq(key);
        self.meta[slot].key = key;
        if off == 0 && src.len() == PAGE {
            self.data[slot].copy_from_slice(src);
            self.meta[slot].flags = F_VALID | F_DIRTY | F_REF;
            return Ok(None);
        }
        self.meta[slot].flags = F_FILL;
        Ok(Some(Fill {
            slot,
            key,
            need: FillNeed::Read,
            evict_key: key,
        }))
    }

    /// Whether `fill`'s slot is still being filled with its key: an
    /// [`Cache::invalidate`] or [`Cache::drop_dev`] during the device
    /// read takes the slot back, and another key may have it by now.
    fn filling_for(&self, fill: &Fill) -> bool {
        self.meta
            .get(fill.slot)
            .is_some_and(|m| m.key == fill.key && m.flags & F_FILL != 0)
    }

    /// Cache `fill`'s page and copy `out` from it; a fill whose slot was
    /// taken back only copies `out`.
    pub fn install_read(
        &mut self,
        fill: &Fill,
        page: &[u8],
        off: usize,
        out: &mut [u8],
    ) -> Result<(), BlockError> {
        if fill.slot >= N
            || page.len() < PAGE
            || off.checked_add(out.len()).map(|e| e > PAGE).unwrap_or(true)
        {
            return Err(BlockError::Inval);
        }
        if !self.filling_for(fill) {
            out.copy_from_slice(&page[off..off + out.len()]);
            return Ok(());
        }
        self.data[fill.slot].copy_from_slice(&page[..PAGE]);
        self.meta[fill.slot].key = fill.key;
        self.meta[fill.slot].flags = F_VALID | F_REF;
        out.copy_from_slice(&self.data[fill.slot][off..off + out.len()]);
        Ok(())
    }

    /// Merge `src` into `fill`'s page and cache it dirty: `true`. `false`
    /// when the slot was taken back during the read: nothing is written,
    /// and the caller plans the write again.
    pub fn install_write(
        &mut self,
        fill: &Fill,
        page: &[u8],
        off: usize,
        src: &[u8],
    ) -> Result<bool, BlockError> {
        if fill.slot >= N
            || page.len() < PAGE
            || off.checked_add(src.len()).map(|e| e > PAGE).unwrap_or(true)
        {
            return Err(BlockError::Inval);
        }
        if !self.filling_for(fill) {
            return Ok(false);
        }
        self.data[fill.slot].copy_from_slice(&page[..PAGE]);
        self.data[fill.slot][off..off + src.len()].copy_from_slice(src);
        self.meta[fill.slot].key = fill.key;
        self.meta[fill.slot].flags = F_VALID | F_DIRTY | F_REF;
        Ok(true)
    }

    /// Give `fill`'s slot back after a failed read, unless it was taken
    /// back already, when it may hold another key's page or fill.
    pub fn abort_fill(&mut self, fill: &Fill) {
        if self.filling_for(fill) {
            self.meta[fill.slot].flags = 0;
        }
    }

    pub fn want_readahead(&self) -> Option<CacheKey> {
        if self.run == 0 || READAHEAD_PAGES == 0 {
            return None;
        }
        Some(self.last.next_page())
    }

    /// Copy the next dirty page not in writeback into `dst`, clear dirty,
    /// and start its writeback. A write that hits during the device I/O
    /// sets dirty again. `dev` limits the scan to one device when `Some`.
    pub fn take_dirty(
        &mut self,
        start: usize,
        dev: Option<u64>,
        dst: &mut [u8],
    ) -> Option<(usize, CacheKey)> {
        if dst.len() < PAGE {
            return None;
        }
        let mut s = start;
        while s < N {
            let f = self.meta[s].flags;
            if f & (F_VALID | F_DIRTY | F_FILL | F_WB) == F_VALID | F_DIRTY {
                let key = self.meta[s].key;
                if let Some(d) = dev
                    && key.dev != d
                {
                    s += 1;
                    continue;
                }
                dst[..PAGE].copy_from_slice(&self.data[s]);
                self.begin_writeback(s);
                return Some((s, key));
            }
            s += 1;
        }
        None
    }

    /// Clear `slot`'s dirty bit and put it in writeback, numbered as the
    /// next writeback the cache begins.
    fn begin_writeback(&mut self, slot: usize) {
        self.wb_seq = self.wb_seq.wrapping_add(1);
        let m = &mut self.meta[slot];
        m.flags = (m.flags | F_WB) & !F_DIRTY;
        m.wb_start = self.wb_seq;
    }

    /// The write that [`Cache::take_dirty`], [`Cache::flush_slot`], or a
    /// `Writeback` fill started on `slot` finished with `res`. On `Err` the
    /// page is dirty again if the slot still holds `key`.
    pub fn end_writeback(&mut self, slot: usize, key: CacheKey, res: Result<(), BlockError>) {
        let Some(m) = self.meta.get_mut(slot) else {
            return;
        };
        if m.flags & F_WB == 0 {
            return;
        }
        m.flags &= !F_WB;
        if res.is_err() && m.key == key && m.flags & F_VALID != 0 {
            m.flags |= F_DIRTY;
        }
    }

    pub fn in_writeback(&self, slot: usize) -> bool {
        self.meta.get(slot).is_some_and(|m| m.flags & F_WB != 0)
    }

    /// The number of `slot`'s writeback while one is in flight.
    pub fn writeback_start(&self, slot: usize) -> Option<u64> {
        self.meta
            .get(slot)
            .filter(|m| m.flags & F_WB != 0)
            .map(|m| m.wb_start)
    }

    /// Whether a read is filling `slot`: until its `install_*` or
    /// [`Cache::abort_fill`].
    pub fn filling(&self, slot: usize) -> bool {
        self.meta.get(slot).is_some_and(|m| m.flags & F_FILL != 0)
    }

    /// Where a flush begins: writebacks numbered above this began after it.
    pub fn flush_begin(&self) -> u64 {
        self.wb_seq
    }

    /// A flush's next step on `slot`, for a flush that began at `since`
    /// ([`Cache::flush_begin`]); `dev` limits it to one device when `Some`.
    /// A flush asks once for each slot until it is done, so a writer that
    /// keeps dirtying pages cannot hold it off: it writes each slot at
    /// most once and waits for at most two of its writebacks, one begun
    /// before it and one after. A page dirty when the flush began is
    /// written by the flush, or by a writeback that began after it and
    /// that the flush waits for; a page dirtied again after its copy was
    /// taken is the next flush's.
    pub fn flush_slot(
        &mut self,
        slot: usize,
        dev: Option<u64>,
        since: u64,
        dst: &mut [u8],
    ) -> FlushStep {
        let Some(m) = self.meta.get(slot).copied() else {
            return FlushStep::Done;
        };
        if dev.is_some_and(|d| m.key.dev != d) {
            return FlushStep::Done;
        }
        if m.flags & F_WB != 0 {
            return FlushStep::Wait {
                slot,
                key: m.key,
                start: m.wb_start,
                done: m.wb_start > since,
            };
        }
        if m.flags & (F_VALID | F_DIRTY | F_FILL) != F_VALID | F_DIRTY || dst.len() < PAGE {
            return FlushStep::Done;
        }
        dst[..PAGE].copy_from_slice(&self.data[slot]);
        self.begin_writeback(slot);
        FlushStep::Write(slot, m.key)
    }

    /// Forget every page of `dev`. A slot in writeback keeps `F_WB`, so it
    /// is not reused before [`Cache::end_writeback`].
    pub fn drop_dev(&mut self, dev: u64) {
        let mut i = 0usize;
        while i < N {
            if self.meta[i].key.dev == dev {
                self.meta[i].flags &= F_WB;
            }
            i += 1;
        }
    }

    /// Drop a page without writeback, only for backing that an unlink or
    /// a shrink frees, so a later alloc cannot see stale data. A
    /// relocation keeps its data: it uses [`cached_evict_range`], which
    /// writes dirty pages back first. A slot in writeback keeps `F_WB`
    /// (see [`Cache::drop_dev`]).
    pub fn invalidate(&mut self, key: CacheKey) {
        if let Some(i) = self.find(key) {
            self.meta[i].flags &= F_WB;
        }
    }

    /// Copy page `key` into `dst` and clear its dirty bit when it is
    /// valid, dirty, and not filling; its slot. A failed write of the copy
    /// re-marks it with [`Cache::redirty`].
    pub fn take_dirty_at(&mut self, key: CacheKey, dst: &mut [u8]) -> Option<usize> {
        if dst.len() < PAGE {
            return None;
        }
        let i = self.find(key)?;
        let f = self.meta[i].flags;
        if f & (F_VALID | F_DIRTY | F_FILL) != F_VALID | F_DIRTY {
            return None;
        }
        dst[..PAGE].copy_from_slice(&self.data[i]);
        self.meta[i].flags = f & !F_DIRTY;
        Some(i)
    }

    /// Mark `slot` dirty again after the write of a copy
    /// [`Cache::take_dirty_at`] took failed, if it still holds `key`.
    pub fn redirty(&mut self, slot: usize, key: CacheKey) {
        if let Some(m) = self.meta.get_mut(slot)
            && m.key == key
            && m.flags & F_VALID != 0
        {
            m.flags |= F_DIRTY;
        }
    }
}
impl<const N: usize> Default for Cache<N> {
    fn default() -> Self {
        Self::new()
    }
}

fn page_off(byte: u64) -> usize {
    (byte as usize) & (PAGE - 1)
}

/// The host path cannot sleep, so a busy slot is an error: `QueueFull`
/// for one in writeback, `Io` for one filling.
fn busy<const N: usize>(c: &Cache<N>, fill: &Fill) -> BlockError {
    if c.in_writeback(fill.slot) {
        BlockError::QueueFull
    } else {
        BlockError::Io
    }
}

/// Write back a `Writeback` fill's victim and end its writeback.
fn write_victim<B: Backend, const N: usize>(
    c: &mut Cache<N>,
    b: &B,
    fill: &Fill,
    evict: &[u8],
) -> Result<(), BlockError> {
    let res = b.write(fill.evict_key.offset, evict);
    if res.is_ok() {
        c.stats.device_writes = c.stats.device_writes.saturating_add(1);
    }
    c.end_writeback(fill.slot, fill.evict_key, res);
    res
}

/// Host / convenience path: may call `b` with the cache exclusive.
/// `scratch` carries a dirty victim's page out to `b` and a missed page in
/// from it, one at a time: the caller's page, so that neither is on this
/// frame, which a syscall's read runs on (DESIGN §4.5).
pub fn cached_read<B: Backend, const N: usize>(
    c: &mut Cache<N>,
    b: &B,
    dev: u64,
    byte_off: u64,
    buf: &mut [u8],
    scratch: &mut [u8; PAGE],
) -> Result<(), BlockError> {
    if buf.is_empty() {
        return Ok(());
    }
    let mut done = 0usize;
    while done < buf.len() {
        let off = byte_off.saturating_add(done as u64);
        let key = CacheKey::page(dev, off);
        let pin = page_off(off);
        let n = (PAGE - pin).min(buf.len() - done);
        if let Some(fill) = c.plan_read(key, pin, &mut buf[done..done + n], scratch)? {
            match fill.need {
                FillNeed::None => return Err(busy(c, &fill)),
                FillNeed::Writeback => {
                    write_victim(c, b, &fill, scratch)?;
                    continue;
                }
                FillNeed::Read => {
                    if let Err(e) = b.read(fill.key.offset, scratch) {
                        c.abort_fill(&fill);
                        return Err(e);
                    }
                    c.stats.device_reads = c.stats.device_reads.saturating_add(1);
                    c.install_read(&fill, scratch, pin, &mut buf[done..done + n])?;
                }
            }
        }
        done += n;
    }
    if let Some(rk) = c.want_readahead()
        && c.find(rk).is_none()
    {
        let mut dummy = [0u8; 1];
        // A readahead that cannot be planned is skipped: it is only a hint,
        // and the demand read above has already succeeded.
        if let Ok(Some(fill)) = c.plan_read(rk, 0, &mut dummy, scratch) {
            match fill.need {
                FillNeed::None => {}
                // The victim's error stays recorded: `end_writeback` leaves
                // the page dirty for the next flush to retry and report.
                FillNeed::Writeback => {
                    let _kept_dirty = write_victim(c, b, &fill, scratch).is_err();
                }
                FillNeed::Read => {
                    let mut one = [0u8; 1];
                    if b.read(rk.offset, scratch).is_ok() {
                        c.stats.device_reads = c.stats.device_reads.saturating_add(1);
                        if c.install_read(&fill, scratch, 0, &mut one).is_err() {
                            c.abort_fill(&fill);
                        }
                    } else {
                        c.abort_fill(&fill);
                    }
                }
            }
        }
    }
    Ok(())
}

/// As [`cached_read`], `scratch` included.
pub fn cached_write<B: Backend, const N: usize>(
    c: &mut Cache<N>,
    b: &B,
    dev: u64,
    byte_off: u64,
    buf: &[u8],
    scratch: &mut [u8; PAGE],
) -> Result<(), BlockError> {
    if buf.is_empty() {
        return Ok(());
    }
    let mut done = 0usize;
    while done < buf.len() {
        let off = byte_off.saturating_add(done as u64);
        let key = CacheKey::page(dev, off);
        let pin = page_off(off);
        let n = (PAGE - pin).min(buf.len() - done);
        if let Some(fill) = c.plan_write(key, pin, &buf[done..done + n], scratch)? {
            match fill.need {
                FillNeed::None => return Err(busy(c, &fill)),
                FillNeed::Writeback => {
                    write_victim(c, b, &fill, scratch)?;
                    continue;
                }
                FillNeed::Read => {
                    if let Err(e) = b.read(fill.key.offset, scratch) {
                        c.abort_fill(&fill);
                        return Err(e);
                    }
                    c.stats.device_reads = c.stats.device_reads.saturating_add(1);
                    if !c.install_write(&fill, scratch, pin, &buf[done..done + n])? {
                        continue;
                    }
                }
            }
        }
        done += n;
    }
    Ok(())
}

/// Write each dirty page of `dev` in `[off, off + len)` back to `b`, then
/// drop it, as a relocation of the backing must before it copies the
/// range. A write error re-marks the page dirty, keeps it, and returns.
pub fn cached_evict_range<B: Backend, const N: usize>(
    c: &mut Cache<N>,
    b: &B,
    dev: u64,
    off: u64,
    len: u64,
) -> Result<(), BlockError> {
    let end = off.checked_add(len).ok_or(BlockError::Inval)?;
    let mut data = [0u8; PAGE];
    let mut key = CacheKey::page(dev, off);
    while key.offset < end {
        if let Some(slot) = c.take_dirty_at(key, &mut data) {
            let res = b.write(key.offset, &data);
            if let Err(e) = res {
                c.redirty(slot, key);
                return Err(e);
            }
            c.stats.device_writes = c.stats.device_writes.saturating_add(1);
        }
        c.invalidate(key);
        let next = key.next_page();
        if next.offset == key.offset {
            break;
        }
        key = next;
    }
    Ok(())
}

/// Write each page dirty when the flush began ([`Cache::flush_slot`]),
/// then send `b` a `Flush` (DESIGN §10.6). The host cannot wait, so a
/// write already in flight is `Err(QueueFull)` and no `Flush` is sent.
pub fn cached_flush<B: Backend, const N: usize>(
    c: &mut Cache<N>,
    b: &B,
    dev: Option<u64>,
) -> Result<(), BlockError> {
    let mut data = [0u8; PAGE];
    let since = c.flush_begin();
    let mut slot = 0usize;
    while slot < N {
        match c.flush_slot(slot, dev, since, &mut data) {
            FlushStep::Write(s, key) => {
                let res = b.write(key.offset, &data);
                if res.is_ok() {
                    c.stats.device_writes = c.stats.device_writes.saturating_add(1);
                }
                c.end_writeback(s, key, res);
                res?;
            }
            FlushStep::Wait { .. } => return Err(BlockError::QueueFull),
            FlushStep::Done => {}
        }
        slot += 1;
    }
    b.flush()?;
    c.stats.device_flushes = c.stats.device_flushes.saturating_add(1);
    Ok(())
}

#[cfg(test)]
mod tests;
