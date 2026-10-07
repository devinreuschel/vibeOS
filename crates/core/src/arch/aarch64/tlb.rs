//! aarch64 TLB, I-cache, and DMA-barrier encodings (ROADMAP §11.2).
//!
//! The hardware ops live in the kernel. This module names the sequences
//! so host tests can check them without executing `tlbi` / `dmb`.

/// Inner-shareable broadcast TLB maintenance after a PTE write
/// (ROADMAP §11.2, DESIGN §7.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlbOp {
    /// `dsb ishst` after the descriptor store.
    DsbIshst,
    /// Leaf change: `tlbi vale1is`.
    TlbiVale1is,
    /// Table page freed: `tlbi vae1is`.
    TlbiVae1is,
    /// One ASID: `tlbi aside1is`.
    TlbiAside1is,
    /// Local ASID-0 flush when ASIDs are off: `tlbi aside1`.
    TlbiAside1,
    /// Local generation flush: `tlbi vmalle1`.
    TlbiVmalle1,
    /// `dsb ish` after the broadcast TLBI (KVA-free deferral waits here).
    DsbIsh,
    /// `dsb nsh` after a local `vmalle1` (ASID rollover, no broadcast).
    DsbNsh,
    /// `isb` on the issuing core.
    Isb,
}

/// Leaf change: store, then this sequence. Permission-only and
/// global→nG use the same maintenance.
pub const LEAF_INVAL: [TlbOp; 4] = [
    TlbOp::DsbIshst,
    TlbOp::TlbiVale1is,
    TlbOp::DsbIsh,
    TlbOp::Isb,
];

/// Table page freed: `vae1is` instead of `vale1is`.
pub const TABLE_INVAL: [TlbOp; 4] = [
    TlbOp::DsbIshst,
    TlbOp::TlbiVae1is,
    TlbOp::DsbIsh,
    TlbOp::Isb,
];

/// One ASID (not a rollover). A rollover broadcasts nothing.
pub const ASID_INVAL: [TlbOp; 4] = [
    TlbOp::DsbIshst,
    TlbOp::TlbiAside1is,
    TlbOp::DsbIsh,
    TlbOp::Isb,
];

/// Flush-pending CPU before it loads an ASID of the new generation.
pub const ROLLOVER_LOCAL: [TlbOp; 3] = [TlbOp::TlbiVmalle1, TlbOp::DsbNsh, TlbOp::Isb];

/// Break-before-make: invalidate, maintain, write, maintain.
pub const BBM_AFTER_INVALID: [TlbOp; 3] = [TlbOp::DsbIshst, TlbOp::TlbiVale1is, TlbOp::DsbIsh];
pub const BBM_AFTER_MAKE: [TlbOp; 2] = [TlbOp::DsbIshst, TlbOp::Isb];

/// `TLBI VA*E1` / `VA*E1IS` register operand (Arm ARM DDI0487).
///
/// Bits 63:48 are `asid`. Bits 47:44 are the FEAT_TTL hint and stay 0, so
/// hardware does not treat a mismatched granule or level as "invalidate
/// nothing". Bits 43:0 are VA[55:12].
pub const fn tlbi_va_operand(va: u64, asid: u16) -> u64 {
    ((va >> 12) & ((1u64 << 44) - 1)) | ((asid as u64) << 48)
}

/// `dmb oshst` / `dmb oshld` / `dmb osh` (Linux arm64 DMA barriers; F098).
pub const DMA_WMB: &str = "dmb oshst";
pub const DMA_RMB: &str = "dmb oshld";
pub const DMA_MB: &str = "dmb osh";

/// I-cache sync when a user page becomes executable (ROADMAP §11.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IcacheOp {
    DcCvau,
    DsbIsh,
    IcIvau,
    Isb,
}

