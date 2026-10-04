//! virtio-input: keyboard and pointer. ROADMAP §11.5.
//!
//! PCI modern transport. Eventq buffers are harvested on IRQ and on
//! `pop`, so a missing MSI still feeds the console mux.

use vibeos::dev::{DevRef, Driver, IdMatch, Instance, ProbeError};
use vibeos::dma::{self, DmaAlloc, DmaBuffer};
use vibeos::irq::IrqId;
use vibeos::kbd::{DecodedKey, RING_CAP, Ring};
use vibeos::lock::RANK_DEVICE;
use vibeos::pci::{CMD_INTX_DISABLE, CMD_MASTER, CMD_MEM};
use vibeos::virtio::{
    self, COMMON_OFF_DF, COMMON_OFF_DFSEL, COMMON_OFF_DR, COMMON_OFF_DRSEL, COMMON_OFF_MSIX_CFG,
    COMMON_OFF_QDESC, COMMON_OFF_QDEVICE, COMMON_OFF_QDRIVER, COMMON_OFF_QENABLE, COMMON_OFF_QMSIX,
    COMMON_OFF_QNOTIFY, COMMON_OFF_QSEL, COMMON_OFF_QSIZE, COMMON_OFF_STATUS, DESC_F_WRITE,
    DEV_INPUT_MODERN, F_EVENT_IDX, MSI_NO_VECTOR, ModernCaps, OFFER, PciCap, STATUS_ACKNOWLEDGE,
    STATUS_DRIVER, STATUS_DRIVER_OK, STATUS_FEATURES_OK, SplitLayout, VENDOR_ID, VirtioError,
    notify_addr,
};
use vibeos::virtio_input::{EVENT_SIZE, EvDecoder, Event};

use crate::arch::{self, current::Arch};
use crate::dev_init;
use crate::dma_init;
use crate::irq_init;
use crate::pci_init;
use crate::sync_init::SpinMutex;
use crate::virtio_init;

const MAX_DEVS: usize = 4;
const MAX_Q: usize = 16;

struct Dev {
    vq: arch::current::SplitQueue,
    qdma: DmaBuffer,
    data: DmaBuffer,
    doorbell: u64,
    isr: u64,
    common: u64,
    irq: u32,
    key: u64,
    qsz: u16,
    desc_slot: [u8; MAX_Q],
}

struct State {
    devs: [Option<Dev>; MAX_DEVS],
    ring: Ring<DecodedKey, RING_CAP>,
    dec: EvDecoder,
}

static STATE: SpinMutex<State> = SpinMutex::with_rank(
    State {
        devs: [None, None, None, None],
        ring: Ring::empty(DecodedKey::Char(0)),
        dec: EvDecoder::new(),
    },
    RANK_DEVICE,
);

fn r8(va: u64, off: u16) -> u8 {
    // SAFETY: invariant I54: `va` is a BAR `map_mmio` mapped uncached;
    // established by `pci_init::map_mmio`.
    unsafe { core::ptr::read_volatile((va.wrapping_add(off as u64)) as *const u8) }
}

fn w8(va: u64, off: u16, v: u8) {
    // SAFETY: invariant I54: as `r8`; established by `pci_init::map_mmio`.
    unsafe { core::ptr::write_volatile((va.wrapping_add(off as u64)) as *mut u8, v) }
}

fn r16(va: u64, off: u16) -> u16 {
    // SAFETY: invariant I54: as `r8`; established by `pci_init::map_mmio`.
    unsafe {
        u16::from_le(core::ptr::read_volatile(
            (va.wrapping_add(off as u64)) as *const u16,
        ))
    }
}

fn w16(va: u64, off: u16, v: u16) {
    // SAFETY: invariant I54: as `r8`; established by `pci_init::map_mmio`.
    unsafe {
        core::ptr::write_volatile((va.wrapping_add(off as u64)) as *mut u16, v.to_le());
    }
}

fn r32(va: u64, off: u16) -> u32 {
    // SAFETY: invariant I54: as `r8`; established by `pci_init::map_mmio`.
    unsafe {
        u32::from_le(core::ptr::read_volatile(
            (va.wrapping_add(off as u64)) as *const u32,
        ))
    }
}

fn w32(va: u64, off: u16, v: u32) {
    // SAFETY: invariant I54: as `r8`; established by `pci_init::map_mmio`.
    unsafe {
        core::ptr::write_volatile((va.wrapping_add(off as u64)) as *mut u32, v.to_le());
    }
}

