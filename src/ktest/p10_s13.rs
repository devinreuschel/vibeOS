//! In-guest tests of P10-S13, Block layer: Flush, FUA and writeback ordering (DESIGN §8.2).

use super::{Outcome, Test, test};
use crate::block_init;
use crate::virtio_blk_init;

pub(super) const TESTS: &[Test] = &[test("block_fua_write", block_fua_write)];

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
