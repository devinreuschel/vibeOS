//! The x86_64 page-table format: the pure half of the `PageTable` seam row
//! (PORTABILITY §11.1). Four levels of 512 eight-byte entries, a 9-bit index
//! per level above the 12-bit page offset, the frame address in bits 12..=51,
//! and a leaf at level 1 or, with `PS` set, at level 2 (2 MiB). The kernel
//! half is the upper 256 slots of the root. `PageFlags` keeps this format's
//! bit values, so its encoding here is the identity on the flag bits.

use crate::paging::{PageFlags, PhysAddr, VirtAddr};

/// Levels of the walk, the root being level 4.
pub const LEVELS: u8 = 4;

/// Entries in one table.
pub const PTES_PER_TABLE: usize = 512;

/// Root slots `KERNEL_PML4_FIRST..PTES_PER_TABLE` are the shared kernel half.
pub const KERNEL_PML4_FIRST: usize = 256;

/// Physical-address mask for a PTE. Bits 12..=51 are the frame address on
/// current hardware; bits 0..12 and 52..63 are flags / reserved / NX.
pub const PTE_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// The index `va` selects in a table at `level` (4 is the root, 1 holds
/// 4 KiB leaves).
#[inline]
pub const fn index(va: VirtAddr, level: u8) -> usize {
    let shift = 12 + 9 * (level as u32 - 1);
    ((va.0 >> shift) & 0x1FF) as usize
}

/// Compose a raw PTE from `(phys, flags)`.
#[inline]
pub const fn make_pte(phys: PhysAddr, flags: PageFlags) -> u64 {
    (phys.0 & PTE_ADDR_MASK) | (flags.0 & !PTE_ADDR_MASK)
}

/// Extract the physical frame address a PTE points at.
#[inline]
pub const fn pte_phys(entry: u64) -> PhysAddr {
    PhysAddr(entry & PTE_ADDR_MASK)
}

/// Extract just the flag bits from a PTE.
#[inline]
pub const fn pte_flags(entry: u64) -> PageFlags {
    PageFlags(entry & !PTE_ADDR_MASK)
}

/// The huge-leaf rule: a present entry at `level` is a leaf when `level` is
/// 1 or its `PS` bit (`PageFlags::HUGE`) is set.
#[inline]
pub const fn is_leaf(entry: u64, level: u8) -> bool {
    level == 1 || entry & PageFlags::HUGE != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexing_matches_hardware_layout() {
        // 0x1_2345_6789_abcd: pick out per-level indices.
        let v = VirtAddr(0x0000_1234_5678_9abc);
        // l1 = bits 12..21, l2 = 21..30, l3 = 30..39, l4 = 39..48.
        assert_eq!(
            index(v, 1),
            ((0x0000_1234_5678_9abcu64 >> 12) & 0x1FF) as usize
        );
        assert_eq!(
            index(v, 2),
            ((0x0000_1234_5678_9abcu64 >> 21) & 0x1FF) as usize
        );
        assert_eq!(
            index(v, 3),
            ((0x0000_1234_5678_9abcu64 >> 30) & 0x1FF) as usize
        );
        assert_eq!(
            index(v, 4),
            ((0x0000_1234_5678_9abcu64 >> 39) & 0x1FF) as usize
        );
    }

    #[test]
    fn make_pte_round_trip() {
        let phys = PhysAddr(0x0000_0000_ABCD_E000);
        let f = PageFlags::present().with(PageFlags::WRITABLE | PageFlags::NX);
        let entry = make_pte(phys, f);
        assert_eq!(pte_phys(entry), phys);
        assert_eq!(pte_flags(entry).0, f.0);
        // NX bit lives at 63 and must survive the round trip.
        assert!(pte_flags(entry).contains(PageFlags::NX));
    }
}
