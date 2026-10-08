//! GIC INTID map, priorities, and GICv2m encodings (DESIGN §5.3, §5.4, §11.5).
//!
//! Builders and constants only. The kernel half programs the distributor.

use crate::irq::chip::IrqSpecifier;

/// Device-tree interrupt type cell: SPI.
pub const GIC_SPI: u32 = 0;
/// Device-tree interrupt type cell: PPI.
pub const GIC_PPI: u32 = 1;

pub const SGI_BASE: u32 = 0;
pub const SGI_COUNT: u32 = 16;
pub const PPI_BASE: u32 = 16;
pub const SPI_BASE: u32 = 32;
/// First special INTID (IHI 0069). 1020–1023 are special (1023 is spurious);
/// 1024–8191 are reserved. LPIs start at [`LPI_BASE`].
pub const SPECIAL_INTID_BASE: u32 = 1020;
pub const LPI_BASE: u32 = 8192;

/// SGIs the kernel uses (DESIGN §5.3). 8–15 stay with the Secure world.
pub const SGI_RESCHEDULE: u32 = 0;
pub const SGI_CALL: u32 = 1;
pub const SGI_STOP: u32 = 2;

/// Priority classes: lower value is higher priority (DESIGN §5.3).
pub const PRIO_NMI: u8 = 0x00;
pub const PRIO_IPI_TICK: u8 = 0x40;
pub const PRIO_DEVICE: u8 = 0xa0;

/// LPI Configuration table bits (IHI 0069). Priority occupies [7:2].
pub const LPI_PROP_ENABLE: u8 = 1;
pub const LPI_PROP_GROUP1: u8 = 1 << 1;

/// One LPI Configuration table byte: Group 1, enabled, `priority` in [7:2].
pub const fn lpi_config(priority: u8) -> u8 {
    (priority & 0xFC) | LPI_PROP_GROUP1 | LPI_PROP_ENABLE
}

/// Byte offset of `intid` in the LPI Configuration table: INTID minus
/// [`LPI_BASE`]. The table starts at LPI 8192, not INTID 0 (IHI 0069).
pub const fn lpi_prop_index(intid: u32) -> Option<usize> {
    match intid.checked_sub(LPI_BASE) {
        Some(i) => Some(i as usize),
        None => None,
    }
}

/// GICv2m MSI_TYPER and SETSPI_NS (ARM IHI 0069 / GICv2m).
pub const V2M_MSI_TYPER: u32 = 0x008;
pub const V2M_MSI_SETSPI_NS: u32 = 0x040;
const V2M_TYPER_BASE_SHIFT: u32 = 16;
const V2M_TYPER_MASK: u32 = 0x3FF;

/// ITS translation register, from the ITS frame (IHI 0069G).
pub const GITS_TRANSLATER: u32 = 0x1_0040;

/// `GICD_IROUTER<n>` base. Offset is `0x6000 + 8*n` with `n` the INTID
/// (IHI 0069). `n` 0–31 are reserved; the first SPI (INTID 32) sits at
/// `0x6100`.
pub const GICD_IROUTER: u64 = 0x6000;

/// Byte offset of `GICD_IROUTER<intid>` from the distributor base.
#[must_use]
pub const fn gicd_irouter(intid: u32) -> u64 {
    GICD_IROUTER.saturating_add((intid as u64).saturating_mul(8))
}

/// SPI or PPI INTID from a 3-cell GIC specifier (type, number).
pub const fn gic_intid(ty: u32, num: u32) -> Option<u32> {
    match ty {
        GIC_SPI => match num.checked_add(SPI_BASE) {
            Some(intid) if is_spi(intid) => Some(intid),
            _ => None,
        },
        GIC_PPI => match num.checked_add(PPI_BASE) {
            Some(intid) if is_ppi(intid) => Some(intid),
            _ => None,
        },
        _ => None,
    }
}

