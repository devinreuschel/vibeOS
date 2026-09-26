//! In-guest tests of P10-S13, Block layer: Flush, FUA and writeback ordering (DESIGN §8.2).

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use vibeos::block::BlockError;
use vibeos::cache::PAGE;

use super::{Outcome, Test, spawn_thread, test};
use crate::block_init;
use crate::cache_init::{self, DEV_RAM0, testing};
use crate::thread_init;
use crate::time_init;
use crate::virtio_blk_init;

pub(super) const TESTS: &[Test] = &[
    test("cache_flush_waits_writeback", cache_flush_waits_writeback),
    test("block_fua_write", block_fua_write),
];

/// ram0's last ten pages (sectors 176..256): ten dirty pages put the
/// 16-page cache over its dirty ratio, so `blk-wb` writes them.
const WB_FIRST_LBA: u64 = 176;
const WB_PAGES: usize = 10;
/// How long a flush must stay blocked on the held write.
const BLOCKED_MS: u64 = 200;
const WAIT_NS: u64 = 2_000_000_000;

const FLUSH_PENDING: u32 = 0;
const FLUSH_OK: u32 = 1;
const FLUSH_ERR: u32 = 2;
static FLUSH_RES: AtomicU32 = AtomicU32::new(FLUSH_PENDING);

fn flush_ram0() {
    let res = match cache_init::flush(DEV_RAM0) {
        Ok(()) => FLUSH_OK,
        Err(_) => FLUSH_ERR,
    };
    FLUSH_RES.store(res, Ordering::Release);
}

/// Poll `f` until it holds or `WAIT_NS` passes.
fn wait_for(f: impl Fn() -> bool) -> bool {
    let t0 = time_init::now_ns();
    while !f() {
        if time_init::now_ns().saturating_sub(t0) > WAIT_NS {
            return false;
        }
        thread_init::sleep_ms(1);
    }
    true
}

/// Releases the hold and puts the saved pages back through the cache on
/// every path out of [`cache_flush_waits_writeback`].
struct WbGuard {
    saved: Vec<u8>,
    restored: bool,
}

impl WbGuard {
    fn restore(&mut self) -> Result<(), BlockError> {
        testing::release();
        // A failed flush thread may still be waiting: wait out its flush.
        let _pending = wait_for(|| FLUSH_RES.load(Ordering::Acquire) != FLUSH_PENDING);
        cache_init::write(DEV_RAM0, WB_FIRST_LBA, &self.saved)?;
        cache_init::flush(DEV_RAM0)?;
        self.restored = true;
        Ok(())
    }
}

impl Drop for WbGuard {
    fn drop(&mut self) {
        if !self.restored && self.restore().is_err() {
            crate::klog!(
                vibeos::log::Level::Warn,
                "vibeOS: ktest: cache_flush_waits_writeback: ram0 restore failed"
            );
        }
    }
}

