//! Synchronization: the kernel half of subsystem `sync` (DESIGN §1.3).

#[cfg(feature = "kernel_tests")]
pub mod ktest;
pub(crate) mod sync_init;
