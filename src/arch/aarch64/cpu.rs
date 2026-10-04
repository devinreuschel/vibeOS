//! aarch64 CPU primitives: DAIF, halt, `wfi`, system registers.
//!
//! DAIF writes omit `nomem` so they are compiler barriers (ROADMAP §10.3,
//! F091). They never rewrite `SCTLR_EL1`, `CNTKCTL_EL1`, `PMUSERENR_EL0`,
//! or `CPACR` (#202 wrote those once).

use core::arch::asm;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

use vibeos::arch::aarch64::sysreg;
use vibeos::log::Level;

use crate::cell::BootCell;

/// PSTATE.I (IRQs).
const DAIF_I: u64 = 1 << 7;

static EL2_VHE: BootCell<bool> = BootCell::new();
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

/// ISA floor: FEAT_LSE (`Atomic >= 2`) and FEAT_PAN (`PAN != 0`).
pub fn check_isa_floor() {
    let isar0: u64;
    let mmfr1: u64;
    // SAFETY: ID registers are readable at EL1/EL2; established here.
    unsafe {
        asm!("mrs {0}, ID_AA64ISAR0_EL1", out(reg) isar0, options(nomem, nostack, preserves_flags));
        asm!("mrs {0}, ID_AA64MMFR1_EL1", out(reg) mmfr1, options(nomem, nostack, preserves_flags));
    }
    let atomic = (isar0 >> 20) & 0xF;
    let pan = (mmfr1 >> 20) & 0xF;
    if atomic < 2 || pan == 0 {
        crate::boot::halt_with("vibeOS: cpu: isa floor (lse+pan) missing");
    }
    // Release: pairs with the Acquire load in `isa_ok`.
    ISA_OK.store(true, Ordering::Release);
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

/// Write `TTBR1_EL1`. There is no `TTBR1_EL2`; under VHE the `_EL1`
/// name reaches the host root (DESIGN §11.2).
///
/// # Safety
/// `ttbr1` is a complete TTBR1 root that maps this CPU's code, stack, and
/// everything it touches next.
pub unsafe fn write_ttbr1(ttbr1: u64) {
    // SAFETY: this fn's `# Safety` (here); `msr` only loads the root.
    unsafe {
        asm!("msr ttbr1_el1, {0}", in(reg) ttbr1, options(nostack, preserves_flags));
        asm!("isb", options(nostack, preserves_flags));
    }
}

pub fn read_ttbr1() -> u64 {
    let v: u64;
    // SAFETY: TTBR1_EL1 is readable at EL1 and at EL2 with VHE; established here.
    unsafe {
        asm!("mrs {0}, ttbr1_el1", out(reg) v, options(nomem, nostack, preserves_flags));
    }
    v
}

/// Write MAIR and TCR from the computed `*_EL1` values. Not SCTLR.
pub fn write_translation_regs() {
    let asid16 = {
        let mmfr0: u64;
        // SAFETY: ID register; established here.
        unsafe {
            asm!("mrs {0}, ID_AA64MMFR0_EL1", out(reg) mmfr0, options(nomem, nostack, preserves_flags));
        }
        sysreg::asid16_from_mmfr0(mmfr0)
    };
    let mair = sysreg::mair_el1();
    let tcr = sysreg::tcr_el1(asid16);
    // SAFETY: whole writes of the computed translation policy; SCTLR is
    // left as Limine/#202 set it. VHE redirects the `_EL1` names.
    // established here.
    unsafe {
        asm!("msr mair_el1, {0}", in(reg) mair, options(nostack, preserves_flags));
        asm!("msr tcr_el1, {0}", in(reg) tcr, options(nostack, preserves_flags));
        asm!("isb", options(nostack, preserves_flags));
    }
}

pub fn tlbi_all() {
    // Inner-shareable broadcast (ROADMAP §11.2 / Arm ARM DDI0487 TLBI).
    // SAFETY: EL1 (VHE host) regime; established here.
    unsafe {
        asm!("tlbi vmalle1is", options(nostack, preserves_flags));
        asm!("dsb ish", options(nostack, preserves_flags));
        asm!("isb", options(nostack, preserves_flags));
    }
}

pub fn tlbi_va(va: u64) {
    let page = va >> 12;
    // Inner-shareable leaf invalidate (Arm ARM `tlbi vale1is`).
    // SAFETY: EL1 (VHE host) regime; established here.
    unsafe {
        asm!("tlbi vale1is, {0}", in(reg) page, options(nostack, preserves_flags));
        asm!("dsb ish", options(nostack, preserves_flags));
        asm!("isb", options(nostack, preserves_flags));
    }
}

/// Write `TTBR0_EL1` (ASID in the high bits when `TCR.A1` is clear).
///
/// # Safety
/// `ttbr0` is a complete user root, or the empty ASID-0 root.
#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(dead_code, reason = "in-guest ASID switch (kernel_tests)")
)]
pub unsafe fn write_ttbr0(ttbr0: u64) {
    // SAFETY: this fn's `# Safety` (here).
    unsafe {
        asm!("msr ttbr0_el1, {0}", in(reg) ttbr0, options(nostack, preserves_flags));
        asm!("isb", options(nostack, preserves_flags));
    }
}

#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(dead_code, reason = "in-guest ASID switch (kernel_tests)")
)]
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
        if el2_vhe() {
            let cnthctl = sysreg::cnthctl_el2();
            asm!("msr cnthctl_el2, {0}", in(reg) cnthctl, options(nostack, preserves_flags));
        } else {
            let cntkctl = sysreg::cntkctl_el1();
            asm!("msr cntkctl_el1, {0}", in(reg) cntkctl, options(nostack, preserves_flags));
        }
        asm!("msr pmuserenr_el0, {0}", in(reg) pmuserenr, options(nostack, preserves_flags));
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

/// Clear PAN for a kernel access to a user VA. Restore with [`set_pan`].
#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(dead_code, reason = "in-guest ASID user-VA walk (kernel_tests)")
)]
pub fn clear_pan() {
    // SAFETY: FEAT_PAN is the ISA floor; established here.
    unsafe { asm!("msr pan, #0", options(nostack, preserves_flags)) };
}

#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(dead_code, reason = "in-guest ASID user-VA walk (kernel_tests)")
)]
pub fn set_pan() {
    // SAFETY: as `clear_pan`; established here.
    unsafe { asm!("msr pan, #1", options(nostack, preserves_flags)) };
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

#[inline]
pub fn hw_rng64() -> Option<u64> {
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
