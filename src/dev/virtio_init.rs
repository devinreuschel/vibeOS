//! Modern virtio-pci transport + virtio-rng. ROADMAP §6.5.
//!
//! Transport + virtio-rng. virtio-blk is `virtio_blk_init`. ROADMAP §6.5.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use vibeos::dev::{ClaimError, DevRef, Device, Driver, IdMatch, Instance, ProbeError};
use vibeos::dma::{self, DmaAlloc, DmaBuffer};
use vibeos::irq::IrqError;
use vibeos::kalloc::TryBox;
use vibeos::lock::RANK_DEVICE;
use vibeos::pci::{Bdf, CMD_INTX_DISABLE, CMD_MASTER, CMD_MEM, MAX_BARS};
use vibeos::virtio::{
    self, COMMON_OFF_DF, COMMON_OFF_DFSEL, COMMON_OFF_DR, COMMON_OFF_DRSEL, COMMON_OFF_MSIX_CFG,
    COMMON_OFF_QDESC, COMMON_OFF_QDEVICE, COMMON_OFF_QDRIVER, COMMON_OFF_QENABLE, COMMON_OFF_QMSIX,
    COMMON_OFF_QNOTIFY, COMMON_OFF_QSEL, COMMON_OFF_QSIZE, COMMON_OFF_STATUS, DEV_RNG_LEGACY,
    DEV_RNG_MODERN, F_EVENT_IDX, F_INDIRECT_DESC, MSI_NO_VECTOR, ModernCaps, OFFER, PciCap,
    STATUS_ACKNOWLEDGE, STATUS_DRIVER, STATUS_DRIVER_OK, STATUS_FEATURES_OK, SplitLayout,
    VENDOR_ID, VirtioError, notify_addr, pick_features, write_indirect_write,
};

use crate::arch::{self, current::Arch};
use crate::dev_init;
use crate::dma_init;
use crate::irq_init;
use crate::pci_init;
use crate::per_cpu_init;
use crate::sync_init::SpinMutex;
use crate::work_init;

struct Q {
    vq: arch::current::SplitQueue,
    qdma: DmaBuffer,
    data: DmaBuffer,
    doorbell: u64,
    features: u64,
    /// The last completion's payload. `rng_take` claims and reads it under
    /// `Q`, which `harvest` holds to replace it, so no byte reaches two
    /// readers (ROADMAP §10.12, F121).
    pool: [u8; RNG_PAYLOAD],
    pool_len: u8,
    pool_pos: u8,
}

static Q: SpinMutex<Option<Q>> = SpinMutex::with_rank(None, RANK_DEVICE);
pub(crate) static ISR_VA: AtomicU64 = AtomicU64::new(0);
pub(super) static TOP_HITS: AtomicU32 = AtomicU32::new(0);
pub(super) static THREAD_HITS: AtomicU32 = AtomicU32::new(0);
pub(super) static COMPLETIONS: AtomicU32 = AtomicU32::new(0);
pub(super) static LAST_LEN: AtomicU32 = AtomicU32::new(0);
pub(super) static ALLOCED: AtomicBool = AtomicBool::new(false);
pub(super) static SOFT_HITS: AtomicU32 = AtomicU32::new(0);
static BOUND: AtomicBool = AtomicBool::new(false);
pub(super) static FEATURES: AtomicU64 = AtomicU64::new(0);
pub(super) static QDMA_DEV: AtomicU64 = AtomicU64::new(0);
pub(super) static DATA_DEV: AtomicU64 = AtomicU64::new(0);
pub(super) static DATA_VIRT: AtomicU64 = AtomicU64::new(0);
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
/// The bound device ([`bdf_key`], 0 for none), its common configuration
/// and its vector (0 for none), which `remove` stops and frees.
static DEV_KEY: AtomicU64 = AtomicU64::new(0);
static COMMON_VA: AtomicU64 = AtomicU64::new(0);
static VEC: AtomicU8 = AtomicU8::new(0);
/// Frames kept because a device's bus mastering would not turn off.
pub(crate) static KEPT_FRAMES: AtomicU64 = AtomicU64::new(0);

pub(crate) const RNG_PAYLOAD: usize = 32;
const RNG_PAYLOAD_OFF: usize = 16;

