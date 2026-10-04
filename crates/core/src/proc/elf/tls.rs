//! TLS variant I (aarch64) layout helpers (ROADMAP §11.6).

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use crate::paging::PAGE_SIZE_4K;

use super::TlsSeg;

impl TlsSeg {
    /// Variant I (aarch64): 16-byte TCB plus the TLS block, page-rounded.
    /// The thread pointer is the TCB; TLS data sits at `TP + 16`.
    pub fn map_len_variant_i(&self) -> Option<u64> {
        let extra = self.align.max(16).saturating_sub(1);
        let need = 16u64
            .checked_add(extra)?
            .checked_add(self.block_len()?)?
            .max(PAGE_SIZE_4K);
        Some(need.checked_add(PAGE_SIZE_4K - 1)? & !(PAGE_SIZE_4K - 1))
    }

    /// Variant I thread pointer and TLS-block start for a mapping at `map`.
    /// `TP + 16` is the first TLS byte and is aligned to the block's
    /// alignment (at least 16, the TCB). `None` when `map` is not aligned
    /// to [`TlsSeg::map_align`] or the sizes overflow.
    pub fn thread_pointer_variant_i(&self, map: u64) -> Option<(u64, u64)> {
        if map & self.map_align().checked_sub(1)? != 0 {
            return None;
        }
        let a = self.align.max(16);
        let start = map.checked_add(16)?;
        let start = start.checked_add(a.checked_sub(1)?)? & !a.checked_sub(1)?;
        let tp = start.checked_sub(16)?;
        let end = start.checked_add(self.block_len()?)?;
        let map_end = map.checked_add(self.map_len_variant_i()?)?;
        (tp >= map && end <= map_end).then_some((tp, start))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Variant I: TP is the TCB, TLS data starts at TP+16 and is aligned.
    #[test]
    fn tls_thread_pointer_variant_i_is_aligned() {
        let seg = |memsz, align| TlsSeg {
            vaddr: 0,
            offset: 0,
            filesz: 0,
            memsz,
            align,
        };
        for align in [1u64, 8, 16, 64, 4096, 1 << 20] {
            for memsz in [0u64, 8, 100, 4088, 5000] {
                let s = seg(memsz, align);
                let map = 0x7FFF_0000_0000 & !(s.map_align() - 1);
                let (tp, start) = s.thread_pointer_variant_i(map).unwrap();
                assert_eq!(start, tp + 16, "{memsz} {align}");
                assert_eq!(start % align.max(16), 0, "{memsz} {align}");
                assert!(start >= map, "{memsz} {align}");
                assert!(
                    start + s.block_len().unwrap() <= map + s.map_len_variant_i().unwrap(),
                    "{memsz} {align}"
                );
            }
        }
        assert_eq!(
            seg(8, 1 << 20).thread_pointer_variant_i(0x7FFF_0000_1000),
            None
        );
    }
}
