//! Build a fresh PML4 from buddy frames and install it. DESIGN §4.3.
//!
//! Contents of the PML4 at CR3-install time:
//!   1. Kernel image, mapped per section with the right permissions
//!      (`.text` X+RO, `.rodata` RO+NX, `.data`/`.bss` RW+NX). Section
//!      symbols come from `linker.ld` (DESIGN §3.4).
//!   2. Physmap over [0, map_end) at Limine's HHDM offset, 2 MiB pages,
//!      RW+NX. `map_end` is the max of usable-RAM high water, kernel
//!      image end, and framebuffer extent, capped at `PHYSMAP_CAP` so a
//!      hostile firmware description of an MMIO BAR does not swallow the
//!      whole boot (DESIGN §9.2).
//!   3. Low identity window, [0, 512 MiB), 2 MiB pages. First 2 MiB
//!      executable so the AP trampoline at `0x8000` can run in real
//!      mode before enabling paging (DESIGN §7.3); everything above is
//!      NX.
//!   4. Bootloader stack window: the PML4 entry from Limine's active
//!      tables that covers RSP, borrowed verbatim so `_start`'s own
//!      stack keeps working across `mov cr3`.
//!
//! After the load, the MMIO PTE-attribute step (DESIGN §3.3 step 8) runs.
//! Slice B has no discovered devices yet (ACPI arrives in phase 2), so
//! `patch_physmap_uc` gets called with an empty set and simply prints
//! the marker. The plumbing is here so phase 2 can call
//! `paging::PageTables::patch_physmap_uc` on discovered LAPIC / I/O APIC
//! / HPET pages without touching this file.
//!
//! Single-CPU today, SMP later. The TLB shootdown API in `flush_all` is a
//! deliberate no-op wrapper (DESIGN §4.3: "Write the shootdown hook as a
//! no-op single-CPU function from the start so the call sites are already
//! correct when APs arrive.").

// panic-test builds jump to a deliberate panic right after the limine
// handshake and never reach any of this. Suppress the dead-code chatter
// only in that build.
#![cfg_attr(feature = "panic-test", allow(dead_code))]

use core::cell::UnsafeCell;
use core::fmt::Write;

use limine::memmap::{Entry, MEMMAP_USABLE};

use vibeos::paging::{
    self, FrameAllocator, MapError, PAGE_SIZE, PAGE_SIZE_2M, PHYSMAP_CAP, PTE_ADDR_MASK,
    PTE_GLOBAL, PTE_NX, PTE_PRESENT, PTE_WRITABLE, PageTables, PhysAddr, VirtAddr, align_down,
    align_up, pml4_index,
};
use vibeos::pmm::Buddy;

use crate::marker;
use crate::pmm_init;
use crate::serial::{self, Serial};
use crate::x86;

/// Same BootCell pattern the PMM slice uses: single-CPU, pre-interrupts,
/// so an UnsafeCell with a Sync claim is fine. Phase 4 swaps this for an
/// IRQ-aware mutex.
struct BootCell<T>(UnsafeCell<T>);
unsafe impl<T> Sync for BootCell<T> {}
impl<T> BootCell<T> {
    const fn new(v: T) -> Self {
        Self(UnsafeCell::new(v))
    }
    /// # Safety
    /// Only sound during single-threaded, IRQs-off boot.
    #[allow(clippy::mut_from_ref)]
    unsafe fn get_mut(&self) -> &mut T {
        unsafe { &mut *self.0.get() }
    }
}

/// Global kernel `PageTables` once installed. Slice B publishes this so
/// later phases (heap in slice C, MMIO patches in phase 2) can look up
/// the active mapping without re-plumbing arguments through every call.
static KERNEL_PT: BootCell<Option<PageTables>> = BootCell::new(None);

/// Next free virtual address inside the MMIO window (DESIGN §4.1:
/// 0xFFFF_E000_0000_0000..). Bumped by `ioremap`. Empty pending phase 2.
#[allow(dead_code)] // phase 2 wires up the first caller (LAPIC / HPET)
static MMIO_NEXT: BootCell<u64> = BootCell::new(paging::MMIO_WINDOW_START);

// Section boundary symbols from linker.ld.
unsafe extern "C" {
    static __kernel_vma_start: u8;
    static __kernel_vma_end: u8;
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __data_start: u8;
    static __data_end: u8;
}

