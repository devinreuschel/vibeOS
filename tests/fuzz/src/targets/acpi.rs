//! `acpi_walk` and `acpi_tables`: the ACPI table parsers.

use vibeos::acpi;

use crate::physmem::{BASE, FlatMem};

/// `acpi::walk` over the input mapped at [`BASE`], the RSDP first.
pub fn walk(data: &[u8]) {
    let _ = acpi::walk(&FlatMem::new(data), BASE);
}

/// Each table parser on the whole input, then `parse_gas` at an offset
/// below 64 that the input's first byte picks.
pub fn tables(data: &[u8]) {
    let _ = acpi::parse_rsdp(data);
    let _ = acpi::parse_sdt_header(data);
    let _ = acpi::validate_sdt(data);
    let _ = acpi::parse_madt(data);
    let _ = acpi::parse_hpet(data);
    let _ = acpi::parse_fadt(data);
    let _ = acpi::parse_mcfg(data);
    if let Some(&b) = data.first() {
        let _ = acpi::parse_gas(data, usize::from(b % 64));
    }
}
