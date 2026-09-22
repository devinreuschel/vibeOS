//! Kernel ACPI bring-up. Library parsers live in `vibeos::acpi`.
//!
//! DESIGN §3.3: after CR3, walk tables through the physmap, UC-patch
//! LAPIC / I/O APIC / HPET before first MMIO touch, then later print
//! `acpi: xsdt N tables` after GDT/PIC/IDT (live steps 3–5 after KVA).

use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::acpi::{self, AcpiError, AcpiInfo, PhysMem};
use vibeos::marker;
use vibeos::paging::{self, PAGE_SIZE_4K, PhysAddr, VirtAddr};

use crate::cell::BootCell;
use crate::paging_init;

static INFO: BootCell<AcpiInfo> = BootCell::new();
static MMIO_UC: AtomicBool = AtomicBool::new(false);

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
        if paging_init::translate(va).is_none()
            && unsafe { paging_init::map_4k(va, PhysAddr(p), flags) }.is_err()
        {
            return false;
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
    matches!(
        unsafe { paging_init::patch_physmap_uc(PhysAddr(phys), len) },
        Ok(n) if n > 0
    )
}

/// Parse ACPI, UC-patch discovered MMIO, stash the result.
///
/// # Safety
/// After `paging_init::install`, single-CPU, IRQs off.
pub unsafe fn init(rsdp_phys: u64) {
    let mut info = match acpi::walk(&HhdmPhys, rsdp_phys) {
        Ok(i) => i,
        Err(e) => halt_acpi(e),
    };

    let mut patched = false;
    if let Some(madt) = &info.madt {
        patched |= uc_mmio(madt.lapic_base, PAGE_SIZE_4K);
        for io in madt.ioapics.iter().take(madt.ioapic_count) {
            patched |= uc_mmio(io.addr as u64, PAGE_SIZE_4K);
        }
    }
    let mut hpet_uc = false;
    if let Some(hpet) = &info.hpet {
        hpet_uc = uc_mmio(hpet.base, PAGE_SIZE_4K);
        patched |= hpet_uc;
    }

    if patched {
        MMIO_UC.store(true, Ordering::Release);
        crate::marker!(marker::PAGING_MMIO_UC);
    }

    // First MMIO touch: HPET GEN_CAP period, only after that page is UC.
    if hpet_uc && let Some(hpet) = info.hpet.as_mut() {
        let va = (paging_init::HHDM_BASE.wrapping_add(hpet.base)) as *const u64;
        let cap = unsafe { va.read_volatile() };
        hpet.period_fs = (cap >> 32) as u32;
    }
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

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn mmio_uc_patched() -> bool {
    MMIO_UC.load(Ordering::Acquire)
}

fn halt_acpi(e: AcpiError) -> ! {
    crate::marker!("vibeOS: acpi: {}", e.as_str());
    crate::x86::halt();
}