/// [`IrqSpecifier`] for a device-tree GIC interrupt.
pub const fn gic_spec(ty: u32, num: u32) -> Option<IrqSpecifier> {
    match gic_intid(ty, num) {
        Some(intid) => Some(IrqSpecifier::Gic { intid }),
        None => None,
    }
}

pub const fn is_sgi(intid: u32) -> bool {
    intid < SGI_COUNT
}

pub const fn is_ppi(intid: u32) -> bool {
    intid >= PPI_BASE && intid < SPI_BASE
}

pub const fn is_spi(intid: u32) -> bool {
    intid >= SPI_BASE && intid < SPECIAL_INTID_BASE
}

pub const fn is_lpi(intid: u32) -> bool {
    intid >= LPI_BASE
}

/// Special (1020–1023) or reserved (1024–8191) INTID. `ICC_IAR1` /
/// `GICC_IAR` can return these; they are not EOI'd or dispatched.
pub const fn is_special(intid: u32) -> bool {
    intid >= SPECIAL_INTID_BASE && intid < LPI_BASE
}

/// After IAR: drop with no EOI and no distributor write (IHI 0069).
pub const fn ack_drops(intid: u32) -> bool {
    is_special(intid)
}

/// `GICC_IAR` / `GICC_EOIR` INTID field (IHI 0048B §4.4.4 bits [9:0]).
const V2_IAR_INTID_MASK: u32 = 0x3FF;

/// A GICv2 `GICC_IAR` read, split for dispatch and EOI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct V2Ack {
    /// INTID, bits [9:0]. Dispatch, `is_sgi`, and [`ack_drops`] use this.
    pub intid: u32,
    /// Value for `GICC_EOIR`: the raw IAR, including an SGI's source CPUID.
    pub eoi: u32,
}

/// Split a raw GICv2 `GICC_IAR` (IHI 0048B §4.4.4, §4.4.5).
///
/// Bits [9:0] are the INTID. `GICC_EOIR` is written with the raw value:
/// an SGI's source CPUID is bits [12:10], and an EOI that drops them
/// does not complete.
#[must_use]
pub const fn v2_ack(raw_iar: u32) -> V2Ack {
    V2Ack {
        intid: raw_iar & V2_IAR_INTID_MASK,
        eoi: raw_iar,
    }
}

/// Priority for an INTID: NMI reserved, then SGI/tick, then devices.
pub const fn priority_for(intid: u32) -> u8 {
    if is_sgi(intid) || is_ppi(intid) {
        PRIO_IPI_TICK
    } else {
        PRIO_DEVICE
    }
}

pub const fn v2m_typer_spi_base(typer: u32) -> u32 {
    (typer >> V2M_TYPER_BASE_SHIFT) & V2M_TYPER_MASK
}

pub const fn v2m_typer_spi_count(typer: u32) -> u32 {
    typer & V2M_TYPER_MASK
}

/// MSI address and data for a GICv2m SPI (SETSPI_NS).
pub fn v2m_compose(frame: u64, spi: u32) -> (u64, u32) {
    (frame.saturating_add(u64::from(V2M_MSI_SETSPI_NS)), spi)
}

/// MSI address and data for an ITS EventID.
pub fn its_compose(its_base: u64, event_id: u32) -> (u64, u32) {
    (
        its_base.saturating_add(u64::from(GITS_TRANSLATER)),
        event_id,
    )
}

/// Aff0 ≥ 16 while `ICC_CTLR_EL1.RSS` is clear (ARM ARM).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum SgiError {
    Aff0,
}

impl SgiError {
    pub const fn as_str(self) -> &'static str {
        match self {
            SgiError::Aff0 => "aff0 needs ICC_CTLR_EL1.RSS",
        }
    }
}

impl From<SgiError> for crate::kerror::KError {
    fn from(e: SgiError) -> Self {
        match e {
            SgiError::Aff0 => Self::Inval,
        }
    }
}