/// `cache_init::flush` sends no `Flush` while `blk-wb` holds one write of
/// the device, and the held page is on the device once it does.
fn cache_flush_waits_writeback() -> Outcome {
    if !cache_init::live() || !block_init::live() {
        return Outcome::Fail("no ram0 cache");
    }
    if cache_init::flush(DEV_RAM0).is_err() {
        return Outcome::Fail("pre-flush");
    }
    let mut saved = alloc::vec![0u8; WB_PAGES * PAGE];
    if cache_init::read(DEV_RAM0, WB_FIRST_LBA, &mut saved).is_err() {
        return Outcome::Fail("save");
    }
    FLUSH_RES.store(FLUSH_OK, Ordering::Release);
    let mut guard = WbGuard {
        saved,
        restored: false,
    };
    let mut dirty = alloc::vec![0u8; WB_PAGES * PAGE];
    let mut i = 0usize;
    while i < dirty.len() {
        dirty[i] = guard.saved[i] ^ 0xA5 ^ (i / PAGE) as u8;
        i += 1;
    }
    let held_off = WB_FIRST_LBA * 512;
    testing::hold_wb(DEV_RAM0, held_off);
    if cache_init::write(DEV_RAM0, WB_FIRST_LBA, &dirty).is_err() {
        return Outcome::Fail("dirty");
    }
    if !wait_for(testing::held) {
        return Outcome::Fail("blk-wb never held");
    }
    let flushes0 = block_init::flushes();
    FLUSH_RES.store(FLUSH_PENDING, Ordering::Release);
    let _flusher = spawn_thread("ktest-flush", flush_ram0);
    thread_init::sleep_ms(BLOCKED_MS);
    if FLUSH_RES.load(Ordering::Acquire) != FLUSH_PENDING {
        return Outcome::Fail("flush returned while a write was held");
    }
    if block_init::flushes() != flushes0 {
        return Outcome::Fail("Flush sent while a write was held");
    }
    testing::release();
    if !wait_for(|| FLUSH_RES.load(Ordering::Acquire) != FLUSH_PENDING) {
        return Outcome::Fail("flush never returned");
    }
    if FLUSH_RES.load(Ordering::Acquire) != FLUSH_OK {
        return Outcome::Fail("flush failed");
    }
    if block_init::flushes() <= flushes0 {
        return Outcome::Fail("no Flush after release");
    }
    let mut page = [0u8; PAGE];
    if block_init::read(WB_FIRST_LBA, &mut page).is_err() {
        return Outcome::Fail("raw read");
    }
    if page[..] != dirty[..PAGE] {
        return Outcome::Fail("held page not on ram0");
    }
    match guard.restore() {
        Ok(()) => Outcome::Ok,
        Err(_) => Outcome::Fail("restore"),
    }
}

/// A ram0 sector past the stamped MBR partitions (`part_init`); restored.
const RAM0_FUA_LBA: u64 = 200;

/// `write_fua` on one sector through `write`/`read`/`write_fua`/`flushes`:
/// the queue sends a `Flush` for it (neither driver has FUA), the data
/// reads back, and the sector is restored.
fn fua_roundtrip(
    lba: u64,
    read: fn(u64, &mut [u8]) -> Result<(), vibeos::block::BlockError>,
    write: fn(u64, &[u8]) -> Result<(), vibeos::block::BlockError>,
    write_fua: fn(u64, &[u8]) -> Result<(), vibeos::block::BlockError>,
    flushes: fn() -> u64,
) -> Outcome {
    let mut saved = [0u8; 512];
    if read(lba, &mut saved).is_err() {
        return Outcome::Fail("save read");
    }
    let mut buf = [0u8; 512];
    let mut i = 0usize;
    while i < buf.len() {
        buf[i] = (i as u8).wrapping_mul(7).wrapping_add(0x3D) ^ saved[i];
        i += 1;
    }
    let before = flushes();
    let res = write_fua(lba, &buf);
    let after = flushes();
    let mut out = [0u8; 512];
    let back = read(lba, &mut out);
    if write(lba, &saved).is_err() {
        return Outcome::Fail("restore");
    }
    if res.is_err() {
        return Outcome::Fail("write_fua");
    }
    if after <= before {
        return Outcome::Fail("no flush for fua");
    }
    if back.is_err() || out != buf {
        return Outcome::Fail("fua data");
    }
    Outcome::Ok
}

fn block_fua_write() -> Outcome {
    if !block_init::live() {
        return Outcome::Fail("no ram0");
    }
    let r = fua_roundtrip(
        RAM0_FUA_LBA,
        block_init::read,
        block_init::write,
        block_init::write_fua,
        block_init::flushes,
    );
    if !matches!(r, Outcome::Ok) {
        return r;
    }
    if !virtio_blk_init::live() {
        return Outcome::Ok;
    }
    if virtio_blk_init::logical_block_size() != 512 {
        return Outcome::Skip("vda not 512");
    }
    fua_roundtrip(
        virtio_blk_init::persist_lba().saturating_add(1),
        virtio_blk_init::read,
        virtio_blk_init::write,
        virtio_blk_init::write_fua,
        virtio_blk_init::flushes,
    )
}
