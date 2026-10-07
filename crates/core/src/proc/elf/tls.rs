//! TLS variant I (aarch64) layout helpers (ROADMAP §11.6).

#![deny(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use crate::paging::PAGE_SIZE_4K;

use super::TlsSeg;

/// `x` rounded up to a power-of-two `a`. `None` when `a` is 0 or the add overflows.
fn align_up(x: u64, a: u64) -> Option<u64> {
    let mask = a.checked_sub(1)?;
    x.checked_add(mask).map(|n| n & !mask)
}

impl TlsSeg {
    /// Variant I (aarch64) mapping size, page-rounded and at least one page.
    /// Covers the worst-case pad to align TP (`a - 1`), the TCB gap
    /// `align_up(16, a)`, and the TLS block. `a` is `max(align, 16)`.
    pub fn map_len_variant_i(&self) -> Option<u64> {
        let a = self.align.max(16);
        let need = a
            .checked_sub(1)?
            .checked_add(align_up(16, a)?)?
            .checked_add(self.block_len()?)?
            .max(PAGE_SIZE_4K);
        Some(need.checked_add(PAGE_SIZE_4K - 1)? & !(PAGE_SIZE_4K - 1))
    }

    /// Variant I thread pointer and TLS-block start for a mapping at `map`.
    /// TP is `align_up(map, a)` with `a = max(align, 16)`; the block starts
    /// at `TP + align_up(16, a)`, which is `TP + max(16, p_align)` when
    /// `p_align` is a power of two. `None` when `map` is not aligned to
    /// [`TlsSeg::map_align`] or the sizes overflow.
    pub fn thread_pointer_variant_i(&self, map: u64) -> Option<(u64, u64)> {
        if map & self.map_align().checked_sub(1)? != 0 {
            return None;
        }
        let a = self.align.max(16);
        let tp = align_up(map, a)?;
        let start = tp.checked_add(align_up(16, a)?)?;
        let end = start.checked_add(self.block_len()?)?;
        let map_end = map.checked_add(self.map_len_variant_i()?)?;
        (tp >= map && end <= map_end).then_some((tp, start))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Variant I: TP is aligned to `p_align`, and the TLS block starts
    /// at `TP + max(16, p_align)`.
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
                let a = align.max(16);
                assert_eq!(start, tp + a, "{memsz} {align}");
                assert_eq!(tp % align, 0, "{memsz} {align}");
                assert!(start >= map, "{memsz} {align}");
                let block = s.block_len().unwrap();
                let map_len = s.map_len_variant_i().unwrap();
                // `a - 1` is the pad when `map` is not yet aligned to `a`.
                assert!(map_len >= (a - 1) + a + block, "{memsz} {align}");
                assert!(start + block <= map + map_len, "{memsz} {align}");
            }
        }
        assert_eq!(
            seg(8, 1 << 20).thread_pointer_variant_i(0x7FFF_0000_1000),
            None
        );
    }
}
