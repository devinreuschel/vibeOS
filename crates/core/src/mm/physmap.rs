//! Physmap slot and the portable RAM-only builder (MEMORY.md §4.1, §11.2).

use core::ops::Range;

use crate::paging::{PAGE_SIZE_1G, PAGE_SIZE_2M, PageSize, PhysAddr, VirtAddr};

/// One architecture's physmap slot (MEMORY.md §4.1, PORTABILITY.md §11.2).
/// The HHDM offset Limine reports must lie in it; RAM whose alias would
/// pass `end` is left out of the physmap and the buddy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysmapSlot {
    pub start: u64,
    pub end: u64,
}

impl PhysmapSlot {
    pub const fn contains(self, offset: u64) -> bool {
        offset >= self.start && offset < self.end
    }

    /// Bytes from `offset` to `end`, or 0 when `offset` is outside.
    pub const fn room(self, offset: u64) -> u64 {
        if self.contains(offset) {
            self.end - offset
        } else {
            0
        }
    }
}

/// x86_64 physmap slot: `0xFFFF_8000_0000_0000` – `0xFFFF_C000_0000_0000`.
pub const PHYSMAP_X86_64: PhysmapSlot = PhysmapSlot {
    start: 0xFFFF_8000_0000_0000,
    end: 0xFFFF_C000_0000_0000,
};

/// aarch64 physmap slot: `0xFFFF_0000_0000_0000` – `0xFFFF_6000_0000_0000`.
pub const PHYSMAP_AARCH64: PhysmapSlot = PhysmapSlot {
    start: 0xFFFF_0000_0000_0000,
    end: 0xFFFF_6000_0000_0000,
};

pub const TIB: u64 = 1 << 40;

/// Whether `offset` lies in `slot`. `boot::capture` halts when this is false.
pub const fn hhdm_in_slot(offset: u64, slot: PhysmapSlot) -> bool {
    slot.contains(offset)
}

/// How much of physical `[0, ram_end)` aliases inside `slot` at `offset`.
/// `mapped` is the physical end that fits; `leftover` is the rest in bytes.
pub const fn physmap_ram_fit(slot: PhysmapSlot, offset: u64, ram_end: u64) -> (u64, u64) {
    let room = slot.room(offset);
    let mapped = if ram_end < room { ram_end } else { room };
    (mapped, ram_end.saturating_sub(mapped))
}

/// Clip physical `[start, end)` to what aliases inside `slot` at `offset`.
/// Returns `(mapped_end, leftover_bytes)`: map `[start, mapped_end)`, and
/// `leftover_bytes` is the tail that would pass the slot.
pub const fn clip_range_to_slot(
    slot: PhysmapSlot,
    offset: u64,
    start: u64,
    end: u64,
) -> (u64, u64) {
    if start >= end {
        return (start, 0);
    }
    let room = slot.room(offset);
    let Some(alias) = offset.checked_add(start) else {
        return (start, end - start);
    };
    if alias >= slot.end || room == 0 {
        return (start, end - start);
    }
    let max_phys = match offset.checked_add(room) {
        Some(_) => room,
        None => return (start, end - start),
    };
    if start >= max_phys {
        return (start, end - start);
    }
    let mapped_end = if end < max_phys { end } else { max_phys };
    (mapped_end, end - mapped_end)
}

/// MiB in the leftover-RAM marker: `leftover_bytes.div_ceil(1 MiB)`.
pub const fn leftover_mib(leftover_bytes: u64) -> u64 {
    leftover_bytes.div_ceil(1 << 20)
}

/// Whether physical `phys` aliases inside `slot` at `offset`.
pub const fn phys_in_slot(slot: PhysmapSlot, offset: u64, phys: u64) -> bool {
    match offset.checked_add(phys) {
        Some(va) => va >= slot.start && va < slot.end,
        None => false,
    }
}

/// One coalesced physmap run: `[pa, pa+len)` at `va` with leaf `size`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysmapRun {
    pub pa: PhysAddr,
    pub va: VirtAddr,
    pub len: u64,
    pub size: PageSize,
}

