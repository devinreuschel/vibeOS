//! aarch64 page-table format: 4 KiB granule, 48-bit VA, four levels
//! (ROADMAP §11.2, PORTABILITY §11.2).
//!
//! Descriptor layout is the Arm ARM's (DDI0487): bit 0 valid, bit 1 table
//! or page versus block, AttrIndx[4:2], AP[7:6], SH[9:8], AF[10], nG[11],
//! output address, Contiguous[52], PXN[53], UXN[54]. `PageFlags` keeps
//! x86_64's bit values; this module maps them.

use crate::paging::{PageFlags, PhysAddr, VirtAddr};

/// Levels of the walk; the root is level 4 (ARM L0).
pub const LEVELS: u8 = 4;

/// Entries in one table.
pub const PTES_PER_TABLE: usize = 512;

/// A TTBR1 root is all kernel; user space is a separate TTBR0 root.
pub const KERNEL_ROOT_FIRST: usize = 0;

/// Output-address mask: bits 12..=47 (48-bit PA).
pub const DESC_ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;

pub const DESC_VALID: u64 = 1 << 0;
pub const DESC_TABLE: u64 = 1 << 1;
pub const DESC_PAGE: u64 = 1 << 1;
pub const DESC_AF: u64 = 1 << 10;
pub const DESC_NG: u64 = 1 << 11;
pub const DESC_CONTIGUOUS: u64 = 1 << 52;
pub const DESC_PXN: u64 = 1 << 53;
pub const DESC_UXN: u64 = 1 << 54;

const ATTR_SHIFT: u32 = 2;
const AP_SHIFT: u32 = 6;
const SH_SHIFT: u32 = 8;

/// MAIR index 0: Normal write-back.
pub const ATTR_NORMAL_WB: u64 = 0;
/// MAIR index 1: Device-nGnRE.
pub const ATTR_DEVICE: u64 = 1;
/// MAIR index 2: Normal non-cacheable.
pub const ATTR_NORMAL_NC: u64 = 2;

/// Inner Shareable.
pub const SH_INNER: u64 = 0b11;
/// Outer Shareable (Device).
pub const SH_OUTER: u64 = 0b10;

/// Kernel half on a 48-bit TTBR1.
pub const KERNEL_VA_START: u64 = 0xFFFF_0000_0000_0000;

/// UXN in `PageFlags` (bit 54), the same position as the descriptor.
pub const PAGE_UXN: u64 = DESC_UXN;

/// The index `va` selects in a table at `level` (4 is the root).
#[inline]
pub const fn index(va: VirtAddr, level: u8) -> usize {
    let shift = 12 + 9 * (level as u32 - 1);
    ((va.0 >> shift) & 0x1FF) as usize
}

/// A 48-bit VA: bits 63:48 all 0 (TTBR0) or all 1 (TTBR1).
#[inline]
pub const fn is_canonical(va: u64) -> bool {
    let top = va >> 48;
    top == 0 || top == 0xFFFF
}

#[inline]
pub const fn is_kernel_va(va: VirtAddr) -> bool {
    va.0 >= KERNEL_VA_START
}

/// Table descriptor: valid, table, address. No APTable/UXNTable, so the
/// leaf sets permissions (Arm ARM table descriptor).
#[inline]
pub const fn make_table(phys: PhysAddr) -> u64 {
    DESC_VALID | DESC_TABLE | (phys.0 & DESC_ADDR_MASK)
}

/// Leaf or block from `(va, phys, flags)`.
///
/// `PageFlags::HUGE` is a block (bit 1 clear). A 4 KiB leaf is a page
/// (bit 1 set). Kernel-half VAs always get UXN. `NX` is PXN, and UXN as
/// well when the VA is user. AF is always set. PCD+PWT is Device-nGnRE;
/// PWT alone is Normal non-cacheable; otherwise Normal write-back.
#[inline]
pub const fn make_pte(va: VirtAddr, phys: PhysAddr, flags: PageFlags) -> u64 {
    if !flags.contains(PageFlags::PRESENT) {
        return 0;
    }
    let mut d = phys.0 & DESC_ADDR_MASK;
    d |= DESC_VALID | DESC_AF;
    if flags.contains(PageFlags::HUGE) {
        // block: bit 1 stays clear
    } else {
        d |= DESC_PAGE;
    }
    let user = flags.contains(PageFlags::USER);
    let write = flags.contains(PageFlags::WRITABLE);
    let ap = match (user, write) {
        (false, true) => 0b00,
        (true, true) => 0b01,
        (false, false) => 0b10,
        (true, false) => 0b11,
    };
    d |= ap << AP_SHIFT;

    let (attr, sh) = if flags.contains(PageFlags::PCD | PageFlags::PWT) {
        (ATTR_DEVICE, SH_OUTER)
    } else if flags.contains(PageFlags::PWT) {
        (ATTR_NORMAL_NC, SH_INNER)
    } else {
        (ATTR_NORMAL_WB, SH_INNER)
    };
    d |= attr << ATTR_SHIFT;
    d |= sh << SH_SHIFT;

    if !flags.contains(PageFlags::GLOBAL) {
        d |= DESC_NG;
    }
    if flags.contains(PageFlags::CONTIGUOUS) {
        d |= DESC_CONTIGUOUS;
    }

    let nx = flags.contains(PageFlags::NX);
    let kernel = is_kernel_va(va);
    if kernel || nx || flags.contains(PAGE_UXN) {
        d |= DESC_UXN;
    }
    if nx {
        d |= DESC_PXN;
    } else if user {
        d |= DESC_PXN;
    }
    d
}

