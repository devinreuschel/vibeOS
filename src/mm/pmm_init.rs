//! Kernel-side wiring: turn [`BootInfo`]'s usable RAM into a running
//! Buddy allocator. Portable buddy logic lives in `vibeos::pmm`; this
//! module is the binary-crate half.
//!
//! DESIGN §4.2 lists the regions that must not enter the free lists:
//!   - physical frame 0
//!   - the AP trampoline page `boot::capture` chose (DESIGN §7.3)
//!   - the loaded kernel image span
//!   - each framebuffer and each boot module
//!   - anything not marked `USABLE`, including bootloader- and
//!     ACPI-reclaimable
//!   - `/reserved-memory` and the FDT memreserve block (`MachineDesc`)
//!   - RAM whose physmap alias would pass the DESIGN §4.1 slot
//!
//! `vibeos::pmm::clip_usable` subtracts the first four from every USABLE
//! range as it reads `BootInfo`, with no fixed-size list (DESIGN §2.4);
//! `Buddy::insert_region` drops frame 0 again as defence in depth.

use vibeos::lock::RANK_BUDDY;
use vibeos::physmap::{clip_range_to_slot, leftover_mib};
use vibeos::pmm::{Buddy, PAGE_SIZE, PmmStats, clip_usable};

use crate::boot::{self, BootInfo};
use crate::sync_init::SpinMutex;

static BUDDY: SpinMutex<Buddy> = SpinMutex::with_rank(Buddy::new(0), RANK_BUDDY);

/// Post-init access to the global buddy. IRQ-aware, rank buddy.
pub fn with_buddy<R>(f: impl FnOnce(&mut Buddy) -> R) -> R {
    let mut g = BUDDY.lock();
    f(&mut g)
}

/// Ingest usable RAM into the global buddy. Returns the resulting stats
/// snapshot. Call once.
///
/// # Safety
/// - Limine's HHDM must still map every USABLE range, so the buddy can
///   write free-list nodes into it at `phys + info.hhdm_offset`.
/// - Single CPU, before interrupts are enabled.
pub unsafe fn init(info: &BootInfo) -> PmmStats {
    let mut buddy = BUDDY.lock();
    buddy.set_hhdm(info.hhdm_offset);

    // DESIGN §2.4: frame 0, the trampoline page (kept forever, even after
    // every AP is up), the kernel image, and each framebuffer and module.
    // Limine keeps usable entries clear of the last three; excluding them
    // anyway keeps a quirky firmware from handing us the scanout region.
    let tramp = info.trampoline_page;
    let excl = || {
        core::iter::once(0..PAGE_SIZE)
            .chain(tramp.map(|p| p..p.saturating_add(PAGE_SIZE)))
            .chain(core::iter::once(info.kernel_phys.clone()))
            .chain(
                info.framebuffers()
                    .map(|fb| fb.phys..fb.phys.saturating_add(fb.size)),
            )
            .chain(info.modules())
            .chain(crate::machine_init::reserved_ranges())
    };

    let slot = boot::physmap_slot();
    let offset = info.hhdm_offset;
    let mut leftover = 0u64;
    for r in info.ram_ranges() {
        let (_, left) = clip_range_to_slot(slot, offset, r.start, r.end);
        leftover = leftover.saturating_add(left);
    }
    // Free-list nodes, page tables, and heap pages are all reached through
    // the physmap once cr3 switches, so RAM past the slot stays out.
    for r in info.usable() {
        let (end, _) = clip_range_to_slot(slot, offset, r.start, r.end);
        if r.start < end {
            clip_usable(r.start..end, excl, |part| {
                // SAFETY: `Buddy::insert_region`'s contract; Limine's HHDM
                // maps every USABLE range (this fn's `# Safety` contract),
                // clipped to the physmap slot so the kernel's physmap reaches
                // it too (invariant I14), `clip_usable` hands each part once
                // and outside frame 0, the trampoline page, the kernel image,
                // framebuffers and modules (invariant I15); established here.
                unsafe { buddy.insert_region(part.start, part.end) };
            });
        }
    }
    if leftover > 0 {
        crate::marker!(
            "vibeOS: pmm: {} MiB past the physmap slot ignored",
            leftover_mib(leftover)
        );
    }

    buddy.stats()
}
