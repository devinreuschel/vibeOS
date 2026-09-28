//! The x86_64 port: its kernel half (DESIGN §1.3).

pub(crate) mod apic_init;
pub mod catch;
pub mod cpu;
pub mod gdt;
pub mod gs;
pub mod idt;
pub mod pic;
mod trampoline;
