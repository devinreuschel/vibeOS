//! Scheduler: the kernel half of subsystem `sched` (DESIGN §1.3).

#[cfg(feature = "kernel_tests")]
pub mod ktest;
pub(crate) mod sched_init;
pub(crate) mod thread_init;
pub(crate) mod work_init;
