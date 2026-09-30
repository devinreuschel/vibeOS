//! `FakeCfg`: PCI config space decoded from a fuzz input (C-FUZZ).
//!
//! The input is a sequence of records, `bus u8`, `devfn u8`, then 256
//! config bytes; the last record is zero-padded, at most [`MAX_RECORDS`]
//! are read, and a later record replaces an earlier one. `read32` aligns
//! the offset down to 4, as CF8 and ECAM do: below 256 it returns the
//! record, or the overlay once written; from 256 to 4095 it returns 0; past
//! 4095, or for a function with no record, it returns all ones. `write32`
//! stores to the overlay. After an all-ones write to a BAR dword, a read
//! returns `!(low - 1) & !0xF | (raw & 0xF)`, where `low` is the lowest set
//! bit of `raw & !0xF` (0 when there is none), so a BAR sizes to its own
//! alignment.

use std::collections::BTreeMap;

use vibeos::pci::{Bdf, CfgIo};

/// Bytes of one record: bus, devfn, then the 256-byte header.
pub const RECORD: usize = 2 + 256;
/// Records read from one input.
pub const MAX_RECORDS: usize = 64;
/// The BAR dwords, `0x10` to `0x24`.
const BARS: core::ops::RangeInclusive<u16> = 0x10..=0x24;

struct Func {
    cfg: [u8; 256],
    /// Bit `i`: BAR `i` read back its size mask after an all-ones write.
    sizing: u8,
}

pub struct FakeCfg {
    funcs: BTreeMap<(u8, u8), Func>,
}

/// `devfn` of `bdf`: device in bits 7:3, function in bits 2:0.
pub fn devfn(bdf: Bdf) -> u8 {
    ((bdf.device & 0x1F) << 3) | (bdf.function & 7)
}

impl FakeCfg {
    pub fn parse(data: &[u8]) -> Self {
        let mut funcs = BTreeMap::new();
        for rec in data.chunks(RECORD).take(MAX_RECORDS) {
            let mut full = [0u8; RECORD];
            full[..rec.len()].copy_from_slice(rec);
            let mut cfg = [0u8; 256];
            cfg.copy_from_slice(&full[2..]);
            funcs.insert((full[0], full[1]), Func { cfg, sizing: 0 });
        }
        Self { funcs }
    }

    /// Encode `(bus, devfn, header)` records, the seed generator's encoder.
    pub fn encode(records: &[(u8, u8, &[u8; 256])]) -> Vec<u8> {
        let mut out = Vec::with_capacity(records.len() * RECORD);
        for &(bus, df, cfg) in records {
            out.push(bus);
            out.push(df);
            out.extend_from_slice(cfg);
        }
        out
    }
}

fn dword(cfg: &[u8; 256], off: usize) -> u32 {
    u32::from_le_bytes([cfg[off], cfg[off + 1], cfg[off + 2], cfg[off + 3]])
}

/// What a BAR holding `raw` reads back after an all-ones write.
pub fn bar_mask(raw: u32) -> u32 {
    let addr = raw & !0xF;
    let low = if addr == 0 {
        0
    } else {
        1u32 << addr.trailing_zeros()
    };
    (!low.wrapping_sub(1) & !0xF) | (raw & 0xF)
}

fn bar_index(off: u16) -> Option<u8> {
    BARS.contains(&off).then(|| ((off - 0x10) / 4) as u8)
}

impl CfgIo for FakeCfg {
    fn read32(&mut self, bdf: Bdf, offset: u16) -> u32 {
        let off = offset & !3;
        if off > 4095 {
            return 0xFFFF_FFFF;
        }
        let Some(f) = self.funcs.get(&(bdf.bus, devfn(bdf))) else {
            return 0xFFFF_FFFF;
        };
        if off >= 256 {
            return 0;
        }
        let raw = dword(&f.cfg, usize::from(off));
        match bar_index(off) {
            Some(i) if f.sizing & (1 << i) != 0 => bar_mask(raw),
            _ => raw,
        }
    }

