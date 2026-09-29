//! In-guest tests for dev (kernel_tests only). Rows: the list in crate::ktest.

use vibeos::dev::{ClaimError, Device, Driver, IdMatch, ProbeError};
use vibeos::dma::{self, DMA32_BOUNDARY, DmaAlloc};
use vibeos::fs::O_RDWR;
use vibeos::pci::{self, Bdf, CFG_COMMAND, CFG_VENDOR, CMD_MASTER, CMD_MEM};
use vibeos::virtio::F_VERSION_1;

use crate::dev_init;
use crate::dma_init;
use crate::fs_init;
use crate::ktest::{
    EDU_IDENT, EDU_IDENT_VAL, Outcome, bar0_va, fid, find_edu, mmio_r32, mmio_w32,
    quiescent_free_frames, spin_until_ns,
};
use crate::paging_init;
use crate::pci_init;
use crate::virtio_init;

const PCI_QEMU_IDS: &[(u16, u16)] = &[
    (0x8086, 0x1237), // 440FX
    (0x8086, 0x7000), // PIIX3 ISA
    (0x8086, 0x7010), // PIIX3 IDE
    (0x8086, 0x7113), // PIIX4 ACPI
    (0x1234, 0x1111), // Bochs VGA
    (0x8086, 0x100e), // e1000
];

pub(crate) fn test_pci_qemu_set() -> Outcome {
    if !pci_init::live() {
        return Outcome::Fail("pci not live");
    }
    if dev_init::len() < PCI_QEMU_IDS.len() {
        return Outcome::Fail("device count");
    }
    let mut i = 0usize;
    while i < PCI_QEMU_IDS.len() {
        let (v, d) = PCI_QEMU_IDS[i];
        if dev_init::find_id(v, d).is_none() {
            return Outcome::Fail("missing qemu id");
        }
        i += 1;
    }
    Outcome::Ok
}

pub(crate) fn test_pci_bar_map() -> Outcome {
    let Some((_, d)) = dev_init::find_id(0x1234, 0x1111) else {
        return Outcome::Fail("no vga");
    };
    let r = d.resources[0];
    if r.is_empty() {
        return Outcome::Fail("vga bar0 empty");
    }
    if r.size == 0 || r.size > pci::MAX_BAR_MAP {
        return Outcome::Fail("vga bar0 size");
    }
    if r.mapped_va == 0 {
        return Outcome::Fail("vga bar0 unmapped");
    }
    // WB physmap alias, not an ioremap UC window over the console FB.
    if r.mapped_va != paging_init::HHDM_BASE.wrapping_add(r.addr) {
        return Outcome::Fail("vga bar0 not wb physmap");
    }
    Outcome::Ok
}

pub(crate) fn test_pci_cfg_rw() -> Outcome {
    let bdf = Bdf::new(0, 0, 0);
    let id = pci_init::cfg_read32(bdf, CFG_VENDOR);
    if id as u16 != 0x8086 {
        return Outcome::Fail("host vendor");
    }
    if (id >> 16) as u16 != 0x1237 {
        return Outcome::Fail("host device");
    }
    let prev = pci_init::cfg_read32(bdf, CFG_COMMAND) as u16;
    pci_init::enable_mem_master(bdf);
    let now = pci_init::cfg_read32(bdf, CFG_COMMAND) as u16;
    pci_init::cfg_write32(bdf, CFG_COMMAND, prev as u32);
    if now & (CMD_MEM | CMD_MASTER) != CMD_MEM | CMD_MASTER {
        return Outcome::Fail("cmd bits");
    }
    Outcome::Ok
}

