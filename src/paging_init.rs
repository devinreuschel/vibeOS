//! Kernel-side bring-up for the paging subsystem: build a fresh PML4
//! from buddy frames, install it, and take over from Limine's tables.
//!
//! Portable page-table walk logic lives in `vibeos::paging`. This module
//! is the binary half: it pulls table frames from `pmm_init::with_buddy`,
//! reads linker section symbols, drives CR3 and EFER through
//! `crate::x86`, and emits the phase-1 `paging: cr3 ok` marker.
//!
//! DESIGN §3.3 step 7 orders us: PMM first, then a fresh PML4 with the
//! contents §4.3 lists, then EFER.NXE, then `mov cr3`. Steps §4.3 also
//! makes explicit: `invlpg` after every single-PTE edit (baked into
//! `Mapper` via the shootdown hook), and no splitting of 2 MiB pages
//! when patching MMIO attributes.

use core::fmt::Write;

use vibeos::marker;
use vibeos::paging::{
    self, FrameAlloc, IoremapWindow, MapError, MapMode, Mapper, PAGE_SIZE_2M, PAGE_SIZE_4K,
    PageFlags, PageSize, PhysAddr, VirtAddr,
};

use crate::pmm_init;
use crate::serial::Serial;
use crate::x86;

// ------------------ constants matching DESIGN §4.1 ------------------

/// HHDM base for our physmap. Same VA Limine already gave us (DESIGN
/// §4.1), so the switch does not invalidate any pointer already computed
/// as `hhdm_offset + phys` — including the buddy allocator's intrusive
/// free-list nodes, which live inside the free pages and are reached via
/// `phys + hhdm_offset`. If Limine ever drifts to a different offset,
/// the first `Buddy::allocate` after `mov cr3` walks an unmapped VA and
/// faults with no useful backtrace. `assert_limine_hhdm` fails loud
/// against that drift at boot.
pub const HHDM_BASE: u64 = 0xFFFF_8000_0000_0000;

/// Panic if Limine handed us an HHDM offset different from
/// [`HHDM_BASE`]. Kept out of `install` so `main` can call it right
/// after reading the HHDM response, before any code has committed to
/// the constant.
pub fn assert_limine_hhdm(offset: u64) {
    assert!(
        offset == HHDM_BASE,
        "paging: limine hhdm offset {:#x} != expected {:#x}; buddy nodes would fault after cr3",
        offset,
        HHDM_BASE,
    );
}

/// Low identity window base and size (DESIGN §4.1). 512 MiB is enough
/// to keep the AP trampoline reachable and to give phase 2's early
/// probing room; the first 2 MiB is executable so the SIPI target at
/// physical 0x8000 is fetchable.
const LOW_ID_BASE: u64 = 0;
const LOW_ID_SIZE: u64 = 512 * 1024 * 1024;

/// Hard cap on physmap extent (DESIGN §4.1, §9.2). Firmware sometimes
/// reports multi-terabyte MMIO BARs as memmap entries; walking that at
/// boot never finishes.
const PHYSMAP_CAP: u64 = 8 * 1024 * 1024 * 1024;

// Linker-provided section boundaries. Names match `linker.ld`.
unsafe extern "C" {
    static __kernel_vma_start: u8;
    static __kernel_vma_end: u8;
    static __limine_requests_start: u8;
    static __limine_requests_end: u8;
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __data_start: u8;
    static __data_end: u8;
}

#[inline]
fn sym_addr(sym: &u8) -> u64 {
    (sym as *const u8) as u64
}

// ------------------ frame allocator glue ------------------

/// Adapter: `FrameAlloc` for `paging::Mapper` backed by the global
/// buddy. Every allocation goes through `pmm_init::with_buddy` so the
/// same single-CPU safety story applies (DESIGN §7.7 upgrade in phase 4).
struct BuddyFrames;

unsafe impl FrameAlloc for BuddyFrames {
    fn alloc_frame(&mut self) -> Option<PhysAddr> {
        let raw = unsafe { pmm_init::with_buddy(|b| b.allocate_frame()) }?;
        Some(PhysAddr(raw))
    }
}

// ------------------ MMIO window (ioremap) ------------------

/// Global reservation state for the ioremap window (DESIGN §4.1). No
/// lock: single-CPU during boot; phase 4 wraps this alongside the
/// buddy behind an IRQ-aware mutex.
///
/// Reachable from phase 2 (APIC/HPET) via `ioremap` below; keeps the
/// item pub-reachable so the linker does not GC it.
#[allow(dead_code)]
static mut IOREMAP: IoremapWindow = IoremapWindow::new();

