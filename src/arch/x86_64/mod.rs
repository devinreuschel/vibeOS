//! The x86_64 port: its kernel half (DESIGN §1.3).

#[allow(
    clippy::let_underscore_must_use,
    reason = "audit pending, ROADMAP §10.1"
)]
#[allow(
    clippy::undocumented_unsafe_blocks,
    reason = "audit pending, ROADMAP §10.1"
)]
pub(crate) mod apic_init;
#[cfg(feature = "kernel_tests")]
#[allow(
    clippy::undocumented_unsafe_blocks,
    reason = "audit pending, ROADMAP §10.1"
)]
#[allow(clippy::unwrap_used, reason = "audit pending, ROADMAP §10.1")]
pub mod catch;
pub mod cpu;
#[allow(clippy::disallowed_types, reason = "audit pending, ROADMAP §10.1")]
#[allow(clippy::expect_used, reason = "audit pending, ROADMAP §10.1")]
#[allow(
    clippy::undocumented_unsafe_blocks,
    reason = "audit pending, ROADMAP §10.1"
)]
pub mod gdt;
#[allow(
    clippy::undocumented_unsafe_blocks,
    reason = "audit pending, ROADMAP §10.1"
)]
pub mod gs;
#[allow(clippy::panic, reason = "audit pending, ROADMAP §10.1")]
#[allow(
    clippy::undocumented_unsafe_blocks,
    reason = "audit pending, ROADMAP §10.1"
)]
pub mod idt;
#[allow(
    clippy::undocumented_unsafe_blocks,
    reason = "audit pending, ROADMAP §10.1"
)]
pub mod pic;
mod trampoline;
