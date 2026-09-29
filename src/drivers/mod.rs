//! Drivers: the kernel half of subsystem `drivers` (DESIGN §1.3).

#[cfg(feature = "kernel_tests")]
pub mod ktest;
pub(crate) mod virtio_blk_init;
