//! virtio-blk request packing and config math. ROADMAP §7.2.
//!
//! Header/status/discard layouts are little-endian packed against the
//! OASIS virtio-blk constants. Kernel MMIO / VQ / IRQ live in
//! `virtio_blk_init`.

use crate::block::BlockError;
use crate::virtio::{self, F_EVENT_IDX, F_INDIRECT_DESC, F_VERSION_1};

/// virtio-blk feature bits (device-specific, not transport).
pub const F_SIZE_MAX: u64 = 1 << 1;
pub const F_SEG_MAX: u64 = 1 << 2;
pub const F_GEOMETRY: u64 = 1 << 4;
pub const F_RO: u64 = 1 << 5;
pub const F_BLK_SIZE: u64 = 1 << 6;
pub const F_FLUSH: u64 = 1 << 9;
pub const F_TOPOLOGY: u64 = 1 << 10;
pub const F_CONFIG_WCE: u64 = 1 << 11;
pub const F_MQ: u64 = 1 << 12;
pub const F_DISCARD: u64 = 1 << 13;

/// Transport + blk features we will accept. Never [`F_RO`].
pub const OFFER: u64 = F_VERSION_1
    | F_INDIRECT_DESC
    | F_EVENT_IDX
    | F_SIZE_MAX
    | F_SEG_MAX
    | F_BLK_SIZE
    | F_TOPOLOGY
    | F_FLUSH
    | F_MQ
    | F_DISCARD;

pub const T_IN: u32 = 0;
pub const T_OUT: u32 = 1;
pub const T_FLUSH: u32 = 4;
pub const T_DISCARD: u32 = 11;

pub const S_OK: u8 = 0;
pub const S_IOERR: u8 = 1;
pub const S_UNSUPP: u8 = 2;

pub const HDR_LEN: usize = 16;
pub const DISCARD_LEN: usize = 16;
pub const STATUS_LEN: usize = 1;

/// virtio-blk `sector` is always 512-byte units, even when `blk_size` is 4K.
pub const SECTOR: u32 = 512;

pub const CFG_CAPACITY: u16 = 0;
pub const CFG_SIZE_MAX: u16 = 8;
pub const CFG_SEG_MAX: u16 = 12;
pub const CFG_GEOMETRY: u16 = 16;
pub const CFG_BLK_SIZE: u16 = 20;
pub const CFG_TOPOLOGY: u16 = 24;
pub const CFG_WRITEBACK: u16 = 32;
pub const CFG_NUM_QUEUES: u16 = 34;
pub const CFG_MAX_DISCARD_SECTORS: u16 = 36;
pub const CFG_MAX_DISCARD_SEG: u16 = 40;
pub const CFG_DISCARD_ALIGN: u16 = 44;

pub const NAME: &str = "vda";

pub fn pick_features(device: u64) -> Result<u64, virtio::VirtioError> {
    virtio::pick_features(device, OFFER)
}

/// Logical block size. Missing [`F_BLK_SIZE`] → 512. Never trust a
/// zero or non-multiple-of-512 `blk_size` from config.
pub fn pick_blk_size(feat: u64, cfg_blk_size: u32) -> u32 {
    if feat & F_BLK_SIZE != 0 && cfg_blk_size >= SECTOR && cfg_blk_size.is_multiple_of(SECTOR) {
        cfg_blk_size
    } else {
        SECTOR
    }
}

pub fn sector_for_lba(lba: u64, blk_size: u32) -> Option<u64> {
    if blk_size < SECTOR || !blk_size.is_multiple_of(SECTOR) {
        return None;
    }
    lba.checked_mul((blk_size / SECTOR) as u64)
}

pub fn logical_capacity(capacity_512: u64, blk_size: u32) -> Option<u64> {
    if blk_size < SECTOR || !blk_size.is_multiple_of(SECTOR) {
        return None;
    }
    let n = (blk_size / SECTOR) as u64;
    Some(capacity_512 / n)
}

pub fn pack_header(out: &mut [u8; HDR_LEN], typ: u32, sector: u64) {
    out[0..4].copy_from_slice(&typ.to_le_bytes());
    out[4..8].copy_from_slice(&0u32.to_le_bytes());
    out[8..16].copy_from_slice(&sector.to_le_bytes());
}

pub fn unpack_header(buf: &[u8; HDR_LEN]) -> (u32, u64) {
    let typ = u32::from_le_bytes(buf[0..4].try_into().unwrap_or([0; 4]));
    let sector = u64::from_le_bytes(buf[8..16].try_into().unwrap_or([0; 8]));
    (typ, sector)
}

pub fn pack_discard(out: &mut [u8; DISCARD_LEN], sector: u64, nsect: u32, flags: u32) {
    out[0..8].copy_from_slice(&sector.to_le_bytes());
    out[8..12].copy_from_slice(&nsect.to_le_bytes());
    out[12..16].copy_from_slice(&flags.to_le_bytes());
}

pub fn map_status(st: u8) -> Result<(), BlockError> {
    match st {
        S_OK => Ok(()),
        S_IOERR => Err(BlockError::Io),
        S_UNSUPP => Err(BlockError::Inval),
        _ => Err(BlockError::Io),
    }
}

