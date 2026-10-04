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
use crate::machine_init;
use crate::paging_init;

static INFO: BootCell<AcpiInfo> = BootCell::new();
pub(super) static MMIO_UC: AtomicBool = AtomicBool::new(false);

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
        let src = (paging_init::hhdm_offset().wrapping_add(addr)) as *const u8;
        // SAFETY: `ensure_ram` just mapped every 4 KiB leaf of
        // `[addr, end)` in the physmap, cacheable and readable, so `src`
        // is valid for `buf.len()` bytes; `buf` is a distinct `&mut`, so
        // the ranges do not overlap. Established here.
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
        let va = VirtAddr(paging_init::hhdm_offset().wrapping_add(p));
        if paging_init::translate(va).is_none() {
            // SAFETY: the physmap maps a frame only at its own HHDM address
            // and `translate` found no leaf at `va`, so the new leaf aliases
            // no other mapping of `p`, which is what `Mapper::map_page` asks;
            // established here.
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
    matches!(
        // SAFETY: `map_gap` just made the physmap cover `[phys, phys+len)`,
        // which is `patch_physmap_uc`'s requirement; established here.
        unsafe { paging_init::patch_physmap_uc(PhysAddr(phys), len) },
        Ok(n) if n > 0
    )
}

/// Parse ACPI, UC-patch discovered MMIO, stash the result.
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
        // Release: pairs with the Acquire load in `acpi::ktest`.
        MMIO_UC.store(true, Ordering::Release);
        crate::marker!(marker::PAGING_MMIO_UC);
    }

    // First MMIO touch: HPET GEN_CAP period, only after that page is UC.
    if hpet_uc && let Some(hpet) = info.hpet.as_mut() {
        let va = (paging_init::hhdm_offset().wrapping_add(hpet.base)) as *const u64;
        // SAFETY: invariant I49, established here: `uc_mmio` returned true,
        // so the HPET register page is mapped UC in the physmap, and
        // GEN_CAP is its aligned first register.
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