    fn write32(&mut self, bdf: Bdf, offset: u16, value: u32) {
        let off = offset & !3;
        let Some(f) = self.funcs.get_mut(&(bdf.bus, devfn(bdf))) else {
            return;
        };
        if off >= 256 {
            return;
        }
        match bar_index(off) {
            Some(i) if value == 0xFFFF_FFFF => {
                f.sizing |= 1 << i;
                return;
            }
            Some(i) => f.sizing &= !(1 << i),
            None => {}
        }
        let o = usize::from(off);
        f.cfg[o..o + 4].copy_from_slice(&value.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vibeos::pci;

    fn header(vendor: u16, bar0: u32) -> [u8; 256] {
        let mut c = [0u8; 256];
        c[0..2].copy_from_slice(&vendor.to_le_bytes());
        c[0x10..0x14].copy_from_slice(&bar0.to_le_bytes());
        c
    }

    #[test]
    fn missing_function_and_far_offsets_read_ones() {
        let h = header(0x1234, 0);
        let mut cfg = FakeCfg::parse(&FakeCfg::encode(&[(0, 0, &h)]));
        assert_eq!(cfg.read32(Bdf::new(0, 1, 0), 0), 0xFFFF_FFFF);
        assert_eq!(cfg.read32(Bdf::new(0, 0, 0), 0x100), 0);
        assert_eq!(cfg.read32(Bdf::new(0, 0, 0), 0xFFC), 0);
        assert_eq!(cfg.read32(Bdf::new(0, 0, 0), 0x1000), 0xFFFF_FFFF);
        assert_eq!(cfg.read32(Bdf::new(0, 0, 0), 0xFFFF), 0xFFFF_FFFF);
    }

    #[test]
    fn offsets_align_down() {
        let mut h = header(0x1234, 0);
        h[2..4].copy_from_slice(&0x5678u16.to_le_bytes());
        let mut cfg = FakeCfg::parse(&FakeCfg::encode(&[(0, 0, &h)]));
        assert_eq!(cfg.read32(Bdf::new(0, 0, 0), 3), 0x5678_1234);
        assert_eq!(pci::read16(&mut cfg, Bdf::new(0, 0, 0), 2), 0x5678);
        assert_eq!(pci::read8(&mut cfg, Bdf::new(0, 0, 0), 0xFF), 0);
    }

    #[test]
    fn later_record_replaces_and_last_is_padded() {
        let a = header(0x1111, 0);
        let b = header(0x2222, 0);
        let mut data = FakeCfg::encode(&[(0, 8, &a), (0, 8, &b)]);
        data.extend_from_slice(&[1, 0, 0x33]);
        let mut cfg = FakeCfg::parse(&data);
        assert_eq!(pci::vendor_id(&mut cfg, Bdf::new(0, 1, 0)), 0x2222);
        assert_eq!(pci::vendor_id(&mut cfg, Bdf::new(1, 0, 0)), 0x0033);
    }

    #[test]
    fn writes_go_to_the_overlay() {
        let h = header(0x1234, 0);
        let mut cfg = FakeCfg::parse(&FakeCfg::encode(&[(0, 0, &h)]));
        cfg.write32(Bdf::new(0, 0, 0), 0x41, 0xDEAD_BEEF);
        assert_eq!(cfg.read32(Bdf::new(0, 0, 0), 0x40), 0xDEAD_BEEF);
        // No record: the write is dropped.
        cfg.write32(Bdf::new(0, 2, 0), 0x40, 1);
        assert_eq!(cfg.read32(Bdf::new(0, 2, 0), 0x40), 0xFFFF_FFFF);
    }

    #[test]
    fn bar_sizes_to_its_alignment() {
        assert_eq!(bar_mask(0xFEB0_0000), 0xFFF0_0000);
        assert_eq!(bar_mask(0xFEB0_000C), 0xFFF0_000C);
        assert_eq!(bar_mask(0xC001), 0xFFFF_C001);
        assert_eq!(bar_mask(0), 0);
        let h = header(0x1234, 0xFEB0_0000);
        let mut cfg = FakeCfg::parse(&FakeCfg::encode(&[(0, 0, &h)]));
        let (bar, wide) = pci::probe_bar(&mut cfg, Bdf::new(0, 0, 0), 0);
        assert!(!wide);
        assert_eq!(bar.kind, pci::BarKind::Mem32);
        assert_eq!((bar.addr, bar.size), (0xFEB0_0000, 0x10_0000));
        // probe_bar restored the BAR, which reads its address again.
        assert_eq!(cfg.read32(Bdf::new(0, 0, 0), 0x10), 0xFEB0_0000);
    }
}