/// `ICC_SGI1R_EL1` for one PE (IHI 0069, ARM ARM `ICC_SGI1R_EL1`).
///
/// `rss` is `ICC_CTLR_EL1.RSS` (bit 18). With it, RS (bits [47:44]) is
/// `Aff0 >> 4` and TargetList is bit `Aff0 & 15`. Without it RS is RES0,
/// and Aff0 ≥ 16 is [`SgiError::Aff0`]: discovery leaves that CPU offline.
pub fn sgi1r(intid: u32, mpidr: u64, rss: bool) -> Result<u64, SgiError> {
    let aff0 = mpidr & 0xFF;
    let aff1 = (mpidr >> 8) & 0xFF;
    let aff2 = (mpidr >> 16) & 0xFF;
    let aff3 = (mpidr >> 32) & 0xFF;
    // `bit` stays in 0..16, so the TargetList shift cannot overflow.
    let (rs, bit) = if rss {
        (aff0 >> 4, aff0 & 15)
    } else if aff0 < 16 {
        (0, aff0)
    } else {
        return Err(SgiError::Aff0);
    };
    Ok((u64::from(intid & 0xF) << 24)
        | (aff1 << 16)
        | (aff2 << 32)
        | (aff3 << 48)
        | (rs << 44)
        | (1u64 << bit))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intid_map() {
        assert_eq!(gic_intid(GIC_SPI, 0), Some(32));
        assert_eq!(gic_intid(GIC_PPI, 11), Some(27));
        assert_eq!(gic_intid(2, 0), None);
        assert_eq!(gic_spec(GIC_PPI, 11), Some(IrqSpecifier::Gic { intid: 27 }));
        assert!(is_sgi(SGI_RESCHEDULE));
        assert!(is_ppi(27));
        assert!(is_spi(32));
        assert!(is_spi(1019));
        assert!(!is_spi(31));
        assert!(!is_spi(1020));
        assert!(!is_spi(1023));
        assert!(!is_spi(1024));
        assert!(!is_spi(8191));
        assert!(!is_spi(8192));
        assert!(is_lpi(8192));
        assert!(is_special(1020));
        assert!(is_special(1023));
        assert!(is_special(8191));
        assert!(!is_special(1019));
        assert!(!is_special(8192));
        assert!(ack_drops(1020));
        assert!(ack_drops(1023));
        assert!(ack_drops(8191));
        assert!(!ack_drops(0));
        assert!(!ack_drops(32));
        assert!(!ack_drops(1019));
        assert!(!ack_drops(8192));
        assert_eq!(gic_intid(GIC_SPI, 987), Some(1019));
        assert_eq!(gic_intid(GIC_SPI, 988), None);
        assert_eq!(gic_intid(GIC_PPI, 15), Some(31));
        assert_eq!(gic_intid(GIC_PPI, 16), None);
        const {
            assert!(!is_spi(SPECIAL_INTID_BASE));
            assert!(is_special(1023));
            assert!(ack_drops(1023));
            assert!(!ack_drops(32));
        }
        assert_eq!(priority_for(SGI_CALL), PRIO_IPI_TICK);
        assert_eq!(priority_for(27), PRIO_IPI_TICK);
        assert_eq!(priority_for(64), PRIO_DEVICE);
        assert_eq!(
            lpi_config(PRIO_DEVICE),
            0xA0 | LPI_PROP_GROUP1 | LPI_PROP_ENABLE
        );
        assert_eq!(lpi_config(PRIO_DEVICE) & 1, 1);
        assert_eq!(lpi_config(PRIO_DEVICE) & 2, 2);
        assert_eq!(lpi_prop_index(LPI_BASE), Some(0));
        assert_eq!(lpi_prop_index(LPI_BASE + 3), Some(3));
        assert_eq!(lpi_prop_index(LPI_BASE - 1), None);
        assert_eq!(gicd_irouter(32), 0x6100);
        assert_eq!(gicd_irouter(48), 0x6180);
        assert_eq!(gicd_irouter(80), 0x6280);
        const {
            assert!(PRIO_NMI == 0);
            assert!(PRIO_NMI < PRIO_IPI_TICK);
            assert!(PRIO_IPI_TICK < PRIO_DEVICE);
        }
    }

    #[test]
    fn v2_ack_eoi_is_the_raw_iar() {
        // IHI 0048B §4.4.4: INTID is bits [9:0], an SGI's source CPUID is
        // bits [12:10]. §4.4.5: GICC_EOIR must carry that CPUID.
        // SGI 1 (call function) from CPU 5: 1 | (5 << 10).
        let a = v2_ack(0x1401);
        assert_eq!(a.intid, 1);
        assert_eq!(a.eoi, 0x1401);
        assert!(is_sgi(a.intid));
        // The raw word sits in the reserved INTID range. Dispatching it
        // would drop the SGI with no EOI.
        assert!(ack_drops(0x1401));
        assert!(!ack_drops(a.intid));

        // SGI 0 from CPU 7, the top of the CPUID field.
        let a = v2_ack(0x1C00);
        assert_eq!(a.intid, 0);
        assert_eq!(a.eoi, 0x1C00);
        assert!(is_sgi(a.intid));
        assert!(ack_drops(0x1C00));
        assert!(!ack_drops(a.intid));

        // CPU 0: the CPUID field is zero, so the raw word is the INTID.
        let a = v2_ack(2);
        assert_eq!(a.intid, 2);
        assert_eq!(a.eoi, 2);

        // SPI and spurious. CPUID is RAZ; EOI is still the raw read.
        let a = v2_ack(32);
        assert_eq!(a.intid, 32);
        assert_eq!(a.eoi, 32);
        assert!(!ack_drops(a.intid));
        let a = v2_ack(1023);
        assert_eq!(a.intid, 1023);
        assert_eq!(a.eoi, 1023);
        assert!(ack_drops(a.intid));

        // Reserved IAR bits ride along. EOI writes the register as read.
        let a = v2_ack(0x8000_1401);
        assert_eq!(a.intid, 1);
        assert_eq!(a.eoi, 0x8000_1401);
    }

    #[test]
    fn sgi1r_aff0() {
        // ICC_SGI1R_EL1 with RSS: RS = Aff0[7:4] in bits [47:44],
        // TargetList bit Aff0[3:0]. Literals, not the encoder's formula.
        let cases = [
            (0u64, 0x0000_0000_0000_0001u64),
            (15, 0x0000_0000_0000_8000),
            (16, 0x0000_1000_0000_0001),
            (63, 0x0000_3000_0000_8000),
            (255, 0x0000_F000_0000_8000),
        ];
        for (aff0, enc) in cases {
            assert_eq!(sgi1r(0, aff0, true), Ok(enc), "aff0 {aff0}");
        }
        // Other affinity levels and the INTID stay in their fields.
        let mpidr = (0xAAu64 << 32) | (0xBB << 16) | (0xCC << 8) | 16;
        assert_eq!(sgi1r(7, mpidr, true), Ok(0x00AA_10BB_07CC_0001));
        assert_eq!(sgi1r(0, 0, false), Ok(0x1));
        assert_eq!(sgi1r(0, 15, false), Ok(0x8000));
        assert_eq!(sgi1r(0, 16, false), Err(SgiError::Aff0));
        assert_eq!(sgi1r(0, 63, false), Err(SgiError::Aff0));
        assert_eq!(sgi1r(0, 255, false), Err(SgiError::Aff0));
    }

    #[test]
    fn v2m_and_its_compose() {
        assert_eq!(v2m_typer_spi_base(0x0040_0020), 0x40);
        assert_eq!(v2m_typer_spi_count(0x0040_0020), 0x20);
        assert_eq!(v2m_compose(0x802_0000, 64), (0x802_0040, 64));
        assert_eq!(its_compose(0x808_0000, 3), (0x808_0000 + 0x1_0040, 3));
    }
}
