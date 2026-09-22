#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
//! Per-CPU areas via `GS_BASE`. DESIGN §3.3 step 11, §7.5.
//!
//! Heap array sized from the MADT CPU count. After GDT (`mov gs` zeros
//! the hidden base). Before the first timer IRQ: ISRs must not `gs:[0]`
//! until this is live. `KERNEL_GS_BASE` matches `GS_BASE`.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use vibeos::per_cpu::PerCpu;
use vibeos::thread::Tcb;

use crate::acpi_init;
use crate::x86;

pub const IA32_GS_BASE: u32 = 0xC000_0101;
pub const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;

struct BootCell<T>(core::cell::UnsafeCell<T>);
unsafe impl<T> Sync for BootCell<T> {}
impl<T> BootCell<T> {
    const fn new(v: T) -> Self {
        Self(core::cell::UnsafeCell::new(v))
    }
    unsafe fn get_mut(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
    unsafe fn get(&self) -> &T {
        unsafe { &*self.0.get() }
    }
}

static CPUS: BootCell<Option<Box<[PerCpu]>>> = BootCell::new(None);
static LIVE: AtomicBool = AtomicBool::new(false);
/// Bit `cpu_id`. MADTs with >64 CPUs need a wider mask later.
static ONLINE: AtomicU64 = AtomicU64::new(0);

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
    let mut v = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        v.push(PerCpu::empty());
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
    boxed[0].apic_id = apic_id();
    boxed[0].ready.store(true, Ordering::Relaxed);
    let ptr = boxed[0].self_ptr as u64;
    unsafe {
        x86::wrmsr(IA32_GS_BASE, ptr);
        x86::wrmsr(IA32_KERNEL_GS_BASE, ptr);
    }
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    *unsafe { CPUS.get_mut() } = Some(boxed);
    LIVE.store(true, Ordering::Release);
    ONLINE.store(1, Ordering::Release);
}

pub fn is_live() -> bool {
    LIVE.load(Ordering::Acquire)
}

pub fn cpu_count() -> usize {
    unsafe { CPUS.get().as_ref().map(|c| c.len()).unwrap_or(0) }
}

pub fn cpu(id: u32) -> Option<&'static PerCpu> {
    let cpus = unsafe { CPUS.get().as_ref()? };
    cpus.get(id as usize)
}

pub fn cpu_mut(id: u32) -> Option<&'static mut PerCpu> {
    let cpus = unsafe { CPUS.get_mut().as_mut()? };
    cpus.get_mut(id as usize)
}

/// `gs:[0]` == `self_ptr`. Only after [`init_bsp`] (and AP `install_gs`).
pub fn current() -> &'static PerCpu {
    assert!(is_live(), "per_cpu: not live");
    let p = gs_self();
    assert!(!p.is_null(), "per_cpu: gs null");
    unsafe { &*p }
}

pub fn current_mut() -> &'static mut PerCpu {
    assert!(is_live(), "per_cpu: not live");
    let p = gs_self();
    assert!(!p.is_null(), "per_cpu: gs null");
    unsafe { &mut *p }
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

pub fn try_current_mut() -> Option<&'static mut PerCpu> {
    if !is_live() {
        return None;
    }
    let p = gs_self();
    if p.is_null() {
        return None;
    }
    Some(unsafe { &mut *p })
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
    if let Some(c) = try_current_mut() {
        c.irq_nest = c.irq_nest.saturating_add(1);
    }
}

pub fn irq_nest_leave() {
    if let Some(c) = try_current_mut() {
        assert!(c.irq_nest > 0, "irq nest underflow");
        c.irq_nest -= 1;
    }
}

pub fn irq_nest() -> u32 {
    try_current().map(|c| c.irq_nest).unwrap_or(0)
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

pub fn set_current_thread(tcb: *mut Tcb) {
    current_mut().current = tcb;
}

pub fn current_thread() -> *mut Tcb {
    current().current
}

pub fn set_tsc_per_ms(v: u64) {
    if let Some(c) = try_current_mut() {
        c.tsc_per_ms = v;
    }
}

pub fn set_timer_mode(mode: vibeos::apic::TimerMode) {
    if let Some(c) = try_current_mut() {
        c.timer_mode = mode;
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
