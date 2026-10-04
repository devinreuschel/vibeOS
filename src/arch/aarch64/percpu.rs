//! Per-CPU base: `TPIDR_EL1` at EL1, `TPIDR_EL2` at EL2 (ROADMAP §11.4).

use core::arch::asm;
use core::mem::offset_of;

use vibeos::atomic::statics::{AtomicBool, Ordering};
use vibeos::smp::per_cpu::PerCpu;
use vibeos::thread::Tcb;

use super::cpu;

pub const CURRENT_OFFSET: usize = offset_of!(PerCpu, current);
pub const CPU_ID_OFFSET: usize = offset_of!(PerCpu, cpu_id);

static LIVE: AtomicBool = AtomicBool::new(false);

#[inline(always)]
pub fn is_live() -> bool {
    // Acquire: pairs with the Release store in `mark_live`.
    LIVE.load(Ordering::Acquire)
}

pub(crate) fn mark_live() {
    // Release: pairs with the Acquire load in `is_live`.
    LIVE.store(true, Ordering::Release);
}

fn tpidr() -> u64 {
    let v: u64;
    // SAFETY: I4, established at `smp::per_cpu_init::init_bsp`: the
    // chosen TPIDR holds this CPU's `PerCpu`.
    unsafe {
        if cpu::el2_vhe() {
            asm!("mrs {0}, tpidr_el2", out(reg) v, options(nomem, nostack, preserves_flags));
        } else {
            asm!("mrs {0}, tpidr_el1", out(reg) v, options(nomem, nostack, preserves_flags));
        }
    }
    v
}

#[inline(always)]
pub fn current_tcb() -> *mut Tcb {
    if !is_live() {
        return core::ptr::null_mut();
    }
    let base = tpidr();
    if base == 0 {
        return core::ptr::null_mut();
    }
    // SAFETY: `base` is this CPU's `PerCpu` (I4); `current` is a pointer
    // field at `CURRENT_OFFSET`. established here.
    unsafe { ((base as *const u8).add(CURRENT_OFFSET) as *const *mut Tcb).read() }
}

#[inline(always)]
pub fn cpu_id_hint() -> u32 {
    if !is_live() {
        return 0;
    }
    let base = tpidr();
    if base == 0 {
        return 0;
    }
    // SAFETY: as `current_tcb`; `cpu_id` is a `u32` at `CPU_ID_OFFSET`. established here.
    unsafe { ((base as *const u8).add(CPU_ID_OFFSET) as *const u32).read() }
}

/// Point TPIDR at `ptr`, this CPU's `PerCpu`.
///
/// # Safety
/// `ptr` is this CPU's `PerCpu` and stays in place for good.
pub unsafe fn install_base(ptr: *mut PerCpu) {
    let v = ptr as u64;
    // SAFETY: this fn's `# Safety` (here); I4 established here.
    unsafe {
        if cpu::el2_vhe() {
            asm!("msr tpidr_el2, {0}", in(reg) v, options(nomem, nostack, preserves_flags));
        } else {
            asm!("msr tpidr_el1, {0}", in(reg) v, options(nomem, nostack, preserves_flags));
        }
        asm!("isb", options(nostack, preserves_flags));
    }
}

/// MPIDR affinity (bits 31:0), the firmware CPU id.
pub fn hw_cpu_id() -> u32 {
    let v: u64;
    // SAFETY: MPIDR_EL1 is readable; established here.
    unsafe { asm!("mrs {0}, mpidr_el1", out(reg) v, options(nomem, nostack, preserves_flags)) };
    (v & 0x00FF_FFFF) as u32
}

pub fn gs_self() -> *mut PerCpu {
    tpidr() as *mut PerCpu
}
