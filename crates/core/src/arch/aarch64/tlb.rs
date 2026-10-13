//! aarch64 TLB, I-cache, and DMA-barrier encodings (ROADMAP §11.2).
//!
//! The hardware ops live in the kernel, which runs these arrays in order
//! (`tlbi_va`, `tlbi_all`, `bbm_break`, `bbm_make`). Host tests check that
//! order without executing `tlbi` / `dmb`.

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
    /// Broadcast all stage-1 EL1 entries: `tlbi vmalle1is`.
    TlbiVmalle1is,
    /// `dsb ish` after the broadcast TLBI (KVA-free deferral waits here).
    DsbIsh,
    /// `dsb nsh` after a local `vmalle1` (ASID rollover, no broadcast).
    DsbNsh,
    /// `isb` on the issuing core.
    Isb,
}

impl TlbOp {
    /// Instruction this op emits (Arm ARM DDI0487). The kernel's `exec_tlb`
    /// uses the same text.
    pub const fn mnemonic(self) -> &'static str {
        match self {
            Self::DsbIshst => "dsb ishst",
            Self::TlbiVale1is => "tlbi vale1is",
            Self::TlbiVae1is => "tlbi vae1is",
            Self::TlbiAside1is => "tlbi aside1is",
            Self::TlbiAside1 => "tlbi aside1",
            Self::TlbiVmalle1 => "tlbi vmalle1",
            Self::TlbiVmalle1is => "tlbi vmalle1is",
            Self::DsbIsh => "dsb ish",
            Self::DsbNsh => "dsb nsh",
            Self::Isb => "isb",
        }
    }
}

/// Leaf change: the kernel's `tlbi_va` runs this after the descriptor
/// store. Permission-only and global→nG use the same maintenance.
pub const LEAF_INVAL: [TlbOp; 4] = [
    TlbOp::DsbIshst,
    TlbOp::TlbiVale1is,
    TlbOp::DsbIsh,
    TlbOp::Isb,
];

/// Full EL1 invalidation: the kernel's `tlbi_all` runs this.
pub const ALL_INVAL: [TlbOp; 4] = [
    TlbOp::DsbIshst,
    TlbOp::TlbiVmalle1is,
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

/// After a TTBR0 write, with the `isb` that follows it. Local only: every
/// user root is ASID 0 until ASIDs are wired, and a CPU's TLB holds user
/// entries only from roots it loaded, each flushed when it switched away.
/// The broadcast `vmalle1is` would also drop global kernel entries on every
/// CPU. The leading `dsb ishst` orders the PTE stores made before the switch
/// ahead of the local invalidate. The kernel's `tlbi_asid0_local` runs this.
pub const TTBR0_SWITCH: [TlbOp; 4] = [
    TlbOp::DsbIshst,
    TlbOp::TlbiAside1,
    TlbOp::DsbNsh,
    TlbOp::Isb,
];

/// Break-before-make, after the invalid store and before the new one.
/// The kernel's `bbm_break` runs this.
pub const BBM_AFTER_INVALID: [TlbOp; 3] = [TlbOp::DsbIshst, TlbOp::TlbiVale1is, TlbOp::DsbIsh];
/// Break-before-make, after the new store. The kernel's `bbm_make` runs this.
pub const BBM_AFTER_MAKE: [TlbOp; 2] = [TlbOp::DsbIshst, TlbOp::Isb];
/// A new table's zeroed words, before the descriptor that links it. The
/// kernel's `publish_table` runs this (Arm ARM DDI0487: a table walk is not
/// ordered after a plain store).
pub const PUBLISH_TABLE: [TlbOp; 1] = [TlbOp::DsbIshst];

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

/// One step of the I-cache sync a user page needs once it becomes
/// executable (ROADMAP §11.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IcacheOp {
    /// `dc cvau, <va>`, one D-cache line.
    DcCvau,
    DsbIsh,
    /// `ic ivau, <va>`, one I-cache line.
    IcIvau,
    Isb,
}

/// The op order for `CTR_EL0.IDC` / `DIC`: a bit set drops its step.
/// Returns the count written to `buf`.
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

