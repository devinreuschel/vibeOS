//! Kernel ACPI bring-up. Library parsers live in `vibeos::acpi`.
//!
//! DESIGN §3.3: after CR3, walk tables through the RAM-only physmap.
//! LAPIC / I/O APIC / HPET are reached only through `ioremap` (ROADMAP
//! §11.2). Firmware tables live in RAM-typed ranges already mapped.

use core::sync::atomic::{AtomicU64, Ordering};

use vibeos::acpi::{self, AcpiError, AcpiInfo, MAX_IOAPICS, PhysMem};
use vibeos::paging::{PAGE_SIZE_4K, PhysAddr, VirtAddr};

use crate::cell::BootCell;
use crate::machine_init;
use crate::paging_init;

static INFO: BootCell<AcpiInfo> = BootCell::new();
static LAPIC_VA: AtomicU64 = AtomicU64::new(0);
static HPET_VA: AtomicU64 = AtomicU64::new(0);
static IOAPIC_PHYS: [AtomicU64; MAX_IOAPICS] = [const { AtomicU64::new(0) }; MAX_IOAPICS];
static IOAPIC_VA: [AtomicU64; MAX_IOAPICS] = [const { AtomicU64::new(0) }; MAX_IOAPICS];

struct HhdmPhys;

impl PhysMem for HhdmPhys {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        if buf.is_empty() {
            return true;
        }
        let Some(end) = addr.checked_add(buf.len() as u64) else {
            return false;
        };
        if !physmap_covers(addr, end - addr) {
            return false;
        }
        let src = (paging_init::hhdm_offset().wrapping_add(addr)) as *const u8;
        // SAFETY: `physmap_covers` found a present physmap leaf for every
        // 4 KiB of `[addr, end)`, RAM-typed and readable, so `src` is valid
        // for `buf.len()` bytes; `buf` is a distinct `&mut`. Established here.
        unsafe { core::ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), buf.len()) };
        true
    }
}

fn align_down(x: u64, a: u64) -> u64 {
    x & !(a - 1)
}
fn align_up(x: u64, a: u64) -> u64 {
    (x + a - 1) & !(a - 1)
}

/// True when the RAM-only physmap already covers `[phys, phys+len)`.
fn physmap_covers(phys: u64, len: u64) -> bool {
    if len == 0 {
        return true;
    }
    let start = align_down(phys, PAGE_SIZE_4K);
    let end = align_up(phys.saturating_add(len), PAGE_SIZE_4K);
    let mut p = start;
    while p < end {
        let va = VirtAddr(paging_init::hhdm_offset().wrapping_add(p));
        if paging_init::translate(va).is_none() {
            return false;
        }
        p = match p.checked_add(PAGE_SIZE_4K) {
            Some(n) => n,
            None => break,
        };
    }
    true
}

fn ioremap_page(phys: u64) -> Option<u64> {
    if phys == 0 {
        return None;
    }
    // SAFETY: invariant I49: `[phys, phys+4K)` is a device register page
    // (LAPIC, I/O APIC, or HPET) that no cacheable alias reaches;
    // established here from the MADT/HPET table `acpi::walk` just parsed.
    unsafe { paging_init::ioremap(PhysAddr(phys), PAGE_SIZE_4K) }.map(|v| v.as_u64())
}

/// The ioremap VA of the MADT LAPIC page, if mapped.
pub fn lapic_va() -> Option<u64> {
    // Acquire: pairs with the Release store in `init`.
    let v = LAPIC_VA.load(Ordering::Acquire);
    (v != 0).then_some(v)
}

/// The ioremap VA of the HPET page, if mapped.
pub fn hpet_va() -> Option<u64> {
    // Acquire: pairs with the Release store in `init`.
    let v = HPET_VA.load(Ordering::Acquire);
    (v != 0).then_some(v)
}

/// The ioremap VA of the I/O APIC at `phys`, if mapped.
pub fn ioapic_va(phys: u64) -> Option<u64> {
    if phys == 0 {
        return None;
    }
    for i in 0..MAX_IOAPICS {
        // Acquire: pairs with the Release store in `init`.
        if IOAPIC_PHYS[i].load(Ordering::Acquire) == phys {
            // Acquire: pairs with the Release store in `init`.
            let v = IOAPIC_VA[i].load(Ordering::Acquire);
            return (v != 0).then_some(v);
        }
    }
    None
}

/// Parse ACPI, ioremap discovered MMIO, stash the result.
///
/// # Safety
/// After `paging_init::install`, single-CPU, IRQs off.
pub unsafe fn init(rsdp_phys: u64) {
    if rsdp_phys == 0 {
        return;
    }
    let mut info = match acpi::walk(&HhdmPhys, rsdp_phys) {
        Ok(i) => i,
        Err(e) => halt_acpi(e),
    };

    if let Some(madt) = &info.madt {
        if let Some(va) = ioremap_page(madt.lapic_base) {
            // Release: pairs with the Acquire load in `lapic_va`.
            LAPIC_VA.store(va, Ordering::Release);
        }
        for (i, io) in madt.ioapics.iter().take(madt.ioapic_count).enumerate() {
            let Some(slot) = IOAPIC_PHYS.get(i) else {
                break;
            };
            if let Some(va) = ioremap_page(io.addr as u64) {
                // Release: pairs with the Acquire load in `ioapic_va`.
                slot.store(io.addr as u64, Ordering::Release);
                // Release: pairs with the Acquire load in `ioapic_va`.
                IOAPIC_VA[i].store(va, Ordering::Release);
            }
        }
    }
    let mut hpet_mapped = false;
    if let Some(hpet) = &info.hpet
        && let Some(va) = ioremap_page(hpet.base)
    {
        // Release: pairs with the Acquire load in `hpet_va`.
        HPET_VA.store(va, Ordering::Release);
        hpet_mapped = true;
    }

    // First MMIO touch: HPET GEN_CAP period, only after that page is UC.
    if hpet_mapped && let Some(hpet) = info.hpet.as_mut() {
        // Acquire: pairs with the Release store in `init`.
        let va = HPET_VA.load(Ordering::Acquire) as *const u64;
        // SAFETY: invariant I49, established here: `ioremap_page` returned
        // a Device mapping of the HPET register page, and GEN_CAP is its
        // aligned first register.
        let cap = unsafe { va.read_volatile() };
        hpet.period_fs = (cap >> 32) as u32;
    }
    machine_init::set_from_acpi(&info);
    // SAFETY: invariant I22, established at `cell::BootCell::set`: this is
    // the one write, on the BSP before SMP (`acpi::acpi_init::init`'s
    // `# Safety`), and no reader runs until it returns.
    unsafe { INFO.set(info) };
}

/// Marker + summary. DESIGN §3.3 step 12, after GDT/PIC/IDT.
pub fn report() {
    let Some(info) = INFO.try_get() else {
        return;
    };
    crate::marker!("vibeOS: acpi: xsdt {} tables", info.table_count);
    let hpet = if info.hpet_present() {
        "present"
    } else {
        "absent"
    };
    crate::marker!(
        "vibeOS: acpi: {} cpus, {} ioapics, hpet {hpet}",
        info.cpu_count(),
        info.ioapic_count()
    );
}

pub fn info() -> Option<&'static AcpiInfo> {
    INFO.try_get()
}

fn halt_acpi(e: AcpiError) -> ! {
    crate::marker!("vibeOS: acpi: {}", e.as_str());
    crate::arch::current::halt();
}
