#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
//! BSP `PerCpu` via `GS_BASE`. DESIGN §3.3 step 11, §7.5.
//!
//! After GDT (`mov gs` zeros the hidden base). Before IRQ0: ISRs must
//! not `gs:[0]` until this is live.

use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::per_cpu::PerCpu;
use vibeos::thread::Tcb;

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
    #[allow(dead_code)]
    unsafe fn get(&self) -> &T {
        unsafe { &*self.0.get() }
    }
}

static BSP: BootCell<PerCpu> = BootCell::new(PerCpu::empty());
static LIVE: AtomicBool = AtomicBool::new(false);

fn apic_id() -> u32 {
    let (_, ebx, _, _) = x86::cpuid(1, 0);
    ebx >> 24
}

/// Allocate the BSP area, set `GS_BASE` / `KERNEL_GS_BASE`.
///
/// `current` / `idle` stay null until [`crate::thread_init::init_bootstrap`].
/// Order: this, then bootstrap current, then any path that can switch.
///
/// # Safety
/// GDT already loaded (`mov gs` already happened). Single-CPU. IRQs
/// still masked at the controller, or at least no ISR uses `per_cpu`.
pub unsafe fn init_bsp() {
    let cpu = unsafe { BSP.get_mut() };
    *cpu = PerCpu::empty();
    cpu.self_ptr = cpu as *mut PerCpu;
    cpu.cpu_id = 0;
    cpu.apic_id = apic_id();
    let ptr = cpu as *mut PerCpu as u64;
    unsafe {
        x86::wrmsr(IA32_GS_BASE, ptr);
        x86::wrmsr(IA32_KERNEL_GS_BASE, ptr);
    }
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    LIVE.store(true, Ordering::Release);
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn is_live() -> bool {
    LIVE.load(Ordering::Acquire)
}

/// `gs:[0]` == `self_ptr`. Only after [`init_bsp`].
#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn current() -> &'static PerCpu {
    assert!(is_live(), "per_cpu: not live");
    unsafe { &*gs_self() }
}

pub fn current_mut() -> &'static mut PerCpu {
    assert!(is_live(), "per_cpu: not live");
    unsafe { &mut *gs_self() }
}

pub fn try_current() -> Option<&'static PerCpu> {
    if !is_live() {
        return None;
    }
    Some(unsafe { &*gs_self() })
}

pub fn try_current_mut() -> Option<&'static mut PerCpu> {
    if !is_live() {
        return None;
    }
    Some(unsafe { &mut *gs_self() })
}

/// InterruptGuard nesting. No-op before [`init_bsp`].
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

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn irq_nest() -> u32 {
    try_current().map(|c| c.irq_nest).unwrap_or(0)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
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

/// Field access that is safe from an ISR once `GS_BASE` is live.
#[macro_export]
macro_rules! per_cpu {
    ($field:ident) => {
        $crate::per_cpu_init::current().$field
    };
}
