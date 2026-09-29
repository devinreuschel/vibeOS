//! Time: the kernel half of subsystem `time` (DESIGN §1.3).

#[cfg(feature = "kernel_tests")]
pub mod ktest;
pub(crate) mod time_init;