fn w64(va: u64, off: u16, v: u64) {
    w32(va, off, v as u32);
    w32(va, off + 4, (v >> 32) as u32);
}

fn region(dev: &DevRef, cap: PciCap) -> Option<u64> {
    let r = dev.resources.get(cap.bar as usize)?;
    if (cap.offset as u64) >= r.size {
        return None;
    }
    let va = dev_init::bar_va(dev, cap.bar)?;
    Some(va.wrapping_add(cap.offset as u64))
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

fn clamp_qsize(hw: u16) -> u16 {
    let n = hw.min(MAX_Q as u16);
    if n == 0 {
        return 0;
    }
    1u16 << (15u32 - n.leading_zeros())
}

fn kick(doorbell: u64, qi: u16) {
    dma::dma_wmb::<Arch>();
    // SAFETY: invariant I54: `doorbell` is the queue notify register;
    // established by `pci_init::map_mmio` and `virtio::notify_addr`.
    unsafe {
        core::ptr::write_volatile(doorbell as *mut u16, virtio::queue_notify(qi));
    }
}

fn ack_isr(st: &State) {
    for d in st.devs.iter().flatten() {
        if d.isr != 0 {
            let _ = r8(d.isr, 0);
        }
    }
}

fn event_at(data: &DmaBuffer, slot: u8) -> Option<Event> {
    let off = (slot as usize).checked_mul(EVENT_SIZE)?;
    let va = data.virt().wrapping_add(off as u64) as *const u8;
    let mut raw = [0u8; EVENT_SIZE];
    let mut i = 0usize;
    while i < EVENT_SIZE {
        if let Some(b) = raw.get_mut(i) {
            // SAFETY: invariant I53: `data` holds `qsz` events; `slot < qsz`;
            // established by `virtio_input_init::setup`.
            *b = unsafe { va.add(i).read_volatile() };
        }
        i = i.saturating_add(1);
    }
    Event::from_le_bytes(&raw)
}

fn irq_opt(raw: u32) -> Option<IrqId> {
    if raw == 0 {
        None
    } else {
        Some(IrqId::from_raw(raw))
    }
}

fn refill(d: &mut Dev, slot: u8) {
    if u16::from(slot) >= d.qsz {
        return;
    }
    let Some(off) = (slot as u64).checked_mul(EVENT_SIZE as u64) else {
        return;
    };
    let addr = d.data.device().as_u64().wrapping_add(off);
    let Ok(head) = d.vq.add(addr, EVENT_SIZE as u32, DESC_F_WRITE) else {
        return;
    };
    if let Some(s) = d.desc_slot.get_mut(head as usize) {
        *s = slot;
    }
    let old = d.vq.last_avail();
    d.vq.publish();
    d.qdma.sync_for_device::<Arch>();
    if d.vq.should_kick(old) {
        kick(d.doorbell, 0);
    }
}

fn harvest(st: &mut State) {
    let mut i = 0usize;
    while i < MAX_DEVS {
        let Some(d) = st.devs.get_mut(i).and_then(Option::as_mut) else {
            i = i.saturating_add(1);
            continue;
        };
        d.data.sync_for_cpu::<Arch>();
        while let Some(u) = d.vq.get_used() {
            let slot = d.desc_slot.get(u.id as usize).copied().unwrap_or(0);
            if let Some(ev) = event_at(&d.data, slot)
                && let Some(k) = st.dec.feed(ev)
            {
                st.ring.push(k);
            }
            refill(d, slot);
        }
        i = i.saturating_add(1);
    }
}

fn input_top(_ctx: Option<&(dyn core::any::Any + Send + Sync)>) {
    let st = STATE.lock();
    ack_isr(&st);
}

fn input_work(_ctx: Option<&(dyn core::any::Any + Send + Sync)>) {
    let mut st = STATE.lock();
    harvest(&mut st);
}

fn fail_setup(dev: &DevRef, common: u64, irq: Option<IrqId>, bufs: [Option<DmaBuffer>; 2]) {
    let stopped = virtio_init::stop_device(dev.addr, common);
    irq_init::disable_msix(dev);
    if let Some(v) = irq {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "cleanup after a failed probe: a vector that fails to free stays allocated (DESIGN §2.5)"
        )]
        let _ = irq_init::free_vector(v);
    }
    let mut kept = 0usize;
    for b in bufs.into_iter().flatten() {
        kept = kept.saturating_add(virtio_init::release(stopped, b));
    }
    virtio_init::report_stuck(dev.addr, stopped, kept);
}

