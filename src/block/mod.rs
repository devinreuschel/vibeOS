//! Block layer: the kernel half of subsystem `block` (DESIGN §1.3).

pub(crate) mod block_init;
pub(crate) mod cache_init;
#[cfg(feature = "kernel_tests")]
pub mod ktest;
pub(crate) mod part_init;
