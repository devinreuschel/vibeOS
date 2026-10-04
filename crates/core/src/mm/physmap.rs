//! Physmap slot: where Limine's HHDM offset may sit (MEMORY.md §4.1).

/// One architecture's physmap slot (MEMORY.md §4.1, PORTABILITY.md §11.2).
/// The HHDM offset Limine reports must lie in it; RAM whose alias would
/// pass `end` is left to the §11.2 builder.
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