fn setup(dev: &DevRef, caps: ModernCaps) -> Result<Dev, VirtioError> {
    let common_cap = caps.common.ok_or(VirtioError::NoCaps)?;
    let notify_cap = caps.notify.ok_or(VirtioError::NoCaps)?;
    let isr_cap = caps.isr.ok_or(VirtioError::NoCaps)?;
    let common = region(dev, common_cap).ok_or(VirtioError::NoCaps)?;
    let notify_base = region(dev, notify_cap).ok_or(VirtioError::NoCaps)?;
    let isr = region(dev, isr_cap).ok_or(VirtioError::NoCaps)?;
    pci_init::update_command(dev.addr, CMD_MEM, CMD_MASTER);
    if !virtio_init::reset(common) {
        return Err(VirtioError::Failed);
    }
    pci_init::update_command(dev.addr, CMD_MASTER, 0);
    w8(common, COMMON_OFF_STATUS, STATUS_ACKNOWLEDGE);
    w8(
        common,
        COMMON_OFF_STATUS,
        STATUS_ACKNOWLEDGE | STATUS_DRIVER,
    );
    let device_feat = read_features(common);
    let feat = match virtio::pick_features(device_feat, OFFER) {
        Ok(f) => f,
        Err(e) => {
            fail_setup(dev, common, None, [None, None]);
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
        fail_setup(dev, common, None, [None, None]);
        return Err(VirtioError::Features);
    }

    let mut irq_raw = 0u32;
    if let Some(irq) = irq_init::alloc_msi(dev, 1).ok().and_then(|s| s.get(0)) {
        let cpu = irq_init::threaded_cpu();
        if irq_init::set_affinity(irq, cpu).is_ok()
            && irq_init::set_threaded(irq, Some(input_top), input_work, None).is_ok()
            && irq_init::enable_msix(dev, 0, irq).is_ok()
        {
            pci_init::update_command(dev.addr, CMD_INTX_DISABLE, 0);
            w16(common, COMMON_OFF_MSIX_CFG, MSI_NO_VECTOR);
            irq_raw = irq.raw();
        } else {
            irq_init::disable_msix(dev);
            #[expect(
                clippy::let_underscore_must_use,
                reason = "optional IRQ: poll still feeds the mux (DESIGN §2.5)"
            )]
            let _ = irq_init::free_vector(irq);
        }
    }

    w16(common, COMMON_OFF_QSEL, 0);
    let qsz = clamp_qsize(r16(common, COMMON_OFF_QSIZE));
    if qsz == 0 {
        fail_setup(dev, common, irq_opt(irq_raw), [None, None]);
        return Err(VirtioError::BadQueue);
    }
    w16(common, COMMON_OFF_QSIZE, qsz);
    let Some(layout) = SplitLayout::new(qsz) else {
        fail_setup(dev, common, irq_opt(irq_raw), [None, None]);
        return Err(VirtioError::BadQueue);
    };
    let Some(qdma) = dma_init::alloc(DmaAlloc::dma32(layout.total as u64)) else {
        fail_setup(dev, common, irq_opt(irq_raw), [None, None]);
        return Err(VirtioError::Failed);
    };
    let data_len = (qsz as u64).saturating_mul(EVENT_SIZE as u64);
    let Some(data) = dma_init::alloc(DmaAlloc::dma32(data_len.max(8))) else {
        fail_setup(dev, common, irq_opt(irq_raw), [Some(qdma), None]);
        return Err(VirtioError::Failed);
    };
    // SAFETY: both buffers were just allocated and are not shared yet;
    // established by `dma_init::alloc`.
    unsafe {
        core::ptr::write_bytes(qdma.virt() as *mut u8, 0, qdma.len() as usize);
        core::ptr::write_bytes(data.virt() as *mut u8, 0, data.len() as usize);
    }
    // SAFETY: invariant I53: `qdma` is a page-aligned DMA buffer of at
    // least `layout.total` bytes; established by `dma_init::alloc`.
    let mut vq = unsafe {
        arch::current::SplitQueue::new(layout, qdma.virt() as *mut u8, feat & F_EVENT_IDX != 0)
    };
    vq.init();
    qdma.sync_for_device::<Arch>();
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
        fail_setup(dev, common, irq_opt(irq_raw), [Some(qdma), Some(data)]);
        return Err(VirtioError::Notify);
    };

    let st = r8(common, COMMON_OFF_STATUS);
    w8(common, COMMON_OFF_STATUS, st | STATUS_DRIVER_OK);

    let mut desc_slot = [0u8; MAX_Q];
    let mut slot = 0u8;
    while slot < qsz as u8 {
        let Some(off) = (slot as u64).checked_mul(EVENT_SIZE as u64) else {
            break;
        };
        let addr = data.device().as_u64().wrapping_add(off);
        let Ok(head) = vq.add(addr, EVENT_SIZE as u32, DESC_F_WRITE) else {
            break;
        };
        if let Some(s) = desc_slot.get_mut(head as usize) {
            *s = slot;
        }
        let old = vq.last_avail();
        vq.publish();
        if vq.should_kick(old) {
            kick(doorbell, 0);
        }
        slot = slot.saturating_add(1);
    }
    qdma.sync_for_device::<Arch>();
    data.sync_for_device::<Arch>();

    crate::klog!(
        vibeos::log::Level::Info,
        "vibeOS: virtio: input {} qsz={} irq={}",
        dev.addr,
        qsz,
        irq_raw
    );
    Ok(Dev {
        vq,
        qdma,
        data,
        doorbell,
        isr,
        common,
        irq: irq_raw,
        key: virtio_init::bdf_key(dev.addr),
        qsz,
        desc_slot,
    })
}

