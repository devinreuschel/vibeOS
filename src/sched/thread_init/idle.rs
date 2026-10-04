//! Idle wait: closed lost-wakeup window (DESIGN §7.8, §11.1).

use crate::per_cpu_init;

/// x86_64: `sti; hlt` as one pair. aarch64: `wfi` while IRQs stay masked,
/// then unmask.
pub fn halt_if_idle() {
    loop {
        // SAFETY: mask IRQs; the restore below or the next pass unmasks.
        // established here.
        #[cfg(target_arch = "x86_64")]
        unsafe {
            core::arch::asm!("cli", options(nostack, preserves_flags));
        }
        #[cfg(target_arch = "aarch64")]
        crate::arch::current::irq_disable();
        crate::sched::irqoff::off_here();
        crate::ipi_init::drain_inbox();
        if !per_cpu_init::current().runq.is_empty() {
            crate::sched::irqoff::on();
            // SAFETY: restore the IF=1 this idle loop runs with. established here.
            #[cfg(target_arch = "x86_64")]
            unsafe {
                core::arch::asm!("sti", options(nostack, preserves_flags));
            }
            #[cfg(target_arch = "aarch64")]
            crate::arch::current::irq_enable();
            return;
        }
        #[cfg(all(feature = "kernel_tests", target_arch = "aarch64"))]
        crate::arch::aarch64::ktest::idle_pre_wait();
        crate::sched::irqoff::on();
        // SAFETY: x86 `sti; hlt` is one pair (DESIGN §7.8). aarch64 `wfi`
        // wakes on a pending IRQ while DAIF stays set. established here.
        #[cfg(target_arch = "x86_64")]
        unsafe {
            core::arch::asm!("sti; hlt", options(nomem, nostack));
        }
        #[cfg(target_arch = "aarch64")]
        {
            crate::arch::current::idle_wait();
            crate::arch::current::irq_enable();
        }
    }
}
