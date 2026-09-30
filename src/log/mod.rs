//! Logging: the kernel half of subsystem `log` (DESIGN §1.3).

pub(crate) mod diag;
pub(crate) mod ksyms;
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
pub(crate) mod log_init;
pub(crate) mod panic;
#[cfg(feature = "panic_nest_test")]
pub(crate) mod panic_test;
pub(crate) mod serial;
pub(crate) mod trace_init;
pub(crate) mod vmcoreinfo_init;
