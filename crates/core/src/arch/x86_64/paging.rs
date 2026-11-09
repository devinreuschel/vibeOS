//! The x86_64 page-table format: the pure half of the `PageTable` seam row
//! (PORTABILITY §11.1). Four levels of 512 eight-byte entries, a 9-bit index
//! per level above the 12-bit page offset, the frame address in bits 12..=51,
//! and a leaf at level 1 or, with `PS` set, at level 2 (2 MiB). The kernel
//! half is the upper 256 slots of the root. `PageFlags` keeps this format's
//! bit values, so its encoding here is the identity on the flag bits.

use crate::log::vmcore::Machine;
use crate::log::vmcore::walk::{CoreFormat, Desc};
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

/// MAXPHYADDR when CPUID.80000008H is absent: the SDM's 36-bit floor, which
/// every 64-bit CPU reports at least.
pub const MAXPHYADDR_FLOOR: u32 = 36;

/// One past the highest physical address a CPU with CPUID.80000008H:EAX[7:0]
/// equal to `bits` can name, clamped to the 52 bits the PTE address field
/// holds. A higher address sets a reserved bit in the table walk, so it is
/// refused rather than mapped. `bits` of 0 is an absent leaf.
pub const fn phys_limit_for_maxphyaddr(bits: u32) -> u64 {
    let bits = if bits < MAXPHYADDR_FLOOR {
        MAXPHYADDR_FLOOR
    } else if bits > 52 {
        52
    } else {
        bits
    };
    1u64 << bits
}

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

/// The core tool's view of this format (ROADMAP §10.7): 4-level tables from
/// CR3's PML4, the kernel half at slot 256 up. `crate::arch` re-exports it.
pub struct X86_64;

impl CoreFormat for X86_64 {
    const MACHINE: Machine = Machine::X86_64;
    const LEVELS: u8 = LEVELS;
    const ENTRIES: usize = PTES_PER_TABLE;
    const KERNEL_ROOT_FIRST: usize = KERNEL_PML4_FIRST;
    const KERNEL_VA_START: u64 = 0xFFFF_8000_0000_0000;
    fn index(va: u64, level: u8) -> usize {
        index(VirtAddr(va), level)
    }
    fn desc(entry: u64, level: u8) -> Desc {
        if entry & PageFlags::PRESENT == 0 {
            Desc::None
        } else if is_leaf(entry, level) {
            Desc::Leaf
        } else {
            Desc::Table
        }
    }
    fn entry_phys(entry: u64) -> u64 {
        pte_phys(entry).as_u64()
    }
    fn entry_flags(entry: u64) -> PageFlags {
        pte_flags(entry)
    }
    fn va_ok(va: u64) -> bool {
        crate::paging::is_canonical(va)
    }
    /// Present, writable and user, as the kernel's mapper links a table.
    fn table_entry(pa: u64) -> u64 {
        make_pte(
            PhysAddr(pa),
            PageFlags(PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER),
        )
    }
    fn leaf_entry(_va: u64, pa: u64, flags: u64) -> u64 {
        make_pte(PhysAddr(pa), PageFlags(flags))
    }
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
    fn phys_limit_follows_maxphyaddr() {
        assert_eq!(phys_limit_for_maxphyaddr(40), 1 << 40);
        assert_eq!(phys_limit_for_maxphyaddr(46), 1 << 46);
        assert_eq!(phys_limit_for_maxphyaddr(52), 1 << 52);
        // An absent leaf, and a field above the 52-bit address field.
        assert_eq!(phys_limit_for_maxphyaddr(0), 1 << 36);
        assert_eq!(phys_limit_for_maxphyaddr(0xFF), 1 << 52);
    }

    #[test]
    fn firmware_addresses_past_maxphyaddr_are_refused() {
        use crate::physmap::phys_range_below;
        let limit = phys_limit_for_maxphyaddr(40);
        // The top page of the 64-bit space, and a garbage HPET-sized base
        // with a non-canonical bit set, both past 1 TiB.
        assert!(!phys_range_below(0xFFFF_FFFF_FFFF_F000, 0x1000, limit));
        assert!(!phys_range_below(0x0100_0000_0000_0000, 0x1000, limit));
        assert!(!phys_range_below(limit - 0x1000, 0x1001, limit));
        assert!(phys_range_below(limit - 0x1000, 0x1000, limit));
        // The LAPIC, I/O APIC and HPET pages sit well under the limit.
        assert!(phys_range_below(0xFEE0_0000, 0x1000, limit));
        assert!(phys_range_below(0xFEC0_0000, 0x1000, limit));
        assert!(phys_range_below(0xFED0_0000, 0x1000, limit));
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