pub(crate) fn test_pci_claim_exclusive() -> Outcome {
    let Some((i, d)) = dev_init::find_id(0x8086, 0x100e) else {
        return Outcome::Fail("no e1000");
    };
    let mut b = 0u8;
    let mut found = false;
    while (b as usize) < pci::MAX_BARS {
        if !d.resources[b as usize].is_empty() {
            found = true;
            break;
        }
        b += 1;
    }
    if !found {
        return Outcome::Fail("e1000 no bar");
    }
    if let Err(e) = dev_init::claim(i, b) {
        return Outcome::Fail(e.as_str());
    }
    match dev_init::claim(i, b) {
        Err(ClaimError::Already) => Outcome::Ok,
        Err(_) => Outcome::Fail("wrong claim err"),
        Ok(()) => Outcome::Fail("double claim"),
    }
}

struct HostBridgeDrv;

static HOST_BRIDGE_IDS: &[IdMatch] = &[IdMatch::vid_did(0x8086, 0x1237)];

static HOST_BRIDGE_DRV: HostBridgeDrv = HostBridgeDrv;

impl Driver for HostBridgeDrv {
    fn name(&self) -> &'static str {
        "host-bridge"
    }
    fn ids(&self) -> &'static [IdMatch] {
        HOST_BRIDGE_IDS
    }
    fn order(&self) -> u8 {
        1
    }
    fn probe(&self, _dev: &mut Device) -> Result<(), ProbeError> {
        Ok(())
    }
    fn remove(&self, _dev: &mut Device) {}
}

pub(crate) fn test_pci_bind_order() -> Outcome {
    if !dev_init::register_driver(&HOST_BRIDGE_DRV) {
        return Outcome::Fail("register");
    }
    dev_init::bind_all();
    let Some((_, d)) = dev_init::find_id(0x8086, 0x1237) else {
        return Outcome::Fail("no host");
    };
    match d.bound {
        Some("host-bridge") => Outcome::Ok,
        Some(_) => Outcome::Fail("wrong driver"),
        None => Outcome::Fail("unbound"),
    }
}

