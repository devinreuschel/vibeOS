//! In-guest tests for acpi (kernel_tests only). Rows: the list in crate::ktest.

use vibeos::paging::{PageFlags, VirtAddr};

use crate::acpi_init;
use crate::ktest::Outcome;
use crate::paging_init;

fn leaf_is_uc(phys: u64) -> bool {
    if phys == 0 {
        return false;
    }
    let va = VirtAddr(paging_init::HHDM_BASE.wrapping_add(phys));
    match paging_init::translate(va) {
        Some((_, _, flags)) => flags.contains(PageFlags::PCD | PageFlags::PWT),
        None => false,
    }
}

pub(crate) fn test_acpi_discovery() -> Outcome {
    let Some(info) = acpi_init::info() else {
        return Outcome::Fail("no acpi info");
    };
    if info.table_count == 0 {
        return Outcome::Fail("zero tables");
    }
    if info.cpu_count() == 0 {
        return Outcome::Fail("no enabled cpus");
    }
    if info.ioapic_count() == 0 {
        return Outcome::Fail("no ioapic");
    }
    if !info.hpet_present() {
        return Outcome::Fail("no hpet");
    }
    if !acpi_init::mmio_uc_patched() {
        return Outcome::Fail("mmio uc not patched");
    }
    let Some(madt) = info.madt.as_ref() else {
        return Outcome::Fail("no madt");
    };
    if !leaf_is_uc(madt.lapic_base) {
        return Outcome::Fail("lapic not uc");
    }
    for io in madt.ioapics.iter().take(madt.ioapic_count) {
        if !leaf_is_uc(io.addr as u64) {
            return Outcome::Fail("ioapic not uc");
        }
    }
    let Some(hpet) = info.hpet else {
        return Outcome::Fail("no hpet");
    };
    if !leaf_is_uc(hpet.base) {
        return Outcome::Fail("hpet not uc");
    }
    if hpet.period_fs == 0 {
        return Outcome::Fail("hpet period unread");
    }
    Outcome::Ok
}