/// Reserve and map `[phys, phys+len)` into the ioremap window with UC
/// attributes. Returns the VA (offset within the page preserved so a
/// device register at `phys+7` is at `va+7`).
///
/// # Safety
/// Caller vouches that `[phys, phys+len)` is real device MMIO and that
/// no aliased mapping through the physmap will be used to touch the
/// same registers with cacheable attributes.
#[allow(dead_code)] // wired for phase 2 (APIC/HPET); slice B ships the API only
pub unsafe fn ioremap(phys: PhysAddr, len: u64) -> Option<VirtAddr> {
    let va_offset = unsafe { &mut *core::ptr::addr_of_mut!(IOREMAP) }.reserve(phys, len)?;
    // Aligned base for the map_range call: strip the intra-page offset
    // so we map on whole 4 KiB grains, then hand the caller the original
    // offset-preserving VA.
    let base_va = VirtAddr(va_offset.as_u64() & !(PAGE_SIZE_4K - 1));
    let base_pa = PhysAddr(phys.as_u64() & !(PAGE_SIZE_4K - 1));
    let head = phys.as_u64() & (PAGE_SIZE_4K - 1);
    let round_len = paging_align_up(head + len, PAGE_SIZE_4K);
    let mut alloc = BuddyFrames;
    let mut mapper = current_mapper();
    unsafe {
        mapper
            .map_range(base_va, base_pa, round_len, paging::mmio_flags(),
                       MapMode::Fresh, &mut alloc)
            .ok()?;
    }
    Some(va_offset)
}

/// Patch a physmap-covered MMIO region UC in place. Preserves 2 MiB
/// page size. Returns the leaf count touched, or `MapError` if any VA
/// in the region is not currently mapped through the physmap (in which
/// case the caller wanted `ioremap`).
///
/// # Safety
/// Only sound after `install`; the physmap must cover `[phys, phys+len)`.
#[allow(dead_code)] // wired for phase 2; slice B ships the API only
pub unsafe fn patch_physmap_uc(phys: PhysAddr, len: u64) -> Result<usize, MapError> {
    let mut mapper = current_mapper();
    let n = unsafe { mapper.patch_physmap_uc(VirtAddr(HHDM_BASE), phys, len)? };
    // DESIGN §4.3: invlpg after every single-PTE edit, even MMIO patches.
    // Iterate over the whole range in 4 KiB grains; over-invalidating a
    // 2 MiB leaf is harmless and much simpler than mirroring the walk's
    // step choices back out here.
    let mut off: u64 = 0;
    let hhdm_end = HHDM_BASE.wrapping_add(phys.as_u64()).wrapping_add(len);
    let mut va = HHDM_BASE.wrapping_add(phys.as_u64());
    while va < hhdm_end {
        x86::invlpg(va);
        paging::tlb_shootdown_others(VirtAddr(va));
        off += PAGE_SIZE_4K;
        va = HHDM_BASE.wrapping_add(phys.as_u64()).wrapping_add(off);
    }
    Ok(n)
}

/// Fabricate a `Mapper` pointing at the current CR3. Only safe after
/// [`install`] has installed our own PML4.
#[allow(dead_code)]
fn current_mapper() -> Mapper {
    // CR3 low bits are flags (PCID etc); the physical address lives at
    // 12..52. Same mask used by the paging library on PTEs.
    let cr3 = x86::read_cr3() & paging::PTE_ADDR_MASK;
    unsafe { Mapper::new(PhysAddr(cr3), HHDM_BASE) }
}

// ------------------ install ------------------

/// Summary of what got mapped. Printed after `paging: cr3 ok` so the
/// e2e log records the numbers even when nothing goes wrong.
#[derive(Copy, Clone, Debug)]
pub struct PagingReport {
    pub map_end: u64,
    pub kernel_text_bytes: u64,
    pub kernel_rodata_bytes: u64,
    pub kernel_data_bytes: u64,
    pub duplicated_stack_entry: bool,
}

