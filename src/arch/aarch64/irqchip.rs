//! GIC + generic timer facade under the `apic_init` name shared code uses.

use vibeos::apic::{IpiError, IpiMode, Polarity, TimerMode, Trigger};

use super::{gic, timer};

static TICKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// # Safety
/// KVA live; IRQs masked; boot CPU only.
pub unsafe fn init() {
    // SAFETY: this fn's `# Safety`; established here.
    unsafe { gic::init() };
}

pub fn prove() {
    timer::enable();
}

pub fn eoi() {}

pub fn eoi_for(_vec: u8) {}

pub fn in_service(_vec: u8) -> bool {
    false
}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn owns_tick() -> bool {
    true
}

pub fn timer_mode() -> TimerMode {
    TimerMode::Periodic
}

pub fn route_gsi(
    _gsi: u32,
    _vec: u8,
    _dest: u8,
    _trig: Trigger,
    _pol: Polarity,
) -> Result<(), IpiError> {
    Ok(())
}

pub fn mask_gsi(_gsi: u32) {}
pub fn unmask_gsi(_gsi: u32) {}

pub fn send_ipi(_apic: u8, _vec: u8, _mode: IpiMode) -> Result<(), IpiError> {
    Ok(())
}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn send_ipi_cpu(_cpu: u32, _vec: u8) -> Result<(), IpiError> {
    Ok(())
}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn send_ipi_all_ex_self(_vec: u8) -> Result<(), IpiError> {
    Ok(())
}

/// # Safety
/// Unused on the boot-CPU-only slice; established here.
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub unsafe fn enable_ap() {}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn arm_ap() {}

pub fn on_timer_irq() {
    // Relaxed: a count; pairs with nothing.
    TICKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    crate::time_init::on_hw_tick();
    timer::on_tick();
    crate::sched_init::on_timer_tick();
}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn on_error_irq() {}
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn on_thermal_irq() {}
#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn on_spurious_irq() {}

#[expect(dead_code, reason = "boot-CPU S7; unused on this path")]
pub fn ticks() -> u64 {
    // Relaxed: a count; pairs with nothing.
    TICKS.load(core::sync::atomic::Ordering::Relaxed)
}
