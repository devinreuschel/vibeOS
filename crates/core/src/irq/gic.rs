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

/// GICv2m MSI_TYPER and SETSPI_NS (ARM IHI 0069 / GICv2m).
pub const V2M_MSI_TYPER: u32 = 0x008;
pub const V2M_MSI_SETSPI_NS: u32 = 0x040;
const V2M_TYPER_BASE_SHIFT: u32 = 16;
const V2M_TYPER_MASK: u32 = 0x3FF;

/// ITS translation register, from the ITS frame (IHI 0069G).
pub const GITS_TRANSLATER: u32 = 0x1_0040;

/// SPI or PPI INTID from a 3-cell GIC specifier (type, number).
pub const fn gic_intid(ty: u32, num: u32) -> Option<u32> {
    match ty {
        GIC_SPI => num.checked_add(SPI_BASE),
        GIC_PPI => num.checked_add(PPI_BASE),
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
    intid >= SPI_BASE && intid < LPI_BASE
}

pub const fn is_lpi(intid: u32) -> bool {
    intid >= LPI_BASE
}

/// Special (1020–1023) or reserved (1024–8191) INTID. `ICC_IAR1` /
/// `GICC_IAR` can return these; they are not EOI'd or dispatched.
pub const fn is_special(intid: u32) -> bool {
    intid >= SPECIAL_INTID_BASE && intid < LPI_BASE
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
        assert!(is_lpi(8192));
        assert!(is_special(1020));
        assert!(is_special(1023));
        assert!(is_special(8191));
        assert!(!is_special(1019));
        assert!(!is_special(8192));
        assert_eq!(priority_for(SGI_CALL), PRIO_IPI_TICK);
        assert_eq!(priority_for(27), PRIO_IPI_TICK);
        assert_eq!(priority_for(64), PRIO_DEVICE);
        const {
            assert!(PRIO_NMI == 0);
            assert!(PRIO_NMI < PRIO_IPI_TICK);
            assert!(PRIO_IPI_TICK < PRIO_DEVICE);
        }
    }

    #[test]
    fn v2m_and_its_compose() {
        assert_eq!(v2m_typer_spi_base(0x0040_0020), 0x40);
        assert_eq!(v2m_typer_spi_count(0x0040_0020), 0x20);
        assert_eq!(v2m_compose(0x802_0000, 64), (0x802_0040, 64));
        assert_eq!(its_compose(0x808_0000, 3), (0x808_0000 + 0x1_0040, 3));
    }
}
