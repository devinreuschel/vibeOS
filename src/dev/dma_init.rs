//! Buddy-backed [`vibeos::dma::DmaBuffer`]. ROADMAP §6.4.
//!
//! Device address is identity-mapped phys. Never a HHDM VA.

use vibeos::dma::{self, DmaAlloc, DmaBuffer};

use crate::arch::current::Arch;
use crate::paging_init;
use crate::pmm_init;

/// The kernel's one way to get a [`DmaBuffer`]; free it with [`free`].
pub fn alloc(spec: DmaAlloc) -> Option<DmaBuffer> {
    pmm_init::with_buddy(|b| {
        dma::alloc_from_buddy::<Arch>(b, spec, |p| paging_init::HHDM_BASE.wrapping_add(p))
    })
}

/// Takes the buffer by value: its frames go back to the buddy once.
pub fn free(buf: DmaBuffer) {
    pmm_init::with_buddy(|b| dma::free_to_buddy::<Arch>(b, buf));
}
