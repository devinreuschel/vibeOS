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
//! makes explicit: the caller runs `invlpg` after every single-PTE edit
//! and, for a kernel-half edit, drops PT and calls
//! `paging::tlb_shootdown_others` (`Mapper` does neither), and no
//! splitting of 2 MiB pages when patching MMIO attributes. The kernel
//! tables are reached only through [`current_mapper`], whose
//! [`MapperGuard`] holds PT for as long as it lives, so holding the guard
//! is the proof that PT is held.

use core::fmt::Write;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use vibeos::lock::RANK_PT;
use vibeos::marker;
use vibeos::paging::{
    self, FrameAlloc, IoremapWindow, MapError, MapMode, Mapper, PAGE_SIZE_2M, PAGE_SIZE_4K,
    PageFlags, PageSize, PhysAddr, VirtAddr,
};
use vibeos::pmm::Frames;

use crate::boot::{self, BootInfo};
use crate::pmm_init;
use crate::sync_init::{SpinMutex, SpinMutexGuard};
use crate::x86;

// ------------------ constants matching DESIGN §4.1 ------------------

/// The physmap constants live in `vibeos::paging` (MEMORY.md §4.1); these
/// paths stay for the callers that name them here.
pub use vibeos::paging::{HHDM_BASE, PHYSMAP_CAP};

/// Low identity window base and size (DESIGN §4.1). It exists for AP
/// bring-up; [`teardown_identity`] removes all of it but the trampoline
/// page after `smp: done`.
const LOW_ID_BASE: u64 = 0;
const LOW_ID_SIZE: u64 = 512 * 1024 * 1024;