/// What `CTR_EL0` says about code and data cache maintenance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheInfo {
    /// `IDC`: no D-cache clean needed to make writes visible to fetch.
    pub idc: bool,
    /// `DIC`: no I-cache invalidate needed.
    pub dic: bool,
    /// Smallest D-cache line, bytes (`DminLine`).
    pub dline: u64,
    /// Smallest I-cache line, bytes (`IminLine`).
    pub iline: u64,
}

impl CacheInfo {
    /// Decode `CTR_EL0` (DDI0487): `IminLine` 3:0 and `DminLine` 19:16 are
    /// log2 of the line in words, `IDC` is bit 28, `DIC` bit 29.
    pub const fn from_ctr(ctr: u64) -> Self {
        Self {
            idc: ctr & (1 << 28) != 0,
            dic: ctr & (1 << 29) != 0,
            dline: 4 << ((ctr >> 16) & 0xF),
            iline: 4 << (ctr & 0xF),
        }
    }
}

/// Every `line`-byte line (a power of two) that `[start, start+len)`
/// touches, as its aligned start address, once each.
pub fn lines(start: u64, len: u64, line: u64) -> impl Iterator<Item = u64> {
    let first = start & !(line - 1);
    let end = if len == 0 {
        first
    } else {
        start.saturating_add(len)
    };
    (first..end).step_by(line as usize)
}

