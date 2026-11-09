#[cfg(target_arch = "aarch64")]
pub mod aarch64;
pub mod current;
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
#[cfg(target_arch = "x86_64")]
pub mod x86_64;

#[cfg(all(feature = "kernel_tests", target_arch = "aarch64"))]
pub use aarch64::catch;
#[cfg(all(feature = "kernel_tests", target_arch = "aarch64"))]
pub use aarch64::ktest;
#[cfg(target_arch = "aarch64")]
pub use aarch64::percpu::{cpu_id_hint, current_tcb};
#[cfg(target_arch = "aarch64")]
pub use aarch64::{cpu, gdt, gs, idt, pic};
#[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
pub use x86_64::catch;
#[cfg(target_arch = "x86_64")]
pub use x86_64::percpu::{cpu_id_hint, current_tcb};
#[cfg(target_arch = "x86_64")]
pub use x86_64::{cpu, gdt, gs, idt, pic};
