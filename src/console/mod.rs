//! Console: the kernel half of subsystem `console` (DESIGN §1.3).

pub(crate) mod console_init;
pub(crate) mod fb_init;
#[cfg(target_arch = "x86_64")]
pub(crate) mod kbd_init;
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
