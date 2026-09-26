//! Modern virtio-pci transport + virtio-rng. ROADMAP §6.5.
//!
//! Transport + virtio-rng. virtio-blk is `virtio_blk_init`. ROADMAP §6.5.

#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]

use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use vibeos::dev::{Device, Driver, IdMatch, ProbeError};
use vibeos::dma::{self, DmaAlloc, DmaBuffer};
use vibeos::irq::IrqError;
use vibeos::lock::RANK_DEVICE;
use vibeos::pci::MAX_BARS;
use vibeos::virtio::{
    self, COMMON_OFF_DF, COMMON_OFF_DFSEL, COMMON_OFF_DR, COMMON_OFF_DRSEL, COMMON_OFF_MSIX_CFG,
    COMMON_OFF_QDESC, COMMON_OFF_QDEVICE, COMMON_OFF_QDRIVER, COMMON_OFF_QENABLE, COMMON_OFF_QMSIX,
    COMMON_OFF_QNOTIFY, COMMON_OFF_QSEL, COMMON_OFF_QSIZE, COMMON_OFF_STATUS, DEV_RNG_LEGACY,
    DEV_RNG_MODERN, F_EVENT_IDX, F_INDIRECT_DESC, MSI_NO_VECTOR, ModernCaps, OFFER, PciCap,
    STATUS_ACKNOWLEDGE, STATUS_DRIVER, STATUS_DRIVER_OK, STATUS_FEATURES_OK, SplitLayout,
    SplitQueue, VENDOR_ID, VirtioError, notify_addr, pick_features, write_indirect_write,
};

use crate::dev_init;
use crate::dma_init;
use crate::irq_init;
use crate::pci_init;
use crate::per_cpu_init;
use crate::sync_init::SpinMutex;
use crate::work_init;

struct Q {
    vq: SplitQueue,
    qdma: DmaBuffer,
    data: DmaBuffer,
    doorbell: u64,
    common: u64,
    features: u64,
    vec: u8,
}

static Q: SpinMutex<Option<Q>> = SpinMutex::with_rank(None, RANK_DEVICE);
static ISR_VA: AtomicU64 = AtomicU64::new(0);
static TOP_HITS: AtomicU32 = AtomicU32::new(0);
static THREAD_HITS: AtomicU32 = AtomicU32::new(0);
static COMPLETIONS: AtomicU32 = AtomicU32::new(0);
static LAST_LEN: AtomicU32 = AtomicU32::new(0);
static ALLOCED: AtomicBool = AtomicBool::new(false);
static SOFT_HITS: AtomicU32 = AtomicU32::new(0);
static BOUND: AtomicBool = AtomicBool::new(false);
static FEATURES: AtomicU64 = AtomicU64::new(0);
static QDMA_DEV: AtomicU64 = AtomicU64::new(0);
static DATA_DEV: AtomicU64 = AtomicU64::new(0);
static DATA_VIRT: AtomicU64 = AtomicU64::new(0);
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

const RNG_PAYLOAD: usize = 32;
const RNG_PAYLOAD_OFF: usize = 16;
static POOL: [AtomicU8; RNG_PAYLOAD] = [const { AtomicU8::new(0) }; RNG_PAYLOAD];
static POOL_LEN: AtomicU32 = AtomicU32::new(0);
static POOL_POS: AtomicU32 = AtomicU32::new(0);

fn r8(va: u64, off: u16) -> u8 {
    unsafe { core::ptr::read_volatile((va.wrapping_add(off as u64)) as *const u8) }
}

fn w8(va: u64, off: u16, v: u8) {
    unsafe { core::ptr::write_volatile((va.wrapping_add(off as u64)) as *mut u8, v) }
}

fn r16(va: u64, off: u16) -> u16 {
    unsafe {
        u16::from_le(core::ptr::read_volatile(
            (va.wrapping_add(off as u64)) as *const u16,
        ))
    }
}

