//! Page-granular block cache. ROADMAP §7.4.
//!
//! Keyed by `(dev, page-aligned byte offset)`. Read-through, write-back,
//! clock (second-chance) eviction, sequential readahead, dirty-ratio cap.
//!
//! Phase 12 will unify this with the file page cache: same frames, same
//! clock, same writeback. The key grows an inode id; do not add a second
//! private cache beside this one.

use crate::block::BlockError;

pub const PAGE: usize = 4096;
pub const DEFAULT_PAGES: usize = 16;
pub const DIRTY_RATIO_PCT: u32 = 50;
pub const READAHEAD_PAGES: u32 = 1;

const F_VALID: u8 = 1;
const F_DIRTY: u8 = 2;
const F_REF: u8 = 4;
const F_FILL: u8 = 8;

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

/// I/O the caller must run with the cache lock dropped, then [`Cache::install`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FillNeed {
    None,
    Read,
    WritebackThenRead,
    Writeback,
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

    fn clock_slot(&mut self) -> Option<usize> {
        let mut steps = 0usize;
        while steps < N * 2 {
            let i = self.hand;
            self.hand = (self.hand + 1) % N;
            let f = self.meta[i].flags;
            if f & F_FILL != 0 {
                steps += 1;
                continue;
            }
            if f & F_VALID == 0 {
                return Some(i);
            }
            if f & F_REF != 0 {
                self.meta[i].flags = f & !F_REF;
                steps += 1;
                continue;
            }
            return Some(i);
        }
        None
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

    fn stash_evict(&self, slot: usize, evict_out: &mut [u8]) -> Result<(), BlockError> {
        self.copy_page(slot, evict_out)
    }

    /// Copy a hit into `out`. On miss, pick a slot (maybe needing writeback).
    /// `evict_out` must be ≥ PAGE when the plan returns a writeback need.
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
        self.stats.misses = self.stats.misses.saturating_add(1);
        let slot = self.clock_slot().ok_or(BlockError::Failed)?;
        let mut fill = Fill {
            slot,
            key,
            need: FillNeed::Read,
            evict_key: CacheKey { dev: 0, offset: 0 },
        };
        let f = self.meta[slot].flags;
        if f & (F_VALID | F_DIRTY) == F_VALID | F_DIRTY {
            fill.need = FillNeed::WritebackThenRead;
            fill.evict_key = self.meta[slot].key;
            self.stash_evict(slot, evict_out)?;
            self.stats.evicts = self.stats.evicts.saturating_add(1);
        } else if f & F_VALID != 0 {
            self.stats.evicts = self.stats.evicts.saturating_add(1);
        }
        self.meta[slot].key = key;
        self.meta[slot].flags = F_FILL;
        let _ = self.note_seq(key);
        Ok(Some(fill))
    }

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
        self.stats.misses = self.stats.misses.saturating_add(1);
        let slot = self.clock_slot().ok_or(BlockError::Failed)?;
        let whole = off == 0 && src.len() == PAGE;
        let mut fill = Fill {
            slot,
            key,
            need: if whole {
                FillNeed::None
            } else {
                FillNeed::Read
            },
            evict_key: CacheKey { dev: 0, offset: 0 },
        };
        let f = self.meta[slot].flags;
        if f & (F_VALID | F_DIRTY) == F_VALID | F_DIRTY {
            fill.need = if whole {
                FillNeed::Writeback
            } else {
                FillNeed::WritebackThenRead
            };
            fill.evict_key = self.meta[slot].key;
            self.stash_evict(slot, evict_out)?;
            self.stats.evicts = self.stats.evicts.saturating_add(1);
        } else if f & F_VALID != 0 {
            self.stats.evicts = self.stats.evicts.saturating_add(1);
        }
        if whole {
            self.data[slot].copy_from_slice(src);
            self.meta[slot].key = key;
            self.meta[slot].flags = F_VALID | F_DIRTY | F_REF;
            let _ = self.note_seq(key);
            return Ok(if matches!(fill.need, FillNeed::Writeback) {
                Some(fill)
            } else {
                None
            });
        }
        self.meta[slot].key = key;
        self.meta[slot].flags = F_FILL;
        let _ = self.note_seq(key);
        Ok(Some(fill))
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

    /// Copy the next dirty page into `dst` and clear dirty. A write that
    /// hits during the subsequent device I/O sets dirty again.
    /// `dev` limits the scan to one device when `Some`.
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
            if f & (F_VALID | F_DIRTY | F_FILL) == F_VALID | F_DIRTY {
                let key = self.meta[s].key;
                if let Some(d) = dev
                    && key.dev != d
                {
                    s += 1;
                    continue;
                }
                dst[..PAGE].copy_from_slice(&self.data[s]);
                self.meta[s].flags = f & !F_DIRTY;
                return Some((s, key));
            }
            s += 1;
        }
        None
    }

    #[allow(dead_code)]
    pub fn mark_clean(&mut self, slot: usize, key: CacheKey) {
        if slot < N && self.meta[slot].key == key && self.meta[slot].flags & F_VALID != 0 {
            self.meta[slot].flags &= !F_DIRTY;
        }
    }

    pub fn mark_dirty(&mut self, slot: usize, key: CacheKey) {
        if slot < N && self.meta[slot].key == key && self.meta[slot].flags & F_VALID != 0 {
            self.meta[slot].flags |= F_DIRTY;
        }
    }

    pub fn restore_evict(&mut self, slot: usize, key: CacheKey, data: &[u8]) {
        if slot >= N || data.len() < PAGE {
            return;
        }
        self.data[slot].copy_from_slice(&data[..PAGE]);
        self.meta[slot].key = key;
        self.meta[slot].flags = F_VALID | F_DIRTY | F_REF;
    }

    #[allow(dead_code)]
    pub fn drop_dev(&mut self, dev: u32) {
        let mut i = 0usize;
        while i < N {
            if self.meta[i].key.dev == dev {
                self.meta[i].flags = 0;
            }
            i += 1;
        }
    }

    /// Drop a page without writeback. tmpfs uses this when a file
    /// frees or relocates backing so a later alloc cannot see stale data.
    pub fn invalidate(&mut self, key: CacheKey) {
        if let Some(i) = self.find(key) {
            self.meta[i].flags = 0;
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
        match c.plan_read(key, pin, &mut buf[done..done + n], &mut evict)? {
            None => {}
            Some(fill) => {
                if matches!(fill.need, FillNeed::None) {
                    return Err(BlockError::Io);
                }
                if matches!(fill.need, FillNeed::Writeback | FillNeed::WritebackThenRead) {
                    if let Err(e) = b.write(fill.evict_key.offset, &evict) {
                        c.restore_evict(fill.slot, fill.evict_key, &evict);
                        return Err(e);
                    }
                    c.stats.device_writes = c.stats.device_writes.saturating_add(1);
                }
                let mut page = [0u8; PAGE];
                if matches!(fill.need, FillNeed::Read | FillNeed::WritebackThenRead) {
                    if let Err(e) = b.read(fill.key.offset, &mut page) {
                        c.abort_fill(fill.slot);
                        return Err(e);
                    }
                    c.stats.device_reads = c.stats.device_reads.saturating_add(1);
                }
                c.install_read(&fill, &page, pin, &mut buf[done..done + n])?;
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
                FillNeed::None => c.abort_fill(fill.slot),
                FillNeed::Writeback | FillNeed::WritebackThenRead => {
                    let _ = b.write(fill.evict_key.offset, &evict);
                    c.stats.device_writes = c.stats.device_writes.saturating_add(1);
                    if matches!(fill.need, FillNeed::WritebackThenRead | FillNeed::Read) {
                        let mut page = [0u8; PAGE];
                        if b.read(rk.offset, &mut page).is_ok() {
                            c.stats.device_reads = c.stats.device_reads.saturating_add(1);
                            let mut one = [0u8; 1];
                            let _ = c.install_read(&fill, &page, 0, &mut one);
                        } else {
                            c.abort_fill(fill.slot);
                        }
                    } else {
                        c.abort_fill(fill.slot);
                    }
                }
                FillNeed::Read => {
                    let mut page = [0u8; PAGE];
                    if b.read(rk.offset, &mut page).is_ok() {
                        c.stats.device_reads = c.stats.device_reads.saturating_add(1);
                        let mut one = [0u8; 1];
                        let _ = c.install_read(&fill, &page, 0, &mut one);
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
        match c.plan_write(key, pin, &buf[done..done + n], &mut evict)? {
            None => {}
            Some(fill) => {
                if matches!(fill.need, FillNeed::None) {
                    return Err(BlockError::Io);
                }
                if matches!(fill.need, FillNeed::Writeback | FillNeed::WritebackThenRead) {
                    if let Err(e) = b.write(fill.evict_key.offset, &evict) {
                        c.restore_evict(fill.slot, fill.evict_key, &evict);
                        return Err(e);
                    }
                    c.stats.device_writes = c.stats.device_writes.saturating_add(1);
                }
                if matches!(fill.need, FillNeed::Writeback) {
                    // whole-page write already installed
                } else {
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

fn writeback_all<B: Backend, const N: usize>(
    c: &mut Cache<N>,
    b: &B,
    dev: Option<u32>,
) -> Result<(), BlockError> {
    let mut data = [0u8; PAGE];
    let mut start = 0usize;
    while let Some((slot, key)) = c.take_dirty(start, dev, &mut data) {
        if let Err(e) = b.write(key.offset, &data) {
            c.mark_dirty(slot, key);
            return Err(e);
        }
        c.stats.device_writes = c.stats.device_writes.saturating_add(1);
        start = slot + 1;
    }
    Ok(())
}

pub fn cached_flush<B: Backend, const N: usize>(
    c: &mut Cache<N>,
    b: &B,
    dev: Option<u32>,
) -> Result<(), BlockError> {
    writeback_all(c, b, dev)?;
    b.flush()?;
    c.stats.device_flushes = c.stats.device_flushes.saturating_add(1);
    Ok(())
}

pub fn cached_barrier<B: Backend, const N: usize>(
    c: &mut Cache<N>,
    b: &B,
    dev: u32,
) -> Result<(), BlockError> {
    writeback_all(c, b, Some(dev))
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
    }

    impl Mem {
        fn new(n: usize) -> Self {
            Self {
                data: Mutex::new(vec![0u8; n]),
                reads: Mutex::new(0),
                writes: Mutex::new(0),
                flushes: Mutex::new(0),
            }
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
            *self.writes.lock().unwrap() += 1;
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
    fn barrier_writes_dirty_no_device_flush() {
        let mem = Mem::new(PAGE * 4);
        let mut c = Cache::<4>::new();
        let buf = [9u8; 512];
        cached_write(&mut c, &mem, 0, 0, &buf).unwrap();
        cached_barrier(&mut c, &mem, 0).unwrap();
        assert!(*mem.writes.lock().unwrap() >= 1);
        assert_eq!(*mem.flushes.lock().unwrap(), 0);
        cached_flush(&mut c, &mem, Some(0)).unwrap();
        assert_eq!(*mem.flushes.lock().unwrap(), 1);
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
    fn key_page_align() {
        let k = CacheKey::page(3, 5000);
        assert_eq!(k.dev, 3);
        assert_eq!(k.offset, 4096);
        assert_eq!(k.next_page().offset, 8192);
    }
}
