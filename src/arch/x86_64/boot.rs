//! `BootHandover` on x86_64 (PORTABILITY §11.1): Limine base revision 3,
//! whose responses `boot::capture` normalizes into `BootInfo` through it.

use vibeos::arch::BootHandover;

use super::Arch;
use crate::boot::{self, BootInfo};

impl BootHandover for Arch {
    type Info = BootInfo;

    /// Limine base revision 3, until ROADMAP §11.1's bump.
    const BASE_REVISION: u64 = 3;

    #[inline]
    fn info() -> &'static BootInfo {
        boot::info()
    }

    /// Base revision 3 hands back a physical RSDP, other revisions an HHDM
    /// address, which this translates.
    #[inline]
    fn table_phys(raw: u64, hhdm_offset: u64) -> u64 {
        raw.checked_sub(hhdm_offset).unwrap_or(raw)
    }
}
