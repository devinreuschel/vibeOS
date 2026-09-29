//! The x86_64 port: its kernel half (DESIGN §1.3).

pub(crate) mod apic_init;
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
pub mod catch;
pub mod cpu;
pub mod gdt;
pub mod gs;
pub mod idt;
pub mod pic;
mod trampoline;