pub fn nq_from_config(feat: u64, cfg_num_queues: u16, common_num_queues: u16) -> u16 {
    let offered = if feat & F_MQ != 0 {
        cfg_num_queues.max(1)
    } else {
        1
    };
    let hw = common_num_queues.max(1);
    offered.min(hw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::write_marker;
    use crate::virtio::{self, DESC_F_NEXT, DESC_F_WRITE, F_VERSION_1};

    #[test]
    fn spec_constants() {
        assert_eq!(T_IN, 0);
        assert_eq!(T_OUT, 1);
        assert_eq!(T_FLUSH, 4);
        assert_eq!(T_DISCARD, 11);
        assert_eq!(S_OK, 0);
        assert_eq!(S_IOERR, 1);
        assert_eq!(S_UNSUPP, 2);
        assert_eq!(F_BLK_SIZE, 1 << 6);
        assert_eq!(F_FLUSH, 1 << 9);
        assert_eq!(F_TOPOLOGY, 1 << 10);
        assert_eq!(F_MQ, 1 << 12);
        assert_eq!(F_DISCARD, 1 << 13);
        assert_eq!(CFG_CAPACITY, 0);
        assert_eq!(CFG_BLK_SIZE, 20);
        assert_eq!(CFG_TOPOLOGY, 24);
        assert_eq!(CFG_NUM_QUEUES, 34);
        assert_eq!(HDR_LEN, 16);
        assert_eq!(virtio::DEV_BLK_MODERN, 0x1042);
        assert_eq!(virtio::DEV_BLK_LEGACY, 0x1001);
        assert_eq!(DESC_F_NEXT, 1);
        assert_eq!(DESC_F_WRITE, 2);
    }

    #[test]
    fn header_le_packed() {
        let mut h = [0u8; 16];
        pack_header(&mut h, T_OUT, 0x0102_0304_0506_0708);
        assert_eq!(&h[0..4], &[1, 0, 0, 0]);
        assert_eq!(&h[4..8], &[0, 0, 0, 0]);
        assert_eq!(&h[8..16], &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
        let (t, s) = unpack_header(&h);
        assert_eq!(t, T_OUT);
        assert_eq!(s, 0x0102_0304_0506_0708);
        pack_header(&mut h, T_IN, 0);
        assert_eq!(h[0], 0);
        pack_header(&mut h, T_FLUSH, 0);
        assert_eq!(h[0], 4);
        pack_header(&mut h, T_DISCARD, 99);
        assert_eq!(h[0], 11);
        assert_eq!(unpack_header(&h).1, 99);
    }

    #[test]
    fn discard_le_packed() {
        let mut d = [0u8; 16];
        pack_discard(&mut d, 8, 3, 0);
        assert_eq!(&d[0..8], &[8, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&d[8..12], &[3, 0, 0, 0]);
        assert_eq!(&d[12..16], &[0, 0, 0, 0]);
    }

    #[test]
    fn blk_size_default_and_4k() {
        assert_eq!(pick_blk_size(0, 4096), 512);
        assert_eq!(pick_blk_size(F_BLK_SIZE, 0), 512);
        assert_eq!(pick_blk_size(F_BLK_SIZE, 513), 512);
        assert_eq!(pick_blk_size(F_BLK_SIZE, 4096), 4096);
        assert_eq!(pick_blk_size(F_BLK_SIZE, 512), 512);
        assert_eq!(sector_for_lba(3, 4096), Some(24));
        assert_eq!(sector_for_lba(3, 512), Some(3));
        assert_eq!(sector_for_lba(1, 100), None);
        assert_eq!(logical_capacity(1024, 512), Some(1024));
        assert_eq!(logical_capacity(1024, 4096), Some(128));
        assert_eq!(map_status(S_OK), Ok(()));
        assert_eq!(map_status(S_IOERR), Err(BlockError::Io));
        assert_eq!(map_status(S_UNSUPP), Err(BlockError::Inval));
        assert!(BlockError::Io.retryable());
        assert!(!BlockError::Inval.retryable());
    }

    #[test]
    fn version1_required_and_no_ro() {
        assert_eq!(
            pick_features(F_FLUSH | F_BLK_SIZE),
            Err(virtio::VirtioError::NoVersion1)
        );
        let f = pick_features(F_VERSION_1 | F_FLUSH | F_RO | F_MQ | F_DISCARD).unwrap();
        assert_eq!(f & F_VERSION_1, F_VERSION_1);
        assert_eq!(f & F_FLUSH, F_FLUSH);
        assert_eq!(f & F_MQ, F_MQ);
        assert_eq!(f & F_DISCARD, F_DISCARD);
        assert_eq!(f & F_RO, 0);
        assert_eq!(nq_from_config(0, 8, 8), 1);
        assert_eq!(nq_from_config(F_MQ, 4, 8), 4);
        assert_eq!(nq_from_config(F_MQ, 8, 2), 2);
        assert_eq!(nq_from_config(F_MQ, 0, 4), 1);
    }

    #[test]
    fn marker_vda() {
        let mut s = String::new();
        write_marker(&mut s, NAME, 8192).unwrap();
        assert_eq!(s, "vibeOS: block: vda 8192 sectors");
    }
}
