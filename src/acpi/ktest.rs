//! In-guest tests for acpi (kernel_tests only). Rows: [`TESTS`].

use core::sync::atomic::Ordering;

use vibeos::paging::{PageFlags, VirtAddr};

use crate::acpi_init;
use crate::ktest::{Outcome, Test, test};
use crate::machine_init;
use crate::paging_init;

/// Whether `acpi_init::init` UC-patched at least one MMIO leaf.
pub(crate) fn mmio_uc_patched() -> bool {
    acpi_init::MMIO_UC.load(Ordering::Acquire)
}

fn leaf_is_uc(phys: u64) -> bool {
    if phys == 0 {
        return false;
    }
    let va = VirtAddr(paging_init::hhdm_offset().wrapping_add(phys));
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
    if !mmio_uc_patched() {
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
    let Some(desc) = machine_init::info() else {
        return Outcome::Fail("no machine desc");
    };
    if desc.cpu_count() != info.cpu_count() {
        return Outcome::Fail("machine desc cpu count");
    }
    if desc.lapic_base() != Some(madt.lapic_base) {
        return Outcome::Fail("machine desc lapic");
    }
    if desc.ioapic_count() != info.ioapic_count() {
        return Outcome::Fail("machine desc ioapic count");
    }
    if desc
        .hpet_info()
        .is_none_or(|h| h.base != hpet.base || h.period_fs != hpet.period_fs)
    {
        return Outcome::Fail("machine desc hpet");
    }
    Outcome::Ok
}

/// This subsystem's in-guest tests, in run order; `crate::ktest::GROUPS`
/// runs them (DESIGN §8.2).
pub(crate) const TESTS: &[Test] = &[test("acpi_discovery", test_acpi_discovery)];