/// What [`walk_physmap`] mapped and what it dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysmapWalk {
    pub mapped: u64,
    pub leftover: u64,
}

/// Walk RAM-typed `[start, end)` ranges and emit coalesced runs at the
/// largest page each span's alignment allows (1 GiB when `have_1g`),
/// except `kernel` which is always 4 KiB. Ranges whose alias would pass
/// `slot` are leftover. Nothing that is not in `ram` is emitted.
pub fn walk_physmap<I, F>(
    slot: PhysmapSlot,
    offset: u64,
    ram: I,
    kernel: Range<u64>,
    have_1g: bool,
    mut emit: F,
) -> PhysmapWalk
where
    I: IntoIterator<Item = Range<u64>>,
    F: FnMut(PhysmapRun),
{
    let mut mapped = 0u64;
    let mut leftover = 0u64;
    for r in ram {
        if r.start >= r.end {
            continue;
        }
        let (hi, left) = clip_range_to_slot(slot, offset, r.start, r.end);
        leftover = leftover.saturating_add(left);
        if r.start >= hi {
            continue;
        }
        mapped = mapped.saturating_add(hi - r.start);
        emit_clipped(offset, r.start, hi, &kernel, have_1g, &mut emit);
    }
    PhysmapWalk { mapped, leftover }
}

fn emit_clipped<F: FnMut(PhysmapRun)>(
    offset: u64,
    start: u64,
    end: u64,
    kernel: &Range<u64>,
    have_1g: bool,
    emit: &mut F,
) {
    let mut p = start;
    while p < end {
        let k0 = kernel.start.max(p);
        let k1 = kernel.end.min(end);
        if k0 < k1 && p < k0 {
            emit_span(offset, p, k0, false, have_1g, emit);
            p = k0;
        } else if k0 < k1 && p < k1 {
            emit_span(offset, p, k1, true, have_1g, emit);
            p = k1;
        } else {
            emit_span(offset, p, end, false, have_1g, emit);
            p = end;
        }
    }
}

fn emit_span<F: FnMut(PhysmapRun)>(
    offset: u64,
    mut start: u64,
    end: u64,
    force_4k: bool,
    have_1g: bool,
    emit: &mut F,
) {
    while start < end {
        let rem = end - start;
        let size = choose_size(start, rem, force_4k, have_1g);
        let step = size.bytes();
        let mut len = step;
        while start.saturating_add(len) < end {
            let next = start + len;
            let nrem = end - next;
            if choose_size(next, nrem, force_4k, have_1g) != size {
                break;
            }
            match len.checked_add(step) {
                Some(n) => len = n,
                None => break,
            }
        }
        let va = match offset.checked_add(start) {
            Some(v) => v,
            None => break,
        };
        emit(PhysmapRun {
            pa: PhysAddr(start),
            va: VirtAddr(va),
            len,
            size,
        });
        start = match start.checked_add(len) {
            Some(n) => n,
            None => break,
        };
    }
}

fn choose_size(pa: u64, rem: u64, force_4k: bool, have_1g: bool) -> PageSize {
    if force_4k {
        return PageSize::Size4K;
    }
    if have_1g && pa & (PAGE_SIZE_1G - 1) == 0 && rem >= PAGE_SIZE_1G {
        return PageSize::Size1G;
    }
    if pa & (PAGE_SIZE_2M - 1) == 0 && rem >= PAGE_SIZE_2M {
        return PageSize::Size2M;
    }
    PageSize::Size4K
}

/// Limine memory-map types, as `limine::memmap` numbers them.
pub const MEMMAP_USABLE: u64 = 0;
pub const MEMMAP_RESERVED: u64 = 1;
pub const MEMMAP_ACPI_RECLAIMABLE: u64 = 2;
pub const MEMMAP_ACPI_NVS: u64 = 3;
pub const MEMMAP_BAD_MEMORY: u64 = 4;
pub const MEMMAP_BOOTLOADER_RECLAIMABLE: u64 = 5;
pub const MEMMAP_EXECUTABLE_AND_MODULES: u64 = 6;
pub const MEMMAP_FRAMEBUFFER: u64 = 7;
pub const MEMMAP_MAPPED_RESERVED: u64 = 8;

