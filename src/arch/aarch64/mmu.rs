//! `PageTable` on aarch64: the format from the pure half and TTBR1.

use vibeos::arch::PageTable;
use vibeos::arch::aarch64::paging;
use vibeos::paging::{PageFlags, PhysAddr, VirtAddr};

use super::{Arch, cpu};

impl PageTable for Arch {
    const LEVELS: u8 = paging::LEVELS;
    const ENTRIES: usize = paging::PTES_PER_TABLE;
    const KERNEL_ROOT_FIRST: usize = paging::KERNEL_ROOT_FIRST;
    const KERNEL_VA_START: u64 = paging::KERNEL_VA_START;
    const KERNEL_UXN: u64 = paging::PAGE_UXN;

    #[inline]
    fn index(va: VirtAddr, level: u8) -> usize {
        paging::index(va, level)
    }

    #[inline]
    fn make_entry(va: VirtAddr, pa: PhysAddr, flags: PageFlags) -> u64 {
        paging::make_pte(va, pa, flags)
    }

    #[inline]
    fn make_table(pa: PhysAddr) -> u64 {
        paging::make_table(pa)
    }

    #[inline]
    fn va_ok(va: u64) -> bool {
        paging::is_canonical(va)
    }

    #[inline]
    fn is_kernel_va(va: VirtAddr) -> bool {
        paging::is_kernel_va(va)
    }

    #[inline]
    fn entry_phys(entry: u64) -> PhysAddr {
        paging::pte_phys(entry)
    }

    #[inline]
    fn entry_flags(entry: u64) -> PageFlags {
        paging::pte_flags(entry)
    }

    #[inline]
    fn root() -> PhysAddr {
        PhysAddr(cpu::read_ttbr1() & paging::DESC_ADDR_MASK)
    }

    /// TTBR0's table address. A user space is never TTBR1 (`root`).
    #[inline]
    fn user_root() -> PhysAddr {
        PhysAddr(cpu::read_ttbr0() & paging::DESC_ADDR_MASK)
    }

    #[inline]
    unsafe fn set_root(root: PhysAddr) {
        cpu::write_translation_regs();
        // SAFETY: `root` is a complete TTBR1 that maps this CPU's code,
        // stack and everything it touches next, the `# Safety` contract of
        // `vibeos::arch::PageTable::set_root`; established here by the
        // caller's unsafe call.
        unsafe { cpu::write_ttbr1(root.as_u64()) };
        cpu::tlbi_all();
    }

    #[inline]
    fn flush_local(va: VirtAddr) {
        cpu::tlbi_va(va.as_u64());
    }

    #[inline]
    fn flush_local_all() {
        cpu::tlbi_all();
    }
}

/// NX is a leaf bit on aarch64; Limine already honours UXN/PXN.
pub fn enable_nx() {}