/// Build and install a fresh PML4 per DESIGN §4.3. Returns a summary
/// suitable for logging.
///
/// # Safety
/// - `kernel_phys_base` must be the physical base Limine loaded us at.
/// - The buddy allocator must be initialized (via `pmm_init::init`).
/// - Must run single-CPU with interrupts off (matches slice A's
///   invariant on `pmm_init`).
/// - `map_end` must be at most `PHYSMAP_CAP` after the caller's own
///   ceiling: we recompute internally, so the value passed here is
///   just the RAM high-water hint.
pub unsafe fn install(
    kernel_phys_base: u64,
    ram_high_water: u64,
    fb_phys_end: u64,
) -> PagingReport {
    let mut alloc = BuddyFrames;

    // Allocate + zero the fresh PML4.
    let root = alloc
        .alloc_frame()
        .expect("pmm out of frames while allocating PML4");
    let hhdm_ptr = root.as_u64().wrapping_add(HHDM_BASE) as *mut u64;
    for i in 0..paging::PTES_PER_TABLE {
        unsafe { hhdm_ptr.add(i).write_volatile(0) };
    }
    let mut mapper = unsafe { Mapper::new(root, HHDM_BASE) };

    // ---- 1. Kernel image, per section ----
    let vma_start = sym_addr(unsafe { &__kernel_vma_start });
    let text_bytes = map_kernel_section(
        &mut mapper, &mut alloc, kernel_phys_base, vma_start,
        sym_addr(unsafe { &__text_start }), sym_addr(unsafe { &__text_end }),
        paging::kernel_text_flags(),
    );
    let rodata_bytes = map_kernel_section(
        &mut mapper, &mut alloc, kernel_phys_base, vma_start,
        sym_addr(unsafe { &__rodata_start }), sym_addr(unsafe { &__rodata_end }),
        paging::kernel_rodata_flags(),
    );
    // Limine's request table sits before .text but must remain readable
    // (Limine walks it during handoff; we still read `is_supported`
    // etc after paging is up on future paths). Read-only + NX.
    let _limine_req_bytes = map_kernel_section(
        &mut mapper, &mut alloc, kernel_phys_base, vma_start,
        sym_addr(unsafe { &__limine_requests_start }),
        sym_addr(unsafe { &__limine_requests_end }),
        paging::kernel_rodata_flags(),
    );
    let data_bytes = map_kernel_section(
        &mut mapper, &mut alloc, kernel_phys_base, vma_start,
        sym_addr(unsafe { &__data_start }), sym_addr(unsafe { &__data_end }),
        paging::kernel_data_flags(),
    );

    // ---- 2. Physmap [0, map_end) with 2 MiB pages ----
    let map_end = physmap_extent(kernel_phys_base, ram_high_water, fb_phys_end);
    unsafe {
        mapper
            .map_range(
                VirtAddr(HHDM_BASE),
                PhysAddr(0),
                map_end,
                paging::physmap_flags(),
                MapMode::Fresh,
                &mut alloc,
            )
            .expect("physmap map_range");
    }

    // ---- 3. Low identity, 512 MiB, first 2 MiB executable ----
    // First 2 MiB: writable + executable (trampoline lives at 0x8000).
    // Rest: writable + NX. Everything with the GLOBAL bit so the TLB
    // survives CR3 reloads (DESIGN §4.3 TLB section).
    unsafe {
        mapper
            .map_page(
                VirtAddr(LOW_ID_BASE),
                PhysAddr(LOW_ID_BASE),
                PageFlags(PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::GLOBAL),
                PageSize::Size2M,
                MapMode::Fresh,
                &mut alloc,
            )
            .expect("low identity first 2 MiB");
        mapper
            .map_range(
                VirtAddr(LOW_ID_BASE + PAGE_SIZE_2M),
                PhysAddr(LOW_ID_BASE + PAGE_SIZE_2M),
                LOW_ID_SIZE - PAGE_SIZE_2M,
                PageFlags(
                    PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::GLOBAL | PageFlags::NX,
                ),
                MapMode::Fresh,
                &mut alloc,
            )
            .expect("low identity tail");
    }

    // ---- 4. Bootloader stack window ----
    // If the current RSP falls inside a region we already mapped
    // (typically Limine puts the stack in HHDM), the switch survives on
    // its own. Otherwise copy the covering PML4 entry from Limine's
    // active tables so the stack VA stays live across `mov cr3`.
    let rsp = x86::read_rsp();
    let duplicated = if mapper.translate(VirtAddr(rsp)).is_none() {
        unsafe { duplicate_pml4_entry_from_current(&mut mapper, VirtAddr(rsp)) };
        true
    } else {
        false
    };

    // ---- 5. EFER.NXE ----
    // Set NXE before installing so the NX bits in our leaves are honored
    // rather than treated as reserved-bit violations. DESIGN §7.5's AP
    // pitfall (missed NXE -> fault on first kernel page) applies here
    // too: our own kernel .rodata / .data / .bss all carry NX.
    let efer = x86::rdmsr(x86::IA32_EFER);
    if efer & x86::EFER_NXE == 0 {
        unsafe { x86::wrmsr(x86::IA32_EFER, efer | x86::EFER_NXE) };
    }

    // ---- 6. Install ----
    unsafe { x86::write_cr3(mapper.root().as_u64()) };

    PagingReport {
        map_end,
        kernel_text_bytes: text_bytes,
        kernel_rodata_bytes: rodata_bytes,
        kernel_data_bytes: data_bytes,
        duplicated_stack_entry: duplicated,
    }
}