fn w16(va: u64, off: u16, v: u16) {
    unsafe {
        core::ptr::write_volatile((va.wrapping_add(off as u64)) as *mut u16, v.to_le());
    }
}

fn r32(va: u64, off: u16) -> u32 {
    unsafe {
        u32::from_le(core::ptr::read_volatile(
            (va.wrapping_add(off as u64)) as *const u32,
        ))
    }
}

fn w32(va: u64, off: u16, v: u32) {
    unsafe {
        core::ptr::write_volatile((va.wrapping_add(off as u64)) as *mut u32, v.to_le());
    }
}

fn w64(va: u64, off: u16, v: u64) {
    w32(va, off, v as u32);
    w32(va, off + 4, (v >> 32) as u32);
}

fn region(dev: &Device, cap: PciCap) -> Option<u64> {
    let bir = cap.bar as usize;
    if bir >= MAX_BARS {
        return None;
    }
    let r = dev.resources[bir];
    if r.mapped_va == 0 || (cap.offset as u64) >= r.size {
        return None;
    }
    Some(r.mapped_va.wrapping_add(cap.offset as u64))
}

fn clamp_qsize(hw: u16) -> u16 {
    let n = hw.min(16);
    if n == 0 {
        return 0;
    }
    1u16 << (15u32 - n.leading_zeros())
}

fn read_features(common: u64) -> u64 {
    w32(common, COMMON_OFF_DFSEL, 0);
    let lo = r32(common, COMMON_OFF_DF) as u64;
    w32(common, COMMON_OFF_DFSEL, 1);
    let hi = r32(common, COMMON_OFF_DF) as u64;
    lo | (hi << 32)
}

fn write_features(common: u64, feat: u64) {
    w32(common, COMMON_OFF_DRSEL, 0);
    w32(common, COMMON_OFF_DR, feat as u32);
    w32(common, COMMON_OFF_DRSEL, 1);
    w32(common, COMMON_OFF_DR, (feat >> 32) as u32);
}

fn reset(common: u64) -> bool {
    w8(common, COMMON_OFF_STATUS, 0);
    let mut n = 0u32;
    while r8(common, COMMON_OFF_STATUS) != 0 {
        if n > 1_000_000 {
            return false;
        }
        n += 1;
        core::hint::spin_loop();
    }
    true
}

fn fail_status(common: u64) {
    let st = r8(common, COMMON_OFF_STATUS);
    w8(common, COMMON_OFF_STATUS, st | virtio::STATUS_FAILED);
}

fn fail_armed(dev: &Device, common: u64, vec: u8) {
    irq_init::disable_msix(dev);
    let _ = irq_init::free_vector(vec);
    fail_status(common);
}

fn on_soft(_arg: usize) {
    let _b = Box::new(0x22u8);
    SOFT_HITS.fetch_add(1, Ordering::SeqCst);
}

fn rng_top() {
    TOP_HITS.fetch_add(1, Ordering::SeqCst);
    let isr = ISR_VA.load(Ordering::Acquire);
    if isr != 0 {
        let _ = r8(isr, 0);
    }
    let _ = work_init::raise_softirq(on_soft, 1);
}

fn publish_pool(virt: u64, len: u32) {
    let n = (len as usize).min(RNG_PAYLOAD);
    let p = virt.wrapping_add(RNG_PAYLOAD_OFF as u64) as *const u8;
    let mut i = 0usize;
    while i < n {
        let b = unsafe { p.add(i).read_volatile() };
        POOL[i].store(b, Ordering::Relaxed);
        i += 1;
    }
    POOL_LEN.store(n as u32, Ordering::Release);
    POOL_POS.store(0, Ordering::Release);
}

