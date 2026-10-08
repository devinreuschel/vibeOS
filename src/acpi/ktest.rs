//! In-guest tests for acpi (kernel_tests only). Rows: [`TESTS`].

#[cfg(target_arch = "x86_64")]
use vibeos::paging::{PageFlags, VirtAddr};

#[cfg(target_arch = "x86_64")]
use crate::acpi_init;
use crate::ktest::{Outcome, Test, test};
#[cfg(target_arch = "x86_64")]
use crate::machine_init;
#[cfg(target_arch = "x86_64")]
use crate::paging_init;

#[cfg(target_arch = "x86_64")]
fn leaf_is_uc(va: u64) -> bool {
    if va == 0 {
        return false;
    }
    match paging_init::translate(VirtAddr(va)) {
        Some((_, _, flags)) => flags.contains(PageFlags::PCD | PageFlags::PWT),
        None => false,
    }
}

pub(crate) fn test_acpi_discovery() -> Outcome {
    #[cfg(target_arch = "aarch64")]
    {
        Outcome::Skip("x86 ACPI tables")
    }
    #[cfg(target_arch = "x86_64")]
    {
        test_acpi_discovery_x86()
    }
}

#[cfg(target_arch = "x86_64")]
fn test_acpi_discovery_x86() -> Outcome {
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
    let Some(madt) = info.madt.as_ref() else {
        return Outcome::Fail("no madt");
    };
    let Some(lapic) = acpi_init::lapic_va() else {
        return Outcome::Fail("lapic not ioremapped");
    };
    if !leaf_is_uc(lapic) {
        return Outcome::Fail("lapic not uc");
    }
    for io in madt.ioapics.iter().take(madt.ioapic_count) {
        let Some(va) = acpi_init::ioapic_va(io.addr as u64) else {
            return Outcome::Fail("ioapic not ioremapped");
        };
        if !leaf_is_uc(va) {
            return Outcome::Fail("ioapic not uc");
        }
    }
    let Some(hpet) = info.hpet else {
        return Outcome::Fail("no hpet");
    };
    let Some(hpet_va) = acpi_init::hpet_va() else {
        return Outcome::Fail("hpet not ioremapped");
    };
    if !leaf_is_uc(hpet_va) {
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
