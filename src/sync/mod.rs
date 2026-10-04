//! Synchronization: the kernel half of subsystem `sync` (DESIGN §1.3).

// The operation gate's sleep (`blocking_init::gate_sleep`) is the one
// production user of the blocking primitives; the in-guest tests use the
// rest until a caller lands, which drops this expectation.
#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(
        dead_code,
        reason = "BlockingMutex, RwLock, Semaphore and Channel have no production caller yet; the in-guest tests use them"
    )
)]
pub(crate) mod blocking_init;
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
pub(crate) mod sync_init;