fn harvest() {
    let mut n = 0u32;
    let mut last = 0u32;
    {
        let mut g = Q.lock();
        if let Some(q) = g.as_mut() {
            while let Some(u) = q.vq.get_used() {
                n += 1;
                last = u.len;
            }
            if n != 0 {
                q.data.sync_for_cpu();
                publish_pool(q.data.virt(), last);
                IN_FLIGHT.store(false, Ordering::Release);
            }
        }
    }
    if n != 0 {
        LAST_LEN.store(last, Ordering::SeqCst);
        COMPLETIONS.fetch_add(n, Ordering::SeqCst);
    }
}

fn rng_work() {
    THREAD_HITS.fetch_add(1, Ordering::SeqCst);
    let _b = Box::new(0x11u8);
    ALLOCED.store(true, Ordering::SeqCst);
    harvest();
}

fn kick(doorbell: u64) {
    dma::dma_wmb();
    unsafe {
        core::ptr::write_volatile(doorbell as *mut u16, 0u16);
    }
}

fn setup(dev: &mut Device, caps: ModernCaps) -> Result<(), VirtioError> {
    let common_cap = caps.common.ok_or(VirtioError::NoCaps)?;
    let notify_cap = caps.notify.ok_or(VirtioError::NoCaps)?;
    let isr_cap = caps.isr.ok_or(VirtioError::NoCaps)?;
    let common = region(dev, common_cap).ok_or(VirtioError::NoCaps)?;
    let notify_base = region(dev, notify_cap).ok_or(VirtioError::NoCaps)?;
    let isr = region(dev, isr_cap).ok_or(VirtioError::NoCaps)?;
    if !reset(common) {
        return Err(VirtioError::Failed);
    }
    w8(common, COMMON_OFF_STATUS, STATUS_ACKNOWLEDGE);
    w8(
        common,
        COMMON_OFF_STATUS,
        STATUS_ACKNOWLEDGE | STATUS_DRIVER,
    );
    let device_feat = read_features(common);
    let feat = match pick_features(device_feat, OFFER) {
        Ok(f) => f,
        Err(e) => {
            fail_status(common);
            return Err(e);
        }
    };
    write_features(common, feat);
    w8(
        common,
        COMMON_OFF_STATUS,
        STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK,
    );
    if r8(common, COMMON_OFF_STATUS) & STATUS_FEATURES_OK == 0 {
        fail_status(common);
        return Err(VirtioError::Features);
    }

    let cpu = irq_init::threaded_cpu();
    let Some(pc) = per_cpu_init::cpu(cpu) else {
        fail_status(common);
        return Err(VirtioError::Failed);
    };
    let vec = match irq_init::allocate_vector(cpu) {
        Ok(v) => v,
        Err(_) => {
            fail_status(common);
            return Err(VirtioError::Failed);
        }
    };
    if irq_init::set_threaded(vec, Some(rng_top), rng_work).is_err() {
        let _ = irq_init::free_vector(vec);
        fail_status(common);
        return Err(VirtioError::Failed);
    }
    if let Err(e) = irq_init::enable_msix(dev, 0, vec, pc.apic_id.load(Ordering::Relaxed) as u8) {
        let _ = irq_init::free_vector(vec);
        fail_status(common);
        return match e {
            IrqError::NoRoute => Err(VirtioError::NoCaps),
            _ => Err(VirtioError::Failed),
        };
    }
    w16(common, COMMON_OFF_MSIX_CFG, MSI_NO_VECTOR);

    w16(common, COMMON_OFF_QSEL, 0);
    let hw_qs = r16(common, COMMON_OFF_QSIZE);
    let qsz = clamp_qsize(hw_qs);
    if qsz == 0 {
        fail_armed(dev, common, vec);
        return Err(VirtioError::BadQueue);
    }
    w16(common, COMMON_OFF_QSIZE, qsz);
    let Some(layout) = SplitLayout::new(qsz) else {
        fail_armed(dev, common, vec);
        return Err(VirtioError::BadQueue);
    };
    let Some(qdma) = dma_init::alloc(DmaAlloc::dma32(layout.total as u64)) else {
        fail_armed(dev, common, vec);
        return Err(VirtioError::Failed);
    };
    let Some(data) = dma_init::alloc(DmaAlloc::dma32(64)) else {
        dma_init::free(qdma);
        fail_armed(dev, common, vec);
        return Err(VirtioError::Failed);
    };
    unsafe {
        core::ptr::write_bytes(qdma.virt() as *mut u8, 0, qdma.len() as usize);
        core::ptr::write_bytes(data.virt() as *mut u8, 0, data.len() as usize);
    }
    let mut vq = SplitQueue::new(layout, qdma.virt() as *mut u8, feat & F_EVENT_IDX != 0);
    vq.init();
    qdma.sync_for_device();
    w64(
        common,
        COMMON_OFF_QDESC,
        qdma.device().as_u64() + layout.desc_off as u64,
    );
    w64(
        common,
        COMMON_OFF_QDRIVER,
        qdma.device().as_u64() + layout.avail_off as u64,
    );
    w64(
        common,
        COMMON_OFF_QDEVICE,
        qdma.device().as_u64() + layout.used_off as u64,
    );
    w16(common, COMMON_OFF_QMSIX, 0);
    w16(common, COMMON_OFF_QENABLE, 1);
    let qoff = r16(common, COMMON_OFF_QNOTIFY);
    let Some(doorbell) = notify_addr(
        notify_base,
        0,
        notify_cap.length,
        qoff,
        notify_cap.notify_off_multiplier,
    ) else {
        dma_init::free(qdma);
        dma_init::free(data);
        fail_armed(dev, common, vec);
        return Err(VirtioError::Notify);
    };

    let st = r8(common, COMMON_OFF_STATUS);
    w8(common, COMMON_OFF_STATUS, st | STATUS_DRIVER_OK);

    ISR_VA.store(isr, Ordering::Release);
    FEATURES.store(feat, Ordering::Release);
    QDMA_DEV.store(qdma.device().as_u64(), Ordering::Release);
    DATA_DEV.store(data.device().as_u64(), Ordering::Release);
    DATA_VIRT.store(data.virt(), Ordering::Release);

    let mut g = Q.lock();
    *g = Some(Q {
        vq,
        qdma,
        data,
        doorbell,
        common,
        features: feat,
        vec,
    });

    crate::marker!(
        "vibeOS: virtio: rng {} qsz={} feat={:#x} notify_off={}*{}",
        dev.addr,
        qsz,
        feat,
        qoff,
        notify_cap.notify_off_multiplier
    );
    Ok(())
}

