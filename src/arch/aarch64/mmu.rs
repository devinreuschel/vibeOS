//! `PageTable` on aarch64: the format from the pure half and TTBR1.

use core::arch::{asm, global_asm};

use vibeos::arch::PageTable;
use vibeos::arch::aarch64::paging;
use vibeos::paging::{PageFlags, PhysAddr, VirtAddr};

use super::{Arch, cpu};

unsafe extern "C" {
    fn vibeos_ttbr1_takeover();
    fn vibeos_ttbr1_takeover_end();
}

// Entered at its physical address, TTBR0 identity-mapping these bytes.
// x0 reserved empty TTBR1, x1 new TTBR1, x2 MAIR, x3 TCR.
// MAIR, TCR, and the empty root become visible together, then the TLB
// drop, then the real root. Fetching stays on TTBR0 for the whole window.
global_asm!(
    ".pushsection .text",
    ".global vibeos_ttbr1_takeover",
    ".type vibeos_ttbr1_takeover, @function",
    "vibeos_ttbr1_takeover:",
    "msr mair_el1, x2",
    "msr tcr_el1, x3",
    "msr ttbr1_el1, x0",
    "isb",
    "tlbi vmalle1",
    "dsb nsh",
    "isb",
    "msr ttbr1_el1, x1",
    "isb",
    "ret",
    ".global vibeos_ttbr1_takeover_end",
    "vibeos_ttbr1_takeover_end:",
    ".popsection",
);

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
        // SAFETY: `root` is a complete TTBR1 that maps this CPU's code,
        // stack and everything it touches next, the `# Safety` contract of
        // `vibeos::arch::PageTable::set_root`; established here by the
        // caller's unsafe call.
        unsafe { takeover_ttbr1(root.as_u64()) };
    }

    #[inline]
    fn flush_local(va: VirtAddr) {
        cpu::tlbi_va(va.as_u64());
    }

    #[inline]
    fn flush_local_all() {
        cpu::tlbi_all();
    }

    #[inline]
    fn bbm_break(va: VirtAddr) {
        cpu::bbm_break(va.as_u64());
    }

    #[inline]
    fn bbm_make() {
        cpu::bbm_make();
    }
}

/// NX is a leaf bit on aarch64; Limine already honours UXN/PXN.
pub fn enable_nx() {}

/// Limine's TTBR0 is 4 KiB, T0SZ 16: a level-0 root, the shape
/// `build_identity_into` writes. Any other TG0 or T0SZ mis-walks it.
fn ttbr0_is_4k_48(tcr: u64) -> bool {
    let tg0 = (tcr >> 14) & 0b11;
    let t0sz = tcr & 0x3F;
    tg0 == 0 && t0sz == 16
}

/// Entry PA and the pages the takeover trampoline occupies.
fn trampoline() -> Option<(u64, [u64; 4], usize)> {
    let start = vibeos_ttbr1_takeover as *const () as u64;
    let end = vibeos_ttbr1_takeover_end as *const () as u64;
    let start_pa = super::secondary::va_to_pa(start)?;
    let last_pa = super::secondary::va_to_pa(end.checked_sub(1)?)?;
    if start_pa >> 48 != 0 || last_pa >> 48 != 0 {
        return None;
    }
    let mut pages = [0u64; 4];
    let mut n = 0usize;
    let mut pa = start_pa & !0xFFF;
    let last = last_pa & !0xFFF;
    loop {
        if n >= pages.len() {
            return None;
        }
        pages[n] = pa;
        n += 1;
        if pa >= last {
            return Some((start_pa, pages, n));
        }
        pa = pa.checked_add(4096)?;
    }
}

/// Install `new_root` as TTBR1 through a reserved empty root.
///
/// # Safety
/// `new_root` maps this CPU's code, stack, and everything the return
/// touches. Boot CPU, IRQs off, still on Limine's tables.
unsafe fn takeover_ttbr1(new_root: u64) {
    let reserved = match super::secondary::alloc_zeroed_page() {
        Some(pa) => pa,
        None => crate::boot::halt_with("vibeOS: paging: no reserved TTBR1"),
    };
    let (tramp, pages, n_pages) = match trampoline() {
        Some(p) => p,
        None => crate::boot::halt_with("vibeOS: paging: takeover trampoline"),
    };
    let mut tables = [0u64; 8];
    let Some((id, empty, _)) =
        super::secondary::build_identity_into(&pages[..n_pages], &mut tables)
    else {
        crate::boot::halt_with("vibeOS: paging: takeover identity");
    };
    if !ttbr0_is_4k_48(cpu::read_tcr()) {
        crate::boot::halt_with("vibeOS: paging: ttbr0 tcr");
    }
    let (mair, tcr) = cpu::mair_tcr();
    // SAFETY: `id` maps `tramp` executable at its PA, `reserved` and
    // `empty` are zeroed tables, and `new_root` maps the return and the
    // stack. established here.
    unsafe {
        asm!(
            "dsb ishst",
            "msr ttbr0_el1, x4",
            "isb",
            // Drop Limine's TTBR0 entries before the identity fetch.
            "tlbi vmalle1",
            "dsb nsh",
            "isb",
            "blr x6",
            "msr ttbr0_el1, x5",
            "isb",
            "tlbi vmalle1",
            "dsb nsh",
            "isb",
            in("x0") reserved,
            in("x1") new_root,
            in("x2") mair,
            in("x3") tcr,
            in("x4") id,
            in("x5") empty,
            in("x6") tramp,
            out("x30") _,
            options(nostack),
        );
    }
}
