//! Kernel ACPI bring-up. Library parsers live in `vibeos::acpi`.
//!
//! DESIGN §3.3: after CR3, walk tables through the physmap, UC-patch
//! LAPIC / I/O APIC / HPET before first MMIO touch, then later print
//! `acpi: xsdt N tables` after GDT/PIC/IDT (live steps 3–5 after KVA).

use core::cell::UnsafeCell;
use core::fmt::Write;

use vibeos::acpi::{self, AcpiError, AcpiInfo, PhysMem};
use vibeos::marker;
use vibeos::paging::{self, PhysAddr, VirtAddr, PAGE_SIZE_4K};

use crate::paging_init;
use crate::serial::{self, Serial};

struct BootCell<T>(UnsafeCell<Option<T>>);
unsafe impl<T> Sync for BootCell<T> {}
impl<T> BootCell<T> {
    const fn new() -> Self {
        Self(UnsafeCell::new(None))
    }
    unsafe fn set(&self, v: T) {
        unsafe { *self.0.get() = Some(v) };
    }
    fn get(&self) -> Option<&T> {
        unsafe { (*self.0.get()).as_ref() }
    }
    unsafe fn get_mut(&self) -> Option<&mut T> {
        unsafe { (*self.0.get()).as_mut() }
    }
}

static INFO: BootCell<AcpiInfo> = BootCell::new();
static mut MMIO_UC: bool = false;

struct HhdmPhys;

impl PhysMem for HhdmPhys {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        if buf.is_empty() {
            return true;
        }
        let Some(end) = addr.checked_add(buf.len() as u64) else {
            return false;
        };
        if !ensure_ram(addr, end - addr) {
            return false;
        }
        let src = (paging_init::HHDM_BASE.wrapping_add(addr)) as *const u8;
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

/// Map any missing physmap 4 KiB leaves covering `[phys, phys+len)` as
/// ordinary RAM (cacheable). ACPI tables live in reclaimable RAM, not MMIO.
fn ensure_ram(phys: u64, len: u64) -> bool {
    map_gap(phys, len, paging::physmap_flags())
}

fn map_gap(phys: u64, len: u64, flags: paging::PageFlags) -> bool {
    if len == 0 {
        return true;
    }
    let start = align_down(phys, PAGE_SIZE_4K);
    let end = align_up(phys.saturating_add(len), PAGE_SIZE_4K);
    let mut p = start;
    while p < end {
        let va = VirtAddr(paging_init::HHDM_BASE.wrapping_add(p));
        if paging_init::translate(va).is_none() {
            if unsafe { paging_init::map_4k(va, PhysAddr(p), flags) }.is_err() {
                return false;
            }
        }
        p = match p.checked_add(PAGE_SIZE_4K) {
            Some(n) => n,
            None => break,
        };
    }
    true
}

/// Make `[phys, phys+len)` UC in the physmap. Map missing leaves with
/// MMIO flags first so `patch_physmap_uc` has something to touch —
/// LAPIC/IOAPIC/HPET sit above a 128 MiB `map_end`.
///
/// Returns true only if `patch_physmap_uc` actually edited a leaf.
fn uc_mmio(phys: u64, len: u64) -> bool {
    if phys == 0 || len == 0 {
        return false;
    }
    if !map_gap(phys, len, paging::mmio_flags()) {
        return false;
    }
    match unsafe { paging_init::patch_physmap_uc(PhysAddr(phys), len) } {
        Ok(n) if n > 0 => true,
        _ => false,
    }
}

/// Parse ACPI, UC-patch discovered MMIO, stash the result.
///
/// # Safety
/// After `paging_init::install`, single-CPU, IRQs off.
pub unsafe fn init(rsdp_phys: u64) {
    let info = match acpi::walk(&HhdmPhys, rsdp_phys) {
        Ok(i) => i,
        Err(e) => halt_acpi(e),
    };

    let mut patched = false;
    if let Some(madt) = &info.madt {
        patched |= uc_mmio(madt.lapic_base, PAGE_SIZE_4K);
        for i in 0..madt.ioapic_count {
            patched |= uc_mmio(madt.ioapics[i].addr as u64, PAGE_SIZE_4K);
        }
    }
    let mut hpet_uc = false;
    if let Some(hpet) = &info.hpet {
        hpet_uc = uc_mmio(hpet.base, PAGE_SIZE_4K);
        patched |= hpet_uc;
    }

    unsafe { INFO.set(info) };

    if patched {
        unsafe { MMIO_UC = true };
        serial::line(marker::PAGING_MMIO_UC);
    }

    // First MMIO touch: HPET GEN_CAP period, only after that page is UC.
    if hpet_uc {
        if let Some(hpet) = unsafe { INFO.get_mut() }.and_then(|i| i.hpet.as_mut()) {
            let va = (paging_init::HHDM_BASE.wrapping_add(hpet.base)) as *const u64;
            let cap = unsafe { va.read_volatile() };
            hpet.period_fs = (cap >> 32) as u32;
        }
    }
}

/// Marker + summary. DESIGN §3.3 step 12, after GDT/PIC/IDT.
pub fn report() {
    let Some(info) = INFO.get() else {
        return;
    };
    let _ = writeln!(
        Serial,
        "vibeOS: acpi: xsdt {} tables",
        info.table_count
    );
    let hpet = if info.hpet_present() {
        "present"
    } else {
        "absent"
    };
    let _ = writeln!(
        Serial,
        "vibeOS: acpi: {} cpus, {} ioapics, hpet {hpet}",
        info.cpu_count(),
        info.ioapic_count()
    );
}

pub fn info() -> Option<&'static AcpiInfo> {
    unsafe { (*INFO.0.get()).as_ref() }
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn mmio_uc_patched() -> bool {
    unsafe { MMIO_UC }
}

fn halt_acpi(e: AcpiError) -> ! {
    let _ = writeln!(Serial, "vibeOS: acpi: {}", e.as_str());
    crate::x86::halt();
}
