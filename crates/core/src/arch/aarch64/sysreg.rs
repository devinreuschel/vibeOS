//! Computed aarch64 system-register values (ROADMAP §11.1, §11.2).
//!
//! Each function returns one whole value. The boot CPU writes it once;
//! it never read-modify-writes. #206's secondary stub writes the same
//! numbers. Bits follow the Arm ARM (DDI0487) and Linux arm64's documented
//! EL1 policy: TBI0 set, TBI1 clear, A1 clear, SCTLR.A clear.

/// MAIR_EL1: Attr0 Normal WB, Attr1 Device-nGnRE, Attr2 Normal NC.
pub const fn mair_el1() -> u64 {
    const NORMAL_WB: u64 = 0xFF;
    const DEVICE_NGNRE: u64 = 0x04;
    const NORMAL_NC: u64 = 0x44;
    NORMAL_WB | (DEVICE_NGNRE << 8) | (NORMAL_NC << 16)
}

/// `TCR_EL1` for 4 KiB, 48-bit VA, Inner-shareable WB, TBI0, optional 16-bit ASID.
pub const fn tcr_el1(asid16: bool) -> u64 {
    const T0SZ: u64 = 16;
    const T1SZ: u64 = 16 << 16;
    const IRGN0_WBWA: u64 = 0b01 << 8;
    const ORGN0_WBWA: u64 = 0b01 << 10;
    const SH0_INNER: u64 = 0b11 << 12;
    const TG0_4K: u64 = 0;
    const IRGN1_WBWA: u64 = 0b01 << 24;
    const ORGN1_WBWA: u64 = 0b01 << 26;
    const SH1_INNER: u64 = 0b11 << 28;
    const TG1_4K: u64 = 0b10 << 30;
    const IPS_48: u64 = 0b101 << 32;
    const TBI0: u64 = 1 << 37;
    let as_bit = if asid16 { 1 << 36 } else { 0 };
    T0SZ | T1SZ
        | IRGN0_WBWA
        | ORGN0_WBWA
        | SH0_INNER
        | TG0_4K
        | IRGN1_WBWA
        | ORGN1_WBWA
        | SH1_INNER
        | TG1_4K
        | IPS_48
        | as_bit
        | TBI0
}

/// `SCTLR_EL1`: MMU and caches on, alignment check off, SP alignment on,
/// EL0 cache ops, no WXN. One computed value (ROADMAP §11.2).
pub const fn sctlr_el1() -> u64 {
    const M: u64 = 1 << 0;
    const C: u64 = 1 << 2;
    const SA: u64 = 1 << 3;
    const SA0: u64 = 1 << 4;
    const NAA: u64 = 1 << 6;
    const EOS: u64 = 1 << 11;
    const I: u64 = 1 << 12;
    const DZE: u64 = 1 << 14;
    const UCT: u64 = 1 << 15;
    const NTWI: u64 = 1 << 16;
    const NTWE: u64 = 1 << 18;
    const EIS: u64 = 1 << 22;
    const UCI: u64 = 1 << 26;
    M | C | SA | SA0 | NAA | EOS | I | DZE | UCT | NTWI | NTWE | EIS | UCI
}

/// `CNTKCTL_EL1`: EL0 may read the virtual counter.
pub const fn cntkctl_el1() -> u64 {
    const EL0VCTEN: u64 = 1 << 1;
    EL0VCTEN
}

/// `CNTHCTL_EL2` when entered at EL2: EL1 accesses the physical counter
/// and timer (Arm ARM CNTHCTL_EL2).
pub const fn cnthctl_el2() -> u64 {
    const EL1PCTEN: u64 = 1 << 0;
    const EL1PCEN: u64 = 1 << 1;
    EL1PCTEN | EL1PCEN
}

/// `PMUSERENR_EL0`: no EL0 PMU access.
pub const fn pmuserenr_el0() -> u64 {
    0
}

/// `CPACR_EL1`: FPEN no-trap; ZEN and SMEN no-trap so a later feature
/// check, not a trap, decides SVE/SME.
pub const fn cpacr_el1() -> u64 {
    const ZEN: u64 = 0b11 << 16;
    const FPEN: u64 = 0b11 << 20;
    const SMEN: u64 = 0b11 << 24;
    ZEN | FPEN | SMEN
}

/// `ID_AA64MMFR0_EL1.ASIDBits` field (bits 7:4): 2 means 16-bit ASIDs.
pub const fn asid16_from_mmfr0(mmfr0: u64) -> bool {
    ((mmfr0 >> 4) & 0xF) == 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mair_indices() {
        assert_eq!(mair_el1() & 0xFF, 0xFF);
        assert_eq!((mair_el1() >> 8) & 0xFF, 0x04);
        assert_eq!((mair_el1() >> 16) & 0xFF, 0x44);
    }

    #[test]
    fn tcr_policy() {
        let t = tcr_el1(true);
        assert_eq!(t & 0x3F, 16);
        assert_eq!((t >> 16) & 0x3F, 16);
        assert_ne!(t & (1 << 37), 0, "TBI0");
        assert_eq!(t & (1 << 38), 0, "TBI1");
        assert_eq!(t & (1 << 22), 0, "A1");
        assert_ne!(t & (1 << 36), 0, "AS");
        assert_eq!((t >> 30) & 0b11, 0b10, "TG1 4K");
        assert_eq!(tcr_el1(false) & (1 << 36), 0);
    }

    #[test]
    fn sctlr_is_one_value() {
        let s = sctlr_el1();
        assert_ne!(s & 1, 0, "M");
        assert_eq!(s & (1 << 1), 0, "A");
        assert_ne!(s & (1 << 2), 0, "C");
        assert_ne!(s & (1 << 12), 0, "I");
        assert_eq!(s & (1 << 19), 0, "WXN");
        assert_eq!(s, sctlr_el1());
    }

    #[test]
    fn others_are_whole_values() {
        assert_eq!(cntkctl_el1(), 1 << 1);
        assert_eq!(cnthctl_el2(), 0b11);
        assert_eq!(pmuserenr_el0(), 0);
        assert_eq!(cpacr_el1() >> 16 & 0b11, 0b11);
        assert_eq!(cpacr_el1() >> 20 & 0b11, 0b11);
        assert_eq!(cpacr_el1() >> 24 & 0b11, 0b11);
        assert!(asid16_from_mmfr0(2 << 4));
        assert!(!asid16_from_mmfr0(0));
    }
}
