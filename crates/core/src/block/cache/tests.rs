//! Host tests of the page-granular block cache (`block::cache`).

use super::*;
use std::sync::Mutex;

struct Mem {
    data: Mutex<Vec<u8>>,
    reads: Mutex<u64>,
    writes: Mutex<u64>,
    flushes: Mutex<u64>,
    writes_at: Mutex<std::collections::HashMap<u64, u64>>,
    fail_writes: Mutex<u32>,
    /// Fail the next write at this offset, once.
    fail_at: Mutex<Option<u64>>,
    failed_at: Mutex<u64>,
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
            fail_at: Mutex::new(None),
            failed_at: Mutex::new(0),
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
            let mut at = self.fail_at.lock().unwrap();
            if *at == Some(offset) {
                *at = None;
                *self.failed_at.lock().unwrap() += 1;
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

/// A page has one slot while a read fills it: a second read of it
/// waits on that slot (`FillNeed::None`) rather than filling a second
/// copy, which could be found stale after a write to the first; and a
/// fill whose slot was taken back during its read caches nothing, and
/// its abort leaves the slot to whoever has it.
#[test]
fn a_filling_page_has_one_slot() {
    let mut c = Cache::<1>::new();
    let key = CacheKey::page(0, 0);
    let (mut out, mut ev) = ([0u8; 8], [0u8; PAGE]);
    let f1 = c.plan_read(key, 0, &mut out, &mut ev).unwrap().unwrap();
    assert_eq!(f1.need, FillNeed::Read);
    let f2 = c.plan_read(key, 0, &mut out, &mut ev).unwrap().unwrap();
    assert_eq!((f2.need, f2.slot), (FillNeed::None, f1.slot));
    c.invalidate(key);
    c.install_read(&f1, &[7u8; PAGE], 0, &mut out).unwrap();
    assert_eq!(out, [7u8; 8]);
    assert!(c.find(key).is_none());
    let other = CacheKey::page(0, PAGE as u64);
    let f3 = c.plan_read(other, 0, &mut out, &mut ev).unwrap().unwrap();
    assert_eq!(f3.slot, f1.slot, "the freed slot is reused");
    c.abort_fill(&f1);
    assert!(c.filling(f3.slot), "a stale abort leaves the new fill");
    assert!(!c.install_write(&f1, &[0u8; PAGE], 0, &[1u8; 4]).unwrap());
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
    cached_read(&mut c, &mem, 0, 0, &mut out, &mut [0u8; PAGE]).unwrap();
    let r1 = *mem.reads.lock().unwrap();
    assert_eq!(out[0], 0xAB);
    cached_read(&mut c, &mem, 0, 0, &mut out, &mut [0u8; PAGE]).unwrap();
    let r2 = *mem.reads.lock().unwrap();
    assert_eq!(r1, 1);
    assert_eq!(r2, 1);
    assert_eq!(c.stats.hits, 1);
    assert_eq!(c.stats.misses, 1);
    assert_eq!(c.stats.device_reads, 1);
    assert_eq!(c.stats.device_reqs(), 1);
    // same page, different 512 window is still a hit
    cached_read(&mut c, &mem, 0, 512, &mut out, &mut [0u8; PAGE]).unwrap();
    assert_eq!(*mem.reads.lock().unwrap(), 1);
    assert_eq!(c.stats.hits, 2);
}

#[test]
fn writeback_and_flush() {
    let mem = Mem::new(PAGE * 4);
    let mut c = Cache::<4>::new();
    let buf = [0x5Au8; 512];
    cached_write(&mut c, &mem, 1, 0, &buf, &mut [0u8; PAGE]).unwrap();
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
    cached_write(&mut c, &mem, 0, 0, &buf, &mut [0u8; PAGE]).unwrap();
    buf[0] = 2;
    cached_write(&mut c, &mem, 0, PAGE as u64, &buf, &mut [0u8; PAGE]).unwrap();
    // two dirty pages fill the cache; a third must evict
    buf[0] = 3;
    cached_write(&mut c, &mem, 0, 2 * PAGE as u64, &buf, &mut [0u8; PAGE]).unwrap();
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
    cached_write(&mut c, &mem, 0, 0, &buf, &mut [0u8; PAGE]).unwrap();
    cached_write(&mut c, &mem, 0, PAGE as u64, &buf, &mut [0u8; PAGE]).unwrap();
    cached_write(&mut c, &mem, 0, 2 * PAGE as u64, &buf, &mut [0u8; PAGE]).unwrap();
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
    cached_read(&mut c, &mem, 0, 0, &mut a, &mut [0u8; PAGE]).unwrap();
    let r0 = *mem.reads.lock().unwrap();
    cached_read(&mut c, &mem, 0, PAGE as u64, &mut b, &mut [0u8; PAGE]).unwrap();
    let r1 = *mem.reads.lock().unwrap();
    // miss on page1 plus readahead of page2
    assert!(r1 > r0);
    let r2 = *mem.reads.lock().unwrap();
    cached_read(&mut c, &mem, 0, 2 * PAGE as u64, &mut a, &mut [0u8; PAGE]).unwrap();
    // page2 should already be present from readahead
    assert_eq!(*mem.reads.lock().unwrap(), r2);
}

#[test]
fn writeback_inflight_keeps_slot() {
    let mem = Mem::new(PAGE * 16);
    let mut c = Cache::<4>::new();
    let p = PAGE as u64;
    cached_write(&mut c, &mem, 0, 0, &[1u8; PAGE], &mut [0u8; PAGE]).unwrap();
    // blk-wb takes the page and its write stays in flight.
    let mut held = [0u8; PAGE];
    let (slot, key) = c.take_dirty(0, None, &mut held).unwrap();
    assert_eq!(key, CacheKey::page(0, 0));
    assert!(c.in_writeback(slot));
    // The page is dirtied again, then the cache is filled past its size.
    cached_write(&mut c, &mem, 0, 0, &[2u8; PAGE], &mut [0u8; PAGE]).unwrap();
    let mut i = 1u64;
    while i <= 6 {
        cached_write(
            &mut c,
            &mem,
            0,
            i * p,
            &[i as u8 + 10; PAGE],
            &mut [0u8; PAGE],
        )
        .unwrap();
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
    cached_read(&mut c, &mem, 0, 0, &mut out, &mut [0u8; PAGE]).unwrap();
    assert_eq!(out, [2u8; PAGE]);
    // The held write lands, then the flush writes the newer bytes once.
    mem.write(0, &held).unwrap();
    c.end_writeback(slot, key, Ok(()));
    cached_flush(&mut c, &mem, Some(0)).unwrap();
    assert_eq!(*mem.flushes.lock().unwrap(), 1);
    assert_eq!(mem.writes_at(0), 2);
    assert_eq!(mem.get(0, PAGE), vec![2u8; PAGE]);
}

/// A flush asks each slot until it is done and never comes back to
/// it, so a page dirtied again after the flush wrote it does not hold
/// the flush off: the old flush took every dirty page again on each
/// pass and sent its `Flush` only once none was left.
#[test]
fn flush_slot_writes_each_slot_once() {
    let mut c = Cache::<4>::new();
    let mut z = [0u8; PAGE];
    let mem = Mem::new(PAGE * 8);
    cached_write(&mut c, &mem, 0, 0, &[1u8; PAGE], &mut z).unwrap();
    let since = c.flush_begin();
    let slot = c.find(CacheKey::page(0, 0)).unwrap();
    let mut dst = [0u8; PAGE];
    let FlushStep::Write(s, key) = c.flush_slot(slot, Some(0), since, &mut dst) else {
        panic!("dirty page not written");
    };
    // A writer dirties the page again while the flush's write is out.
    cached_write(&mut c, &mem, 0, 0, &[2u8; PAGE], &mut z).unwrap();
    c.end_writeback(s, key, Ok(()));
    // The sweep has moved past the slot; a later flush writes it.
    let mut others = 0;
    for i in 0..4 {
        if i != slot {
            others += 1;
            assert_eq!(c.flush_slot(i, Some(0), since, &mut dst), FlushStep::Done);
        }
    }
    assert_eq!(others, 3);
    assert_eq!(dst, [1u8; PAGE]);
    // Another device's dirty page is not this flush's.
    cached_write(&mut c, &mem, 1, PAGE as u64, &[3u8; PAGE], &mut z).unwrap();
    let other = c.find(CacheKey::page(1, PAGE as u64)).unwrap();
    assert_eq!(
        c.flush_slot(other, Some(0), since, &mut dst),
        FlushStep::Done
    );
}

/// A writeback in flight when the flush began is waited for and the
/// slot asked again, since a write after its copy may predate the
/// flush; one begun after the flush carries every earlier byte, so the
/// slot is done once it ends.
#[test]
fn flush_slot_waits_once_for_each_writeback() {
    let mut c = Cache::<4>::new();
    let mut z = [0u8; PAGE];
    let mem = Mem::new(PAGE * 8);
    let mut held = [0u8; PAGE];
    let mut dst = [0u8; PAGE];
    // Begun before the flush, then dirtied again before it too.
    cached_write(&mut c, &mem, 0, 0, &[1u8; PAGE], &mut z).unwrap();
    let (slot, key) = c.take_dirty(0, None, &mut held).unwrap();
    let before = c.writeback_start(slot).unwrap();
    cached_write(&mut c, &mem, 0, 0, &[2u8; PAGE], &mut z).unwrap();
    let since = c.flush_begin();
    assert_eq!(
        c.flush_slot(slot, Some(0), since, &mut dst),
        FlushStep::Wait {
            slot,
            key,
            start: before,
            done: false
        }
    );
    c.end_writeback(slot, key, Ok(()));
    assert_eq!(
        c.flush_slot(slot, Some(0), since, &mut dst),
        FlushStep::Write(slot, key)
    );
    assert_eq!(dst, [2u8; PAGE]);
    c.end_writeback(slot, key, Ok(()));
    // Begun after the flush (as `blk-wb` would), and dirtied again.
    cached_write(&mut c, &mem, 0, PAGE as u64, &[4u8; PAGE], &mut z).unwrap();
    let since = c.flush_begin();
    let (slot, key) = c.take_dirty(0, None, &mut held).unwrap();
    cached_write(&mut c, &mem, 0, PAGE as u64, &[5u8; PAGE], &mut z).unwrap();
    let after = c.writeback_start(slot).unwrap();
    assert!(after > since);
    assert_eq!(
        c.flush_slot(slot, Some(0), since, &mut dst),
        FlushStep::Wait {
            slot,
            key,
            start: after,
            done: true
        }
    );
    c.end_writeback(slot, key, Ok(()));
    assert_eq!(c.writeback_start(slot), None);
}

#[test]
fn evict_writes_back_in_place() {
    let mem = Mem::new(PAGE * 8);
    let mut c = Cache::<2>::new();
    cached_write(&mut c, &mem, 0, 0, &[1u8; 512], &mut [0u8; PAGE]).unwrap();
    cached_write(&mut c, &mem, 0, PAGE as u64, &[2u8; 512], &mut [0u8; PAGE]).unwrap();
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
    cached_read(
        &mut c,
        &mem,
        0,
        fill.evict_key.offset,
        &mut hit,
        &mut [0u8; PAGE],
    )
    .unwrap();
    assert_ne!(hit[0], 0);
    write_victim(&mut c, &mem, &fill, &evict).unwrap();
    assert!(!c.in_writeback(fill.slot));
    assert_eq!(mem.writes_at(fill.evict_key.offset), 1);
    cached_read(&mut c, &mem, 0, 2 * PAGE as u64, &mut out, &mut [0u8; PAGE]).unwrap();
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
    cached_write(&mut c, &mem, 0, 0, &[7u8; 512], &mut [0u8; PAGE]).unwrap();
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
fn readahead_evict_write_fail_keeps_dirty() {
    let mem = Mem::new(PAGE * 16);
    let mut c = Cache::<3>::new();
    let p = PAGE as u64;
    // Pages 0, 5, and 6 fill the cache dirty, so the readahead's only
    // victims are dirty and the clock picks page 0's slot.
    cached_write(&mut c, &mem, 0, 0, &[0x11u8; PAGE], &mut [0u8; PAGE]).unwrap();
    cached_write(&mut c, &mem, 0, 5 * p, &[0x55u8; PAGE], &mut [0u8; PAGE]).unwrap();
    cached_write(&mut c, &mem, 0, 6 * p, &[0x66u8; PAGE], &mut [0u8; PAGE]).unwrap();
    *mem.fail_at.lock().unwrap() = Some(0);
    let mut out = [0u8; 512];
    cached_read(&mut c, &mem, 0, 5 * p, &mut out, &mut [0u8; PAGE]).unwrap();
    assert_eq!(*mem.failed_at.lock().unwrap(), 0);
    // The page 6 read is sequential: its readahead of page 7 evicts
    // page 0, whose write fails.
    cached_read(&mut c, &mem, 0, 6 * p, &mut out, &mut [0u8; PAGE]).unwrap();
    assert_eq!(*mem.failed_at.lock().unwrap(), 1);
    assert_eq!(*mem.reads.lock().unwrap(), 0);
    assert!(c.find(CacheKey::page(0, 7 * p)).is_none());
    // Page 0 is still cached, unchanged, and dirty.
    let mut page = [0u8; PAGE];
    cached_read(&mut c, &mem, 0, 0, &mut page, &mut [0u8; PAGE]).unwrap();
    assert_eq!(page, [0x11u8; PAGE]);
    assert_eq!(*mem.reads.lock().unwrap(), 0);
    assert_eq!(c.dirty_count(), 3);
    let s0 = c.find(CacheKey::page(0, 0)).unwrap();
    assert!(!c.in_writeback(s0));
    cached_flush(&mut c, &mem, None).unwrap();
    assert_eq!(c.dirty_count(), 0);
    assert_eq!(mem.writes_at(0), 1);
    assert_eq!(mem.get(0, PAGE), vec![0x11u8; PAGE]);
}

#[test]
fn invalidate_keeps_writeback() {
    let mem = Mem::new(PAGE * 8);
    let mut c = Cache::<2>::new();
    cached_write(&mut c, &mem, 0, 0, &[3u8; PAGE], &mut [0u8; PAGE]).unwrap();
    let mut held = [0u8; PAGE];
    let (slot, key) = c.take_dirty(0, None, &mut held).unwrap();
    c.invalidate(key);
    assert!(c.find(key).is_none());
    assert!(c.in_writeback(slot));
    c.drop_dev(0);
    assert!(c.in_writeback(slot));
    // The clock never reuses the slot while its write is in flight.
    cached_write(&mut c, &mem, 0, PAGE as u64, &[4u8; PAGE], &mut [0u8; PAGE]).unwrap();
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
    assert_eq!(k.next_page().dev, 3);
}

#[test]
fn key_ids_are_64_bit() {
    let hi = CacheKey::page(1 << 40, 0);
    assert_ne!(hi, CacheKey::page(0, 0));
    assert_eq!(hi.dev, 1 << 40);
}

#[test]
fn cache_evict_range_writes_dirty() {
    let mem = Mem::new(PAGE * 8);
    let mut c = Cache::<4>::new();
    cached_write(&mut c, &mem, 0, PAGE as u64, &[5u8; 16], &mut [0u8; PAGE]).unwrap();
    cached_write(
        &mut c,
        &mem,
        0,
        2 * PAGE as u64 + 9,
        &[6u8; 4],
        &mut [0u8; PAGE],
    )
    .unwrap();
    cached_write(
        &mut c,
        &mem,
        0,
        3 * PAGE as u64,
        &[8u8; 4],
        &mut [0u8; PAGE],
    )
    .unwrap();
    assert_eq!(mem.get(PAGE, 16), vec![0u8; 16]);
    cached_evict_range(&mut c, &mem, 0, PAGE as u64, 2 * PAGE as u64).unwrap();
    assert_eq!(mem.get(PAGE, 16), vec![5u8; 16]);
    assert_eq!(mem.get(2 * PAGE + 9, 4), vec![6u8; 4]);
    assert!(c.find(CacheKey::page(0, PAGE as u64)).is_none());
    assert!(c.find(CacheKey::page(0, 2 * PAGE as u64)).is_none());
    // Outside the range: still cached and dirty, not written.
    assert!(c.find(CacheKey::page(0, 3 * PAGE as u64)).is_some());
    assert_eq!(mem.get(3 * PAGE, 4), vec![0u8; 4]);
    assert_eq!(c.dirty_count(), 1);
}

#[test]
fn cache_evict_range_keeps_dirty_on_error() {
    let mem = Mem::new(PAGE * 8);
    let mut c = Cache::<4>::new();
    cached_write(&mut c, &mem, 0, 0, &[9u8; 32], &mut [0u8; PAGE]).unwrap();
    *mem.fail_writes.lock().unwrap() = 1;
    assert_eq!(
        cached_evict_range(&mut c, &mem, 0, 0, PAGE as u64),
        Err(BlockError::Io)
    );
    let slot = c.find(CacheKey::page(0, 0)).unwrap();
    assert!(!c.in_writeback(slot));
    assert_eq!(c.dirty_count(), 1);
    assert_eq!(mem.get(0, 32), vec![0u8; 32]);
    cached_evict_range(&mut c, &mem, 0, 0, PAGE as u64).unwrap();
    assert_eq!(mem.get(0, 32), vec![9u8; 32]);
    assert!(c.find(CacheKey::page(0, 0)).is_none());
}