/// Print the phase-1 §1.2 exit marker and the diagnostic follow-ups.
/// Split from `install` so a caller can order the marker after any of
/// its own follow-up printing.
pub fn report(r: &PagingReport) {
    // Exit-gate marker: DESIGN §2.6 shape.
    Serial::write_bytes(marker::PAGING_CR3_OK.as_bytes());
    Serial::write_bytes(b"\n");

    let _ = writeln!(
        Serial,
        "vibeOS: paging: map_end {:#x} ({} MiB)",
        r.map_end,
        r.map_end / (1024 * 1024)
    );
    let _ = writeln!(
        Serial,
        "vibeOS: paging: kernel .text {} B, .rodata {} B, .data+bss {} B",
        r.kernel_text_bytes, r.kernel_rodata_bytes, r.kernel_data_bytes
    );
    if r.duplicated_stack_entry {
        Serial::write_bytes(b"vibeOS: paging: bootloader stack pml4 entry duplicated\n");
    }
}

// ------------------ helpers ------------------

fn map_kernel_section(
    mapper: &mut Mapper,
    alloc: &mut BuddyFrames,
    kernel_phys_base: u64,
    vma_start: u64,
    sec_start: u64,
    sec_end: u64,
    flags: PageFlags,
) -> u64 {
    // Linker aligns every section boundary to 4 KiB (linker.ld) so
    // rounding here is defensive rather than load-bearing.
    let start = paging_align_down(sec_start, PAGE_SIZE_4K);
    let end = paging_align_up(sec_end, PAGE_SIZE_4K);
    if start >= end {
        return 0;
    }
    let len = end - start;
    let phys = kernel_phys_base + (start - vma_start);
    unsafe {
        mapper
            .map_range(VirtAddr(start), PhysAddr(phys), len, flags, MapMode::Fresh, alloc)
            .expect("kernel section map_range");
    }
    len
}

fn physmap_extent(kernel_phys_base: u64, ram_high_water: u64, fb_phys_end: u64) -> u64 {
    let vma_end = sym_addr(unsafe { &__kernel_vma_end });
    let vma_start = sym_addr(unsafe { &__kernel_vma_start });
    let kernel_phys_end = kernel_phys_base + (vma_end - vma_start);
    let mut hi = ram_high_water.max(kernel_phys_end).max(fb_phys_end);
    hi = paging_align_up(hi, PAGE_SIZE_2M);
    hi.min(PHYSMAP_CAP)
}

/// Walk Limine's active PML4 (via current CR3) and copy the entry
/// covering `va` into `mapper`'s new PML4. Preserves the sub-tree so
/// the stack keeps working after `mov cr3`.
///
/// Assumption: the destination PML4 slot is empty. That holds when the
/// mapper's own regions (physmap at 0xFFFF_8000_*, heap at 0xFFFF_C000_*,
/// KVA at 0xFFFF_D000_*, ioremap at 0xFFFF_E000_*, kernel image at
/// 0xFFFF_FFFF_8000_*, low identity at 0x0000_0000_*) do not share a
/// PML4 index with the bootloader's stack. In practice `_start` is only
/// called after Limine translates via HHDM which our physmap covers, so
/// `install`'s `translate(rsp)` returns `Some` and this function is
/// never invoked — but if it is, an overlap would silently overwrite
/// one of our slots with Limine's sub-tree. Trip loudly on that class
/// of failure rather than lose the physmap after `mov cr3`.
///
/// # Safety
/// Only sound during boot, with Limine's tables still installed as CR3.
unsafe fn duplicate_pml4_entry_from_current(mapper: &mut Mapper, va: VirtAddr) {
    let cr3 = x86::read_cr3() & paging::PTE_ADDR_MASK;
    let src = cr3.wrapping_add(HHDM_BASE) as *const u64;
    let idx = va.index(4);
    let entry = unsafe { src.add(idx).read_volatile() };
    if entry & PageFlags::PRESENT == 0 {
        // Nothing to duplicate; the switch would fault anyway. Better to
        // trip that fault immediately with a clear panic than to succeed
        // and lose the log.
        panic!("paging: rsp {:#x} not mapped in Limine's PML4 either", va.as_u64());
    }
    let dst = mapper
        .root()
        .as_u64()
        .wrapping_add(HHDM_BASE) as *mut u64;
    let existing = unsafe { dst.add(idx).read_volatile() };
    assert!(
        existing & PageFlags::PRESENT == 0,
        "paging: bootloader stack {:#x} shares pml4 slot {} with an already-mapped region; \
         Limine duplicate would clobber it (physmap? heap? kva? kernel?)",
        va.as_u64(),
        idx,
    );
    unsafe { dst.add(idx).write_volatile(entry) };
}

#[inline]
const fn paging_align_up(x: u64, a: u64) -> u64 {
    (x + a - 1) & !(a - 1)
}
#[inline]
const fn paging_align_down(x: u64, a: u64) -> u64 {
    x & !(a - 1)
}
