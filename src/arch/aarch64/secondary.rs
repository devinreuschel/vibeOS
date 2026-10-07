//! PSCI secondary entry: identity MMU, #202 sysregs, arrival (ROADMAP §11.4).

use core::arch::{asm, global_asm};

use vibeos::arch::aarch64::paging::{self, make_pte, make_table};
use vibeos::arch::aarch64::psci::{self, SecondaryParam};
use vibeos::arch::aarch64::sysreg;
use vibeos::kalloc::TryBox;
use vibeos::paging::{PageFlags, PhysAddr, VirtAddr};

use crate::pmm_init;

unsafe extern "C" {
    pub(super) fn vibeos_secondary_entry();
    fn vibeos_secondary_end();
    fn vibeos_secondary_continue();
}

global_asm!(
    ".pushsection .text",
    ".align 12",
    ".global vibeos_secondary_entry",
    ".type vibeos_secondary_entry, @function",
    "vibeos_secondary_entry:",
    // x0 = param PA (PSCI context_id). No exclusive, no shared write.
    "    mov x19, #{arrived}",
    "    mrs x1, ID_AA64ISAR0_EL1",
    "    ubfx x1, x1, #20, #4",
    "    cmp x1, #2",
    "    b.ge 1f",
    "    mov x19, #{feature}",
    "1:",
    "    mrs x1, ID_AA64MMFR1_EL1",
    "    ubfx x1, x1, #20, #4",
    "    cbnz x1, 2f",
    "    mov x19, #{feature}",
    "2:",
    "    ldr x1, [x0, #{el2}]",
    "    cbz x1, 3f",
    "    ldr x1, [x0, #{hcr}]",
    "    msr hcr_el2, x1",
    "    isb",
    "    ldr x1, [x0, #{cptr}]",
    "    msr cptr_el2, x1",
    "    ldr x1, [x0, #{cnthctl}]",
    "    msr cnthctl_el2, x1",
    "    ldr x1, [x0, #{hstr}]",
    "    msr hstr_el2, x1",
    "    ldr x1, [x0, #{mdcr}]",
    "    msr mdcr_el2, x1",
    "    ldr x1, [x0, #{sre}]",
    "    msr icc_sre_el2, x1",
    "    ldr x1, [x0, #{hcrx_ok}]",
    "    cbz x1, 21f",
    "    ldr x1, [x0, #{hcrx}]",
    "    msr s3_4_c1_c2_2, x1",
    "21:",
    "    ldr x1, [x0, #{fgt_ok}]",
    "    cbz x1, 22f",
    "    ldr x1, [x0, #{hfgrtr}]",
    "    msr s3_4_c1_c1_4, x1",
    "    ldr x1, [x0, #{hfgwtr}]",
    "    msr s3_4_c1_c1_5, x1",
    "    ldr x1, [x0, #{hfgitr}]",
    "    msr s3_4_c1_c1_6, x1",
    "    ldr x1, [x0, #{hdfgrtr}]",
    "    msr s3_4_c3_c1_4, x1",
    "    ldr x1, [x0, #{hdfgwtr}]",
    "    msr s3_4_c3_c1_5, x1",
    "    ldr x1, [x0, #{hafgrtr}]",
    "    msr s3_4_c3_c1_6, x1",
    "22:",
    "    msr cntvoff_el2, xzr",
    "    isb",
    "    ldr x1, [x0, #{sctlr_el2}]",
    "    msr sctlr_el2, x1",
    "3:",
    // CNTKCTL_EL1 at EL1. At EL2, CNTHCTL_EL2 already holds that value
    // (E2H=1: EL0VCTEN, EL0PCTEN clear), so skip this msr.
    "    ldr x1, [x0, #{el2}]",
    "    cbnz x1, 31f",
    "    ldr x1, [x0, #{cntkctl}]",
    "    msr cntkctl_el1, x1",
    "31:",
    "    ldr x1, [x0, #{pmuserenr}]",
    "    msr pmuserenr_el0, x1",
    "    ldr x1, [x0, #{cpacr}]",
    "    msr cpacr_el1, x1",
    "    ldr x1, [x0, #{mair}]",
    "    msr mair_el1, x1",
    "    ldr x1, [x0, #{tcr}]",
    "    msr tcr_el1, x1",
    "    ldr x1, [x0, #{ttbr0_id}]",
    "    msr ttbr0_el1, x1",
    "    ldr x1, [x0, #{ttbr1}]",
    "    msr ttbr1_el1, x1",
    "    isb",
    "    ldr x1, [x0, #{sctlr}]",
    "    msr sctlr_el1, x1",
    "    isb",
    "    ldr x1, [x0, #{cont}]",
    "    br x1",
    ".global vibeos_secondary_continue",
    "vibeos_secondary_continue:",
    "    ldr x5, [x0, #{param_va}]",
    "    ldr x2, [x0, #{stack}]",
    "    ldr x3, [x0, #{entry}]",
    "    ldr x4, [x0, #{cpu}]",
    "    ldr x1, [x0, #{ttbr0_empty}]",
    "    msr ttbr0_el1, x1",
    "    isb",
    "    tlbi vmalle1",
    "    dsb nsh",
    "    isb",
    "    str x19, [x5, #{status}]",
    "    dsb ish",
    "    cmp x19, #{feature}",
    "    b.ne 4f",
    "    ldr x1, [x5, #{hvc}]",
    "    movz x0, #{cpu_off_lo}",
    "    movk x0, #{cpu_off_hi}, lsl #16",
    "    cbnz x1, 5f",
    "    smc #0",
    "    b 6f",
    "5:",
    "    hvc #0",
    "6:",
    "    wfi",
    "    b 6b",
    "4:",
    "    mov sp, x2",
    "    mov x0, x4",
    "    br x3",
    ".global vibeos_secondary_end",
    "vibeos_secondary_end:",
    ".popsection",
    arrived = const psci::STATUS_ARRIVED,
    feature = const psci::STATUS_FEATURE,
    cpu_off_lo = const (psci::CPU_OFF & 0xFFFF),
    cpu_off_hi = const ((psci::CPU_OFF >> 16) & 0xFFFF),
    el2 = const SecondaryParam::EL2,
    hcr = const SecondaryParam::HCR_EL2,
    cptr = const SecondaryParam::CPTR_EL2,
    cnthctl = const SecondaryParam::CNTHCTL_EL2,
    hstr = const SecondaryParam::HSTR_EL2,
    mdcr = const SecondaryParam::MDCR_EL2,
    sre = const SecondaryParam::ICC_SRE_EL2,
    hcrx = const SecondaryParam::HCRX_EL2,
    hcrx_ok = const SecondaryParam::HCRX_VALID,
    fgt_ok = const SecondaryParam::FGT_VALID,
    hfgrtr = const SecondaryParam::HFGRTR_EL2,
    hfgwtr = const SecondaryParam::HFGWTR_EL2,
    hfgitr = const SecondaryParam::HFGITR_EL2,
    hdfgrtr = const SecondaryParam::HDFGRTR_EL2,
    hdfgwtr = const SecondaryParam::HDFGWTR_EL2,
    hafgrtr = const SecondaryParam::HAFGRTR_EL2,
    sctlr_el2 = const SecondaryParam::SCTLR_EL2,
    cntkctl = const SecondaryParam::CNTKCTL,
    pmuserenr = const SecondaryParam::PMUSERENR,
    cpacr = const SecondaryParam::CPACR,
    mair = const SecondaryParam::MAIR,
    tcr = const SecondaryParam::TCR,
    ttbr0_id = const SecondaryParam::TTBR0_ID,
    ttbr1 = const SecondaryParam::TTBR1,
    sctlr = const SecondaryParam::SCTLR,
    cont = const SecondaryParam::CONTINUE_VA,
    param_va = const SecondaryParam::PARAM_VA,
    stack = const SecondaryParam::STACK_TOP,
    entry = const SecondaryParam::ENTRY_VA,
    cpu = const SecondaryParam::CPU_PTR,
    ttbr0_empty = const SecondaryParam::TTBR0_EMPTY,
    status = const SecondaryParam::STATUS,
    hvc = const SecondaryParam::CONDUIT_HVC,
);

