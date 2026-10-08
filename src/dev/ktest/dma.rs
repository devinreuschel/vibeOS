//! DMA in-guest tests.

use vibeos::dma::{self, DMA32_BOUNDARY, DmaAlloc};
use vibeos::pci::CMD_MASTER;

use crate::arch::current::Arch;
use crate::dma_init;
use crate::ktest::{
    EDU_IDENT, EDU_IDENT_VAL, Outcome, bar0_va, find_edu, mmio_r32, mmio_w32, mmio_w64,
    quiescent_free_frames, spin_until_ns,
};
use crate::paging_init;
use crate::pci_init;

pub(crate) fn test_dma_alloc() -> Outcome {
    let before = quiescent_free_frames();
    let Some(buf) = dma_init::alloc(DmaAlloc::dma32(0x1000)) else {
        return Outcome::Fail("alloc");
    };
    if buf.device().as_u64() != buf.phys() {
        dma_init::free(buf);
        return Outcome::Fail("device != phys");
    }
    if buf.virt() != paging_init::hhdm_offset().wrapping_add(buf.phys()) {
        dma_init::free(buf);
        return Outcome::Fail("virt not hhdm");
    }
    if buf.device().as_u64() == buf.virt() {
        dma_init::free(buf);
        return Outcome::Fail("device is va");
    }
    if buf.phys() >= DMA32_BOUNDARY || dma::crosses_boundary(buf.phys(), buf.len(), DMA32_BOUNDARY)
    {
        dma_init::free(buf);
        return Outcome::Fail("dma32");
    }
    buf.sync_for_device::<Arch>();
    // SAFETY: `buf` is this test's own DMA buffer, at least one byte long,
    // shared with no device; established by `dma_init::alloc`.
    unsafe {
        buf.as_ptr().write_volatile(0xA5);
    }
    buf.sync_for_cpu::<Arch>();
    let sg = match dma::SgList::from_buffer(&buf) {
        Ok(s) => s,
        Err(_) => {
            dma_init::free(buf);
            return Outcome::Fail("sg");
        }
    };
    if sg.n != 1 || sg.entries[0].addr != buf.device() {
        dma_init::free(buf);
        return Outcome::Fail("sg entry");
    }
    dma_init::free(buf);
    if dma_init::alloc(DmaAlloc {
        size: 0x1000,
        align: 0x1000,
        boundary: 0x800,
    })
    .is_some()
    {
        return Outcome::Fail("boundary refuse");
    }
    if quiescent_free_frames() != before {
        return Outcome::Fail("leak");
    }
    Outcome::Ok
}

const EDU_DMA_SRC: u32 = 0x80;

const EDU_DMA_DST: u32 = 0x88;

const EDU_DMA_CNT: u32 = 0x90;

const EDU_DMA_CMD: u32 = 0x98;

const EDU_DMA_BUF: u32 = 0x4_0000;

const EDU_DMA_RUN: u32 = 1;

const EDU_DMA_TO_PCI: u32 = 2;

pub(crate) fn test_dma_edu() -> Outcome {
    let Some(dev) = find_edu() else {
        return Outcome::Skip("no edu");
    };
    let Some(mmio) = bar0_va(&dev) else {
        return Outcome::Fail("edu bar0");
    };
    if mmio_r32(mmio, EDU_IDENT) != EDU_IDENT_VAL {
        return Outcome::Fail("edu ident");
    }
    pci_init::update_command(dev.addr, CMD_MASTER, 0);
    let Some(src) = dma_init::alloc(DmaAlloc::dma32(64)) else {
        return Outcome::Fail("src");
    };
    let Some(dst) = dma_init::alloc(DmaAlloc::dma32(64)) else {
        dma_init::free(src);
        return Outcome::Fail("dst");
    };
    // SAFETY: `src` and `dst` are this test's own 64-byte DMA buffers, not
    // yet handed to the device; established by `dma_init::alloc`.
    unsafe {
        let p = src.as_ptr();
        let q = dst.as_ptr();
        let mut i = 0u32;
        while i < 64 {
            p.add(i as usize).write_volatile((0xC0 + i) as u8);
            q.add(i as usize).write_volatile(0);
            i += 1;
        }
    }
    src.sync_for_device::<Arch>();
    dst.sync_for_device::<Arch>();
    let src_dev = src.device().as_u64();
    if !dma::addr_fits_mask(src_dev, dma::EDU_DMA_MASK) {
        dma_init::free(src);
        dma_init::free(dst);
        return Outcome::Fail("dma mask");
    }
    mmio_w64(mmio, EDU_DMA_SRC, src_dev);
    mmio_w64(mmio, EDU_DMA_DST, u64::from(EDU_DMA_BUF));
    mmio_w32(mmio, EDU_DMA_CNT, 64);
    dma::dma_wmb::<Arch>();
    mmio_w32(mmio, EDU_DMA_CMD, EDU_DMA_RUN);
    if !spin_until_ns(
        || mmio_r32(mmio, EDU_DMA_CMD) & EDU_DMA_RUN == 0,
        2_000_000_000,
    ) {
        dma_init::free(src);
        dma_init::free(dst);
        return Outcome::Fail("dma to edu");
    }
    let dst_dev = dst.device().as_u64();
    if !dma::addr_fits_mask(dst_dev, dma::EDU_DMA_MASK) {
        dma_init::free(src);
        dma_init::free(dst);
        return Outcome::Fail("dma mask");
    }
    mmio_w64(mmio, EDU_DMA_SRC, u64::from(EDU_DMA_BUF));
    mmio_w64(mmio, EDU_DMA_DST, dst_dev);
    mmio_w32(mmio, EDU_DMA_CNT, 64);
    dma::dma_wmb::<Arch>();
    mmio_w32(mmio, EDU_DMA_CMD, EDU_DMA_RUN | EDU_DMA_TO_PCI);
    if !spin_until_ns(
        || mmio_r32(mmio, EDU_DMA_CMD) & EDU_DMA_RUN == 0,
        2_000_000_000,
    ) {
        dma_init::free(src);
        dma_init::free(dst);
        return Outcome::Fail("dma from edu");
    }
    dst.sync_for_cpu::<Arch>();
    let mut bad = false;
    // SAFETY: both 64-byte buffers are this test's own, and the edu DMA
    // into `dst` has completed; established by `dma_init::alloc`.
    unsafe {
        let p = src.as_ptr();
        let q = dst.as_ptr();
        let mut i = 0usize;
        while i < 64 {
            if p.add(i).read_volatile() != q.add(i).read_volatile() {
                bad = true;
                break;
            }
            i += 1;
        }
    }
    dma_init::free(src);
    dma_init::free(dst);
    if bad {
        Outcome::Fail("mismatch")
    } else {
        Outcome::Ok
    }
}