/// Run the sync over `[start, start+len)`: `run(op, va)` for each step, in
/// order, `va` the line for `DcCvau` and `IcIvau` and 0 for a barrier. The
/// kernel's `sync_icache` runs the instruction for each.
pub fn sync_range(info: CacheInfo, start: u64, len: u64, mut run: impl FnMut(IcacheOp, u64)) {
    let mut ops = [IcacheOp::Isb; 5];
    let n = icache_sync(info.idc, info.dic, &mut ops);
    for &op in &ops[..n] {
        match op {
            IcacheOp::DcCvau => lines(start, len, info.dline).for_each(|va| run(op, va)),
            IcacheOp::IcIvau => lines(start, len, info.iline).for_each(|va| run(op, va)),
            IcacheOp::DsbIsh | IcacheOp::Isb => run(op, 0),
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

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
    fn emitted_sequences_match_arm_arm() {
        // DDI0487 leaf maintenance, and DESIGN §7.9: DSB ISHST, TLBI, DSB ISH, ISB.
        assert_eq!(
            LEAF_INVAL.map(TlbOp::mnemonic),
            ["dsb ishst", "tlbi vale1is", "dsb ish", "isb"]
        );
        assert_eq!(
            ALL_INVAL.map(TlbOp::mnemonic),
            ["dsb ishst", "tlbi vmalle1is", "dsb ish", "isb"]
        );
        // Break-before-make (DDI0487, ROADMAP §11.2): invalid store, the
        // first half, new store, the second half.
        assert_eq!(
            BBM_AFTER_INVALID.map(TlbOp::mnemonic),
            ["dsb ishst", "tlbi vale1is", "dsb ish"]
        );
        assert_eq!(BBM_AFTER_MAKE.map(TlbOp::mnemonic), ["dsb ishst", "isb"]);
        assert_eq!(PUBLISH_TABLE.map(TlbOp::mnemonic), ["dsb ishst"]);
    }

    #[test]
    fn ttbr0_switch_is_local_asid0() {
        assert_eq!(
            TTBR0_SWITCH.map(TlbOp::mnemonic),
            ["dsb ishst", "tlbi aside1", "dsb nsh", "isb"]
        );
        // ASID 0 is the operand's bits 63:48, with no VA.
        assert_eq!(tlbi_va_operand(0, 0), 0);
        assert!(!TTBR0_SWITCH.contains(&TlbOp::TlbiAside1is));
        assert!(!TTBR0_SWITCH.contains(&TlbOp::TlbiVmalle1is));
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

    fn recorded(info: CacheInfo, start: u64, len: u64) -> std::vec::Vec<(IcacheOp, u64)> {
        let mut v = std::vec::Vec::new();
        sync_range(info, start, len, |op, va| v.push((op, va)));
        v
    }

    #[test]
    fn ctr_decodes_lines_and_bits() {
        let c = CacheInfo::from_ctr(0x0004_0004 | (1 << 28));
        assert_eq!(
            c,
            CacheInfo {
                idc: true,
                dic: false,
                dline: 64,
                iline: 64
            }
        );
        let c = CacheInfo::from_ctr(0x0003_0002 | (1 << 29));
        assert_eq!((c.idc, c.dic, c.dline, c.iline), (false, true, 32, 16));
        // QEMU max reads 0x8444c004: DminLine 4, IminLine 4, no IDC/DIC.
        let c = CacheInfo::from_ctr(0x8444_C004);
        assert_eq!((c.idc, c.dic, c.dline, c.iline), (false, false, 64, 64));
        // Reset value: 4-byte lines, the conservative floor.
        assert_eq!(CacheInfo::from_ctr(0).dline, 4);
    }

    #[test]
    fn lines_cover_the_range_once_aligned_down() {
        for line in [4u64, 16, 64, 128] {
            for (start, len) in [
                (0x1000u64, 0x1000u64),
                (0x1000, 1),
                (0x1000 + line - 1, 1),
                (0x1000 + line - 1, 2),
                (0x1003, 0x1000),
                (0x1000, line),
                (0x1000, line + 1),
            ] {
                let got: std::vec::Vec<u64> = lines(start, len, line).collect();
                let first = start & !(line - 1);
                let count = (start + len - first).div_ceil(line);
                assert_eq!(got.len() as u64, count, "line {line} {start:#x}+{len:#x}");
                for (i, &va) in got.iter().enumerate() {
                    assert_eq!(va, first + i as u64 * line);
                    assert_eq!(va % line, 0);
                }
                assert!(*got.last().unwrap() < start + len);
                assert!(got[0] <= start && start < got[0] + line);
            }
        }
        assert_eq!(lines(0x1000, 0, 64).count(), 0);
        // A range ending at the top of the address space does not wrap.
        let top = u64::MAX - 63;
        assert_eq!(lines(top, 64, 64).collect::<std::vec::Vec<_>>(), [top]);
    }

    #[test]
    fn sync_range_runs_the_box_sequence_per_line() {
        let info = CacheInfo {
            idc: false,
            dic: false,
            dline: 64,
            iline: 16,
        };
        let r = recorded(info, 0x2000, 0x1000);
        let dc = r.iter().filter(|e| e.0 == IcacheOp::DcCvau).count();
        let ic = r.iter().filter(|e| e.0 == IcacheOp::IcIvau).count();
        assert_eq!((dc, ic), (64, 256));
        // dc cvau, dsb ish, ic ivau, dsb ish, isb, in that order.
        let mut kinds = std::vec::Vec::new();
        for &(op, _) in &r {
            if kinds.last() != Some(&op) {
                kinds.push(op);
            }
        }
        assert_eq!(
            kinds,
            [
                IcacheOp::DcCvau,
                IcacheOp::DsbIsh,
                IcacheOp::IcIvau,
                IcacheOp::DsbIsh,
                IcacheOp::Isb
            ]
        );
        assert_eq!(r[0], (IcacheOp::DcCvau, 0x2000));
        assert_eq!(r[63], (IcacheOp::DcCvau, 0x2000 + 63 * 64));
        assert_eq!(r[65], (IcacheOp::IcIvau, 0x2000));
    }

    #[test]
    fn sync_range_skips_per_idc_and_dic() {
        let mut info = CacheInfo {
            idc: true,
            dic: false,
            dline: 64,
            iline: 64,
        };
        let r = recorded(info, 0x2000, 0x1000);
        assert!(r.iter().all(|e| e.0 != IcacheOp::DcCvau));
        assert_eq!(r.iter().filter(|e| e.0 == IcacheOp::IcIvau).count(), 64);
        info.idc = false;
        info.dic = true;
        let r = recorded(info, 0x2000, 0x1000);
        assert!(r.iter().all(|e| e.0 != IcacheOp::IcIvau));
        assert_eq!(r.iter().filter(|e| e.0 == IcacheOp::DcCvau).count(), 64);
        info.idc = true;
        let r = recorded(info, 0x2000, 0x1000);
        assert_eq!(
            r,
            [
                (IcacheOp::DsbIsh, 0),
                (IcacheOp::DsbIsh, 0),
                (IcacheOp::Isb, 0)
            ]
        );
    }
}