pub(crate) fn test_dma_alloc() -> Outcome {
    let before = quiescent_free_frames();
    let Some(buf) = dma_init::alloc(DmaAlloc::dma32(0x1000)) else {
        return Outcome::Fail("alloc");
    };
    if buf.device().as_u64() != buf.phys() {
        dma_init::free(buf);
        return Outcome::Fail("device != phys");
    }
    if buf.virt() != paging_init::HHDM_BASE.wrapping_add(buf.phys()) {
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
    buf.sync_for_device();
    unsafe {
        buf.as_ptr().write_volatile(0xA5);
    }
    buf.sync_for_cpu();
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
    let Some((_, dev)) = find_edu() else {
        return Outcome::Skip("no edu");
    };
    let Some(mmio) = bar0_va(&dev) else {
        return Outcome::Fail("edu bar0");
    };
    if mmio_r32(mmio, EDU_IDENT) != EDU_IDENT_VAL {
        return Outcome::Fail("edu ident");
    }
    pci_init::enable_mem_master(dev.addr);
    let Some(src) = dma_init::alloc(DmaAlloc::dma32(64)) else {
        return Outcome::Fail("src");
    };
    let Some(dst) = dma_init::alloc(DmaAlloc::dma32(64)) else {
        dma_init::free(src);
        return Outcome::Fail("dst");
    };
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
    src.sync_for_device();
    dst.sync_for_device();
    mmio_w32(mmio, EDU_DMA_SRC, src.device().as_u64() as u32);
    mmio_w32(mmio, EDU_DMA_DST, EDU_DMA_BUF);
    mmio_w32(mmio, EDU_DMA_CNT, 64);
    dma::dma_wmb();
    mmio_w32(mmio, EDU_DMA_CMD, EDU_DMA_RUN);
    if !spin_until_ns(
        || mmio_r32(mmio, EDU_DMA_CMD) & EDU_DMA_RUN == 0,
        2_000_000_000,
    ) {
        dma_init::free(src);
        dma_init::free(dst);
        return Outcome::Fail("dma to edu");
    }
    mmio_w32(mmio, EDU_DMA_SRC, EDU_DMA_BUF);
    mmio_w32(mmio, EDU_DMA_DST, dst.device().as_u64() as u32);
    mmio_w32(mmio, EDU_DMA_CNT, 64);
    dma::dma_wmb();
    mmio_w32(mmio, EDU_DMA_CMD, EDU_DMA_RUN | EDU_DMA_TO_PCI);
    if !spin_until_ns(
        || mmio_r32(mmio, EDU_DMA_CMD) & EDU_DMA_RUN == 0,
        2_000_000_000,
    ) {
        dma_init::free(src);
        dma_init::free(dst);
        return Outcome::Fail("dma from edu");
    }
    dst.sync_for_cpu();
    let mut bad = false;
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

fn find_rng() -> Option<(usize, Device)> {
    dev_init::find_id(0x1af4, 0x1044).or_else(|| dev_init::find_id(0x1af4, 0x1004))
}

pub(crate) fn test_virtio_bind() -> Outcome {
    let Some((_, d)) = find_rng() else {
        return Outcome::Skip("no virtio-rng");
    };
    if !virtio_init::rng_bound() {
        return Outcome::Fail("unbound");
    }
    match d.bound {
        Some("virtio-rng") => {}
        Some(_) => return Outcome::Fail("wrong driver"),
        None => return Outcome::Fail("id match"),
    }
    if virtio_init::rng_features() & F_VERSION_1 == 0 {
        return Outcome::Fail("no VERSION_1");
    }
    if !virtio_init::rng_uses_indirect() && !virtio_init::rng_uses_event_idx() {
        // Modern QEMU offers both; either is enough to prove negotiation.
        return Outcome::Fail("no optional feats");
    }
    Outcome::Ok
}

pub(crate) fn test_virtio_vq() -> Outcome {
    if !virtio_init::rng_bound() {
        return Outcome::Skip("no virtio-rng");
    }
    let qdev = virtio_init::rng_qdma_device();
    let ddev = virtio_init::rng_data_device();
    let dvirt = virtio_init::rng_data_virt();
    if qdev == 0 || ddev == 0 {
        return Outcome::Fail("dma");
    }
    if ddev == dvirt {
        return Outcome::Fail("device is va");
    }
    let c0 = virtio_init::rng_completions();
    let t0 = virtio_init::rng_top_hits();
    let th0 = virtio_init::rng_thread_hits();
    let s0 = virtio_init::rng_soft_hits();
    if virtio_init::rng_request().is_err() {
        return Outcome::Fail("request");
    }
    if !spin_until_ns(|| virtio_init::rng_completions() > c0, 2_000_000_000) {
        return Outcome::Fail("no complete");
    }
    if virtio_init::rng_last_len() == 0 {
        return Outcome::Fail("empty");
    }
    if virtio_init::rng_top_hits() <= t0 {
        return Outcome::Fail("no top");
    }
    if virtio_init::rng_thread_hits() <= th0 {
        return Outcome::Fail("no thread");
    }
    if !virtio_init::rng_alloced() {
        return Outcome::Fail("thread alloc");
    }
    if !spin_until_ns(|| virtio_init::rng_soft_hits() > s0, 2_000_000_000) {
        return Outcome::Fail("no softirq");
    }
    Outcome::Ok
}

pub(crate) fn test_dev_random_source() -> Outcome {
    if !virtio_init::rng_bound() {
        return Outcome::Skip("no virtio-rng");
    }
    if !fs_init::live() {
        return Outcome::Fail("not live");
    }
    let Ok(f) = fid::open("/dev/random", O_RDWR, 0) else {
        return Outcome::Fail("open");
    };
    let mut buf = [0u8; 16];
    let n = fid::read(f, &mut buf);
    let _ = fid::close(f);
    if n.ok() != Some(16) {
        return Outcome::Fail("read");
    }
    match vibeos::entropy::last_source() {
        vibeos::entropy::Source::XorShift => Outcome::Fail("xorshift"),
        vibeos::entropy::Source::VirtioRng | vibeos::entropy::Source::RdRand => Outcome::Ok,
    }
}
