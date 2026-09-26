#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
//! Per-CPU areas via `GS_BASE`. DESIGN §3.3 step 11, §7.5.
//!
//! Heap array sized from the MADT CPU count. After GDT (`mov gs` zeros
//! the hidden base). Before the first timer IRQ: ISRs must not `gs:[0]`
//! until this is live. `KERNEL_GS_BASE` matches `GS_BASE`.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use vibeos::per_cpu::{PerCpu, PerCpuRemote};
use vibeos::thread::Tcb;

use crate::acpi_init;
use crate::cell::BootCell;
use crate::x86;
use crate::x86::InterruptGuard;

pub const IA32_GS_BASE: u32 = 0xC000_0101;
pub const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;

static CPUS: BootCell<Box<[PerCpu]>> = BootCell::new();
/// Each CPU's remote view, apart from `CPUS` so that no `&mut PerCpu`
/// covers memory another CPU holds `&` to (DESIGN §7.5).
static REMOTE: BootCell<Box<[PerCpuRemote]>> = BootCell::new();

// `cpu(id)` shares `&PerCpuRemote` across CPUs, so the view must be `Sync`
// from its atomic fields alone; `scripts/check_cells.py` rejects an
// `unsafe impl` of `Send` or `Sync` for it (invariant I120).
const _: () = {
    const fn sync<T: Sync>() {}
    sync::<PerCpuRemote>();
};
static LIVE: AtomicBool = AtomicBool::new(false);
/// Bit `cpu_id`. MADTs with >64 CPUs need a wider mask later.
static ONLINE: AtomicU64 = AtomicU64::new(0);
static WITH_BUSY: [AtomicBool; 64] = [const { AtomicBool::new(false) }; 64];

fn apic_id() -> u32 {
    let (_, ebx, _, _) = x86::cpuid(1, 0);
    ebx >> 24
}

fn madt_cpu_count() -> usize {
    acpi_init::info().map(|i| i.cpu_count()).unwrap_or(0).max(1)
}

/// Allocate the heap array, install the BSP at slot 0.
///
/// `current` / `idle` stay null until [`crate::thread_init::init_bootstrap`].
///
/// # Safety
/// GDT already loaded (`mov gs` already happened). Single-CPU. IRQs
/// still masked at the controller, or at least no ISR uses `per_cpu`.
pub unsafe fn init_bsp() {
    let n = madt_cpu_count();
    let mut r = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        r.push(PerCpuRemote::new());
        i += 1;
    }
    // SAFETY: single writer before `smp: done`, no reader yet (BootCell's
    // set contract); established here: `init_bsp` runs once on the BSP.
    unsafe { REMOTE.set(r.into_boxed_slice()) };
    let remote: &'static [PerCpuRemote] = REMOTE.get();
    let mut v = Vec::with_capacity(n);
    i = 0;
    while i < n {
        v.push(PerCpu::new(&remote[i]));
        i += 1;
    }
    let mut boxed = v.into_boxed_slice();
    i = 0;
    while i < n {
        let p = &mut boxed[i] as *mut PerCpu;
        boxed[i].self_ptr = p;
        boxed[i].cpu_id = i as u32;
        i += 1;
    }
    remote[0].apic_id.store(apic_id(), Ordering::Relaxed);
    remote[0].ready.store(true, Ordering::Release);
    let ptr = boxed[0].self_ptr as u64;
    unsafe {
        x86::wrmsr(IA32_GS_BASE, ptr);
        x86::wrmsr(IA32_KERNEL_GS_BASE, ptr);
    }
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    unsafe { CPUS.set(boxed) };
    LIVE.store(true, Ordering::Release);
    ONLINE.store(1, Ordering::Release);
}

pub fn is_live() -> bool {
    LIVE.load(Ordering::Acquire)
}

pub fn cpu_count() -> usize {
    CPUS.try_get().map(|c| c.len()).unwrap_or(0)
}

/// CPU `id`'s remote view: the only per-CPU state another CPU reads. It
/// is never taken `&mut`, so it may alias anything (DESIGN §7.5).
pub fn cpu(id: u32) -> Option<&'static PerCpuRemote> {
    REMOTE.try_get()?.get(id as usize)
}

/// CPU `id`'s `PerCpu` address, set once in [`init_bsp`]. For
/// `smp_init::start_one`, which hands it to the AP it starts.
pub fn slot_ptr(id: u32) -> Option<*mut PerCpu> {
    CPUS.try_get()?.get(id as usize).map(|c| c.self_ptr)
}

/// `gs:[0]` == `self_ptr`. Only after [`init_bsp`] (and AP `install_gs`).
pub fn current() -> &'static PerCpu {
    assert!(is_live(), "per_cpu: not live");
    let p = gs_self();
    assert!(!p.is_null(), "per_cpu: gs null");
    unsafe { &*p }
}

pub fn try_current() -> Option<&'static PerCpu> {
    if !is_live() {
        return None;
    }
    let p = gs_self();
    if p.is_null() {
        return None;
    }
    Some(unsafe { &*p })
}