/// Physical address of the stub entry.
pub fn entry_pa() -> Option<u64> {
    va_to_pa(vibeos_secondary_entry as *const () as u64)
}

pub fn stub_range() -> Option<(u64, usize)> {
    let start = vibeos_secondary_entry as *const () as u64;
    let end = vibeos_secondary_end as *const () as u64;
    let len = end.checked_sub(start)? as usize;
    Some((start, len.max(1)))
}

pub fn continue_va() -> u64 {
    vibeos_secondary_continue as *const () as u64
}

pub fn va_to_pa(va: u64) -> Option<u64> {
    let vma = crate::boot::kernel_vma_start();
    let info = crate::boot::info();
    if va >= vma {
        return Some(va.wrapping_sub(vma).wrapping_add(info.kernel_phys.start));
    }
    let hhdm = crate::paging_init::hhdm_offset();
    if va >= hhdm {
        return Some(va.wrapping_sub(hhdm));
    }
    Some(va)
}

fn alloc_zeroed_page() -> Option<u64> {
    let f = pmm_init::with_buddy(|b| b.alloc(0))?;
    let pa = f.into_entry();
    let va = crate::paging_init::hhdm_offset().wrapping_add(pa);
    // SAFETY: buddy page, HHDM maps it (I14). established here.
    unsafe { core::ptr::write_bytes(va as *mut u8, 0, 4096) };
    Some(pa)
}

