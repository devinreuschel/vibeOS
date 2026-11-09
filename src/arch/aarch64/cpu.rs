//! aarch64 CPU primitives: DAIF, halt, `wfi`, system registers.
//!
//! DAIF writes omit `nomem` so they are compiler barriers (ROADMAP §10.3,
//! F091). They never rewrite `SCTLR_EL1`, `CNTKCTL_EL1`, `PMUSERENR_EL0`,
//! or `CPACR` (#202 wrote those once).

use core::arch::asm;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

use vibeos::arch::aarch64::{sysreg, tlb};
use vibeos::log::Level;

use crate::cell::BootCell;

/// PSTATE.I (IRQs).
const DAIF_I: u64 = 1 << 7;

static EL2_VHE: BootCell<bool> = BootCell::new();
/// Every noted CPU's `CTR_EL0` as one ([`tlb::ctr_meet`]).
static CTR: AtomicU64 = AtomicU64::new(tlb::CTR_MEET_TOP);
/// A CPU has met its `CTR_EL0` into [`CTR`].
static CTR_NOTED: AtomicBool = AtomicBool::new(false);
static ISA_OK: AtomicBool = AtomicBool::new(false);
static OVERFLOW_SP: AtomicU64 = AtomicU64::new(0);

/// Whether entry was EL2 with VHE (`HCR_EL2.{E2H,TGE}`).
pub fn el2_vhe() -> bool {
    EL2_VHE.try_get().copied().unwrap_or(false)
}

pub fn current_el() -> u8 {
    let v: u64;
    // SAFETY: `CurrentEL` is always readable at EL1/EL2; established here.
    unsafe { asm!("mrs {0}, CurrentEL", out(reg) v, options(nomem, nostack, preserves_flags)) };
    ((v >> 2) & 3) as u8
}

/// `ID_AA64PFR0_EL1`, readable at EL1 and at EL2 under VHE.
pub fn id_aa64pfr0_el1() -> u64 {
    let v: u64;
    // SAFETY: ID_AA64PFR0_EL1 is readable at EL1 and at EL2 under VHE; established here.
    unsafe {
        asm!(
            "mrs {0}, ID_AA64PFR0_EL1",
            out(reg) v,
            options(nomem, nostack, preserves_flags)
        );
    }
    v
}

/// `ID_AA64DFR0_EL1`, readable at EL1 and at EL2 under VHE.
pub fn id_aa64dfr0_el1() -> u64 {
    let v: u64;
    // SAFETY: ID_AA64DFR0_EL1 is readable at EL1 and at EL2 under VHE; established here.
    unsafe {
        asm!(
            "mrs {0}, ID_AA64DFR0_EL1",
            out(reg) v,
            options(nomem, nostack, preserves_flags)
        );
    }
    v
}

