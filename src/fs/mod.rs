//! Filesystems: the kernel half of subsystem `fs` (DESIGN §1.3).

pub(crate) mod fat_init;
pub(crate) mod file_init;
pub(crate) mod fs_init;
#[cfg(feature = "kernel_tests")]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::let_underscore_must_use,
    clippy::unused_result_ok,
    clippy::disallowed_types,
    clippy::disallowed_macros,
    reason = "kernel_tests-only in-guest tests: a failure ends a test, not the kernel"
)]
pub mod ktest;
#[cfg(feature = "vibefs_crash")]
pub(crate) mod vibefs_crash;
pub(crate) mod vibefs_init;