#[inline]
pub const fn pte_phys(entry: u64) -> PhysAddr {
    PhysAddr(entry & DESC_ADDR_MASK)
}

/// Recover portable `PageFlags` from a descriptor.
#[inline]
pub const fn pte_flags(entry: u64) -> PageFlags {
    if entry & DESC_VALID == 0 {
        return PageFlags::empty();
    }
    let mut f = PageFlags(PageFlags::PRESENT);
    if entry & DESC_PAGE == 0 {
        f = f.with(PageFlags::HUGE);
    }
    let ap = (entry >> AP_SHIFT) & 0b11;
    match ap {
        0b00 => f = f.with(PageFlags::WRITABLE),
        0b01 => f = f.with(PageFlags::WRITABLE | PageFlags::USER),
        0b10 => {}
        _ => f = f.with(PageFlags::USER),
    }
    let attr = (entry >> ATTR_SHIFT) & 0b111;
    if attr == ATTR_DEVICE {
        f = f.with(PageFlags::PCD | PageFlags::PWT);
    } else if attr == ATTR_NORMAL_NC {
        f = f.with(PageFlags::PWT);
    }
    if entry & DESC_NG == 0 {
        f = f.with(PageFlags::GLOBAL);
    }
    if entry & DESC_CONTIGUOUS != 0 {
        f = f.with(PageFlags::CONTIGUOUS);
    }
    if entry & DESC_PXN != 0 && entry & DESC_UXN != 0 {
        f = f.with(PageFlags::NX);
    }
    if entry & DESC_UXN != 0 {
        f = f.with(PAGE_UXN);
    }
    if entry & DESC_AF != 0 {
        f = f.with(PageFlags::ACCESSED);
    }
    f
}