fn hcr_el2() -> u64 {
    let v: u64;
    // SAFETY: readable at EL2; callers check `current_el`. established here.
    unsafe { asm!("mrs {0}, hcr_el2", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// Record the exception level Limine chose. Never changes it.
pub fn note_exception_level() {
    let el = current_el();
    let vhe = el == 2 && {
        let h = hcr_el2();
        h & ((1 << 34) | (1 << 27)) == (1 << 34) | (1 << 27)
    };
    // SAFETY: I22, one write on the BSP before SMP; established here.
    unsafe { EL2_VHE.set(vhe) };
    if vhe {
        crate::marker!("vibeOS: el: 2 vhe");
    } else {
        crate::marker!("vibeOS: el: {el}");
    }
}

/// Meet this CPU's `CTR_EL0` into what [`sync_icache`] uses. Each CPU
/// runs it before it can run user code, so a core with smaller lines or an
/// aliasing I-cache is covered on a part whose cores differ.
pub fn note_cache() {
    let ctr: u64;
    // SAFETY: `CTR_EL0` is readable at EL1; established here.
    unsafe { asm!("mrs {0}, CTR_EL0", out(reg) ctr, options(nomem, nostack, preserves_flags)) };
    // Acquire: pairs with the AcqRel exchange below on another CPU.
    let mut cur = CTR.load(Ordering::Acquire);
    loop {
        // AcqRel: pairs with the Acquire load in `sync_icache`, and with
        // another CPU's exchange here.
        match CTR.compare_exchange_weak(
            cur,
            tlb::ctr_meet(cur, ctr),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => break,
            Err(seen) => cur = seen,
        }
    }
    // Release: pairs with the Acquire load in `sync_icache`.
    CTR_NOTED.store(true, Ordering::Release);
    // A non-coherent device's sub-page DMA region is sized in granules
    // inside page-sized blocks (DESIGN §4.7): a CPU whose `CTR_EL0.CWG`
    // claims more than a page would let a writeback reach past the block.
    assert!(
        tlb::CacheInfo::from_ctr(ctr).granule_fits_page(vibeos::pmm::PAGE_SIZE),
        "vibeOS: CTR_EL0.CWG above a page"
    );
}

/// What every noted CPU's `CTR_EL0` says: [`CTR`] once a CPU has met into
/// it, and `CTR_EL0` 0 (the smallest lines, the largest granule) before.
fn cache_info() -> tlb::CacheInfo {
    // Acquire: pairs with the Release store of `CTR_NOTED` in `note_cache`.
    if CTR_NOTED.load(Ordering::Acquire) {
        // Acquire: pairs with the AcqRel update of `CTR` in `note_cache`.
        tlb::CacheInfo::from_ctr(CTR.load(Ordering::Acquire))
    } else {
        tlb::CacheInfo::from_ctr(0)
    }
}

/// The cache writeback granule every noted CPU's `CTR_EL0.CWG` allows,
/// the largest of them (`tlb::ctr_meet`); `tlb::MAX_CWG` before
/// [`note_cache`].
pub fn dma_granule() -> usize {
    cache_info().cwg as usize
}

/// Run one DMA maintenance step (`tlb::DmaOp`) at `va`.
///
/// # Safety
/// `va` is a line of mapped memory the caller owns for DMA, and for
/// `DcIvac` no CPU store to that line is pending.
unsafe fn exec_dma(op: tlb::DmaOp, va: u64) {
    // SAFETY: EL1 data-cache maintenance by VA on a line of the caller's
    // mapped DMA range, this fn's `# Safety`; established by
    // `arch::aarch64::cpu::dma_clean` and `dma_inval`.
    unsafe {
        match op {
            tlb::DmaOp::DcCvac => {
                asm!("dc cvac, {0}", in(reg) va, options(nostack, preserves_flags))
            }
            tlb::DmaOp::DcCivac => {
                asm!("dc civac, {0}", in(reg) va, options(nostack, preserves_flags))
            }
            tlb::DmaOp::DcIvac => {
                asm!("dc ivac, {0}", in(reg) va, options(nostack, preserves_flags))
            }
            tlb::DmaOp::DsbSy => asm!("dsb sy", options(nostack, preserves_flags)),
        }
    }
}

/// Clean `[start, start+len)` to the Point of Coherency for a non-coherent
/// device: `tlb::dma_clean` over every noted CPU's smallest D-cache line.
///
/// # Safety
/// `[start, start+len)` is mapped memory the caller owns for DMA.
pub unsafe fn dma_clean(start: u64, len: u64) {
    tlb::dma_clean(cache_info(), start, len, |op, va| {
        // SAFETY: `va` is a line of the range this fn's contract names;
        // established by `arch::aarch64::cpu::dma_clean`.
        unsafe { exec_dma(op, va) }
    });
}

/// Invalidate `[start, start+len)` for the CPU after a non-coherent
/// device wrote it: `tlb::dma_inval`, which cleans a line the range
/// covers only in part.
///
/// # Safety
/// `[start, start+len)` is mapped memory the caller owns for DMA, and no
/// CPU store to it is pending that the invalidate would discard.
pub unsafe fn dma_inval(start: u64, len: u64) {
    tlb::dma_inval(cache_info(), start, len, |op, va| {
        // SAFETY: `va` is a line of the range this fn's contract names; a
        // whole line inside it holds no pending CPU store; established by
        // `arch::aarch64::cpu::dma_inval`.
        unsafe { exec_dma(op, va) }
    });
}

/// Make `[start, start+len)`, written through the data side, fetchable by
/// every CPU. Runs `tlb::sync_range` over `CTR_EL0`'s line sizes; before
/// [`note_cache`] it assumes the smallest lines and no IDC or DIC.
///
/// # Safety
/// `[start, start+len)` is mapped, readable memory.
pub unsafe fn sync_icache(start: u64, len: u64) {
    tlb::sync_range(cache_info(), start, len, |op, va| {
        // SAFETY: EL1 cache maintenance by VA; `va` is a line of the mapped
        // range `sync_icache`'s contract names, established by
        // `arch::aarch64::cpu::sync_icache`.
        unsafe {
            match op {
                tlb::IcacheOp::DcCvau => {
                    asm!("dc cvau, {0}", in(reg) va, options(nostack, preserves_flags))
                }
                tlb::IcacheOp::IcIvau => {
                    asm!("ic ivau, {0}", in(reg) va, options(nostack, preserves_flags))
                }
                tlb::IcacheOp::IcIalluis => asm!("ic ialluis", options(nostack, preserves_flags)),
                tlb::IcacheOp::DsbIsh => asm!("dsb ish", options(nostack, preserves_flags)),
                tlb::IcacheOp::Isb => asm!("isb", options(nostack, preserves_flags)),
            }
        }
    });
}

/// `(ID_AA64ISAR0_EL1, ID_AA64MMFR1_EL1)`, readable at EL1 and EL2.
fn isa_ids() -> (u64, u64) {
    let isar0: u64;
    let mmfr1: u64;
    // SAFETY: ID registers are readable at EL1/EL2; established here.
    unsafe {
        asm!("mrs {0}, ID_AA64ISAR0_EL1", out(reg) isar0, options(nomem, nostack, preserves_flags));
        asm!("mrs {0}, ID_AA64MMFR1_EL1", out(reg) mmfr1, options(nomem, nostack, preserves_flags));
    }
    (isar0, mmfr1)
}

/// ISA floor: FEAT_LSE (`Atomic >= 2`) and FEAT_PAN (`PAN != 0`). Runs
/// before the first log line: the kernel builds with `+lse`, so the log
/// path's compare-and-swap is an LSE instruction that a CPU without LSE
/// faults on. A refusal prints through `serial::raw` and halts.
pub fn check_isa_floor() {
    let (isar0, mmfr1) = isa_ids();
    let atomic = (isar0 >> 20) & 0xF;
    let pan = (mmfr1 >> 20) & 0xF;
    if atomic < 2 || pan == 0 {
        crate::serial::raw::put_boot_line(b"vibeOS: cpu: isa floor (lse+pan) missing");
        halt();
    }
    // Release: pairs with the Acquire load in `isa_ok`.
    ISA_OK.store(true, Ordering::Release);
}

/// The ISA info line, after the log path is up and [`check_isa_floor`] passed.
pub fn log_isa_floor() {
    let (isar0, mmfr1) = isa_ids();
    crate::klog!(
        Level::Info,
        "vibeOS: cpu: isar0={isar0:#x} mmfr1={mmfr1:#x} lse=1 pan=1"
    );
}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn isa_ok() -> bool {
    // Acquire: pairs with the Release store in `check_isa_floor`.
    ISA_OK.load(Ordering::Acquire)
}

#[inline]
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn read_sysreg(name_hint: u64) -> u64 {
    let _ = name_hint;
    0
}

pub fn read_ttbr1() -> u64 {
    let v: u64;
    // SAFETY: TTBR1_EL1 is readable at EL1 and at EL2 with VHE; established here.
    unsafe {
        asm!("mrs {0}, ttbr1_el1", out(reg) v, options(nomem, nostack, preserves_flags));
    }
    v
}

/// Computed `MAIR_EL1` and `TCR_EL1`. The TTBR1 takeover writes them.
pub fn mair_tcr() -> (u64, u64) {
    let mmfr0: u64;
    // SAFETY: ID register; established here.
    unsafe {
        asm!("mrs {0}, ID_AA64MMFR0_EL1", out(reg) mmfr0, options(nomem, nostack, preserves_flags));
    }
    (
        sysreg::mair_el1(),
        sysreg::tcr_el1(sysreg::asid16_from_mmfr0(mmfr0)),
    )
}

pub fn read_tcr() -> u64 {
    let v: u64;
    // SAFETY: TCR_EL1 is readable at EL1 and at EL2 with VHE; established here.
    unsafe {
        asm!("mrs {0}, tcr_el1", out(reg) v, options(nomem, nostack, preserves_flags));
    }
    v
}

/// Run one op from a `tlb` sequence. The instruction text is `TlbOp::mnemonic`.
fn exec_tlb(op: tlb::TlbOp, va_operand: u64) {
    // SAFETY: EL1 (VHE host) barrier and TLBI. The register operand is
    // `va_operand` (`tlb::tlbi_va_operand` for a VA form). established here.
    unsafe {
        match op {
            tlb::TlbOp::DsbIshst => asm!("dsb ishst", options(nostack, preserves_flags)),
            tlb::TlbOp::TlbiVale1is => {
                asm!("tlbi vale1is, {0}", in(reg) va_operand, options(nostack, preserves_flags));
            }
            tlb::TlbOp::TlbiVae1is => {
                asm!("tlbi vae1is, {0}", in(reg) va_operand, options(nostack, preserves_flags));
            }
            tlb::TlbOp::TlbiAside1is => {
                asm!("tlbi aside1is, {0}", in(reg) va_operand, options(nostack, preserves_flags));
            }
            tlb::TlbOp::TlbiAside1 => {
                asm!("tlbi aside1, {0}", in(reg) va_operand, options(nostack, preserves_flags));
            }
            tlb::TlbOp::TlbiVmalle1 => asm!("tlbi vmalle1", options(nostack, preserves_flags)),
            tlb::TlbOp::TlbiVmalle1is => asm!("tlbi vmalle1is", options(nostack, preserves_flags)),
            tlb::TlbOp::DsbIsh => asm!("dsb ish", options(nostack, preserves_flags)),
            tlb::TlbOp::DsbNsh => asm!("dsb nsh", options(nostack, preserves_flags)),
            tlb::TlbOp::Isb => asm!("isb", options(nostack, preserves_flags)),
        }
    }
}

fn run_tlb(ops: &[tlb::TlbOp], va_operand: u64) {
    for &op in ops {
        exec_tlb(op, va_operand);
    }
}

/// Broadcast TLB invalidate. Runs `tlb::ALL_INVAL`.
pub fn tlbi_all() {
    run_tlb(&tlb::ALL_INVAL, 0);
}

/// Local ASID-0 invalidate after a TTBR0 write, which `write_ttbr0` ends
/// with an `isb`. Runs `tlb::TTBR0_SWITCH`.
pub fn tlbi_asid0_local() {
    run_tlb(&tlb::TTBR0_SWITCH, 0);
}

/// Leaf TLB invalidate. Runs `tlb::LEAF_INVAL`. ASID 0: TTBR0 does not
/// carry one yet (ROADMAP §11.2).
pub fn tlbi_va(va: u64) {
    run_tlb(&tlb::LEAF_INVAL, tlb::tlbi_va_operand(va, 0));
}

/// Break-before-make maintenance after the invalid store. Runs
/// `tlb::BBM_AFTER_INVALID`.
pub(crate) fn bbm_break(va: u64) {
    run_tlb(&tlb::BBM_AFTER_INVALID, tlb::tlbi_va_operand(va, 0));
}

/// Barriers after the new store of break-before-make. Runs
/// `tlb::BBM_AFTER_MAKE`.
pub(crate) fn bbm_make() {
    run_tlb(&tlb::BBM_AFTER_MAKE, 0);
}

/// Publish a new table before the descriptor that links it. Runs
/// `tlb::PUBLISH_TABLE`.
pub(crate) fn publish_table() {
    run_tlb(&tlb::PUBLISH_TABLE, 0);
}

/// Write `TTBR0_EL1` (ASID in the high bits when `TCR.A1` is clear).
///
/// # Safety
/// `ttbr0` is a complete user root, or the empty ASID-0 root.
pub unsafe fn write_ttbr0(ttbr0: u64) {
    // SAFETY: this fn's `# Safety` (here).
    unsafe {
        asm!("msr ttbr0_el1, {0}", in(reg) ttbr0, options(nostack, preserves_flags));
        asm!("isb", options(nostack, preserves_flags));
    }
}

/// `CTR_EL0`: the cache line sizes (`sysreg::dcache_line_bytes`).
pub fn read_ctr_el0() -> u64 {
    let v: u64;
    // SAFETY: CTR_EL0 is readable at EL1 with no trap set for it; established here.
    unsafe {
        asm!("mrs {0}, ctr_el0", out(reg) v, options(nomem, nostack, preserves_flags));
    }
    v
}

pub fn read_ttbr0() -> u64 {
    let v: u64;
    // SAFETY: TTBR0_EL1 is readable at EL1 / VHE EL2; established here.
    unsafe {
        asm!("mrs {0}, ttbr0_el1", out(reg) v, options(nomem, nostack, preserves_flags));
    }
    v
}

/// Write the #202 computed EL0/EL1 environment registers whole.
pub fn apply_computed_sysregs() {
    let sctlr = sysreg::sctlr_el1();
    let pmuserenr = sysreg::pmuserenr_el0();
    let cpacr = sysreg::cpacr_el1();
    // SAFETY: whole writes of the computed values; never RMW. established here.
    unsafe {
        // E2H=1 renames CNTHCTL_EL2 bits 0 and 1 to EL0PCTEN and EL0VCTEN.
        // The E2H=0 value (EL1PCTEN|EL1PCEN) would let EL0 read CNTPCT.
        let cnt = sysreg::cntkctl_el1();
        if el2_vhe() {
            asm!("msr cnthctl_el2, {0}", in(reg) cnt, options(nostack, preserves_flags));
        } else {
            asm!("msr cntkctl_el1, {0}", in(reg) cnt, options(nostack, preserves_flags));
        }
        // PMUSERENR_EL0 is UNDEFINED without FEAT_PMUv3.
        if sysreg::feat_pmuv3(id_aa64dfr0_el1()) {
            asm!("msr pmuserenr_el0, {0}", in(reg) pmuserenr, options(nostack, preserves_flags));
        }
        asm!("msr cpacr_el1, {0}", in(reg) cpacr, options(nostack, preserves_flags));
        asm!("msr sctlr_el1, {0}", in(reg) sctlr, options(nostack, preserves_flags));
        asm!("isb", options(nostack, preserves_flags));
    }
}

/// Print the §11.1 exception-level marker. Does not write [`EL2_VHE`].
pub fn print_exception_level() {
    let el = current_el();
    if el2_vhe() {
        crate::marker!("vibeOS: el: 2 vhe");
    } else {
        crate::marker!("vibeOS: el: {el}");
    }
}

/// Release the debug OS Lock and clear `MDSCR_EL1.{MDE,KDE,SS}`.
pub fn release_debug_os_lock() {
    // SAFETY: OSDLR/OSLAR/MDSCR are writable at EL1; established here.
    unsafe {
        asm!("msr osdlr_el1, xzr", options(nostack, preserves_flags));
        asm!("msr oslar_el1, xzr", options(nostack, preserves_flags));
        asm!("isb", options(nostack, preserves_flags));
        asm!("msr mdscr_el1, xzr", options(nostack, preserves_flags));
        asm!("isb", options(nostack, preserves_flags));
    }
}

#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(dead_code, reason = "in-guest oslsr_clear (kernel_tests)")
)]
pub fn oslsr() -> u64 {
    let v: u64;
    // SAFETY: OSLSR_EL1 is readable; established here.
    unsafe {
        asm!("mrs {0}, oslsr_el1", out(reg) v, options(nomem, nostack, preserves_flags));
    }
    v
}

/// Set PSTATE.PAN. FEAT_PAN is the ISA floor (ROADMAP §11.1); this is
/// not a write of `SCTLR_EL1`.
pub fn set_pan() {
    // SAFETY: FEAT_PAN is the ISA floor; established here.
    unsafe { asm!("msr pan, #1", options(nostack, preserves_flags)) };
}

/// Zero `TPIDRRO_EL0`, the EL0-readable word the overflow stub stashes its
/// SP in (DESIGN §11.5 rule 6). Each CPU clears it before it can return to EL0.
pub fn clear_tpidrro_el0() {
    // SAFETY: `TPIDRRO_EL0` is writable at EL1; established here.
    unsafe {
        asm!(
            "msr tpidrro_el0, xzr",
            options(nomem, nostack, preserves_flags)
        )
    };
}

/// `TPIDRRO_EL0` as this CPU holds it.
#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(dead_code, reason = "in-guest TPIDRRO check (kernel_tests)")
)]
pub fn tpidrro_el0() -> u64 {
    let v: u64;
    // SAFETY: `TPIDRRO_EL0` is readable at EL1; established here.
    unsafe { asm!("mrs {0}, tpidrro_el0", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// Clear PAN for a kernel access to a user VA. Restore with [`set_pan`].
#[cfg(feature = "kernel_tests")]
pub fn clear_pan() {
    // SAFETY: FEAT_PAN is the ISA floor; established here.
    unsafe { asm!("msr pan, #0", options(nostack, preserves_flags)) };
}

/// Whether `mrs PAN` is nonzero (PSTATE.PAN set).
#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(dead_code, reason = "in-guest PAN check (kernel_tests)")
)]
pub fn pan_is_set() -> bool {
    let v: u64;
    // SAFETY: `PAN` is readable once FEAT_PAN is present; established here.
    unsafe { asm!("mrs {0}, pan", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v != 0
}

#[inline]
pub fn interrupts_enabled() -> bool {
    let daif: u64;
    // SAFETY: DAIF read; established here.
    unsafe { asm!("mrs {0}, daif", out(reg) daif, options(nomem, nostack, preserves_flags)) };
    daif & DAIF_I == 0
}

#[inline]
pub fn irq_disable() {
    // SAFETY: mask IRQs on this CPU; no `nomem` (compiler barrier). established here.
    unsafe { asm!("msr daifset, #2", options(nostack, preserves_flags)) };
}

#[inline]
pub fn irq_enable() {
    // SAFETY: unmask IRQs on this CPU; no `nomem`. established here.
    unsafe { asm!("msr daifclr, #2", options(nostack, preserves_flags)) };
}

/// Clear PSTATE.A once the full vector table is live.
pub fn clear_pstate_a() {
    // SAFETY: SError is taken where raised after full VBAR; established here.
    unsafe { asm!("msr daifclr, #4", options(nostack, preserves_flags)) };
}

/// Set D, A, I, and F (a return to EL0, DESIGN §5.10).
#[inline]
pub fn daif_set_all() {
    // SAFETY: mask debug, SError, IRQ, and FIQ; no `nomem`. established here.
    unsafe { asm!("msr daifset, #0xf", options(nostack, preserves_flags)) };
}

/// Clear D, A, I, and F (syscall / fault body, DESIGN §2.9 rule 3).
#[inline]
pub fn daif_clear_all() {
    // SAFETY: unmask after the user frame is saved; established here.
    unsafe { asm!("msr daifclr, #0xf", options(nostack, preserves_flags)) };
}

/// Clear D and A only (IRQ top-half from EL0, DESIGN §5.10).
#[inline]
pub fn daif_clear_da() {
    // SAFETY: SError and debug unmasked; I and F stay set. established here.
    unsafe { asm!("msr daifclr, #0xc", options(nostack, preserves_flags)) };
}

#[inline]
pub fn cli() {
    irq_disable();
}

#[inline]
pub fn sti() {
    irq_enable();
}

/// `CNTVCT_EL0` after `isb`.
#[inline]
pub fn cntvct() -> u64 {
    let v: u64;
    // SAFETY: CNTVCT is the virtual counter; `isb` before the read. established here.
    unsafe {
        asm!(
            "isb",
            "mrs {0}, cntvct_el0",
            out(reg) v,
            options(nomem, nostack, preserves_flags),
        );
    }
    v
}

/// `wfi` with IRQs still masked. Wakes on a pending interrupt anyway.
#[inline]
pub fn idle_wait() {
    // SAFETY: `wfi` waits; DAIF is unchanged; established here.
    unsafe { asm!("wfi", options(nomem, nostack, preserves_flags)) };
}

#[inline]
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn wait_for_interrupt() {
    idle_wait();
}

pub fn halt() -> ! {
    irq_disable();
    loop {
        idle_wait();
    }
}

/// The one-interrupt wait used by calibration loops.
pub fn hlt_once() {
    idle_wait();
}

pub struct InterruptGuard {
    restore: bool,
    _not_send: PhantomData<*const ()>,
}

impl InterruptGuard {
    #[inline]
    #[track_caller]
    pub fn enter() -> Self {
        let enabled = interrupts_enabled();
        irq_disable();
        run_hook(&NEST_ENTER);
        if enabled {
            crate::sched::irqoff::off(crate::sched::irqoff::Site::caller(
                core::panic::Location::caller(),
            ));
        }
        Self {
            restore: enabled,
            _not_send: PhantomData,
        }
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        // Nest tracks live guards, including those entered with IRQs
        // already off (x86's `InterruptGuard::drop`).
        run_hook(&NEST_LEAVE);
        if self.restore {
            crate::sched::irqoff::on();
            irq_enable();
        }
    }
}

/// Select `SP_ELx` and put the live stack on it (DESIGN §11.5).
///
/// Exception entry sets `PSTATE.SP` to 1, so an IRQ taken while `SPSel`
/// is 0 would run on leftover `SP_EL1` instead of the stack `mov sp`
/// wrote on `SP_EL0`.
pub fn use_sp_elx() {
    // SAFETY: one write of the live SP onto `SP_ELx`; DAIF still masks
    // IRQs as Limine left them, and the `msr`/`mov` pair is one block
    // so nothing uses leftover `SP_EL1`. established here.
    unsafe {
        asm!(
            "mov {tmp}, sp",
            "msr spsel, #1",
            "isb",
            "mov sp, {tmp}",
            tmp = out(reg) _,
        );
    }
}

#[inline]
pub fn stack_pointer() -> u64 {
    let sp: u64;
    // SAFETY: reads SP; established here.
    unsafe { asm!("mov {0}, sp", out(reg) sp, options(nomem, nostack, preserves_flags)) };
    sp
}

#[inline]
pub fn frame_pointer() -> u64 {
    let fp: u64;
    // SAFETY: reads x29; established here.
    unsafe { asm!("mov {0}, x29", out(reg) fp, options(nomem, nostack, preserves_flags)) };
    fp
}

#[inline]
pub fn instruction_pointer() -> u64 {
    let lr: u64;
    // SAFETY: approximate PC from LR; established here.
    unsafe { asm!("mov {0}, x30", out(reg) lr, options(nomem, nostack, preserves_flags)) };
    lr
}

#[inline]
pub fn irq_flags() -> u64 {
    let daif: u64;
    // SAFETY: DAIF read; established here.
    unsafe { asm!("mrs {0}, daif", out(reg) daif, options(nomem, nostack, preserves_flags)) };
    daif
}

pub fn set_overflow_sp(sp: u64) {
    // Release: pairs with the Acquire load in `overflow_sp`.
    OVERFLOW_SP.store(sp, Ordering::Release);
}

#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(dead_code, reason = "in-guest overflow-stack check (kernel_tests)")
)]
pub fn overflow_sp() -> u64 {
    // Acquire: pairs with the Release store in `set_overflow_sp`.
    OVERFLOW_SP.load(Ordering::Acquire)
}

/// `ID_AA64ISAR0_EL1.RNDR` (Arm ARM DDI0487): 0b0001 means `RNDR`/`RNDRRS`.
pub(crate) fn has_rndr() -> bool {
    let isar0: u64;
    // SAFETY: ID registers are readable at EL1/EL2; established here.
    unsafe {
        asm!(
            "mrs {0}, ID_AA64ISAR0_EL1",
            out(reg) isar0,
            options(nomem, nostack, preserves_flags)
        );
    }
    (isar0 >> 60) & 0xF != 0
}

/// One word from `RNDR`, or `None` when the CPU has none or it failed
/// ten times. Does not write CPACR (ROADMAP §11.6).
#[inline]
pub fn hw_rng64() -> Option<u64> {
    if !has_rndr() {
        return None;
    }
    let mut tries = 0u8;
    while tries < 10 {
        let val: u64;
        let nzcv: u64;
        // SAFETY: `S3_3_C2_C4_0` is `RNDR`, readable at EL1 where
        // `ID_AA64ISAR0_EL1.RNDR` is set, which `has_rndr` checked, and
        // `NZCV` is readable at EL1. established here.
        unsafe {
            asm!(
                "mrs {val}, S3_3_C2_C4_0",
                "mrs {nzcv}, nzcv",
                val = out(reg) val,
                nzcv = out(reg) nzcv,
                options(nomem, nostack),
            );
        }
        if let Some(word) = sysreg::rndr_word(nzcv, val) {
            return Some(word);
        }
        tries = tries.saturating_add(1);
    }
    None
}

#[inline]
pub fn user_tls() -> u64 {
    let v: u64;
    // SAFETY: TPIDR_EL0 is the user TLS register; established here.
    unsafe { asm!("mrs {0}, tpidr_el0", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// # Safety
/// `v` is the thread's TLS base.
#[inline]
pub unsafe fn set_user_tls(v: u64) {
    // SAFETY: this fn's `# Safety` (here).
    unsafe { asm!("msr tpidr_el0, {0}", in(reg) v, options(nomem, nostack, preserves_flags)) };
}

static NEST_ENTER: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static NEST_LEAVE: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static CPU_INDEX: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Install the per-CPU hooks: `InterruptGuard`'s nesting count and this
/// CPU's index. The per-CPU module's `init_bsp` calls it once, on the BSP.
pub fn set_per_cpu_hooks(nest_enter: fn(), nest_leave: fn(), cpu_index: fn() -> Option<u32>) {
    // Release: pairs with the Acquire loads in `run_hook` and `cpu_index`.
    NEST_ENTER.store(nest_enter as *mut (), Ordering::Release);
    // Release: pairs with the Acquire load in `run_hook`.
    NEST_LEAVE.store(nest_leave as *mut (), Ordering::Release);
    // Release: pairs with the Acquire load in `cpu_index`.
    CPU_INDEX.store(cpu_index as *mut (), Ordering::Release);
}

#[inline]
fn run_hook(hook: &AtomicPtr<()>) {
    // Acquire: pairs with the Release stores in `set_per_cpu_hooks`.
    let p = hook.load(Ordering::Acquire);
    if p.is_null() {
        return;
    }
    // SAFETY: a non-null hook holds a `fn()`; established by
    // `set_per_cpu_hooks`, its only store. established here.
    let f = unsafe { core::mem::transmute::<*mut (), fn()>(p) };
    f();
}

pub fn cpu_index() -> Option<u32> {
    // Acquire: pairs with the Release store in `set_per_cpu_hooks`.
    let p = CPU_INDEX.load(Ordering::Acquire);
    if p.is_null() {
        return None;
    }
    // SAFETY: a non-null hook holds `fn() -> Option<u32>`; established by
    // `set_per_cpu_hooks`. established here.
    let f = unsafe { core::mem::transmute::<*mut (), fn() -> Option<u32>>(p) };
    f()
}

pub fn read_cr3() -> u64 {
    read_ttbr1()
}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn read_rip() -> u64 {
    instruction_pointer()
}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn read_rbp() -> u64 {
    frame_pointer()
}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn read_rsp() -> u64 {
    stack_pointer()
}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn rflags() -> u64 {
    irq_flags()
}

/// Full-system barrier. Shared code names `mfence`.
#[inline]
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn mfence() {
    // SAFETY: `dsb sy` is a full system barrier; established here.
    unsafe { asm!("dsb sy", options(nostack, preserves_flags)) };
}

/// Unused port I/O names so shared boot code compiles.
///
/// # Safety
/// No port I/O on aarch64; the caller must not rely on the write.
#[inline]
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub unsafe fn outb(_port: u16, _val: u8) {}

/// # Safety
/// As `outb`.
#[inline]
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub unsafe fn outw(_port: u16, _val: u16) {}

/// # Safety
/// As `outb`.
#[inline]
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub unsafe fn outl(_port: u16, _val: u32) {}

/// # Safety
/// As `outb`.
#[inline]
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub unsafe fn inb(_port: u16) -> u8 {
    0
}

/// # Safety
/// As `outb`.
#[inline]
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub unsafe fn inl(_port: u16) -> u32 {
    0
}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn cpuid(_leaf: u32, _sub: u32) -> (u32, u32, u32, u32) {
    (0, 0, 0, 0)
}
