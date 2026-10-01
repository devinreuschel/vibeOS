//! Hard-IRQ context: this CPU's `IN_ISR` flag, which `irq_init::dispatch`
//! sets around a device's handlers (DESIGN §2.2, invariant I2).
//!
//! A leaf: the scheduler and the locks read it, and `irq_init` calls them,
//! so the flag lives below both.

use vibeos::arch::{InterruptMask, PerCpuBase};
use vibeos::atomic::statics::{AtomicBool, Ordering};

use crate::arch::current::Arch;

/// Per CPU, indexed by `cpu_id` as `sync_init`'s `HELD` is: set while
/// `irq_init::dispatch` runs a device's top half on that CPU.
static IN_ISR: [AtomicBool; 64] = [const { AtomicBool::new(false) }; 64];

/// This CPU's flag: CPU 0's until `GS_BASE` holds the per-CPU area, as
/// `PerCpuBase::cpu_id` answers then.
fn slot() -> Option<&'static AtomicBool> {
    IN_ISR.get(Arch::cpu_id() as usize)
}

/// Whether this CPU runs a device's hard-IRQ top half. False with IF on,
/// since a top half runs with IF off.
pub fn in_hard_irq() -> bool {
    // A top half runs with IF=0, so IF=1 answers alone, and an IF=0 read
    // stays on the CPU whose slot it reads (DESIGN §2.9 rule 5).
    if <Arch as InterruptMask>::enabled() {
        return false;
    }
    // Relaxed: only this CPU writes its slot.
    slot().is_some_and(|f| f.load(Ordering::Relaxed))
}

/// Set or clear this CPU's flag. `irq_init::dispatch` only, with IF off.
pub(crate) fn set(on: bool) {
    if let Some(f) = slot() {
        f.store(on, Ordering::Relaxed);
    }
}

/// Test access to the flag (kernel_tests only).
#[cfg(feature = "kernel_tests")]
pub mod testing {
    /// Set or clear this CPU's `IN_ISR` as `irq_init::dispatch` does. Call
    /// with IF off, and clear it on the same CPU before IF goes back on.
    pub fn set_in_isr(on: bool) {
        super::set(on);
    }
}