/// Exclusive `&mut PerCpu` for this CPU. IRQs off. Panics on re-entry.
///
/// Do not `switch_context` inside `f`. The busy flag lives on this stack;
/// the incoming thread would see it set with IF possibly on (DESIGN §9.4).
#[inline(always)]
pub fn with_current<R>(f: impl FnOnce(&mut PerCpu) -> R) -> R {
    let _irq = InterruptGuard::enter();
    assert!(is_live(), "per_cpu: not live");
    let p = gs_self();
    assert!(!p.is_null(), "per_cpu: gs null");
    with_ptr(p, f)
}

/// Exclusive `&mut PerCpu` for `switch_now`'s bookkeeping. Needs IF=0.
///
/// The caller's `InterruptGuard` spans `switch_context`, which the caller
/// runs after this returns: the `&mut` and the busy flag end before the
/// switch, so the incoming thread can take IRQs and `with_current`
/// (DESIGN §7.5, §9.4).
#[inline(always)]
pub fn with_current_switch<R>(f: impl FnOnce(&mut PerCpu) -> R) -> R {
    assert!(
        !x86::interrupts_enabled(),
        "per_cpu: with_current_switch with IF on"
    );
    assert!(is_live(), "per_cpu: not live");
    let p = gs_self();
    assert!(!p.is_null(), "per_cpu: gs null");
    with_ptr(p, f)
}

/// Exclusive `&mut PerCpu` for CPU `id`. IRQs off. BSP bring-up of an AP
/// before its SIPI. Never a remote run queue (DESIGN §7.7).
///
/// # Safety
/// CPU `id` is not running: it has not been sent a SIPI, or it never
/// accepted one. No other scope on its slot is live (the busy flag panics
/// on one on this CPU, not on another).
#[inline(always)]
pub unsafe fn with_cpu<R>(id: u32, f: impl FnOnce(&mut PerCpu) -> R) -> Option<R> {
    let _irq = InterruptGuard::enter();
    let cpus = CPUS.try_get()?;
    let cpu = cpus.get(id as usize)?;
    Some(with_ptr(cpu.self_ptr, f))
}

#[inline(always)]
fn with_ptr<R>(p: *mut PerCpu, f: impl FnOnce(&mut PerCpu) -> R) -> R {
    let id = unsafe { (*p).cpu_id as usize }.min(63);
    if WITH_BUSY[id].swap(true, Ordering::Acquire) {
        panic!("per_cpu: with_current re-entry");
    }
    struct Unlock<'a>(&'a AtomicBool);
    impl Drop for Unlock<'_> {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }
    let _u = Unlock(&WITH_BUSY[id]);
    // SAFETY: invariant I21, established here by the `WITH_BUSY` swap
    // above: no other `with_current`/`with_cpu` scope on this slot is live.
    let pc = unsafe { &mut *p };
    let r = f(pc);
    pc.publish_runq_len();
    r
}

/// Write `GS_BASE` and `KERNEL_GS_BASE` to this CPU's slot.
///
/// # Safety
/// `cpu` is this CPU's `PerCpu`. Call after `mov gs` (GDT load) and
/// before `sti` / any ISR that reads `gs:[0]`.
pub unsafe fn install_gs(cpu: &PerCpu) {
    let ptr = cpu.self_ptr as u64;
    unsafe {
        x86::wrmsr(IA32_GS_BASE, ptr);
        x86::wrmsr(IA32_KERNEL_GS_BASE, ptr);
    }
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
}

/// InterruptGuard nesting. No-op before [`init_bsp`], or if GS is still 0.
pub fn irq_nest_enter() {
    if let Some(c) = try_current() {
        c.irq_nest.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn irq_nest_leave() {
    if let Some(c) = try_current() {
        let old = c.irq_nest.fetch_sub(1, Ordering::Relaxed);
        assert!(old > 0, "irq nest underflow");
    }
}

pub fn irq_nest() -> u32 {
    try_current()
        .map(|c| c.irq_nest.load(Ordering::Relaxed))
        .unwrap_or(0)
}

pub fn gs_self() -> *mut PerCpu {
    let ptr: u64;
    unsafe {
        core::arch::asm!(
            "mov {}, qword ptr gs:[0]",
            out(reg) ptr,
            options(nostack, preserves_flags),
        );
    }
    ptr as *mut PerCpu
}

pub fn set_current_thread(cpu: &mut PerCpu, tcb: *mut Tcb) {
    cpu.current = tcb;
}

pub fn current_thread() -> *mut Tcb {
    current().current
}

pub fn set_tsc_per_ms(v: u64) {
    if try_current().is_some() {
        with_current(|c| c.tsc_per_ms = v);
    }
}

pub fn set_timer_mode(mode: vibeos::apic::TimerMode) {
    if try_current().is_some() {
        with_current(|c| c.timer_mode = mode);
    }
}

pub fn mark_online(cpu_id: u32) {
    if cpu_id >= 64 {
        return;
    }
    ONLINE.fetch_or(1u64 << cpu_id, Ordering::Release);
}

pub fn online_mask() -> u64 {
    ONLINE.load(Ordering::Acquire)
}

pub fn is_online(cpu_id: u32) -> bool {
    if cpu_id >= 64 {
        return false;
    }
    online_mask() & (1u64 << cpu_id) != 0
}

/// Field access that is safe from an ISR once `GS_BASE` is live.
#[macro_export]
macro_rules! per_cpu {
    ($field:ident) => {
        $crate::per_cpu_init::current().$field
    };
}