// Linker-provided section boundaries. Names match `linker.ld`.
unsafe extern "C" {
    static __kernel_vma_start: u8;
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

// SAFETY: `FrameAlloc`'s contract; the buddy hands out order-0 frames, and
// every buddy frame lies inside the physmap at `HHDM_BASE`, mapped writable
// (invariant I14, established at `mm::pmm_init::init`).
unsafe impl FrameAlloc for BuddyFrames {
    fn alloc_frame(&mut self) -> Option<Frames> {
        let f = pmm_init::with_buddy(|b| b.alloc(0))?;
        TABLE_PAGES.fetch_add(1, Ordering::Relaxed);
        Some(f)
    }
}

/// Table pages [`BuddyFrames`] has handed the kernel mapper. A kernel table
/// page is never freed (`Mapper` frees table pages only in
/// `free_user_half`), so this only grows.
pub(super) static TABLE_PAGES: AtomicUsize = AtomicUsize::new(0);

// ------------------ MMIO window (ioremap) ------------------

/// Page-table + ioremap lock. Rank PT. Taken only by [`current_mapper`];
/// callers that already hold its guard pass it to the `_locked`
/// map/unmap helpers and drop it before a shootdown wait.
pub(super) static PT: SpinMutex<IoremapWindow> =
    SpinMutex::with_rank(IoremapWindow::new(), RANK_PT);

/// A `Mapper` over the kernel PML4 together with the PT lock that guards
/// it. Built only by [`current_mapper`]; PT is held for the guard's life
/// and released when it drops. Derefs to `Mapper`.
pub struct MapperGuard {
    mapper: Mapper,
    pt: SpinMutexGuard<'static, IoremapWindow>,
}

impl MapperGuard {
    /// The ioremap window, which PT also guards.
    pub fn window(&mut self) -> &mut IoremapWindow {
        &mut self.pt
    }
}

impl Deref for MapperGuard {
    type Target = Mapper;
    fn deref(&self) -> &Mapper {
        &self.mapper
    }
}

impl DerefMut for MapperGuard {
    fn deref_mut(&mut self) -> &mut Mapper {
        &mut self.mapper
    }
}

/// Run `f` with PT held, handing it the kernel mapper guard.
pub fn with_pt<R>(f: impl FnOnce(&mut MapperGuard) -> R) -> R {
    let mut g = current_mapper();
    f(&mut g)
}

static MAP_END: AtomicU64 = AtomicU64::new(0);
static KERNEL_CR3: AtomicU64 = AtomicU64::new(0);

/// Physmap high water from [`install`]. BARs above this go through ioremap.
pub fn map_end() -> u64 {
    MAP_END.load(Ordering::Relaxed)
}

/// Kernel PML4 physical address. User address spaces copy the upper half
/// from this root; `current_mapper` always walks it so later kernel maps
/// do not land on a private user PML4 slot.
pub fn kernel_cr3() -> u64 {
    KERNEL_CR3.load(Ordering::Acquire)
}

/// Reserve and map `[phys, phys+len)` into the ioremap window with UC
/// attributes. Returns the VA (offset within the page preserved so a
/// device register at `phys+7` is at `va+7`).
///
/// # Safety
/// Caller vouches that `[phys, phys+len)` is real device MMIO and that
/// no aliased mapping through the physmap will be used to touch the
/// same registers with cacheable attributes.
pub unsafe fn ioremap(phys: PhysAddr, len: u64) -> Option<VirtAddr> {
    let (va_offset, base_va, round_len) = {
        let mut pt = current_mapper();
        let va_offset = pt.window().reserve(phys, len)?;
        let base_va = VirtAddr(va_offset.as_u64() & !(PAGE_SIZE_4K - 1));
        let base_pa = PhysAddr(phys.as_u64() & !(PAGE_SIZE_4K - 1));
        let head = phys.as_u64() & (PAGE_SIZE_4K - 1);
        let round_len = paging_align_up(head + len, PAGE_SIZE_4K);
        let mut alloc = BuddyFrames;
        // SAFETY: `Mapper::map_range`'s contract; the caller vouches that
        // `[phys, phys + len)` is device MMIO no cacheable alias touches
        // (this fn's `# Safety` contract, established here, invariant I17),
        // and `reserve` handed out VA no other mapping uses; `pt` holds the
        // page-table lock (invariant I226, established at
        // `mm::paging_init::current_mapper`).
        unsafe {
            pt.map_range(
                base_va,
                base_pa,
                round_len,
                paging::mmio_flags(),
                MapMode::Fresh,
                &mut alloc,
            )
            .ok()?;
        }
        Some((va_offset, base_va, round_len))
    }?;
    let mut off = 0u64;
    while off < round_len {
        paging::tlb_shootdown_others(VirtAddr(base_va.as_u64() + off));
        off += PAGE_SIZE_4K;
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
pub unsafe fn patch_physmap_uc(phys: PhysAddr, len: u64) -> Result<usize, MapError> {
    // SAFETY: `Mapper::patch_physmap_uc`'s contract; `install` has run, so
    // the kernel root `current_mapper` walks is the one the CPU uses (this
    // fn's `# Safety` contract, established here), and the guard holds the
    // page-table lock (invariant I226, established at
    // `mm::paging_init::current_mapper`).
    let n = unsafe { current_mapper().patch_physmap_uc(VirtAddr(HHDM_BASE), phys, len) }?;
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

/// Take PT and build a `Mapper` over the kernel PML4 inside the returned
/// guard. Before [`install`] it walks the tables CR3 holds; after, it
/// walks the kernel root and does not follow the current CR3 (a user
/// thread may have switched it).
pub(crate) fn current_mapper() -> MapperGuard {
    let pt = PT.lock();
    let cr3 = {
        let k = kernel_cr3();
        if k != 0 {
            k
        } else {
            x86::read_cr3() & paging::PTE_ADDR_MASK
        }
    };
    // SAFETY: the kernel PML4 that `paging_init::install` built (or, before
    // it, the boot tables CR3 holds) is never freed, and PT is held for the
    // guard's life, so this is the only live `Mapper` over it; established
    // here.
    let mapper = unsafe { Mapper::new(PhysAddr(cr3), HHDM_BASE) };
    MapperGuard { mapper, pt }
}

/// Map one 4 KiB leaf through `pt`. Local `invlpg` only; the caller
/// broadcasts shootdown after dropping `pt`.
///
/// # Safety
/// Same contract as `Mapper::map_page`.
pub unsafe fn map_4k_locked(
    pt: &mut MapperGuard,
    va: VirtAddr,
    pa: PhysAddr,
    flags: PageFlags,
) -> Result<(), MapError> {
    let mut alloc = BuddyFrames;
    // SAFETY: `Mapper::map_page`'s contract, which this fn's `# Safety`
    // passes on, established here; `pt` holds the page-table lock
    // (invariant I226, established at `mm::paging_init::current_mapper`).
    unsafe {
        pt.map_page(va, pa, flags, PageSize::Size4K, MapMode::Fresh, &mut alloc)?;
    }
    x86::invlpg(va.as_u64());
    Ok(())
}

/// Map one 4 KiB leaf in the live tables, drop PT, then shootdown.
///
/// # Safety
/// Same contract as `Mapper::map_page`. Must run after [`install`].
pub unsafe fn map_4k(va: VirtAddr, pa: PhysAddr, flags: PageFlags) -> Result<(), MapError> {
    // SAFETY: `map_4k_locked`'s contract, which this fn's `# Safety` passes
    // on, established here.
    with_pt(|pt| unsafe { map_4k_locked(pt, va, pa, flags) })?;
    paging::tlb_shootdown_others(va);
    Ok(())
}

/// Map one 2 MiB leaf through `pt`. Local `invlpg` only.
///
/// # Safety
/// Same contract as `Mapper::map_page`.
pub unsafe fn map_2m_locked(
    pt: &mut MapperGuard,
    va: VirtAddr,
    pa: PhysAddr,
    flags: PageFlags,
) -> Result<(), MapError> {
    let mut alloc = BuddyFrames;
    // SAFETY: `Mapper::map_page`'s contract, which this fn's `# Safety`
    // passes on, established here; `pt` holds the page-table lock
    // (invariant I226, established at `mm::paging_init::current_mapper`).
    unsafe {
        pt.map_page(va, pa, flags, PageSize::Size2M, MapMode::Fresh, &mut alloc)?;
    }
    x86::invlpg(va.as_u64());
    Ok(())
}

/// Map one 2 MiB leaf in the live tables, drop PT, then shootdown.
///
/// # Safety
/// Same contract as `Mapper::map_page`. Must run after [`install`].
pub unsafe fn map_2m(va: VirtAddr, pa: PhysAddr, flags: PageFlags) -> Result<(), MapError> {
    // SAFETY: `map_2m_locked`'s contract, which this fn's `# Safety` passes
    // on, established here.
    with_pt(|pt| unsafe { map_2m_locked(pt, va, pa, flags) })?;
    paging::tlb_shootdown_others(va);
    Ok(())
}

/// Map missing physmap leaves covering `[phys, phys+len)` as write-back.
/// Already-present leaves are left alone (so a WB framebuffer is not
/// UC-patched). Used for VGA BAR0 when the BAR outruns Limine's surface.
///
/// Returns whether `phys` itself translates through the HHDM physmap.
///
/// # Safety
/// `[phys, phys+len)` is RAM, or device memory that no other mapping
/// reaches with another memory type (invariant I17): the leaves this maps
/// are write-back.
pub unsafe fn ensure_physmap_wb(phys: PhysAddr, len: u64) -> bool {
    if len == 0 {
        return true;
    }
    let start = phys.as_u64() & !(PAGE_SIZE_4K - 1);
    if start >= PHYSMAP_CAP {
        return false;
    }
    let Some(raw_end) = phys.as_u64().checked_add(len) else {
        return false;
    };
    let end = paging_align_up(raw_end, PAGE_SIZE_4K).min(PHYSMAP_CAP);
    let flags = paging::physmap_flags();
    let mut p = start;
    while p < end {
        let va = VirtAddr(HHDM_BASE.wrapping_add(p));
        if let Some((_, sz, _)) = translate(va) {
            let span = sz.bytes();
            let next = (p & !(span - 1)).saturating_add(span);
            p = if next > p { next } else { p + PAGE_SIZE_4K };
            continue;
        }
        let rem = end - p;
        let try_2m = p & (PAGE_SIZE_2M - 1) == 0 && rem >= PAGE_SIZE_2M;
        if try_2m {
            // SAFETY: `map_2m`'s contract; `[p, p + 2 MiB)` lies in the range
            // this fn's `# Safety` contract vouches for (invariant I17,
            // established here), and its physmap VA was unmapped just now.
            match unsafe { map_2m(va, PhysAddr(p), flags) } {
                Ok(()) => {
                    p += PAGE_SIZE_2M;
                    continue;
                }
                Err(MapError::AlreadyMapped) => {
                    p += PAGE_SIZE_4K;
                    continue;
                }
                Err(MapError::Misaligned)
                | Err(MapError::NotMapped)
                | Err(MapError::OutOfFrames)
                | Err(MapError::PageSizeMismatch)
                | Err(MapError::NonCanonical) => {}
            }
        }
        // SAFETY: `map_4k`'s contract; `[p, p + 4 KiB)` lies in the range
        // this fn's `# Safety` contract vouches for (invariant I17,
        // established here), and a present leaf there returns
        // `AlreadyMapped` rather than being replaced.
        match unsafe { map_4k(va, PhysAddr(p), flags) } {
            Ok(()) | Err(MapError::AlreadyMapped) => p += PAGE_SIZE_4K,
            Err(MapError::Misaligned)
            | Err(MapError::NotMapped)
            | Err(MapError::OutOfFrames)
            | Err(MapError::PageSizeMismatch)
            | Err(MapError::NonCanonical) => break,
        }
    }
    translate(VirtAddr(HHDM_BASE.wrapping_add(phys.as_u64()))).is_some()
}

/// Unmap one leaf through `pt`. Local `invlpg` only.
///
/// # Safety
/// Caller will not use `va` until shootdown.
pub unsafe fn unmap_4k_locked(pt: &mut MapperGuard, va: VirtAddr) -> Option<(PhysAddr, PageSize)> {
    // SAFETY: `Mapper::unmap_page`'s contract; this fn runs the local
    // `invlpg`, and the caller shoots the other CPUs down before it uses
    // `va` (this fn's `# Safety` contract, established here).
    let r = unsafe { pt.unmap_page(va) }?;
    x86::invlpg(va.as_u64());
    Some(r)
}

pub fn translate(va: VirtAddr) -> Option<(PhysAddr, PageSize, PageFlags)> {
    current_mapper().translate(va)
}

/// DESIGN §4.1: every region asserts it is unmapped before claiming it.
pub fn assert_unmapped(start: VirtAddr, end: VirtAddr) {
    // The guard drops at the end of this statement, before any panic.
    let ok = current_mapper().range_unmapped(start, end);
    assert!(
        ok,
        "paging: region {:#x}..{:#x} already mapped",
        start.as_u64(),
        end.as_u64()
    );
}

/// Range dump of the live tables. Coalesces adjacent leaves (ROADMAP §1.7).
/// Returns the first write error; the writes after it are skipped.
pub fn dump_ranges_to(w: &mut impl Write) -> core::fmt::Result {
    {
        let mapper = current_mapper();
        let mut n = 0usize;
        let mut res = Ok(());
        let mut visit = |va: u64, len: u64, flags: PageFlags, size: PageSize| {
            n += 1;
            if res.is_err() {
                return;
            }
            let sz = match size {
                PageSize::Size4K => "4k",
                PageSize::Size2M => "2m",
            };
            res = writeln!(
                w,
                "vibeOS: pt: {va:#x}..{:#x} {sz} flags {:#x}",
                va + len,
                flags.0
            );
        };
        mapper.walk_ranges(VirtAddr(0), VirtAddr(0x0000_8000_0000_0000), &mut visit);
        mapper.walk_ranges(
            VirtAddr(0xFFFF_8000_0000_0000),
            VirtAddr(u64::MAX),
            &mut visit,
        );
        res?;
        writeln!(w, "vibeOS: pt: {n} ranges")
    }
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
/// - The buddy allocator must be initialized (via `pmm_init::init`).
/// - Must run single-CPU with interrupts off (matches slice A's
///   invariant on `pmm_init`).
pub unsafe fn install(info: &BootInfo) -> PagingReport {
    let kernel_phys_base = info.kernel_phys.start;
    let mut alloc = BuddyFrames;

    // Allocate + zero the fresh PML4. Its token moves into the kernel
    // mapper for good: the kernel PML4 is never freed.
    let Some(root) = alloc.alloc_frame() else {
        boot::halt_with("vibeOS: paging: no frame for the PML4");
    };
    let root = PhysAddr(root.into_entry());
    let hhdm_ptr = root.as_u64().wrapping_add(HHDM_BASE) as *mut u64;
    for i in 0..paging::PTES_PER_TABLE {
        // SAFETY: `root` is the order-0 buddy frame above, which Limine's
        // HHDM maps writable at `HHDM_BASE` (invariant I14, established at
        // `mm::pmm_init::init`); `i < 512` words stay inside it.
        unsafe { hhdm_ptr.add(i).write_volatile(0) };
    }
    // SAFETY: `Mapper::new`'s contract; `root` is the zeroed PML4 above,
    // owned by the kernel for good, and `HHDM_BASE` reaches every buddy
    // frame (invariant I14, established at `mm::pmm_init::init`). Boot is
    // single-CPU with IRQs off (this fn's `# Safety` contract), the one
    // case invariant I226 excepts; established here.
    let mut mapper = unsafe { Mapper::new(root, HHDM_BASE) };

    // ---- 1. Kernel image, per section ----
    // SAFETY: the linker script (`linker.ld`) defines this and each `&__*`
    // symbol below inside the kernel image, so a shared reference to its
    // first byte is valid; established here.
    let vma_start = sym_addr(unsafe { &__kernel_vma_start });
    let text_bytes = map_kernel_section(
        &mut mapper,
        &mut alloc,
        kernel_phys_base,
        vma_start,
        // SAFETY: a linker-defined symbol, as above; established here.
        sym_addr(unsafe { &__text_start }),
        // SAFETY: a linker-defined symbol, as above; established here.
        sym_addr(unsafe { &__text_end }),
        paging::kernel_text_flags(),
    );
    let rodata_bytes = map_kernel_section(
        &mut mapper,
        &mut alloc,
        kernel_phys_base,
        vma_start,
        // SAFETY: a linker-defined symbol, as above; established here.
        sym_addr(unsafe { &__rodata_start }),
        // SAFETY: a linker-defined symbol, as above; established here.
        sym_addr(unsafe { &__rodata_end }),
        paging::kernel_rodata_flags(),
    );
    // Limine's request table sits before .text but must remain readable
    // (Limine walks it during handoff; we still read `is_supported`
    // etc after paging is up on future paths). Read-only + NX.
    let _limine_req_bytes = map_kernel_section(
        &mut mapper,
        &mut alloc,
        kernel_phys_base,
        vma_start,
        // SAFETY: a linker-defined symbol, as above; established here.
        sym_addr(unsafe { &__limine_requests_start }),
        // SAFETY: a linker-defined symbol, as above; established here.
        sym_addr(unsafe { &__limine_requests_end }),
        paging::kernel_rodata_flags(),
    );
    let data_bytes = map_kernel_section(
        &mut mapper,
        &mut alloc,
        kernel_phys_base,
        vma_start,
        // SAFETY: a linker-defined symbol, as above; established here.
        sym_addr(unsafe { &__data_start }),
        // SAFETY: a linker-defined symbol, as above; established here.
        sym_addr(unsafe { &__data_end }),
        paging::kernel_data_flags(),
    );

    // ---- 2. Physmap [0, map_end) with 2 MiB pages ----
    let map_end = physmap_extent(info);
    // SAFETY: `Mapper::map_range`'s contract; the physmap maps `[0, map_end)`
    // once, at `HHDM_BASE`, in a root nothing else maps yet (invariant I14,
    // established here).
    let physmap = unsafe {
        mapper.map_range(
            VirtAddr(HHDM_BASE),
            PhysAddr(0),
            map_end,
            paging::physmap_flags(),
            MapMode::Fresh,
            &mut alloc,
        )
    };
    if physmap.is_err() {
        boot::halt_with("vibeOS: paging: physmap map failed");
    }

    // ---- 2b. Modules past map_end ----
    // Limine loads modules top-down, so with RAM past `PHYSMAP_CAP` the
    // initrd lies above `map_end`. Map each such module's pages at its
    // physmap alias, as `acpi_init` maps tables above `map_end`. A module
    // this fails for stays unmapped, and `fat_init::init` then leaves the
    // initrd not live (its recorded error state, DESIGN §2.5).
    for m in info.modules() {
        let start = m.start.max(map_end) & !(PAGE_SIZE_4K - 1);
        let end = paging_align_up(m.end, PAGE_SIZE_4K);
        if end <= start || end > vibeos::heap::HEAP_START - HHDM_BASE {
            continue;
        }
        // SAFETY: `Mapper::map_range`'s contract; `[start, end)` lies above
        // `map_end`, so nothing in this root maps it yet, and it maps once,
        // at its own physmap alias below the heap slot (invariant I14,
        // established here).
        let r = unsafe {
            mapper.map_range(
                VirtAddr(HHDM_BASE + start),
                PhysAddr(start),
                end - start,
                paging::physmap_flags(),
                MapMode::Fresh,
                &mut alloc,
            )
        };
        if r.is_err() {
            continue;
        }
    }

    // ---- 3. Low identity, 512 MiB ----
    // First 2 MiB as 4 KiB leaves: the trampoline page (DESIGN §7.3)
    // present, read-only and executable, and not GLOBAL, so the teardown's
    // flush need not reach it; every other page writable, NX and GLOBAL.
    // Rest: 2 MiB leaves, writable + NX + GLOBAL, so the TLB survives CR3
    // reloads (DESIGN §4.3 TLB section) until `teardown_identity`.
    let tramp = info.trampoline_page;
    let low_4k = |va: u64| {
        if Some(va) == tramp {
            PageFlags(PageFlags::PRESENT)
        } else {
            PageFlags(PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::GLOBAL | PageFlags::NX)
        }
    };
    // SAFETY: `Mapper::map_page`'s and `map_range`'s contract; the low
    // identity window maps `[0, 512 MiB)` onto itself once, in the low half
    // no other kernel mapping uses, for the AP trampoline (DESIGN §4.1);
    // established here.
    let low_id = unsafe {
        (0..PAGE_SIZE_2M)
            .step_by(PAGE_SIZE_4K as usize)
            .try_for_each(|va| {
                mapper.map_page(
                    VirtAddr(LOW_ID_BASE + va),
                    PhysAddr(LOW_ID_BASE + va),
                    low_4k(LOW_ID_BASE + va),
                    PageSize::Size4K,
                    MapMode::Fresh,
                    &mut alloc,
                )
            })
            .and_then(|()| {
                mapper.map_range(
                    VirtAddr(LOW_ID_BASE + PAGE_SIZE_2M),
                    PhysAddr(LOW_ID_BASE + PAGE_SIZE_2M),
                    LOW_ID_SIZE - PAGE_SIZE_2M,
                    PageFlags(
                        PageFlags::PRESENT
                            | PageFlags::WRITABLE
                            | PageFlags::GLOBAL
                            | PageFlags::NX,
                    ),
                    MapMode::Fresh,
                    &mut alloc,
                )
            })
    };
    if low_id.is_err() {
        boot::halt_with("vibeOS: paging: low identity map failed");
    }

    // ---- 4. Bootloader stack window ----
    // If the current RSP falls inside a region we already mapped
    // (typically Limine puts the stack in HHDM), the switch survives on
    // its own. Otherwise copy the covering PML4 entry from Limine's
    // active tables so the stack VA stays live across `mov cr3`.
    let rsp = x86::read_rsp();
    let duplicated = if mapper.translate(VirtAddr(rsp)).is_none() {
        // SAFETY: `duplicate_pml4_entry_from_current`'s contract; boot has
        // not yet written CR3, so Limine's tables are still installed
        // (this fn's `# Safety` contract, established here).
        unsafe { duplicate_pml4_entry_from_current(&mut mapper, VirtAddr(rsp)) };
        true
    } else {
        false
    };

    // ---- 5. EFER.NXE ----
    // Set NXE before installing so the NX bits in our leaves are honored
    // rather than treated as reserved-bit violations. DESIGN §7.3's AP
    // pitfall (missed NXE -> fault on first kernel page) applies here
    // too: our own kernel .rodata / .data / .bss all carry NX.
    let efer = x86::rdmsr(x86::IA32_EFER);
    if efer & x86::EFER_NXE == 0 {
        // SAFETY: setting EFER.NXE only enables the NX bit in PTEs; every
        // table live now (Limine's) and the new ones treat NX as intended,
        // established here.
        unsafe { x86::wrmsr(x86::IA32_EFER, efer | x86::EFER_NXE) };
    }

    // ---- 6. Install ----
    KERNEL_CR3.store(mapper.root().as_u64(), Ordering::Release);
    // SAFETY: the new root maps the kernel image, the physmap (so every
    // pointer computed as `phys + HHDM_BASE` stays valid, invariant I14),
    // the low identity window and the boot stack, so execution continues
    // across the switch; established here.
    unsafe { x86::write_cr3(mapper.root().as_u64()) };
    MAP_END.store(map_end, Ordering::Relaxed);

    PagingReport {
        map_end,
        kernel_text_bytes: text_bytes,
        kernel_rodata_bytes: rodata_bytes,
        kernel_data_bytes: data_bytes,
        duplicated_stack_entry: duplicated,
    }
}

/// The low identity window's range, virtual and physical alike (DESIGN
/// §4.1).
pub(crate) fn identity_window() -> core::ops::Range<u64> {
    LOW_ID_BASE..LOW_ID_BASE + LOW_ID_SIZE
}

/// Identity leaves unmapped per PT hold in [`teardown_identity`] (DESIGN
/// §2.9 rule 2).
const TEARDOWN_BATCH: usize = 64;

/// Remove the low identity window after `smp: done`, all but the leaf that
/// maps `keep` (the trampoline page), then flush the whole TLB, global
/// entries included, on this CPU and every other online one, so a kernel
/// read of VA 0 faults on each (DESIGN §4.1, §4.3). The page tables stay:
/// a kernel table page is never freed.
///
/// # Safety
/// Once, after every AP is up; nothing on any CPU uses an identity address
/// but the trampoline page from here, and this CPU's stack lies outside
/// the window.
pub unsafe fn teardown_identity(keep: Option<u64>) {
    let window = identity_window();
    let mut va = window.start;
    while va < window.end {
        let mut pt = current_mapper();
        let mut n = 0usize;
        while n < TEARDOWN_BATCH && va < window.end {
            let size = match pt.translate(VirtAddr(va)) {
                Some((_, PageSize::Size2M, _)) => PAGE_SIZE_2M,
                Some((_, PageSize::Size4K, _)) | None => PAGE_SIZE_4K,
            };
            if keep != Some(va) {
                // SAFETY: `Mapper::unmap_page`'s contract; the caller keeps
                // every CPU off the window (this fn's `# Safety` contract),
                // and the whole-TLB flush below runs on every CPU before
                // this returns; established here.
                let _unmapped = unsafe { pt.unmap_page(VirtAddr(va)) };
                n += 1;
            }
            va += size;
        }
    }
    flush_tlb_all_local();
    crate::ipi_init::call_mask(u64::MAX, flush_tlb_all_ipi, core::ptr::null_mut(), true);
}

/// Drop every TLB entry on this CPU, global ones included: toggle
/// `CR4.PGE` when it is set, else reload CR3 (Intel SDM Vol. 3A §4.10.4.1).
fn flush_tlb_all_local() {
    let cr4 = x86::read_cr4();
    if cr4 & x86::CR4_PGE != 0 {
        // SAFETY: clearing and restoring `CR4.PGE` changes nothing but
        // which TLB entries survive; every other CR4 bit is written back as
        // read; established here.
        unsafe {
            x86::write_cr4(cr4 & !x86::CR4_PGE);
            x86::write_cr4(cr4);
        }
    } else {
        // SAFETY: reloading CR3 with its own value keeps the same tables;
        // established here.
        unsafe { x86::write_cr3(x86::read_cr3()) };
    }
}

/// [`flush_tlb_all_local`] on a CPU `teardown_identity` calls through
/// `ipi_init::call_mask`: it takes no lock and allocates nothing.
fn flush_tlb_all_ipi(_: *mut ()) {
    flush_tlb_all_local();
}

/// Print the phase-1 §1.2 exit marker and the diagnostic follow-ups.
/// Split from `install` so a caller can order the marker after any of
/// its own follow-up printing.
pub fn report(r: &PagingReport) {
    // Exit-gate marker: DESIGN §2.6 shape.
    crate::marker!(marker::PAGING_CR3_OK);

    crate::marker!(
        "vibeOS: paging: map_end {:#x} ({} MiB)",
        r.map_end,
        r.map_end / (1024 * 1024)
    );
    crate::marker!(
        "vibeOS: paging: kernel .text {} B, .rodata {} B, .data+bss {} B",
        r.kernel_text_bytes,
        r.kernel_rodata_bytes,
        r.kernel_data_bytes
    );
    if r.duplicated_stack_entry {
        crate::marker!("vibeOS: paging: bootloader stack pml4 entry duplicated");
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
    // SAFETY: `Mapper::map_range`'s contract; `[phys, phys + len)` is the
    // section's own load address in the kernel image, mapped once at its
    // link VA in a root nothing else maps yet (`install`'s contract);
    // established here.
    let r = unsafe {
        mapper.map_range(
            VirtAddr(start),
            PhysAddr(phys),
            len,
            flags,
            MapMode::Fresh,
            alloc,
        )
    };
    if r.is_err() {
        boot::halt_with("vibeOS: paging: kernel section map failed");
    }
    len
}

/// Covers usable RAM, the kernel image, every framebuffer, and every
/// Limine module (the initrd, which `fat_init` reads and writes in place),
/// capped at [`PHYSMAP_CAP`]. Never raw memmap entries (DESIGN §4.1).
fn physmap_extent(info: &BootInfo) -> u64 {
    let hi = info
        .usable()
        .map(|r| r.end)
        .chain(info.framebuffers().map(|fb| fb.phys + fb.size))
        .chain(info.modules().map(|m| m.end))
        .fold(info.kernel_phys.end, u64::max);
    paging_align_up(hi, PAGE_SIZE_2M).min(PHYSMAP_CAP)
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
#[allow(
    clippy::panic,
    reason = "boot protocol invariant (DESIGN §3.3): Limine's PML4 maps the boot stack"
)]
unsafe fn duplicate_pml4_entry_from_current(mapper: &mut Mapper, va: VirtAddr) {
    let cr3 = x86::read_cr3() & paging::PTE_ADDR_MASK;
    let src = cr3.wrapping_add(HHDM_BASE) as *const u64;
    let idx = va.index(4);
    // SAFETY: CR3 still holds Limine's PML4 (this fn's `# Safety` contract,
    // established here), which Limine's HHDM maps at `HHDM_BASE`, and
    // `idx < 512`.
    let entry = unsafe { src.add(idx).read_volatile() };
    if entry & PageFlags::PRESENT == 0 {
        // Nothing to duplicate; the switch would fault anyway. Better to
        // trip that fault immediately with a clear panic than to succeed
        // and lose the log.
        panic!(
            "paging: rsp {:#x} not mapped in Limine's PML4 either",
            va.as_u64()
        );
    }
    let dst = mapper.root().as_u64().wrapping_add(HHDM_BASE) as *mut u64;
    // SAFETY: `mapper`'s root is the new PML4, a buddy frame reached at
    // `HHDM_BASE` (`Mapper::new`'s contract, invariant I14), and `idx < 512`;
    // established here.
    let existing = unsafe { dst.add(idx).read_volatile() };
    assert!(
        existing & PageFlags::PRESENT == 0,
        "paging: bootloader stack {:#x} shares pml4 slot {} with an already-mapped region; \
         Limine duplicate would clobber it (physmap? heap? kva? kernel?)",
        va.as_u64(),
        idx,
    );
    // SAFETY: as above, `dst` is the new PML4, and boot is single-CPU, the
    // case invariant I226 excepts; the assert kept the slot empty,
    // established here.
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