fn table_slot(root_pa: u64, va: u64, level: u8) -> Option<*mut u64> {
    let hhdm = crate::paging_init::hhdm_offset();
    let idx = paging::index(VirtAddr(va), level);
    let va = hhdm.wrapping_add(root_pa);
    // SAFETY: `root_pa` is a table page we allocated. established here.
    unsafe { (va as *mut u64).add(idx).as_mut().map(|p| p as *mut u64) }
}

/// Map `va` → `pa` as a 4 KiB leaf in `l0_pa` (TTBR0) with `flags`.
pub fn map_va(
    l0_pa: u64,
    va: u64,
    pa: u64,
    tables: &mut TryBox<[u64; 8]>,
    n: &mut usize,
    flags: PageFlags,
) -> bool {
    let mut table = l0_pa;
    let mut level = 4u8;
    while level > 1 {
        let Some(slot) = table_slot(table, va, level) else {
            return false;
        };
        // SAFETY: slot in a table we own. established here.
        let cur = unsafe { slot.read() };
        if cur & paging::DESC_VALID == 0 {
            let Some(next) = alloc_zeroed_page() else {
                return false;
            };
            if *n < tables.len() {
                tables[*n] = next;
                *n += 1;
            }
            // SAFETY: slot in a table we own. established here.
            unsafe { slot.write(make_table(PhysAddr(next))) };
            table = next;
        } else {
            table = cur & paging::DESC_ADDR_MASK;
        }
        level -= 1;
    }
    let Some(slot) = table_slot(table, va, 1) else {
        return false;
    };
    // SAFETY: leaf slot we own. established here.
    unsafe { slot.write(make_pte(VirtAddr(va), PhysAddr(pa), flags)) };
    true
}

/// Identity-map `pa` as a 4 KiB RW global page in `l0_pa` (TTBR0).
pub fn map_identity(l0_pa: u64, pa: u64, tables: &mut TryBox<[u64; 8]>, n: &mut usize) -> bool {
    map_va(
        l0_pa,
        pa,
        pa,
        tables,
        n,
        PageFlags::empty()
            .with(PageFlags::PRESENT)
            .with(PageFlags::WRITABLE)
            .with(PageFlags::GLOBAL),
    )
}

/// Build a TTBR0 identity root covering `pas`, plus `empty` TTBR0.
pub fn build_identity(pas: &[u64]) -> Option<(u64, u64, TryBox<[u64; 8]>)> {
    let l0 = alloc_zeroed_page()?;
    let empty = alloc_zeroed_page()?;
    let mut tables = TryBox::try_new([0u64; 8]).ok()?;
    let mut n = 0usize;
    tables[n] = l0;
    n += 1;
    for &pa in pas {
        let page = pa & !0xFFFu64;
        if !map_identity(l0, page, &mut tables, &mut n) {
            return None;
        }
    }
    let mut i = 0;
    while i < n {
        let pa = tables[i];
        if pa != 0 && !map_identity(l0, pa, &mut tables, &mut n) {
            return None;
        }
        i += 1;
    }
    Some((l0, empty, tables))
}

/// `dc cvac` over `[va, va+len)`, then `dsb sy`.
pub fn clean_poc(va: u64, len: usize) {
    let start = va & !63;
    let end = va.saturating_add(len as u64);
    let mut p = start;
    while p < end {
        // SAFETY: clean a cache line of kernel memory we own to PoC.
        // established here.
        unsafe { asm!("dc cvac, {0}", in(reg) p, options(nostack, preserves_flags)) };
        p = p.saturating_add(64);
    }
    // SAFETY: `dsb sy` after the cleans. established here.
    unsafe { asm!("dsb sy", options(nostack, preserves_flags)) };
}

pub fn fill_computed(p: &mut SecondaryParam) {
    p.sctlr = sysreg::sctlr_el1();
    p.cntkctl = sysreg::cntkctl_el1();
    p.pmuserenr = sysreg::pmuserenr_el0();
    p.cpacr = sysreg::cpacr_el1();
    // The stub writes CNTHCTL_EL2 only after setting E2H, when bits 0 and 1
    // are EL0PCTEN and EL0VCTEN. capture_el2 stores this same value.
    p.cnthctl_el2 = sysreg::cntkctl_el1();
    let asid16 = {
        let mmfr0: u64;
        // SAFETY: ID register. established here.
        unsafe {
            asm!("mrs {0}, ID_AA64MMFR0_EL1", out(reg) mmfr0, options(nomem, nostack, preserves_flags))
        };
        sysreg::asid16_from_mmfr0(mmfr0)
    };
    p.tcr = sysreg::tcr_el1(asid16);
    p.mair = sysreg::mair_el1();
}