fn r8(va: u64, off: u16) -> u8 {
    // SAFETY: invariant I234: `va` is a register of a BAR `map_mmio`
    // mapped uncached for this bound device, and `off` a register offset
    // inside that capability's region; established by `pci_init::map_mmio`.
    unsafe { core::ptr::read_volatile((va.wrapping_add(off as u64)) as *const u8) }
}

fn w8(va: u64, off: u16, v: u8) {
    // SAFETY: invariant I234: `va` is a register of a BAR `map_mmio`
    // mapped uncached for this bound device, and `off` a register offset
    // inside that capability's region; established by `pci_init::map_mmio`.
    unsafe { core::ptr::write_volatile((va.wrapping_add(off as u64)) as *mut u8, v) }
}

fn r16(va: u64, off: u16) -> u16 {
    // SAFETY: invariant I234: `va` is a register of a BAR `map_mmio`
    // mapped uncached for this bound device, and `off` a register offset
    // inside that capability's region; established by `pci_init::map_mmio`.
    unsafe {
        u16::from_le(core::ptr::read_volatile(
            (va.wrapping_add(off as u64)) as *const u16,
        ))
    }
}

fn w16(va: u64, off: u16, v: u16) {
    // SAFETY: invariant I234: `va` is a register of a BAR `map_mmio`
    // mapped uncached for this bound device, and `off` a register offset
    // inside that capability's region; established by `pci_init::map_mmio`.
    unsafe {
        core::ptr::write_volatile((va.wrapping_add(off as u64)) as *mut u16, v.to_le());
    }
}

fn r32(va: u64, off: u16) -> u32 {
    // SAFETY: invariant I234: `va` is a register of a BAR `map_mmio`
    // mapped uncached for this bound device, and `off` a register offset
    // inside that capability's region; established by `pci_init::map_mmio`.
    unsafe {
        u32::from_le(core::ptr::read_volatile(
            (va.wrapping_add(off as u64)) as *const u32,
        ))
    }
}

fn w32(va: u64, off: u16, v: u32) {
    // SAFETY: invariant I234: `va` is a register of a BAR `map_mmio`
    // mapped uncached for this bound device, and `off` a register offset
    // inside that capability's region; established by `pci_init::map_mmio`.
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

/// `bdf` as a nonzero word, for an atomic that names a device.
pub(crate) fn bdf_key(bdf: Bdf) -> u64 {
    1 << 63
        | u64::from(bdf.segment) << 24
        | u64::from(bdf.bus) << 16
        | u64::from(bdf.device) << 8
        | u64::from(bdf.function)
}

/// Reads of device status [`reset`] makes at most: DESIGN §9.6's cap on a
/// poll of a device that may never answer.
pub(crate) const RESET_POLLS: u32 = 1_000_000;

/// Write device status 0 and poll until it reads 0, at most
/// [`RESET_POLLS`] reads; whether it did.
pub(crate) fn reset(common: u64) -> bool {
    w8(common, COMMON_OFF_STATUS, 0);
    let mut n = 0u32;
    while n < RESET_POLLS {
        if r8(common, COMMON_OFF_STATUS) == 0 {
            return true;
        }
        n += 1;
        core::hint::spin_loop();
    }
    false
}

/// How far [`stop_device`] got. Only `Stuck` keeps memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stopped {
    /// Status read back 0, and bus mastering reads back off.
    Reset,
    /// The reset timed out, but bus mastering reads back off, so the
    /// device can no longer reach memory.
    MasterOff,
    /// Bus mastering still reads back on: the device may still write the
    /// memory it was given.
    Stuck,
}

/// Quiesce a virtio function that may hold queue addresses: device status
/// 0, polled within [`RESET_POLLS`] reads, then `COMMAND.MASTER` cleared
/// and read back (DESIGN §12.3). Run it with no spinlock held and IF as
/// the caller left it, before any vector or memory the device was given
/// is freed (INTERRUPTS.md §5.4).
pub(crate) fn stop_device(bdf: Bdf, common: u64) -> Stopped {
    let reset_ok = reset(common);
    if !reset_ok {
        crate::marker!("vibeOS: virtio: {} reset timeout", bdf);
    }
    let cmd = pci_init::update_command(bdf, 0, CMD_MASTER);
    #[cfg(feature = "kernel_tests")]
    crate::dev::ktest::record_quiesce(bdf, reset_ok, r8(common, COMMON_OFF_STATUS), cmd);
    if cmd & CMD_MASTER != 0 {
        Stopped::Stuck
    } else if reset_ok {
        Stopped::Reset
    } else {
        Stopped::MasterOff
    }
}

