//! Page-granular block cache. ROADMAP §7.4.
//!
//! Keyed by `(dev, page-aligned byte offset)`. Read-through, write-back,
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
    pub dev: u32,
    pub offset: u64,
}

impl CacheKey {
    pub const fn page(dev: u32, byte_off: u64) -> Self {
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
}

impl Meta {
    const EMPTY: Self = Self {
        key: CacheKey { dev: 0, offset: 0 },
        flags: 0,
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

/// One step of a flush, from [`Cache::flush_step`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlushStep {
    /// The page is in `dst` and the slot in writeback: write it, then
    /// [`Cache::end_writeback`].
    Write(usize, CacheKey),
    /// A write of this slot is in flight: wait for it.
    Wait(usize, CacheKey),
    /// No dirty and no writeback slot is left: send the device `Flush`.
    Flush,
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
    seq: u32,
    pub stats: CacheStats,
}

impl<const N: usize> Cache<N> {
    pub const fn new() -> Self {
        Self {
            meta: [Meta::EMPTY; N],
            data: [[0u8; PAGE]; N],
            hand: 0,
            last: CacheKey {
                dev: u32::MAX,
                offset: u64::MAX,
            },
            seq: 0,
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

    pub fn find(&self, key: CacheKey) -> Option<usize> {
        let mut i = 0usize;
        while i < N {
            let m = self.meta[i];
            if m.flags & F_VALID != 0 && m.key == key {
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

    fn any_writeback(&self, dev: Option<u32>) -> Option<usize> {
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
            self.meta[slot].flags = (f | F_WB) & !F_DIRTY;
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
            self.seq = self.seq.saturating_add(1);
        } else {
            self.seq = 0;
        }
        seq && self.seq >= 1
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
        self.data[fill.slot].copy_from_slice(&page[..PAGE]);
        self.meta[fill.slot].key = fill.key;
        self.meta[fill.slot].flags = F_VALID | F_REF;
        out.copy_from_slice(&self.data[fill.slot][off..off + out.len()]);
        Ok(())
    }

    pub fn install_write(
        &mut self,
        fill: &Fill,
        page: &[u8],
        off: usize,
        src: &[u8],
    ) -> Result<(), BlockError> {
        if fill.slot >= N
            || page.len() < PAGE
            || off.checked_add(src.len()).map(|e| e > PAGE).unwrap_or(true)
        {
            return Err(BlockError::Inval);
        }
        self.data[fill.slot].copy_from_slice(&page[..PAGE]);
        self.data[fill.slot][off..off + src.len()].copy_from_slice(src);
        self.meta[fill.slot].key = fill.key;
        self.meta[fill.slot].flags = F_VALID | F_DIRTY | F_REF;
        Ok(())
    }

    pub fn abort_fill(&mut self, slot: usize) {
        if slot < N {
            self.meta[slot].flags = 0;
        }
    }

    pub fn want_readahead(&self) -> Option<CacheKey> {
        if self.seq == 0 || READAHEAD_PAGES == 0 {
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
        dev: Option<u32>,
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
                self.meta[s].flags = (f & !F_DIRTY) | F_WB;
                return Some((s, key));
            }
            s += 1;
        }
        None
    }

    /// The write that [`Cache::take_dirty`], [`Cache::flush_step`], or a
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

    /// Start one dirty page's writeback (into `dst`), else name a slot in
    /// writeback, else `Flush`. `dev` limits it to one device when `Some`.
    pub fn flush_step(&mut self, dev: Option<u32>, dst: &mut [u8]) -> FlushStep {
        if let Some((slot, key)) = self.take_dirty(0, dev, dst) {
            return FlushStep::Write(slot, key);
        }
        match self.any_writeback(dev) {
            Some(i) => FlushStep::Wait(i, self.meta[i].key),
            None => FlushStep::Flush,
        }
    }

    /// Forget every page of `dev`. A slot in writeback keeps `F_WB`, so it
    /// is not reused before [`Cache::end_writeback`].
    #[allow(dead_code)]
    pub fn drop_dev(&mut self, dev: u32) {
        let mut i = 0usize;
        while i < N {
            if self.meta[i].key.dev == dev {
                self.meta[i].flags &= F_WB;
            }
            i += 1;
        }
    }

    /// Drop a page without writeback. tmpfs uses this when a file
    /// frees or relocates backing so a later alloc cannot see stale data.
    /// A slot in writeback keeps `F_WB` (see [`Cache::drop_dev`]).
    pub fn invalidate(&mut self, key: CacheKey) {
        if let Some(i) = self.find(key) {
            self.meta[i].flags &= F_WB;
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
pub fn cached_read<B: Backend, const N: usize>(
    c: &mut Cache<N>,
    b: &B,
    dev: u32,
    byte_off: u64,
    buf: &mut [u8],
) -> Result<(), BlockError> {
    if buf.is_empty() {
        return Ok(());
    }
    let mut evict = [0u8; PAGE];
    let mut done = 0usize;
    while done < buf.len() {
        let off = byte_off.saturating_add(done as u64);
        let key = CacheKey::page(dev, off);
        let pin = page_off(off);
        let n = (PAGE - pin).min(buf.len() - done);
        if let Some(fill) = c.plan_read(key, pin, &mut buf[done..done + n], &mut evict)? {
            match fill.need {
                FillNeed::None => return Err(busy(c, &fill)),
                FillNeed::Writeback => {
                    write_victim(c, b, &fill, &evict)?;
                    continue;
                }
                FillNeed::Read => {
                    let mut page = [0u8; PAGE];
                    if let Err(e) = b.read(fill.key.offset, &mut page) {
                        c.abort_fill(fill.slot);
                        return Err(e);
                    }
                    c.stats.device_reads = c.stats.device_reads.saturating_add(1);
                    c.install_read(&fill, &page, pin, &mut buf[done..done + n])?;
                }
            }
        }
        done += n;
    }
    if let Some(rk) = c.want_readahead()
        && c.find(rk).is_none()
    {
        let mut dummy = [0u8; 1];
        if let Ok(Some(fill)) = c.plan_read(rk, 0, &mut dummy, &mut evict) {
            match fill.need {
                FillNeed::None => {}
                // The victim's error stays recorded: `end_writeback` leaves
                // the page dirty for the next flush to retry and report.
                FillNeed::Writeback => {
                    let _kept_dirty = write_victim(c, b, &fill, &evict).is_err();
                }
                FillNeed::Read => {
                    let mut page = [0u8; PAGE];
                    let mut one = [0u8; 1];
                    if b.read(rk.offset, &mut page).is_ok() {
                        c.stats.device_reads = c.stats.device_reads.saturating_add(1);
                        if c.install_read(&fill, &page, 0, &mut one).is_err() {
                            c.abort_fill(fill.slot);
                        }
                    } else {
                        c.abort_fill(fill.slot);
                    }
                }
            }
        }
    }
    Ok(())
}

pub fn cached_write<B: Backend, const N: usize>(
    c: &mut Cache<N>,
    b: &B,
    dev: u32,
    byte_off: u64,
    buf: &[u8],
) -> Result<(), BlockError> {
    if buf.is_empty() {
        return Ok(());
    }
    let mut evict = [0u8; PAGE];
    let mut done = 0usize;
    while done < buf.len() {
        let off = byte_off.saturating_add(done as u64);
        let key = CacheKey::page(dev, off);
        let pin = page_off(off);
        let n = (PAGE - pin).min(buf.len() - done);
        if let Some(fill) = c.plan_write(key, pin, &buf[done..done + n], &mut evict)? {
            match fill.need {
                FillNeed::None => return Err(busy(c, &fill)),
                FillNeed::Writeback => {
                    write_victim(c, b, &fill, &evict)?;
                    continue;
                }
                FillNeed::Read => {
                    let mut page = [0u8; PAGE];
                    if let Err(e) = b.read(fill.key.offset, &mut page) {
                        c.abort_fill(fill.slot);
                        return Err(e);
                    }
                    c.stats.device_reads = c.stats.device_reads.saturating_add(1);
                    c.install_write(&fill, &page, pin, &buf[done..done + n])?;
                }
            }
        }
        done += n;
    }
    Ok(())
}

/// Write each dirty page, then send `b` a `Flush` once no dirty and no
/// writeback slot is left (DESIGN §10.6). The host cannot wait, so a write
/// already in flight is `Err(QueueFull)` and no `Flush` is sent.
pub fn cached_flush<B: Backend, const N: usize>(
    c: &mut Cache<N>,
    b: &B,
    dev: Option<u32>,
) -> Result<(), BlockError> {
    let mut data = [0u8; PAGE];
    loop {
        match c.flush_step(dev, &mut data) {
            FlushStep::Write(slot, key) => {
                let res = b.write(key.offset, &data);
                if res.is_ok() {
                    c.stats.device_writes = c.stats.device_writes.saturating_add(1);
                }
                c.end_writeback(slot, key, res);
                res?;
            }
            FlushStep::Wait(..) => return Err(BlockError::QueueFull),
            FlushStep::Flush => {
                b.flush()?;
                c.stats.device_flushes = c.stats.device_flushes.saturating_add(1);
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Mem {
        data: Mutex<Vec<u8>>,
        reads: Mutex<u64>,
        writes: Mutex<u64>,
        flushes: Mutex<u64>,
        writes_at: Mutex<std::collections::HashMap<u64, u64>>,
        fail_writes: Mutex<u32>,
    }

    impl Mem {
        fn new(n: usize) -> Self {
            Self {
                data: Mutex::new(vec![0u8; n]),
                reads: Mutex::new(0),
                writes: Mutex::new(0),
                flushes: Mutex::new(0),
                writes_at: Mutex::new(std::collections::HashMap::new()),
                fail_writes: Mutex::new(0),
            }
        }
        fn writes_at(&self, off: u64) -> u64 {
            *self.writes_at.lock().unwrap().get(&off).unwrap_or(&0)
        }
        fn get(&self, off: usize, n: usize) -> Vec<u8> {
            self.data.lock().unwrap()[off..off + n].to_vec()
        }
    }

    impl Backend for Mem {
        fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            *self.reads.lock().unwrap() += 1;
            let o = offset as usize;
            let d = self.data.lock().unwrap();
            if o + buf.len() > d.len() {
                return Err(BlockError::Inval);
            }
            buf.copy_from_slice(&d[o..o + buf.len()]);
            Ok(())
        }
        fn write(&self, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
            {
                let mut f = self.fail_writes.lock().unwrap();
                if *f > 0 {
                    *f -= 1;
                    return Err(BlockError::Io);
                }
            }
            *self.writes.lock().unwrap() += 1;
            *self.writes_at.lock().unwrap().entry(offset).or_insert(0) += 1;
            let o = offset as usize;
            let mut d = self.data.lock().unwrap();
            if o + buf.len() > d.len() {
                return Err(BlockError::Inval);
            }
            d[o..o + buf.len()].copy_from_slice(buf);
            Ok(())
        }
        fn flush(&self) -> Result<(), BlockError> {
            *self.flushes.lock().unwrap() += 1;
            Ok(())
        }
    }

    #[test]
    fn hit_reduces_device_reads() {
        let mem = Mem::new(PAGE * 8);
        let mut c = Cache::<4>::new();
        let mut buf = [0u8; 512];
        buf[0] = 0xAB;
        mem.write(0, &buf).unwrap();
        *mem.reads.lock().unwrap() = 0;
        let mut out = [0u8; 512];
        cached_read(&mut c, &mem, 0, 0, &mut out).unwrap();
        let r1 = *mem.reads.lock().unwrap();
        assert_eq!(out[0], 0xAB);
        cached_read(&mut c, &mem, 0, 0, &mut out).unwrap();
        let r2 = *mem.reads.lock().unwrap();
        assert_eq!(r1, 1);
        assert_eq!(r2, 1);
        assert_eq!(c.stats.hits, 1);
        assert_eq!(c.stats.misses, 1);
        assert_eq!(c.stats.device_reads, 1);
        assert_eq!(c.stats.device_reqs(), 1);
        // same page, different 512 window is still a hit
        cached_read(&mut c, &mem, 0, 512, &mut out).unwrap();
        assert_eq!(*mem.reads.lock().unwrap(), 1);
        assert_eq!(c.stats.hits, 2);
    }

    #[test]
    fn writeback_and_flush() {
        let mem = Mem::new(PAGE * 4);
        let mut c = Cache::<4>::new();
        let buf = [0x5Au8; 512];
        cached_write(&mut c, &mem, 1, 0, &buf).unwrap();
        assert_eq!(*mem.writes.lock().unwrap(), 0);
        assert_eq!(mem.get(0, 1)[0], 0);
        cached_flush(&mut c, &mem, Some(1)).unwrap();
        assert!(*mem.writes.lock().unwrap() >= 1);
        assert_eq!(*mem.flushes.lock().unwrap(), 1);
        assert_eq!(mem.get(0, 512), buf);
    }

    #[test]
    fn clock_eviction() {
        let mem = Mem::new(PAGE * 8);
        let mut c = Cache::<2>::new();
        let mut buf = [0u8; PAGE];
        buf[0] = 1;
        cached_write(&mut c, &mem, 0, 0, &buf).unwrap();
        buf[0] = 2;
        cached_write(&mut c, &mem, 0, PAGE as u64, &buf).unwrap();
        // two dirty pages fill the cache; a third must evict
        buf[0] = 3;
        cached_write(&mut c, &mem, 0, 2 * PAGE as u64, &buf).unwrap();
        assert!(c.stats.evicts >= 1);
        assert!(*mem.writes.lock().unwrap() >= 1);
        cached_flush(&mut c, &mem, None).unwrap();
        assert_eq!(mem.get(2 * PAGE, 1)[0], 3);
    }

    #[test]
    fn dirty_ratio() {
        let mut c = Cache::<4>::new();
        assert!(!c.over_dirty_ratio());
        let mem = Mem::new(PAGE * 8);
        let buf = [1u8; PAGE];
        cached_write(&mut c, &mem, 0, 0, &buf).unwrap();
        cached_write(&mut c, &mem, 0, PAGE as u64, &buf).unwrap();
        cached_write(&mut c, &mem, 0, 2 * PAGE as u64, &buf).unwrap();
        assert!(c.over_dirty_ratio());
        cached_flush(&mut c, &mem, None).unwrap();
        assert!(!c.over_dirty_ratio());
    }

    #[test]
    fn sequential_readahead_touches_next_page() {
        let mem = Mem::new(PAGE * 8);
        let mut c = Cache::<8>::new();
        let mut a = [0u8; 512];
        let mut b = [0u8; 512];
        cached_read(&mut c, &mem, 0, 0, &mut a).unwrap();
        let r0 = *mem.reads.lock().unwrap();
        cached_read(&mut c, &mem, 0, PAGE as u64, &mut b).unwrap();
        let r1 = *mem.reads.lock().unwrap();
        // miss on page1 plus readahead of page2
        assert!(r1 > r0);
        let r2 = *mem.reads.lock().unwrap();
        cached_read(&mut c, &mem, 0, 2 * PAGE as u64, &mut a).unwrap();
        // page2 should already be present from readahead
        assert_eq!(*mem.reads.lock().unwrap(), r2);
    }

    #[test]
    fn writeback_inflight_keeps_slot() {
        let mem = Mem::new(PAGE * 16);
        let mut c = Cache::<4>::new();
        let p = PAGE as u64;
        cached_write(&mut c, &mem, 0, 0, &[1u8; PAGE]).unwrap();
        // blk-wb takes the page and its write stays in flight.
        let mut held = [0u8; PAGE];
        let (slot, key) = c.take_dirty(0, None, &mut held).unwrap();
        assert_eq!(key, CacheKey::page(0, 0));
        assert!(c.in_writeback(slot));
        // The page is dirtied again, then the cache is filled past its size.
        cached_write(&mut c, &mem, 0, 0, &[2u8; PAGE]).unwrap();
        let mut i = 1u64;
        while i <= 6 {
            cached_write(&mut c, &mem, 0, i * p, &[i as u8 + 10; PAGE]).unwrap();
            i += 1;
        }
        assert_eq!(c.find(key), Some(slot));
        assert!(c.in_writeback(slot));
        assert_eq!(mem.writes_at(0), 0);
        assert_eq!(
            cached_flush(&mut c, &mem, Some(0)),
            Err(BlockError::QueueFull)
        );
        assert_eq!(*mem.flushes.lock().unwrap(), 0);
        assert_eq!(mem.writes_at(0), 0);
        let mut out = [0u8; PAGE];
        cached_read(&mut c, &mem, 0, 0, &mut out).unwrap();
        assert_eq!(out, [2u8; PAGE]);
        // The held write lands, then the flush writes the newer bytes once.
        mem.write(0, &held).unwrap();
        c.end_writeback(slot, key, Ok(()));
        cached_flush(&mut c, &mem, Some(0)).unwrap();
        assert_eq!(*mem.flushes.lock().unwrap(), 1);
        assert_eq!(mem.writes_at(0), 2);
        assert_eq!(mem.get(0, PAGE), vec![2u8; PAGE]);
    }

    #[test]
    fn evict_writes_back_in_place() {
        let mem = Mem::new(PAGE * 8);
        let mut c = Cache::<2>::new();
        cached_write(&mut c, &mem, 0, 0, &[1u8; 512]).unwrap();
        cached_write(&mut c, &mem, 0, PAGE as u64, &[2u8; 512]).unwrap();
        let k0 = CacheKey::page(0, 0);
        let s0 = c.find(k0).unwrap();
        let mut evict = [0u8; PAGE];
        let mut out = [0u8; 512];
        // Both slots are dirty: the miss starts a writeback in place and
        // re-keys nothing.
        let fill = c
            .plan_read(CacheKey::page(0, 2 * PAGE as u64), 0, &mut out, &mut evict)
            .unwrap()
            .unwrap();
        assert_eq!(fill.need, FillNeed::Writeback);
        assert!(c.in_writeback(fill.slot));
        assert_eq!(c.find(fill.evict_key), Some(fill.slot));
        assert_eq!(c.stats.evicts, 0);
        let other = if fill.slot == s0 {
            CacheKey::page(0, PAGE as u64)
        } else {
            k0
        };
        assert!(c.find(other).is_some());
        // The victim stays readable while its write is in flight.
        let mut hit = [0u8; 512];
        cached_read(&mut c, &mem, 0, fill.evict_key.offset, &mut hit).unwrap();
        assert_ne!(hit[0], 0);
        write_victim(&mut c, &mem, &fill, &evict).unwrap();
        assert!(!c.in_writeback(fill.slot));
        assert_eq!(mem.writes_at(fill.evict_key.offset), 1);
        cached_read(&mut c, &mem, 0, 2 * PAGE as u64, &mut out).unwrap();
        assert_eq!(c.stats.evicts, 1);
        assert!(c.find(fill.evict_key).is_none());
        cached_flush(&mut c, &mem, None).unwrap();
        assert_eq!(mem.get(0, 1)[0], 1);
        assert_eq!(mem.get(PAGE, 1)[0], 2);
    }

    #[test]
    fn end_writeback_error_redirties() {
        let mem = Mem::new(PAGE * 4);
        let mut c = Cache::<4>::new();
        cached_write(&mut c, &mem, 0, 0, &[7u8; 512]).unwrap();
        *mem.fail_writes.lock().unwrap() = 1;
        assert_eq!(cached_flush(&mut c, &mem, None), Err(BlockError::Io));
        assert_eq!(*mem.flushes.lock().unwrap(), 0);
        let slot = c.find(CacheKey::page(0, 0)).unwrap();
        assert!(!c.in_writeback(slot));
        assert_eq!(c.dirty_count(), 1);
        cached_flush(&mut c, &mem, None).unwrap();
        assert_eq!(c.dirty_count(), 0);
        assert_eq!(*mem.flushes.lock().unwrap(), 1);
        assert_eq!(mem.get(0, 512), vec![7u8; 512]);
    }

    #[test]
    fn invalidate_keeps_writeback() {
        let mem = Mem::new(PAGE * 8);
        let mut c = Cache::<2>::new();
        cached_write(&mut c, &mem, 0, 0, &[3u8; PAGE]).unwrap();
        let mut held = [0u8; PAGE];
        let (slot, key) = c.take_dirty(0, None, &mut held).unwrap();
        c.invalidate(key);
        assert!(c.find(key).is_none());
        assert!(c.in_writeback(slot));
        c.drop_dev(0);
        assert!(c.in_writeback(slot));
        // The clock never reuses the slot while its write is in flight.
        cached_write(&mut c, &mem, 0, PAGE as u64, &[4u8; PAGE]).unwrap();
        assert_ne!(c.find(CacheKey::page(0, PAGE as u64)), Some(slot));
        assert_eq!(
            cached_flush(&mut c, &mem, Some(0)),
            Err(BlockError::QueueFull)
        );
        c.end_writeback(slot, key, Err(BlockError::Io));
        assert!(!c.in_writeback(slot));
        assert!(c.find(key).is_none());
        cached_flush(&mut c, &mem, Some(0)).unwrap();
        assert_eq!(mem.writes_at(0), 0);
    }

    #[test]
    fn key_page_align() {
        let k = CacheKey::page(3, 5000);
        assert_eq!(k.dev, 3);
        assert_eq!(k.offset, 4096);
        assert_eq!(k.next_page().offset, 8192);
    }
}
