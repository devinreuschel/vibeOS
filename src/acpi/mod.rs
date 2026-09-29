//! ACPI: the kernel half of subsystem `acpi` (DESIGN §1.3).

pub(crate) mod acpi_init;
#[cfg(feature = "kernel_tests")]
pub mod ktest;
