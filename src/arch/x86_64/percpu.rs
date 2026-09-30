//! The per-CPU fast path: one `gs`-relative load each (DESIGN §2.9 rule 5,
//! SMP.md §7.5).
//!
//! [`current_tcb`] reads `PerCpu.current` and [`cpu_id_hint`] reads
//! `PerCpu.cpu_id`, each in one instruction, so preemption cannot split
//! the read between two CPUs' areas. Both return null or 0 until
//! [`is_live`]; an AP calls neither before its
//! `smp::per_cpu_init::install_gs`. The per-CPU-live flag lives here, so
//! this module imports nothing from `smp::per_cpu_init`.

use core::arch::asm;
use core::mem::offset_of;

use vibeos::atomic::statics::{AtomicBool, Ordering};
use vibeos::smp::per_cpu::PerCpu;
use vibeos::thread::Tcb;

/// `PerCpu.current`'s offset from `GS_BASE`, which the syscall entry and
/// exit asm load too (C-PERCPU pins it at 24).
pub const CURRENT_OFFSET: usize = offset_of!(PerCpu, current);
/// `PerCpu.cpu_id`'s offset from `GS_BASE` (C-PERCPU pins it at 8).
pub const CPU_ID_OFFSET: usize = offset_of!(PerCpu, cpu_id);

/// Set once `smp::per_cpu_init::init_bsp` has pointed the BSP's `GS_BASE`
/// at its `PerCpu`; never cleared.
static LIVE: AtomicBool = AtomicBool::new(false);

/// Whether the per-CPU areas are live (`GS_BASE` names this CPU's
/// `PerCpu` on the BSP, and on an AP once its `install_gs` ran).
#[inline(always)]
pub fn is_live() -> bool {
    // Acquire: pairs with the Release store in `mark_live`, so a CPU that
    // sees the flag sees the `GS_BASE` write and the slots before it.
    LIVE.load(Ordering::Acquire)
}

/// Mark the per-CPU areas live. `smp::per_cpu_init::init_bsp` calls it
/// once, after it wrote the BSP's `GS_BASE`.
pub(crate) fn mark_live() {
    // Release: pairs with the Acquire load in `is_live`.
    LIVE.store(true, Ordering::Release);
}

/// The running thread's TCB, in one `gs`-relative load of
/// `PerCpu.current`: the value belongs to whichever CPU ran the load, and
/// the thread that ran it is that CPU's current thread, at any IF. Null
/// before [`is_live`] and before `thread_init::init_bootstrap`.
#[inline(always)]
pub fn current_tcb() -> *mut Tcb {
    if !is_live() {
        return core::ptr::null_mut();
    }
    let p: *mut Tcb;
    // SAFETY: invariant I4, established at `smp::per_cpu_init::init_bsp`
    // (on an AP, `smp::per_cpu_init::install_gs`): once `is_live`,
    // `GS_BASE` is this CPU's `PerCpu`, so the one load reads its
    // `current` and touches no stack or flags.
    unsafe {
        asm!(
            "mov {p}, qword ptr gs:[{off}]",
            p = out(reg) p,
            off = const CURRENT_OFFSET,
            options(nostack, preserves_flags, readonly),
        );
    }
    p
}

/// This CPU's id, in one `gs`-relative load of `PerCpu.cpu_id`. Exact
/// while IF=0; with IF=1 a hint, since the thread may move to another CPU
/// right after the load, so no per-CPU table is indexed by it. 0 before
/// [`is_live`].
#[inline(always)]
pub fn cpu_id_hint() -> u32 {
    if !is_live() {
        return 0;
    }
    let id: u32;
    // SAFETY: invariant I4, established at `smp::per_cpu_init::init_bsp`
    // (on an AP, `smp::per_cpu_init::install_gs`): once `is_live`,
    // `GS_BASE` is this CPU's `PerCpu`, so the one load reads its `cpu_id`
    // and touches no stack or flags.
    unsafe {
        asm!(
            "mov {id:e}, dword ptr gs:[{off}]",
            id = out(reg) id,
            off = const CPU_ID_OFFSET,
            options(nostack, preserves_flags, readonly),
        );
    }
    id
}
