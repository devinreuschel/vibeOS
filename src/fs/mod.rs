//! Filesystems: the kernel half of subsystem `fs` (DESIGN §1.3).

pub(crate) mod fat_init;
pub(crate) mod file_init;
pub(crate) mod fs_init;
#[cfg(feature = "kernel_tests")]
pub mod ktest;
#[cfg(feature = "vibefs_crash")]
pub(crate) mod vibefs_crash;
pub(crate) mod vibefs_init;
