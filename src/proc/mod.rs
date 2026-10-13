//! Processes: the kernel half of subsystem `proc` (DESIGN §1.3).

pub(crate) mod addr_space_init;
pub(crate) mod fill_init;
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
pub(crate) mod proc_init;
#[cfg(target_arch = "x86_64")]
pub(crate) mod syscall_init;
#[cfg(target_arch = "aarch64")]
#[path = "syscall_init_aarch64.rs"]
pub(crate) mod syscall_init_aarch64;
#[cfg(target_arch = "aarch64")]
pub(crate) use syscall_init_aarch64 as syscall_init;
pub(crate) mod uaccess_init;
pub(crate) mod user_init;