pub(crate) struct InputDriver;

static INPUT_IDS: &[IdMatch] = &[IdMatch::vid_did(VENDOR_ID, DEV_INPUT_MODERN)];

pub(crate) static INPUT_DRV: InputDriver = InputDriver;

impl Driver for InputDriver {
    fn name(&self) -> &'static str {
        "virtio-input"
    }
    fn ids(&self) -> &'static [IdMatch] {
        INPUT_IDS
    }
    fn order(&self) -> u8 {
        45
    }
    fn probe(&self, dev: &DevRef) -> Result<Option<Instance>, ProbeError> {
        let caps = virtio::read_modern_caps(&mut pci_init::HwCfg, dev.addr);
        if !caps.is_complete() {
            return Err(ProbeError::NoResource);
        }
        dev_init::claim_mem_bars(dev)?;
        match setup(dev, caps) {
            Ok(built) => {
                let mut st = STATE.lock();
                let Some(slot) = st.devs.iter_mut().find(|s| s.is_none()) else {
                    drop(st);
                    fail_setup(
                        dev,
                        built.common,
                        irq_opt(built.irq),
                        [Some(built.qdma), Some(built.data)],
                    );
                    dev_init::release_bars(dev);
                    return Err(ProbeError::Busy);
                };
                *slot = Some(built);
                Ok(None)
            }
            Err(VirtioError::NoVersion1) => {
                dev_init::release_bars(dev);
                Err(ProbeError::Failed)
            }
            Err(_) => {
                crate::klog!(
                    vibeos::log::Level::Warn,
                    "vibeOS: virtio: input probe failed"
                );
                dev_init::release_bars(dev);
                Err(ProbeError::Failed)
            }
        }
    }
    fn remove(&self, dev: &DevRef) {
        let key = virtio_init::bdf_key(dev.addr);
        let mut st = STATE.lock();
        let Some(slot) = st
            .devs
            .iter_mut()
            .find(|s| s.as_ref().is_some_and(|d| d.key == key))
        else {
            return;
        };
        let Some(d) = slot.take() else {
            return;
        };
        drop(st);
        let stopped = virtio_init::stop_device(dev.addr, d.common);
        irq_init::disable_msix(dev);
        if d.irq != 0 && irq_init::free_vector(IrqId::from_raw(d.irq)).is_err() {
            crate::klog!(
                vibeos::log::Level::Warn,
                "vibeOS: virtio: input irq {} not freed",
                d.irq
            );
        }
        let kept = virtio_init::release(stopped, d.qdma)
            .saturating_add(virtio_init::release(stopped, d.data));
        virtio_init::report_stuck(dev.addr, stopped, kept);
        dev_init::release_bars(dev);
    }
}

pub fn init() {
    if !dev_init::register_driver(&INPUT_DRV) {
        crate::klog!(
            vibeos::log::Level::Warn,
            "vibeOS: virtio: input driver not registered: registry full"
        );
    }
}

/// Next decoded key, after a harvest. IRQ-off caller (DESIGN §9.4).
pub fn pop() -> Option<DecodedKey> {
    let mut st = STATE.lock();
    harvest(&mut st);
    st.ring.pop()
}

/// How many virtio-input functions are bound.
#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(dead_code, reason = "in-guest virtio-input proof")
)]
pub fn bound() -> u64 {
    let st = STATE.lock();
    st.devs.iter().filter(|d| d.is_some()).count() as u64
}