#[inline]
fn sym(s: &u8) -> u64 {
    (s as *const u8) as u64
}

/// Wraps the global buddy PMM as a `FrameAllocator` that hands out
/// zeroed 4 KiB frames. Zeroing happens via HHDM so the new frame is
/// safe to install as a page-table page.
pub struct BuddyFrameAlloc {
    buddy: &'static mut Buddy,
    hhdm_offset: u64,
}

impl BuddyFrameAlloc {
    /// # Safety
    /// Single-CPU boot rule (see `pmm_init::buddy_mut`).
    unsafe fn new(hhdm_offset: u64) -> Self {
        Self {
            buddy: unsafe { pmm_init::buddy_mut() },
            hhdm_offset,
        }
    }
}

impl FrameAllocator for BuddyFrameAlloc {
    fn alloc_zeroed(&mut self) -> Option<PhysAddr> {
        let phys = self.buddy.allocate_frame()?;
        unsafe {
            let ptr = (phys.wrapping_add(self.hhdm_offset) as usize) as *mut u64;
            core::ptr::write_bytes(ptr, 0u8, (PAGE_SIZE / 8) as usize);
        }
        Some(phys)
    }
}

/// Compute the physmap upper bound. DESIGN §4.1 / §9.2: max of usable RAM
/// end, kernel image end, and framebuffer extent, capped at 8 GiB.
fn compute_map_end(entries: &[&Entry], kernel_phys_end: u64, hhdm_offset: u64) -> u64 {
    let mut map_end: u64 = 0;
    for e in entries {
        if e.type_ != MEMMAP_USABLE {
            continue;
        }
        let end = e.base.saturating_add(e.length);
        if end > map_end {
            map_end = end;
        }
    }
    if kernel_phys_end > map_end {
        map_end = kernel_phys_end;
    }
    if let Some(fb_resp) = crate::FRAMEBUFFER.response() {
        for fb in fb_resp.framebuffers() {
            let virt = fb.address() as u64;
            if virt == 0 {
                continue;
            }
            let phys = virt.wrapping_sub(hhdm_offset);
            let end = phys.saturating_add(fb.size() as u64);
            if end > map_end {
                map_end = end;
            }
        }
    }
    map_end = align_up(map_end, PAGE_SIZE_2M);
    if map_end > PHYSMAP_CAP {
        map_end = PHYSMAP_CAP;
    }
    map_end
}