#[inline]
pub const fn is_leaf(entry: u64, level: u8) -> bool {
    entry & DESC_VALID != 0 && (level == 1 || entry & DESC_PAGE == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch::PageTable;
    use crate::paging::{
        FrameAlloc, MapMode, Mapper, PAGE_SIZE_4K, PageSize, kernel_data_flags, kernel_text_flags,
        physmap_flags, user_leaf_flags,
    };
    use crate::pmm::testing::Pool;

    #[test]
    fn indexing_matches_4k_48bit() {
        let v = VirtAddr(0x0000_1234_5678_9abc);
        assert_eq!(
            index(v, 1),
            ((0x0000_1234_5678_9abcu64 >> 12) & 0x1FF) as usize
        );
        assert_eq!(
            index(v, 4),
            ((0x0000_1234_5678_9abcu64 >> 39) & 0x1FF) as usize
        );
        let k = VirtAddr(0xFFFF_0000_0000_0000);
        assert_eq!(index(k, 4), 0);
        let img = VirtAddr(0xFFFF_FFFF_8000_0000);
        assert_eq!(index(img, 4), 511);
    }

    #[test]
    fn canonical_48bit() {
        assert!(is_canonical(0));
        assert!(is_canonical(0x0000_FFFF_FFFF_FFFF));
        assert!(is_canonical(0xFFFF_0000_0000_0000));
        assert!(!is_canonical(0x0001_0000_0000_0000));
        assert!(!is_canonical(0xFFFE_FFFF_FFFF_FFFF));
    }

    #[test]
    fn make_pte_sets_uxn_on_kernel() {
        let va = VirtAddr(0xFFFF_8000_0000_0000);
        let pa = PhysAddr(0x0000_0000_ABCD_E000);
        let text = make_pte(va, pa, kernel_text_flags());
        assert_ne!(text & DESC_UXN, 0);
        assert_eq!(text & DESC_PXN, 0);
        assert_ne!(text & DESC_AF, 0);
        let data = make_pte(va, pa, kernel_data_flags());
        assert_ne!(data & DESC_UXN, 0);
        assert_ne!(data & DESC_PXN, 0);
        let wb = make_pte(va, pa, physmap_flags());
        assert_eq!((wb >> ATTR_SHIFT) & 0b111, ATTR_NORMAL_WB);
        let mmio = make_pte(va, pa, crate::paging::mmio_flags());
        assert_eq!((mmio >> ATTR_SHIFT) & 0b111, ATTR_DEVICE);
    }

    #[test]
    fn user_exec_clears_uxn() {
        let va = VirtAddr(0x0000_0000_0040_0000);
        let pa = PhysAddr(0x1000);
        let exec = make_pte(va, pa, user_leaf_flags(false, true));
        assert_eq!(exec & DESC_UXN, 0);
        assert_ne!(exec & DESC_PXN, 0);
        let nox = make_pte(va, pa, user_leaf_flags(true, false));
        assert_ne!(nox & DESC_UXN, 0);
        assert_ne!(nox & DESC_PXN, 0);
    }

    #[test]
    fn table_descriptor_has_no_perm_bits() {
        let t = make_table(PhysAddr(0x2000));
        assert_eq!(t & DESC_VALID, DESC_VALID);
        assert_eq!(t & DESC_TABLE, DESC_TABLE);
        assert_eq!(t & DESC_UXN, 0);
        assert_eq!(t & (0b11 << AP_SHIFT), 0);
        assert_eq!(pte_phys(t), PhysAddr(0x2000));
    }

    /// Host `PageTable` over this format. Hardware methods are no-ops.
    pub struct TestArch;

    impl PageTable for TestArch {
        const LEVELS: u8 = LEVELS;
        const ENTRIES: usize = PTES_PER_TABLE;
        const KERNEL_ROOT_FIRST: usize = KERNEL_ROOT_FIRST;
        const KERNEL_VA_START: u64 = KERNEL_VA_START;
        const KERNEL_UXN: u64 = PAGE_UXN;

        fn index(va: VirtAddr, level: u8) -> usize {
            index(va, level)
        }
        fn make_entry(va: VirtAddr, pa: PhysAddr, flags: PageFlags) -> u64 {
            make_pte(va, pa, flags)
        }
        fn make_table(pa: PhysAddr) -> u64 {
            make_table(pa)
        }
        fn entry_phys(entry: u64) -> PhysAddr {
            pte_phys(entry)
        }
        fn entry_flags(entry: u64) -> PageFlags {
            pte_flags(entry)
        }
        fn va_ok(va: u64) -> bool {
            is_canonical(va)
        }
        fn is_kernel_va(va: VirtAddr) -> bool {
            super::is_kernel_va(va)
        }
        fn root() -> PhysAddr {
            PhysAddr(0)
        }
        unsafe fn set_root(_root: PhysAddr) {}
        fn flush_local(_va: VirtAddr) {}
        fn flush_local_all() {}
    }

    fn fresh(pool: &mut Pool) -> Mapper<TestArch> {
        let root = PhysAddr(pool.alloc_frame().unwrap().into_entry());
        let ptr = root.0.wrapping_add(pool.hhdm()) as *mut u64;
        for i in 0..PTES_PER_TABLE {
            // SAFETY: `root` is an order-0 pool frame; `i` stays in it.
            unsafe { ptr.add(i).write_volatile(0) };
        }
        // SAFETY: `Mapper::new`'s contract; the zeroed pool frame is the
        // root the mapper keeps, established here.
        unsafe { Mapper::new(root, pool.hhdm()) }
    }

    #[test]
    fn mapper_kernel_leaves_keep_uxn() {
        let mut pool = Pool::new(64);
        let mut m = fresh(&mut pool);
        let cases = [
            (VirtAddr(0xFFFF_0000_0020_0000), kernel_text_flags()),
            (VirtAddr(0xFFFF_8000_0040_0000), kernel_data_flags()),
            (VirtAddr(0xFFFF_0000_0060_0000), physmap_flags()),
        ];
        for (va, flags) in cases {
            // SAFETY: host tables; the leaf frame is never touched.
            unsafe {
                m.map_page(
                    va,
                    PhysAddr(0x1000),
                    flags,
                    PageSize::Size4K,
                    MapMode::Fresh,
                    &mut pool,
                )
                .unwrap();
            }
            let raw = m.leaf_raw(va).expect("mapped");
            assert_ne!(raw & DESC_UXN, 0, "kernel VA {:#x} cleared UXN", va.0);
        }
        let user = VirtAddr(0x0000_0000_0080_0000);
        // SAFETY: as above.
        unsafe {
            m.map_page(
                user,
                PhysAddr(0x2000),
                user_leaf_flags(false, true),
                PageSize::Size4K,
                MapMode::Fresh,
                &mut pool,
            )
            .unwrap();
        }
        let raw = m.leaf_raw(user).unwrap();
        assert_eq!(raw & DESC_UXN, 0);
        assert_eq!(raw & DESC_AF, DESC_AF);
        assert_eq!(raw, raw);
        let _ = PAGE_SIZE_4K;
    }
}
