//! Shell: the kernel half of subsystem `shell` (DESIGN §1.3).

pub(crate) mod cmds;
// Tab completion: the REPL's, which only `kernel_shell` builds that are not
// `kernel_tests` builds run.
#[cfg(all(feature = "kernel_shell", not(feature = "kernel_tests")))]
pub(crate) mod complete;
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
pub(crate) mod shell_init;