/// Build and install a fresh kernel PML4. Prints `paging: cr3 ok` on
/// success. Panics on any allocator or mapping failure — a broken paging
/// bring-up cannot be recovered from without paging.
///
/// # Safety
/// - Interrupts must be off (they are: `_start` never re-enabled them).
/// - The global buddy PMM must be initialized (via `pmm_init::init`).
/// - Must run at most once. Slice B does not implement teardown.
pub unsafe fn init(entries: &[&Entry], hhdm_offset: u64, kernel_phys_base: u64) {
    let mut alloc = unsafe { BuddyFrameAlloc::new(hhdm_offset) };

    let pml4_phys = alloc
        .alloc_zeroed()
        .expect("paging: allocator handed us no PML4 frame");
    let mut pt = unsafe { PageTables::new(pml4_phys, hhdm_offset) };

    // 1. Kernel image, per section. Physical base of a section is the
    //    section virt minus the kernel virt base plus the kernel phys base.
    let kernel_virt_base = sym(unsafe { &__kernel_vma_start });
    let kernel_virt_end = sym(unsafe { &__kernel_vma_end });
    let virt_to_phys = |v: u64| v.wrapping_sub(kernel_virt_base).wrapping_add(kernel_phys_base);

    map_section(
        &mut pt,
        &mut alloc,
        sym(unsafe { &__text_start }),
        sym(unsafe { &__text_end }),
        virt_to_phys,
        PTE_PRESENT | PTE_GLOBAL, // R + X (no NX, no writable)
    );
    map_section(
        &mut pt,
        &mut alloc,
        sym(unsafe { &__rodata_start }),
        sym(unsafe { &__rodata_end }),
        virt_to_phys,
        PTE_PRESENT | PTE_GLOBAL | PTE_NX, // RO + NX
    );
    map_section(
        &mut pt,
        &mut alloc,
        sym(unsafe { &__data_start }),
        sym(unsafe { &__data_end }),
        virt_to_phys,
        PTE_PRESENT | PTE_GLOBAL | PTE_WRITABLE | PTE_NX, // RW + NX
    );

    // 2. Physmap, 2 MiB pages, RW+NX+global.
    let kernel_phys_end = kernel_phys_base + (kernel_virt_end - kernel_virt_base);
    let map_end = compute_map_end(entries, kernel_phys_end, hhdm_offset);
    let physmap_base: VirtAddr = hhdm_offset;
    let map_end_2m = align_up(map_end, PAGE_SIZE_2M);
    unsafe {
        pt.map_range_2m(
            physmap_base,
            0,
            map_end_2m,
            PTE_WRITABLE | PTE_NX | PTE_GLOBAL,
            false,
            &mut alloc,
        )
        .expect("paging: physmap");
    }

    // 3. Low identity window, 512 MiB. First 2 MiB executable so the AP
    //    trampoline (real mode -> long mode) can run there.
    unsafe {
        pt.map_2m(
            0,
            0,
            PTE_WRITABLE | PTE_GLOBAL, // X, RW, no NX
            false,
            &mut alloc,
        )
        .expect("paging: identity first 2M");
        pt.map_range_2m(
            PAGE_SIZE_2M,
            PAGE_SIZE_2M,
            paging::LOW_IDENT_LIMIT - PAGE_SIZE_2M,
            PTE_WRITABLE | PTE_GLOBAL | PTE_NX,
            false,
            &mut alloc,
        )
        .expect("paging: identity rest");
    }

    // 4. Bootloader stack window. Grab the PML4 entry from Limine's live
    //    tables that covers the current RSP and drop it verbatim into our
    //    PML4 at the same index. The subtree is shared with Limine; we do
    //    not free it. Cheap and precise: no need to know exactly where
    //    Limine put the stack.
    let rsp = read_rsp();
    let limine_pml4_phys = x86::read_cr3() & PTE_ADDR_MASK;
    unsafe {
        let limine_pml4 =
            (limine_pml4_phys.wrapping_add(hhdm_offset) as usize) as *const u64;
        let idx = pml4_index(rsp);
        let entry = limine_pml4.add(idx).read();
        if entry & PTE_PRESENT == 0 {
            panic!("paging: rsp {rsp:#x} not present in limine's pml4");
        }
        // Only clobber slots not already used by our own mappings. Kernel
        // image lives at pml4 511; physmap/identity/mmio at their own
        // indices. Stack pml4 index will not collide in practice: Limine
        // stacks live in the low half (identity mappable) or an unrelated
        // higher-half slot.
        let ours =
            (pml4_phys.wrapping_add(hhdm_offset) as usize) as *mut u64;
        let existing = ours.add(idx).read();
        if existing & PTE_PRESENT != 0 {
            // Already covered by one of our mappings (e.g. RSP fell into
            // the low identity or physmap range). Nothing to duplicate.
        } else {
            ours.add(idx).write(entry);
        }
    }

    // Set NX-enable before install; every non-.text mapping we just built
    // has PTE_NX set, so an unpatched EFER makes the first CR3 load a
    // reserved-bit fault (DESIGN §9.5).
    unsafe {
        x86::enable_nxe();
        x86::write_cr3(pt.pml4_phys());
    }

    // Publish the tables for later phases before printing the marker, so
    // if something in the format path faults it does so after the marker.
    unsafe {
        *KERNEL_PT.get_mut() = Some(pt);
    }

    serial::line(marker::PAGING_CR3_OK);

    let _ = writeln!(
        Serial,
        "vibeOS: paging: physmap 0..{:#x} @ hhdm {:#x}",
        map_end, hhdm_offset,
    );

    // Step 8: MMIO PTE attribute patch. Nothing to patch until ACPI
    // brings up LAPIC / I/O APIC / HPET (phase 2). The step still emits
    // its marker so the boot contract records that ordering was honored
    // (DESIGN §3.3).
    serial::line(marker::PAGING_MMIO_UC);
}

fn map_section(
    pt: &mut PageTables,
    alloc: &mut BuddyFrameAlloc,
    virt_start: VirtAddr,
    virt_end: VirtAddr,
    to_phys: impl Fn(u64) -> u64,
    flags: u64,
) {
    let start = align_down(virt_start, PAGE_SIZE);
    let end = align_up(virt_end, PAGE_SIZE);
    if end <= start {
        return;
    }
    let len = end - start;
    let phys = to_phys(start);
    unsafe {
        pt.map_range_4k(start, phys, len, flags, false, alloc)
            .unwrap_or_else(|e| panic!("paging: section {start:#x}..{end:#x}: {e:?}"));
    }
}