/// Capture the boot CPU's EL2 controls into `p`.
pub fn capture_el2(p: &mut SecondaryParam) {
    if !super::cpu::el2_vhe() {
        p.el2 = 0;
        return;
    }
    p.el2 = 1;
    // SAFETY: readable at EL2. established here.
    unsafe {
        asm!("mrs {0}, hcr_el2", out(reg) p.hcr_el2, options(nomem, nostack, preserves_flags));
        asm!("mrs {0}, cptr_el2", out(reg) p.cptr_el2, options(nomem, nostack, preserves_flags));
        asm!("mrs {0}, hstr_el2", out(reg) p.hstr_el2, options(nomem, nostack, preserves_flags));
        asm!("mrs {0}, mdcr_el2", out(reg) p.mdcr_el2, options(nomem, nostack, preserves_flags));
        asm!("mrs {0}, icc_sre_el2", out(reg) p.icc_sre_el2, options(nomem, nostack, preserves_flags));
        asm!("mrs {0}, sctlr_el2", out(reg) p.sctlr_el2, options(nomem, nostack, preserves_flags));
    }
    // E2H is 1, so bits 0 and 1 are EL0PCTEN and EL0VCTEN.
    p.cnthctl_el2 = sysreg::cntkctl_el1();
    let mmfr0: u64;
    let mmfr1: u64;
    // SAFETY: ID registers. established here.
    unsafe {
        asm!("mrs {0}, ID_AA64MMFR0_EL1", out(reg) mmfr0, options(nomem, nostack, preserves_flags));
        asm!("mrs {0}, ID_AA64MMFR1_EL1", out(reg) mmfr1, options(nomem, nostack, preserves_flags));
    }
    if (mmfr1 >> 40) & 0xF != 0 {
        p.hcrx_valid = 1;
        // SAFETY: FEAT_HCX. established here.
        unsafe {
            asm!("mrs {0}, s3_4_c1_c2_2", out(reg) p.hcrx_el2, options(nomem, nostack, preserves_flags));
        }
    }
    if (mmfr0 >> 56) & 0xF != 0 {
        p.fgt_valid = 1;
        // SAFETY: FEAT_FGT. established here.
        unsafe {
            asm!("mrs {0}, s3_4_c1_c1_4", out(reg) p.hfgrtr_el2, options(nomem, nostack, preserves_flags));
            asm!("mrs {0}, s3_4_c1_c1_5", out(reg) p.hfgwtr_el2, options(nomem, nostack, preserves_flags));
            asm!("mrs {0}, s3_4_c1_c1_6", out(reg) p.hfgitr_el2, options(nomem, nostack, preserves_flags));
            asm!("mrs {0}, s3_4_c3_c1_4", out(reg) p.hdfgrtr_el2, options(nomem, nostack, preserves_flags));
            asm!("mrs {0}, s3_4_c3_c1_5", out(reg) p.hdfgwtr_el2, options(nomem, nostack, preserves_flags));
            asm!("mrs {0}, s3_4_c3_c1_6", out(reg) p.hafgrtr_el2, options(nomem, nostack, preserves_flags));
        }
    }
}

/// Read the listed registers for the in-guest compare.
#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(dead_code, reason = "in-guest sysreg_compare (kernel_tests)")
)]
pub fn snapshot_sysregs() -> [u64; 8] {
    let mut v = [0u64; 8];
    // SAFETY: EL1 (VHE-redirected) system registers. established here.
    unsafe {
        asm!("mrs {0}, sctlr_el1", out(reg) v[0], options(nomem, nostack, preserves_flags));
        asm!("mrs {0}, tcr_el1", out(reg) v[1], options(nomem, nostack, preserves_flags));
        asm!("mrs {0}, mair_el1", out(reg) v[2], options(nomem, nostack, preserves_flags));
        asm!("mrs {0}, cpacr_el1", out(reg) v[3], options(nomem, nostack, preserves_flags));
        asm!("mrs {0}, cntkctl_el1", out(reg) v[4], options(nomem, nostack, preserves_flags));
        if super::cpu::el2_vhe() {
            asm!("mrs {0}, vbar_el2", out(reg) v[5], options(nomem, nostack, preserves_flags));
        } else {
            asm!("mrs {0}, vbar_el1", out(reg) v[5], options(nomem, nostack, preserves_flags));
        }
        asm!("mrs {0}, ttbr1_el1", out(reg) v[6], options(nomem, nostack, preserves_flags));
        asm!("mrs {0}, oslsr_el1", out(reg) v[7], options(nomem, nostack, preserves_flags));
    }
    v
}