/// Give `buf` back to the buddy allocator, or, after `Stuck`, keep it:
/// forgotten, never dropped (C-FRAMES), and counted in [`KEPT_FRAMES`].
/// The frames it kept.
pub(crate) fn release(stopped: Stopped, buf: DmaBuffer) -> usize {
    if stopped != Stopped::Stuck {
        dma_init::free(buf);
        return 0;
    }
    let n = buf.frame_count();
    core::mem::forget(buf);
    KEPT_FRAMES.fetch_add(n as u64, Ordering::Relaxed);
    n
}

/// The line for a device whose bus mastering stayed on: its `kept` frames
/// stay allocated (PITFALLS.md §9.6's action at the poll cap).
pub(crate) fn report_stuck(bdf: Bdf, stopped: Stopped, kept: usize) {
    if stopped == Stopped::Stuck {
        crate::marker!(
            "vibeOS: virtio: {} bus master stuck, {} frames kept",
            bdf,
            kept
        );
    }
}

/// The one unwind of a probe that fails once bus mastering is on, in
/// INTERRUPTS.md §5.4's order: stop the device, turn MSI-X off and free
/// `vec`, then free `bufs`, or keep them when the device is `Stuck`.
fn fail_probe(dev: &Device, common: u64, vec: Option<u8>, bufs: [Option<DmaBuffer>; 2]) {
    let stopped = stop_device(dev.addr, common);
    irq_init::disable_msix(dev);
    if let Some(v) = vec {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "cleanup after an error the caller already returns: a vector that fails to free stays allocated, which nothing can act on (DESIGN §2.5)"
        )]
        let _ = irq_init::free_vector(v);
    }
    let mut kept = 0usize;
    for b in bufs.into_iter().flatten() {
        kept = kept.saturating_add(release(stopped, b));
    }
    report_stuck(dev.addr, stopped, kept);
}

/// The softirq half: proves a work item may allocate. The test observable
/// counts only an allocation that succeeded.
fn on_soft(_arg: usize) {
    if TryBox::try_new(0x22u8).is_ok() {
        SOFT_HITS.fetch_add(1, Ordering::SeqCst);
    }
}

/// One device, module state: `_ctx` is `None` (ROADMAP §10.12).
fn rng_top(_ctx: Option<&(dyn core::any::Any + Send + Sync)>) {
    TOP_HITS.fetch_add(1, Ordering::SeqCst);
    let isr = ISR_VA.load(Ordering::Acquire);
    if isr != 0 {
        // Reading the ISR status acknowledges the interrupt; the value
        // itself is not needed (virtio 1.x §4.1.4.5).
        let _ = r8(isr, 0);
    }
    if !work_init::raise_softirq(on_soft, 1) {
        crate::klog_ratelimited!(
            1000,
            vibeos::log::Level::Warn,
            "vibeOS: virtio: rng softirq ring full, work dropped"
        );
    }
}

/// Replace the pool with the completion's payload. Runs under `Q`.
fn publish_pool(q: &mut Q, len: u32) {
    let n = (len as usize).min(RNG_PAYLOAD);
    let p = q.data.virt().wrapping_add(RNG_PAYLOAD_OFF as u64) as *const u8;
    for (i, b) in q.pool.iter_mut().enumerate().take(n) {
        // SAFETY: invariant: `data` is the rng's buffer, whose `RNG_PAYLOAD`
        // bytes from `RNG_PAYLOAD_OFF` the device wrote and `harvest` synced
        // for the CPU, and `i < n <= RNG_PAYLOAD`; established by
        // `virtio_init::setup`, which allocates it.
        *b = unsafe { p.add(i).read_volatile() };
    }
    #[cfg(feature = "kernel_tests")]
    let n = {
        let mut n = n;
        crate::dev::ktest::rng_hooks::on_publish(&mut q.pool, &mut n);
        n.min(RNG_PAYLOAD)
    };
    // `n <= RNG_PAYLOAD`, which is 32.
    q.pool_len = n as u8;
    q.pool_pos = 0;
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
                q.data.sync_for_cpu::<Arch>();
                publish_pool(q, last);
                IN_FLIGHT.store(false, Ordering::Release);
            }
        }
    }
    if n != 0 {
        LAST_LEN.store(last, Ordering::SeqCst);
        COMPLETIONS.fetch_add(n, Ordering::SeqCst);
    }
}

