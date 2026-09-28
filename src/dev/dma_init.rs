//! Buddy-backed [`vibeos::dma::DmaBuffer`]. ROADMAP §6.4.
//!
//! Device address is identity-mapped phys. Never a HHDM VA.

#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]

use vibeos::dma::{self, DmaAlloc, DmaBuffer};

use crate::paging_init;
use crate::pmm_init;

/// The kernel's one way to get a [`DmaBuffer`]; free it with [`free`].
pub fn alloc(spec: DmaAlloc) -> Option<DmaBuffer> {
    pmm_init::with_buddy(|b| {
        dma::alloc_from_buddy(b, spec, |p| paging_init::HHDM_BASE.wrapping_add(p))
    })
}

/// Takes the buffer by value: its frames go back to the buddy once.
pub fn free(buf: DmaBuffer) {
    pmm_init::with_buddy(|b| dma::free_to_buddy(b, buf));
}