#[inline]
fn read_rsp() -> u64 {
    let rsp: u64;
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) rsp, options(nomem, nostack, preserves_flags));
    }
    rsp
}

/// TLB shootdown hook (DESIGN §4.3). Single-CPU today: on a single core,
/// `invlpg` after each PTE edit is enough, and this wrapper exists so
/// call sites are already correct when phase 4 wires up the IPI.
#[inline]
#[allow(dead_code)] // phase 4 (SMP) turns this into a real IPI broadcast
pub fn flush_all_cpus(virt: VirtAddr) {
    unsafe { x86::invlpg(virt) };
}

/// Patch the physmap PTE that covers `phys` (and any 2 MiB entries the
/// span crosses) to uncacheable. Does not split 2 MiB pages
/// (DESIGN §4.3 / §1.3). Caller flushes affected pages via
/// `flush_all_cpus`.
///
/// # Safety
/// The physmap must have been built by [`init`]. Slice B has no callers
/// yet; phase 2 wires this up as soon as ACPI hands over the LAPIC /
/// I/O APIC / HPET bases.
#[allow(dead_code)] // phase 2 wires up the first caller (LAPIC / HPET / I/O APIC)
pub unsafe fn patch_physmap_uc(phys: PhysAddr, len: u64) -> Result<usize, MapError> {
    unsafe {
        let pt = KERNEL_PT
            .get_mut()
            .as_mut()
            .expect("paging: patch_physmap_uc before init");
        let hhdm = pt.hhdm_offset();
        let touched = pt.patch_physmap_uc(hhdm, phys, len)?;
        // Invalidate every 2 MiB entry we touched.
        let start = align_down(phys, PAGE_SIZE_2M);
        let end = align_up(phys + len, PAGE_SIZE_2M);
        let mut v = start;
        while v < end {
            flush_all_cpus(hhdm.wrapping_add(v));
            v += PAGE_SIZE_2M;
        }
        Ok(touched)
    }
}

/// Map `len` bytes of device MMIO from the dedicated MMIO window
/// (DESIGN §4.1: `0xFFFF_E000_0000_0000..0xFFFF_E000_1000_0000`). Uses
/// 4 KiB pages, PCD+PWT so the mapping is uncacheable. Returns the
/// virtual address of the start of the mapped range.
///
/// # Safety
/// `phys` must name real device MMIO that the caller is permitted to
/// map, and the range must not overlap RAM handed to the buddy PMM.
#[allow(dead_code)] // phase 2 wires up the first caller (LAPIC / HPET / I/O APIC)
pub unsafe fn ioremap(phys: PhysAddr, len: u64) -> Result<VirtAddr, MapError> {
    if len == 0 {
        return Err(MapError::Misaligned);
    }
    unsafe {
        let pt = KERNEL_PT
            .get_mut()
            .as_mut()
            .expect("paging: ioremap before init");
        // Save the current MMIO watermark for rollback.
        let base_next = *MMIO_NEXT.get_mut();

        let phys_start = align_down(phys, PAGE_SIZE);
        let phys_end = align_up(phys + len, PAGE_SIZE);
        let mapped_len = phys_end - phys_start;

        // Reserve VA. Bump; align to 4 KiB.
        let virt_start = align_up(base_next, PAGE_SIZE);
        let virt_end = virt_start + mapped_len;
        if virt_end > paging::MMIO_WINDOW_END {
            return Err(MapError::NoFrame);
        }

        let mut alloc = BuddyFrameAlloc::new(pt.hhdm_offset());

        pt.map_range_4k(
            virt_start,
            phys_start,
            mapped_len,
            PTE_WRITABLE | PTE_NX | PTE_GLOBAL | paging::PTE_UC,
            false,
            &mut alloc,
        )?;

        *MMIO_NEXT.get_mut() = virt_end;

        // Flush the newly mapped range so a stale non-present TLB entry
        // does not linger; `invlpg` on a present entry is a no-op cost.
        let mut v = virt_start;
        while v < virt_end {
            flush_all_cpus(v);
            v += PAGE_SIZE;
        }

        Ok(virt_start + (phys - phys_start))
    }
}
