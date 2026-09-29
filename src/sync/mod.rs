//! Synchronization: the kernel half of subsystem `sync` (DESIGN §1.3).

// The blocking primitives have no production caller yet; the in-guest tests
// are their only users until one lands, which drops this cfg.
#[cfg(feature = "kernel_tests")]
pub(crate) mod blocking_init;
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
pub(crate) mod sync_init;