/// `idc` / `dic` from `CTR_EL0`. ELF load is the first caller.
pub fn icache_sync(idc: bool, dic: bool, buf: &mut [IcacheOp]) -> usize {
    let mut n = 0;
    if !idc && n < buf.len() {
        buf[n] = IcacheOp::DcCvau;
        n += 1;
    }
    if n < buf.len() {
        buf[n] = IcacheOp::DsbIsh;
        n += 1;
    }
    if !dic && n < buf.len() {
        buf[n] = IcacheOp::IcIvau;
        n += 1;
    }
    if n < buf.len() {
        buf[n] = IcacheOp::DsbIsh;
        n += 1;
    }
    if n < buf.len() {
        buf[n] = IcacheOp::Isb;
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vale1is_operand_clears_ttl_and_asid() {
        // 0xFFFF_8000_1234_5000 >> 12 is 0x000F_FFF8_0001_2345: TTL 0b1111
        // (64 KiB, level 3) and ASID 0x000F. The operand keeps VA[55:12].
        let kernel = tlbi_va_operand(0xFFFF_8000_1234_5000, 0);
        assert_eq!(kernel, 0x0000_0FF8_0001_2345);
        assert_eq!(kernel >> 48, 0, "ASID");
        assert_eq!((kernel >> 44) & 0xF, 0, "TTL");

        let top = tlbi_va_operand(0xFFFF_FFFF_FFFF_F000, 0);
        assert_eq!(top, 0x0000_0FFF_FFFF_FFFF);
        assert_eq!(top >> 44, 0, "TTL and ASID");

        let user = tlbi_va_operand(0x0000_0000_0040_2000, 0);
        assert_eq!(user, 0x402);
        assert_eq!(user >> 48, 0, "ASID");
        assert_eq!((user >> 44) & 0xF, 0, "TTL");

        // TBI0: a tag in bits 63:56 is not an ASID or a TTL hint.
        let tagged = tlbi_va_operand(0x7F00_0000_0040_2000, 0);
        assert_eq!(tagged, 0x402);
        assert_eq!(tagged >> 44, 0, "TTL and ASID");
    }

    #[test]
    fn vale1is_operand_ors_asid_above_the_va() {
        let kernel = tlbi_va_operand(0xFFFF_8000_1234_5000, 0x00AB);
        assert_eq!(kernel, 0x00AB_0FF8_0001_2345);
        assert_eq!((kernel >> 44) & 0xF, 0, "TTL");

        let user = tlbi_va_operand(0x0000_0000_0040_2000, 7);
        assert_eq!(user, 0x0007_0000_0000_0402);
        assert_eq!((user >> 44) & 0xF, 0, "TTL");
    }

    #[test]
    fn leaf_sequence_is_broadcast() {
        assert_eq!(LEAF_INVAL[0], TlbOp::DsbIshst);
        assert_eq!(LEAF_INVAL[1], TlbOp::TlbiVale1is);
        assert_eq!(LEAF_INVAL[2], TlbOp::DsbIsh);
        assert_eq!(LEAF_INVAL[3], TlbOp::Isb);
        assert_eq!(TABLE_INVAL[1], TlbOp::TlbiVae1is);
        assert_eq!(ASID_INVAL[1], TlbOp::TlbiAside1is);
        assert_eq!(ROLLOVER_LOCAL[0], TlbOp::TlbiVmalle1);
        assert_ne!(ROLLOVER_LOCAL.as_slice(), LEAF_INVAL.as_slice());
    }

    #[test]
    fn dma_barriers_are_outer_shareable() {
        assert_eq!(DMA_WMB, "dmb oshst");
        assert_eq!(DMA_RMB, "dmb oshld");
        assert_eq!(DMA_MB, "dmb osh");
    }

    #[test]
    fn icache_omits_when_ctr_says_so() {
        let mut buf = [IcacheOp::Isb; 8];
        let n = icache_sync(false, false, &mut buf);
        assert_eq!(
            &buf[..n],
            &[
                IcacheOp::DcCvau,
                IcacheOp::DsbIsh,
                IcacheOp::IcIvau,
                IcacheOp::DsbIsh,
                IcacheOp::Isb
            ]
        );
        let n = icache_sync(true, true, &mut buf);
        assert_eq!(
            &buf[..n],
            &[IcacheOp::DsbIsh, IcacheOp::DsbIsh, IcacheOp::Isb]
        );
    }
}
