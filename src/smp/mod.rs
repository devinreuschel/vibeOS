//! SMP: the kernel half of subsystem `smp` (DESIGN §1.3).

#[cfg(feature = "hang_test")]
pub(crate) mod hang_test;
#[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
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
pub(crate) mod per_cpu_init;
pub(crate) mod smp_init;
