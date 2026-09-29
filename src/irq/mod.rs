//! Interrupts: the kernel half of subsystem `irq` (DESIGN §1.3).

pub(crate) mod ipi_init;
pub(crate) mod irq_init;
#[cfg(feature = "kernel_tests")]
pub mod ktest;