/// One firmware memory-map entry: physical `[base, base + len)` of type `ty`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemmapEntry {
    pub base: u64,
    pub len: u64,
    pub ty: u64,
}

/// Whether `ty` is a RAM-typed memory-map entry the physmap maps
/// (MEMORY.md §4.1): usable, bootloader-reclaimable, executable and
/// modules, ACPI reclaimable, ACPI NVS.
const fn is_ram_memmap(ty: u64) -> bool {
    matches!(
        ty,
        MEMMAP_USABLE
            | MEMMAP_BOOTLOADER_RECLAIMABLE
            | MEMMAP_EXECUTABLE_AND_MODULES
            | MEMMAP_ACPI_RECLAIMABLE
            | MEMMAP_ACPI_NVS
    )
}

/// Physical ranges of the RAM-typed entries. Anything else, including
/// [`MEMMAP_RESERVED`] and [`MEMMAP_FRAMEBUFFER`], is dropped. Ends
/// saturate: the map is firmware input.
pub fn ram_ranges<I>(entries: I) -> impl Iterator<Item = Range<u64>>
where
    I: IntoIterator<Item = MemmapEntry>,
{
    entries.into_iter().filter_map(|e| {
        if is_ram_memmap(e.ty) {
            Some(e.base..e.base.saturating_add(e.len))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paging::PAGE_SIZE_4K;

    #[test]
    fn hhdm_offset_outside_slot_each_arch() {
        assert!(!hhdm_in_slot(
            PHYSMAP_X86_64.start.wrapping_sub(1),
            PHYSMAP_X86_64
        ));
        assert!(!hhdm_in_slot(PHYSMAP_X86_64.end, PHYSMAP_X86_64));
        assert!(hhdm_in_slot(PHYSMAP_X86_64.start, PHYSMAP_X86_64));
        assert!(hhdm_in_slot(PHYSMAP_X86_64.end - 1, PHYSMAP_X86_64));

        assert!(!hhdm_in_slot(
            PHYSMAP_AARCH64.start.wrapping_sub(1),
            PHYSMAP_AARCH64
        ));
        assert!(!hhdm_in_slot(PHYSMAP_AARCH64.end, PHYSMAP_AARCH64));
        assert!(hhdm_in_slot(PHYSMAP_AARCH64.start, PHYSMAP_AARCH64));
        assert!(hhdm_in_slot(PHYSMAP_AARCH64.end - 1, PHYSMAP_AARCH64));
    }

    #[test]
    fn physmap_ram_fit_40_tib() {
        const RAM: u64 = 40 * TIB;
        let cases = [
            (PHYSMAP_X86_64, 0, RAM, 0),
            (PHYSMAP_X86_64, 20 * TIB, RAM, 0),
            (PHYSMAP_X86_64, 31 * TIB, 33 * TIB, 7 * TIB),
            (PHYSMAP_AARCH64, 0, RAM, 0),
            (PHYSMAP_AARCH64, 20 * TIB, RAM, 0),
            (PHYSMAP_AARCH64, 31 * TIB, RAM, 0),
        ];
        for (slot, add, mapped, leftover) in cases {
            let offset = slot.start + add;
            assert_eq!(
                physmap_ram_fit(slot, offset, RAM),
                (mapped, leftover),
                "slot {slot:?} +{add:#x}"
            );
        }
    }

    #[test]
    fn leftover_mib_rounds_up() {
        assert_eq!(leftover_mib(0), 0);
        assert_eq!(leftover_mib(1), 1);
        assert_eq!(leftover_mib(1 << 20), 1);
        assert_eq!(leftover_mib((1 << 20) + 1), 2);
    }

    fn collect(
        slot: PhysmapSlot,
        offset: u64,
        ram: impl IntoIterator<Item = Range<u64>>,
        kernel: Range<u64>,
        have_1g: bool,
    ) -> (PhysmapWalk, Vec<PhysmapRun>) {
        let mut runs = Vec::new();
        let w = walk_physmap(slot, offset, ram, kernel, have_1g, |r| runs.push(r));
        (w, runs)
    }

    fn any_pa(runs: &[PhysmapRun], pa: u64) -> bool {
        runs.iter()
            .any(|r| pa >= r.pa.0 && pa < r.pa.0.saturating_add(r.len))
    }

    fn ent(base: u64, len: u64, ty: u64) -> MemmapEntry {
        MemmapEntry { base, len, ty }
    }

    /// RAM the filter must keep. Lengths are the expected mapped total:
    /// adding `MEMMAP_RESERVED` (or any other non-RAM type) makes
    /// `ram_ranges` emit more bytes than this.
    fn ram_entries() -> [MemmapEntry; 5] {
        [
            ent(0, 0x1000_0000, MEMMAP_USABLE),
            ent(0x1000_0000, PAGE_SIZE_2M, MEMMAP_BOOTLOADER_RECLAIMABLE),
            ent(0x2000_0000, 0x10_0000, MEMMAP_EXECUTABLE_AND_MODULES),
            ent(0x3000_0000, PAGE_SIZE_4K, MEMMAP_ACPI_RECLAIMABLE),
            ent(0x4000_0000, 2 * PAGE_SIZE_4K, MEMMAP_ACPI_NVS),
        ]
    }

    #[test]
    fn builder_skips_multi_tib_mmio() {
        // Inside the x86_64 slot, so a filter that accepts RESERVED maps
        // these bytes instead of leaving them out.
        let reserved_base = 2 * TIB;
        let reserved_len = 8 * TIB;
        let fb = 0x40_0000_0000u64;
        let fb_base = fb - PAGE_SIZE_4K;
        let fb_len = PAGE_SIZE_2M + 2 * PAGE_SIZE_4K;
        let ram = ram_entries();
        let ram_bytes: u64 = ram.iter().map(|e| e.len).sum();
        let mut entries = ram.to_vec();
        entries.extend([
            ent(reserved_base, reserved_len, MEMMAP_RESERVED),
            ent(fb_base, fb_len, MEMMAP_FRAMEBUFFER),
            ent(0x5000_0000, PAGE_SIZE_4K, MEMMAP_BAD_MEMORY),
            ent(0x6000_0000, PAGE_SIZE_4K, MEMMAP_MAPPED_RESERVED),
        ]);
        let ranges: Vec<_> = ram_ranges(entries).collect();
        let covers = |pa: u64| ranges.iter().any(|r| r.contains(&pa));
        assert_eq!(
            ranges.iter().map(|r| r.end - r.start).sum::<u64>(),
            ram_bytes
        );
        for e in &ram {
            assert!(covers(e.base), "RAM type {:#x} dropped", e.ty);
        }
        assert!(!covers(reserved_base));
        assert!(!covers(reserved_base + reserved_len / 2));
        assert!(!covers(fb_base));
        assert!(!covers(fb));
        assert!(!covers(fb + PAGE_SIZE_2M));
        assert!(!covers(0x5000_0000));
        assert!(!covers(0x6000_0000));

        let (w, runs) = collect(PHYSMAP_X86_64, PHYSMAP_X86_64.start, ranges, 0..0, true);
        assert_eq!(w.mapped, ram_bytes);
        assert_eq!(w.leftover, 0);
        assert!(any_pa(&runs, 0));
        assert!(!any_pa(&runs, reserved_base));
        assert!(!any_pa(&runs, reserved_base + reserved_len / 2));
        assert!(!any_pa(&runs, fb));
        assert!(!any_pa(&runs, fb_base));
        assert!(!any_pa(&runs, fb + PAGE_SIZE_2M));
    }

    #[test]
    fn builder_splits_ram_sharing_2m_with_mmio() {
        let off = PHYSMAP_X86_64.start;
        // RAM [0, 2 MiB + 4 KiB), MMIO in the next bytes of that 2 MiB
        // block is not in `ram`, so the tail of the first 2 MiB is 4 KiB
        // only where RAM actually is.
        let ram = [
            0u64..PAGE_SIZE_2M + PAGE_SIZE_4K,
            2 * PAGE_SIZE_2M..3 * PAGE_SIZE_2M,
        ];
        let (_, runs) = collect(PHYSMAP_X86_64, off, ram, 0..0, false);
        assert!(
            runs.iter()
                .any(|r| r.size == PageSize::Size2M && r.pa.0 == 0)
        );
        assert!(runs.iter().any(|r| {
            r.size == PageSize::Size4K && r.pa.0 == PAGE_SIZE_2M && r.len == PAGE_SIZE_4K
        }));
        assert!(!any_pa(&runs, PAGE_SIZE_2M + PAGE_SIZE_4K));
    }

    #[test]
    fn builder_skips_framebuffers_outside_ram() {
        let fb = 0x40_0000_0000u64;
        let fb2 = fb - PAGE_SIZE_4K;
        // Covers `fb2`, `fb`, and the page at `fb + 2 MiB`.
        let fb_len = (fb + PAGE_SIZE_2M + PAGE_SIZE_4K) - fb2;
        let entries = [
            ent(0, 0x80_0000, MEMMAP_USABLE),
            ent(fb2, fb_len, MEMMAP_FRAMEBUFFER),
        ];
        let (_, runs) = collect(
            PHYSMAP_X86_64,
            PHYSMAP_X86_64.start,
            ram_ranges(entries),
            0..0,
            true,
        );
        assert!(any_pa(&runs, 0));
        assert!(!any_pa(&runs, fb));
        assert!(!any_pa(&runs, fb + PAGE_SIZE_2M));
        assert!(!any_pa(&runs, fb2));
        assert!(!any_pa(&runs, fb2 + PAGE_SIZE_4K));
    }

    #[test]
    fn builder_drops_ram_past_slot() {
        let off = PHYSMAP_X86_64.start + 31 * TIB;
        let (w, runs) = collect(
            PHYSMAP_X86_64,
            off,
            core::iter::once(0u64..70 * TIB),
            0..0,
            true,
        );
        let (fit, left) = physmap_ram_fit(PHYSMAP_X86_64, off, 70 * TIB);
        assert_eq!(w.mapped, fit);
        assert_eq!(w.leftover, left);
        assert!(left > 0);
        assert!(!any_pa(&runs, fit));
        assert_eq!(leftover_mib(left), left.div_ceil(1 << 20));
    }

    #[test]
    fn builder_maps_kernel_span_at_4k() {
        let off = PHYSMAP_X86_64.start;
        let kernel = 0x10_0000u64..0x30_0000;
        let (_, runs) = collect(
            PHYSMAP_X86_64,
            off,
            core::iter::once(0u64..0x40_0000),
            kernel.clone(),
            true,
        );
        for r in &runs {
            let overlap = r.pa.0 < kernel.end && kernel.start < r.pa.0 + r.len;
            if overlap {
                assert_eq!(
                    r.size,
                    PageSize::Size4K,
                    "kernel run {:#x}+{:#x}",
                    r.pa.0,
                    r.len
                );
            }
        }
        assert!(
            runs.iter()
                .any(|r| r.pa.0 == kernel.start && r.size == PageSize::Size4K)
        );
    }

    #[test]
    fn builder_uses_1g_when_aligned() {
        let off = PHYSMAP_X86_64.start;
        let ram = 0u64..2 * PAGE_SIZE_1G;
        let (_, runs) = collect(
            PHYSMAP_X86_64,
            off,
            core::iter::once(ram.clone()),
            0..0,
            true,
        );
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].size, PageSize::Size1G);
        assert_eq!(runs[0].len, 2 * PAGE_SIZE_1G);
        let (_, runs) = collect(PHYSMAP_X86_64, off, core::iter::once(ram), 0..0, false);
        assert!(runs.iter().all(|r| r.size == PageSize::Size2M));
    }
}
