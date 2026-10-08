//! Publish [`MachineDesc`] for SMP, IRQ, and PCI (DESIGN §11.1).
//!
//! The FDT parse runs before the buddy so `/reserved-memory` and the
//! header reservation block stay out. ACPI (MADT, HPET, MCFG) fills the
//! same cell when there is an RSDP. FADT stays on [`acpi_init`].

use core::ops::Range;

use vibeos::acpi::AcpiInfo;
use vibeos::machine::{MAX_RESERVED, MachineDesc, PhysRange};

use crate::boot::BootInfo;
use crate::cell::BootCell;

static DESC: BootCell<MachineDesc> = BootCell::new();

struct EarlyReserved {
    ranges: [PhysRange; MAX_RESERVED],
    count: usize,
}

static EARLY_RESERVED: BootCell<EarlyReserved> = BootCell::new();

pub fn info() -> Option<&'static MachineDesc> {
    DESC.try_get()
}

/// Parse Limine's DTB before [`crate::pmm_init::init`].
///
/// Halts if a reserved range will not fit or the device walk cannot finish.
/// `no-map` ranges are published when every reservation was recorded.
pub fn init_from_dtb(boot: &BootInfo) {
    let Some(dtb) = boot.dtb else {
        return;
    };
    let d = match vibeos::machine::fdt::parse_dtb(dtb) {
        Ok(vibeos::machine::fdt::Parsed::Complete(d)) => {
            publish_nomap(&d);
            d
        }
        Ok(vibeos::machine::fdt::Parsed::Incomplete(desc, e)) => {
            publish_nomap(&desc);
            refuse_dt(e);
        }
        Err(e) => refuse_dt(e),
    };
    if boot.rsdp_phys == 0 {
        let n = d.node_count;
        // SAFETY: invariant I22, established at `cell::BootCell::set`: this
        // is the one write, on the BSP before SMP (`machine::machine_init::init_from_dtb`
        // runs from `normal_boot_tail` before `pmm_init`), and no reader
        // runs until it returns.
        unsafe { DESC.set(d) };
        if n != 0 {
            crate::marker!("vibeOS: dt: {} nodes", n);
        }
    } else if d.reserved_count != 0 {
        // SAFETY: invariant I22, established at `cell::BootCell::set`: the
        // one write of the DTB reserved list, here, before `pmm_init`.
        // ACPI fills `DESC` later.
        unsafe {
            EARLY_RESERVED.set(EarlyReserved {
                ranges: d.reserved,
                count: d.reserved_count,
            });
        }
    }
}

fn publish_nomap(d: &MachineDesc) {
    let mut holes: [Range<u64>; MAX_RESERVED] = core::array::from_fn(|_| 0..0);
    let mut n = 0usize;
    for r in d.nomap_ranges() {
        let Some(slot) = holes.get_mut(n) else {
            crate::boot::halt_with("vibeOS: dt: refused");
        };
        *slot = r;
        n = n.saturating_add(1);
    }
    if n == 0 {
        return;
    }
    let Some(stored) = holes.get(..n) else {
        crate::boot::halt_with("vibeOS: dt: refused");
    };
    crate::boot::set_nomap(stored);
}

fn refuse_dt(e: vibeos::machine::fdt::FdtError) -> ! {
    crate::klog!(vibeos::log::Level::Error, "vibeOS: dt: {}", e.as_str());
    crate::boot::halt_with("vibeOS: dt: refused");
}

/// MADT / HPET / MCFG. No-op when a DTB already filled the cell.
pub fn set_from_acpi(info: &AcpiInfo) {
    if DESC.try_get().is_some() {
        return;
    }
    // SAFETY: invariant I22, established at `cell::BootCell::set`: this is
    // the one write, on the BSP in `acpi::acpi_init::init` before SMP, and
    // no reader of `DESC` runs until it returns.
    unsafe { DESC.set(MachineDesc::from_acpi(info)) };
}

/// DTB reserved ranges, from `DESC` or the early stash when ACPI owns the cell.
pub fn reserved_ranges() -> impl Iterator<Item = Range<u64>> {
    let from_desc = DESC
        .try_get()
        .into_iter()
        .flat_map(MachineDesc::reserved_ranges);
    let from_early = EARLY_RESERVED
        .try_get()
        .into_iter()
        .flat_map(|e| e.ranges.iter().take(e.count).filter_map(|r| r.range()));
    from_desc.chain(from_early)
}