fn claim_bars(dev: &mut Device, caps: &ModernCaps) {
    let mut mark = |c: Option<PciCap>| {
        if let Some(c) = c {
            let i = c.bar as usize;
            if i < MAX_BARS && !dev.resources[i].is_empty() {
                dev.resources[i].claimed = true;
            }
        }
    };
    mark(caps.common);
    mark(caps.notify);
    mark(caps.isr);
    mark(caps.device);
}

struct RngDriver;

static RNG_IDS: &[IdMatch] = &[
    IdMatch::vid_did(VENDOR_ID, DEV_RNG_MODERN),
    IdMatch::vid_did(VENDOR_ID, DEV_RNG_LEGACY),
];

static RNG_DRV: RngDriver = RngDriver;

impl Driver for RngDriver {
    fn name(&self) -> &'static str {
        "virtio-rng"
    }
    fn ids(&self) -> &'static [IdMatch] {
        RNG_IDS
    }
    fn order(&self) -> u8 {
        40
    }
    fn probe(&self, dev: &mut Device) -> Result<(), ProbeError> {
        let caps = virtio::read_modern_caps(&mut pci_init::HwCfg, dev.addr);
        if !caps.is_complete() {
            crate::marker!("vibeOS: virtio: missing modern caps");
            return Err(ProbeError::NoResource);
        }
        claim_bars(dev, &caps);
        match setup(dev, caps) {
            Ok(()) => {
                BOUND.store(true, Ordering::Release);
                Ok(())
            }
            Err(VirtioError::NoVersion1) => {
                crate::marker!("vibeOS: virtio: no VERSION_1");
                Err(ProbeError::Failed)
            }
            Err(e) => {
                crate::marker!("vibeOS: virtio: probe {}", e.as_str());
                Err(ProbeError::Failed)
            }
        }
    }
    fn remove(&self, _dev: &mut Device) {}
}

