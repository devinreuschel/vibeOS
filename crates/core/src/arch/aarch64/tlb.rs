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
    if !idc {
        if n < buf.len() {
            buf[n] = IcacheOp::DcCvau;
            n += 1;
        }
    }
    if n < buf.len() {
        buf[n] = IcacheOp::DsbIsh;
        n += 1;
    }
    if !dic {
        if n < buf.len() {
            buf[n] = IcacheOp::IcIvau;
            n += 1;
        }
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
