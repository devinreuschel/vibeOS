//! `PageTable` on x86_64 (PORTABILITY §11.1): the format from the pure half
//! (`vibeos::arch::x86_64::paging`), the root in CR3, and `invlpg`.

use vibeos::arch::PageTable;
use vibeos::arch::x86_64::paging;
use vibeos::paging::{PageFlags, PhysAddr, VirtAddr};

use super::{Arch, cpu};

impl PageTable for Arch {
    const LEVELS: u8 = paging::LEVELS;
    const ENTRIES: usize = paging::PTES_PER_TABLE;
    const KERNEL_ROOT_FIRST: usize = paging::KERNEL_PML4_FIRST;

    #[inline]
    fn index(va: VirtAddr, level: u8) -> usize {
        paging::index(va, level)
    }

    #[inline]
    fn make_entry(pa: PhysAddr, flags: PageFlags) -> u64 {
        paging::make_pte(pa, flags)
    }

    #[inline]
    fn entry_phys(entry: u64) -> PhysAddr {
        paging::pte_phys(entry)
    }

    #[inline]
    fn entry_flags(entry: u64) -> PageFlags {
        paging::pte_flags(entry)
    }

    /// CR3's table address, its PCD, PWT and PCID bits dropped.
    #[inline]
    fn root() -> PhysAddr {
        PhysAddr(cpu::read_cr3() & paging::PTE_ADDR_MASK)
    }

    #[inline]
    unsafe fn set_root(root: PhysAddr) {
        // SAFETY: `root` is a complete PML4 that maps this CPU's code, stack
        // and everything it touches next, the `# Safety` contract of
        // `vibeos::arch::PageTable::set_root`, which covers `write_cr3`'s;
        // established here by the caller's unsafe call.
        unsafe { cpu::write_cr3(root.as_u64()) };
    }

    #[inline]
    fn flush_local(va: VirtAddr) {
        cpu::invlpg(va.as_u64());
    }

    /// A CR3 reload with its own value, which drops every non-global entry.
    #[inline]
    fn flush_local_all() {
        // SAFETY: reloading CR3 with its own value keeps the same tables, so
        // every address this CPU uses stays mapped; established here.
        unsafe { cpu::write_cr3(cpu::read_cr3()) };
    }
}

/// Turn on the NX bit in page-table entries (EFER.NXE), so the kernel's NX
/// leaves are honored rather than read as reserved-bit violations. Once, on
/// the BSP before its first switch to the kernel root (DESIGN §7.3's AP
/// pitfall applies: an AP sets it in the trampoline).
pub fn enable_nx() {
    let efer = cpu::rdmsr(cpu::IA32_EFER);
    if efer & cpu::EFER_NXE == 0 {
        // SAFETY: setting EFER.NXE only enables the NX bit in PTEs; every
        // table live now (Limine's) and the new ones treat NX as intended,
        // established here.
        unsafe { cpu::wrmsr(cpu::IA32_EFER, efer | cpu::EFER_NXE) };
    }
}