pub fn init() {
    let _ = dev_init::register_driver(&RNG_DRV);
}

pub fn rng_bound() -> bool {
    BOUND.load(Ordering::Acquire)
}

pub fn rng_features() -> u64 {
    FEATURES.load(Ordering::Acquire)
}

pub fn rng_uses_indirect() -> bool {
    rng_features() & F_INDIRECT_DESC != 0
}

pub fn rng_uses_event_idx() -> bool {
    rng_features() & F_EVENT_IDX != 0
}

pub fn rng_qdma_device() -> u64 {
    QDMA_DEV.load(Ordering::Acquire)
}

pub fn rng_data_device() -> u64 {
    DATA_DEV.load(Ordering::Acquire)
}

pub fn rng_data_virt() -> u64 {
    DATA_VIRT.load(Ordering::Acquire)
}

pub fn rng_completions() -> u32 {
    COMPLETIONS.load(Ordering::Acquire)
}

pub fn rng_top_hits() -> u32 {
    TOP_HITS.load(Ordering::Acquire)
}

pub fn rng_thread_hits() -> u32 {
    THREAD_HITS.load(Ordering::Acquire)
}

pub fn rng_alloced() -> bool {
    ALLOCED.load(Ordering::Acquire)
}

pub fn rng_soft_hits() -> u32 {
    SOFT_HITS.load(Ordering::Acquire)
}

pub fn rng_last_len() -> u32 {
    LAST_LEN.load(Ordering::Acquire)
}

/// Copy harvested virtio-rng bytes. Does not take the queue lock.
pub fn rng_take(buf: &mut [u8]) -> usize {
    let mut i = 0usize;
    while i < buf.len() {
        let pos = POOL_POS.load(Ordering::Relaxed);
        let len = POOL_LEN.load(Ordering::Acquire);
        if pos >= len {
            break;
        }
        if POOL_POS
            .compare_exchange(pos, pos + 1, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            buf[i] = POOL[pos as usize].load(Ordering::Relaxed);
            i += 1;
        }
    }
    i
}

/// Submit one entropy buffer. Completion is harvested by the IRQ thread.
pub fn rng_request() -> Result<(), VirtioError> {
    let mut g = Q.lock();
    let q = g.as_mut().ok_or(VirtioError::Failed)?;
    if IN_FLIGHT.load(Ordering::Acquire) {
        return Ok(());
    }
    let table = q.data.device().as_u64();
    let payload = table + RNG_PAYLOAD_OFF as u64;
    unsafe {
        core::ptr::write_bytes(
            (q.data.virt() as *mut u8).add(RNG_PAYLOAD_OFF),
            0,
            RNG_PAYLOAD,
        );
        write_indirect_write(q.data.virt() as *mut u8, payload, RNG_PAYLOAD as u32);
    }
    q.data.sync_for_device();
    if q.features & F_INDIRECT_DESC != 0 {
        q.vq.add_indirect(table, 16)?;
    } else {
        q.vq.add(payload, RNG_PAYLOAD as u32, virtio::DESC_F_WRITE)?;
    }
    let old = q.vq.last_avail;
    q.vq.publish();
    q.qdma.sync_for_device();
    if q.vq.should_kick(old) {
        kick(q.doorbell);
    }
    IN_FLIGHT.store(true, Ordering::Release);
    let _ = q.common;
    let _ = q.vec;
    Ok(())
}