fn rng_work(_ctx: Option<&(dyn core::any::Any + Send + Sync)>) {
    THREAD_HITS.fetch_add(1, Ordering::SeqCst);
    // The threaded half may allocate; the observable records a success.
    if TryBox::try_new(0x11u8).is_ok() {
        ALLOCED.store(true, Ordering::SeqCst);
    }
    harvest();
}

fn kick(doorbell: u64) {
    dma::dma_wmb::<Arch>();
    // SAFETY: invariant I234: `doorbell` is queue 0's notify register inside
    // the notify capability's BAR, which `map_mmio` mapped uncached, checked
    // against the capability length by `virtio::notify_addr`; established by
    // `pci_init::map_mmio`.
    unsafe {
        core::ptr::write_volatile(doorbell as *mut u16, 0u16);
    }
}

fn setup(dev: &Device, caps: ModernCaps) -> Result<(), VirtioError> {
    let common_cap = caps.common.ok_or(VirtioError::NoCaps)?;
    let notify_cap = caps.notify.ok_or(VirtioError::NoCaps)?;
    let isr_cap = caps.isr.ok_or(VirtioError::NoCaps)?;
    let common = region(dev, common_cap).ok_or(VirtioError::NoCaps)?;
    let notify_base = region(dev, notify_cap).ok_or(VirtioError::NoCaps)?;
    let isr = region(dev, isr_cap).ok_or(VirtioError::NoCaps)?;
    // Memory decode before the first touch, with bus mastering off, which
    // firmware can leave on; bus mastering only once the reset completed
    // (DESIGN §12.3).
    pci_init::update_command(dev.addr, CMD_MEM, CMD_MASTER);
    if !reset(common) {
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
    let feat = match pick_features(device_feat, OFFER) {
        Ok(f) => f,
        Err(e) => {
            fail_probe(dev, common, None, [None, None]);
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
        fail_probe(dev, common, None, [None, None]);
        return Err(VirtioError::Features);
    }

    let cpu = irq_init::threaded_cpu();
    let Some(pc) = per_cpu_init::cpu(cpu) else {
        fail_probe(dev, common, None, [None, None]);
        return Err(VirtioError::Failed);
    };
    let vec = match irq_init::allocate_vector(cpu) {
        Ok(v) => v,
        Err(_) => {
            fail_probe(dev, common, None, [None, None]);
            return Err(VirtioError::Failed);
        }
    };
    if irq_init::set_threaded(vec, Some(rng_top), rng_work, None).is_err() {
        fail_probe(dev, common, Some(vec), [None, None]);
        return Err(VirtioError::Failed);
    }
    pci_init::update_command(dev.addr, CMD_INTX_DISABLE, 0);
    if let Err(e) = irq_init::enable_msix(dev, 0, vec, pc.apic_id.load(Ordering::Relaxed) as u8) {
        fail_probe(dev, common, Some(vec), [None, None]);
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
        fail_probe(dev, common, Some(vec), [None, None]);
        return Err(VirtioError::BadQueue);
    }
    w16(common, COMMON_OFF_QSIZE, qsz);
    let Some(layout) = SplitLayout::new(qsz) else {
        fail_probe(dev, common, Some(vec), [None, None]);
        return Err(VirtioError::BadQueue);
    };
    let Some(qdma) = dma_init::alloc(DmaAlloc::dma32(layout.total as u64)) else {
        fail_probe(dev, common, Some(vec), [None, None]);
        return Err(VirtioError::Failed);
    };
    let Some(data) = dma_init::alloc(DmaAlloc::dma32(64)) else {
        fail_probe(dev, common, Some(vec), [Some(qdma), None]);
        return Err(VirtioError::Failed);
    };
    // SAFETY: both buffers were just allocated, are `len()` bytes long at
    // `virt()`, and are not yet shared with the device; established by
    // `dma_init::alloc`.
    unsafe {
        core::ptr::write_bytes(qdma.virt() as *mut u8, 0, qdma.len() as usize);
        core::ptr::write_bytes(data.virt() as *mut u8, 0, data.len() as usize);
    }
    // SAFETY: invariant I233: `qdma` is a page-aligned DMA buffer of at
    // least `layout.total` bytes, which stays allocated beside the queue
    // until the device is reset and it is freed; established by
    // `dma_init::alloc`.
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
    #[cfg(feature = "kernel_tests")]
    if crate::dev::ktest::fail_after_qenable(dev.addr) {
        fail_probe(dev, common, Some(vec), [Some(qdma), Some(data)]);
        return Err(VirtioError::Failed);
    }
    let qoff = r16(common, COMMON_OFF_QNOTIFY);
    let Some(doorbell) = notify_addr(
        notify_base,
        0,
        notify_cap.length,
        qoff,
        notify_cap.notify_off_multiplier,
    ) else {
        fail_probe(dev, common, Some(vec), [Some(qdma), Some(data)]);
        return Err(VirtioError::Notify);
    };

    let st = r8(common, COMMON_OFF_STATUS);
    w8(common, COMMON_OFF_STATUS, st | STATUS_DRIVER_OK);

    ISR_VA.store(isr, Ordering::Release);
    COMMON_VA.store(common, Ordering::Release);
    VEC.store(vec, Ordering::Release);
    DEV_KEY.store(bdf_key(dev.addr), Ordering::Release);
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
        features: feat,
        pool: [0; RNG_PAYLOAD],
        pool_len: 0,
        pool_pos: 0,
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

/// Claim each BAR a capability in `caps` lives in. Several capabilities
/// share a BAR, so `Already` on one this device claimed is success.
fn claim_bars(dev: &DevRef, caps: &ModernCaps) -> Result<(), ProbeError> {
    for c in [caps.common, caps.notify, caps.isr, caps.device]
        .into_iter()
        .flatten()
    {
        let i = c.bar as usize;
        if i >= MAX_BARS || dev.resources[i].is_empty() {
            continue;
        }
        match dev_init::claim(dev, c.bar) {
            Ok(claim) => {
                if let Err(claim) = dev_init::hold(claim, None) {
                    dev_init::release(claim);
                    return Err(ProbeError::Busy);
                }
            }
            Err(ClaimError::Already) => {}
            Err(ClaimError::Overlap) => return Err(ProbeError::Busy),
            Err(ClaimError::Empty | ClaimError::BadIndex | ClaimError::Ram | ClaimError::Full) => {
                return Err(ProbeError::NoResource);
            }
        }
    }
    Ok(())
}

pub(crate) struct RngDriver;

static RNG_IDS: &[IdMatch] = &[
    IdMatch::vid_did(VENDOR_ID, DEV_RNG_MODERN),
    IdMatch::vid_did(VENDOR_ID, DEV_RNG_LEGACY),
];

pub(crate) static RNG_DRV: RngDriver = RngDriver;

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
    /// virtio-rng stays one device with module state (ROADMAP §10.12), so
    /// the registry owns no instance of it.
    /// A second function is refused before anything touches it, so it
    /// cannot overwrite `Q` and `ISR_VA` and orphan the first device's
    /// queue and vector (ROADMAP §10.12, F121). The check is a load:
    /// `dev_init::bind_all` probes one device at a time, and a concurrent
    /// binder would need a claim (a compare-exchange) instead.
    fn probe(&self, dev: &DevRef) -> Result<Option<Instance>, ProbeError> {
        if BOUND.load(Ordering::Acquire) {
            crate::marker!("vibeOS: virtio: rng {} already bound", dev.addr);
            return Err(ProbeError::Busy);
        }
        let caps = virtio::read_modern_caps(&mut pci_init::HwCfg, dev.addr);
        if !caps.is_complete() {
            crate::marker!("vibeOS: virtio: missing modern caps");
            return Err(ProbeError::NoResource);
        }
        claim_bars(dev, &caps)?;
        match setup(dev, caps) {
            Ok(()) => {
                BOUND.store(true, Ordering::Release);
                Ok(None)
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
    /// DEVICES.md §12.2 rule 7's one quiesce. The queue leaves `Q` first,
    /// so the bottom half finds nothing to harvest, and the top half no
    /// ISR; then the device stops before its vector and memory go, and
    /// `BOUND` clears last, when nothing of the device is left.
    fn remove(&self, dev: &DevRef) {
        if DEV_KEY.load(Ordering::Acquire) != bdf_key(dev.addr) {
            return;
        }
        let q = Q.lock().take();
        ISR_VA.store(0, Ordering::Release);
        let common = COMMON_VA.swap(0, Ordering::AcqRel);
        let stopped = if common != 0 {
            stop_device(dev.addr, common)
        } else {
            Stopped::Reset
        };
        irq_init::disable_msix(dev);
        let vec = VEC.swap(0, Ordering::AcqRel);
        if vec != 0 && irq_init::free_vector(vec).is_err() {
            crate::klog!(
                vibeos::log::Level::Warn,
                "vibeOS: virtio: rng {} vector {:#x} not freed",
                dev.addr,
                vec
            );
        }
        if let Some(Q { qdma, data, .. }) = q {
            let kept = release(stopped, qdma).saturating_add(release(stopped, data));
            report_stuck(dev.addr, stopped, kept);
        }
        IN_FLIGHT.store(false, Ordering::Release);
        DEV_KEY.store(0, Ordering::Release);
        BOUND.store(false, Ordering::Release);
    }
}

pub fn init() {
    if !dev_init::register_driver(&RNG_DRV) {
        crate::klog!(
            vibeos::log::Level::Warn,
            "vibeOS: virtio: rng driver not registered: registry full"
        );
    }
}

pub fn rng_bound() -> bool {
    BOUND.load(Ordering::Acquire)
}

/// Copy harvested virtio-rng bytes into `buf`; returns how many, 0 when
/// the pool is empty or no device is bound. The claim and the copy run
/// under `Q`, so a refill cannot replace the pool between them.
pub fn rng_take(buf: &mut [u8]) -> usize {
    // pair order: VFS (fs_init::KERNFS's store lock) before virtio-rng `Q`
    let mut g = Q.lock_nested(1);
    let Some(q) = g.as_mut() else {
        return 0;
    };
    let pos = q.pool_pos as usize;
    let n = buf.len().min((q.pool_len as usize).saturating_sub(pos));
    let (Some(dst), Some(src)) = (buf.get_mut(..n), q.pool.get(pos..pos + n)) else {
        return 0;
    };
    if n == 0 {
        return 0;
    }
    // `pos + n <= pool_len`, a `u8`.
    q.pool_pos = (pos + n) as u8;
    #[cfg(feature = "kernel_tests")]
    crate::dev::ktest::rng_hooks::on_take_claim();
    dst.copy_from_slice(src);
    n
}

/// Submit one entropy buffer. Completion is harvested by the IRQ thread.
/// A `/dev/random` read calls it through `entropy_init::hw_fill` with the
/// kernfs store lock held, both `RANK_DEVICE`.
pub fn rng_request() -> Result<(), VirtioError> {
    // pair order: fs_init::KERNFS's store lock, then Q
    let mut g = Q.lock_nested(1);
    let q = g.as_mut().ok_or(VirtioError::Failed)?;
    if IN_FLIGHT.load(Ordering::Acquire) {
        return Ok(());
    }
    let table = q.data.device().as_u64();
    let payload = table + RNG_PAYLOAD_OFF as u64;
    // SAFETY: invariant: `data` is the rng's 64-byte, page-aligned DMA
    // buffer, not in flight (`IN_FLIGHT` is clear under `Q`): its first 16
    // bytes hold the indirect table and `RNG_PAYLOAD` bytes from
    // `RNG_PAYLOAD_OFF` the payload; established by `virtio_init::setup`.
    unsafe {
        core::ptr::write_bytes(
            (q.data.virt() as *mut u8).add(RNG_PAYLOAD_OFF),
            0,
            RNG_PAYLOAD,
        );
        write_indirect_write(q.data.virt() as *mut u8, payload, RNG_PAYLOAD as u32);
    }
    q.data.sync_for_device::<Arch>();
    if q.features & F_INDIRECT_DESC != 0 {
        q.vq.add_indirect(table, 16)?;
    } else {
        q.vq.add(payload, RNG_PAYLOAD as u32, virtio::DESC_F_WRITE)?;
    }
    let old = q.vq.last_avail;
    q.vq.publish();
    q.qdma.sync_for_device::<Arch>();
    if q.vq.should_kick(old) {
        kick(q.doorbell);
    }
    IN_FLIGHT.store(true, Ordering::Release);
    Ok(())
}
