//! Buddy-backed [`vibeos::dma::DmaBuffer`]. ROADMAP §6.4.
//!
//! Device address is identity-mapped phys. Never a HHDM VA.

#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]

use vibeos::dma::{self, DmaAlloc, DmaBuffer};

use crate::paging_init;
use crate::pmm_init;

pub fn alloc(spec: DmaAlloc) -> Option<DmaBuffer> {
    let (phys, order) = pmm_init::with_buddy(|b| b.allocate_constrained(spec.size, spec.align, spec.boundary))?;
    let virt = paging_init::HHDM_BASE.wrapping_add(phys);
    let buf = DmaBuffer::from_phys(phys, virt, spec.size, order);
    buf.sync_for_device();
    Some(buf)
}

pub fn free(buf: DmaBuffer) {
    pmm_init::with_buddy(|b| dma::free_to_buddy(b, buf));
}
