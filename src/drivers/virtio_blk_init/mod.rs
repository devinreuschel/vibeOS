//! virtio-blk driver. ROADMAP §7.2.
//!
//! Modern transport from Phase 6. One VQ per CPU when `F_MQ` is offered;
//! otherwise a single request queue. Completions run on the threaded IRQ
//! (DESIGN §2.2 / §5.4). `kick` runs `dma_wmb` before the doorbell, and
//! `SplitQueue::should_kick` runs `dma_mb` before its kick-decision load.
//! Status bytes live in DMA, not on the submitter stack.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};

use vibeos::block::blockdev::Backing;
use vibeos::block::{
    BlockDevice, BlockError, Completion, DeviceState, MAX_QUEUE, Op, Queue, Request,
};
use vibeos::dev::{ClaimError, DevRef, Device, Driver, IdMatch, Instance, ProbeError};
use vibeos::dma::{self, DmaAlloc, DmaBuffer};
use vibeos::irq::IrqError;
use vibeos::kalloc::TryBox;
use vibeos::lock::RANK_DEVICE;
use vibeos::pci::MAX_BARS;
use vibeos::virtio::{
    self, COMMON_OFF_DF, COMMON_OFF_DFSEL, COMMON_OFF_DR, COMMON_OFF_DRSEL, COMMON_OFF_MSIX_CFG,
    COMMON_OFF_NUM_QUEUES, COMMON_OFF_QDESC, COMMON_OFF_QDEVICE, COMMON_OFF_QDRIVER,
    COMMON_OFF_QENABLE, COMMON_OFF_QMSIX, COMMON_OFF_QNOTIFY, COMMON_OFF_QSEL, COMMON_OFF_QSIZE,
    COMMON_OFF_STATUS, DESC_F_WRITE, DEV_BLK_LEGACY, DEV_BLK_MODERN, DescBuf, F_EVENT_IDX,
    MSI_NO_VECTOR, ModernCaps, PciCap, STATUS_ACKNOWLEDGE, STATUS_DRIVER, STATUS_DRIVER_OK,
    STATUS_FEATURES_OK, SplitLayout, VENDOR_ID, VirtioError, notify_addr,
};
use vibeos::virtio_blk::{
    CFG_BLK_SIZE, CFG_CAPACITY, CFG_MAX_DISCARD_SECTORS, CFG_NUM_QUEUES, CFG_TOPOLOGY, F_DISCARD,
    F_FLUSH, F_MQ, F_TOPOLOGY, MAX_DISKS, SECTOR, T_DISCARD, T_FLUSH, T_IN, T_OUT, disk_name,
    logical_capacity, map_status, nq_from_config, pack_discard, pack_header, pick_blk_size,
    pick_features, sector_for_lba,
};

use crate::arch::{self, current::Arch};
use crate::block_init::{self, IoWaiter};
use crate::dev_init;
use crate::dma_init;
use crate::irq_init;
use crate::pci_init;
use crate::per_cpu_init;
use crate::sync_init::SpinMutex;
use crate::thread_init;

mod irq;
mod issue;
mod vq;

use irq::{blk_top, blk_work};
#[cfg(feature = "kernel_tests")]
pub use issue::submit;
use issue::{Blk, N_SLOTS, SLOT_STRIDE};
use vq::{FREE, MAX_QSIZE, MAX_VQ, Vq, clamp_qsize};

/// One bound virtio-blk function: what the driver keeps for it. The PCI
/// registry slot of the device owns it as a `dev::Instance`; the block
/// registry's entry and each queue vector hold counted references to it
/// (DESIGN §12.1 rule 1). The queue state stays behind a pointer, so the
/// instance is small.
pub(crate) struct VirtioBlk {
    /// Keeps the device's entry alive while the instance is.
    #[cfg_attr(
        not(feature = "kernel_tests"),
        expect(
            dead_code,
            reason = "held for its count; only the in-guest tests read it"
        )
    )]
    dev: DevRef,
    name: [u8; 4],
    name_len: u8,
    st: SpinMutex<Option<TryBox<Blk>>>,
    isr: AtomicU64,
    live: AtomicBool,
    state: AtomicU8,
    features: AtomicU64,
    blk_size: AtomicU32,
    cap: AtomicU64,
    nq: AtomicU8,
    top_hits: AtomicU32,
    thread_hits: AtomicU32,
    completions: AtomicU32,
    phys_exp: AtomicU8,
    align_off: AtomicU8,
    min_io: AtomicU16,
    opt_io: AtomicU32,
    max_discard: AtomicU32,
    io_reqs: AtomicU64,
    flushes: AtomicU64,
    /// The vectors the probe allocated, one per queue (or one for all),
    /// each as `QUEUE_VEC_LIVE | cpu << 8 | vector`; 0 for none.
    queue_vecs: [AtomicU64; MAX_VQ],
    /// Completions whose device status [`harvest`](Self::harvest) replaces
    /// with `S_UNSUPP` (test-only, AGENTS.md rule 9).
    #[cfg(feature = "kernel_tests")]
    inject_unsupp: AtomicU32,
}

impl VirtioBlk {
    fn new(dev: DevRef, name: &str) -> Self {
        let mut n = [0u8; 4];
        let len = name.len().min(n.len());
        if let (Some(d), Some(s)) = (n.get_mut(..len), name.as_bytes().get(..len)) {
            d.copy_from_slice(s);
        }
        Self {
            dev,
            name: n,
            name_len: len as u8,
            st: SpinMutex::with_rank(None, RANK_DEVICE),
            isr: AtomicU64::new(0),
            live: AtomicBool::new(false),
            state: AtomicU8::new(0),
            features: AtomicU64::new(0),
            blk_size: AtomicU32::new(SECTOR),
            cap: AtomicU64::new(0),
            nq: AtomicU8::new(0),
            top_hits: AtomicU32::new(0),
            thread_hits: AtomicU32::new(0),
            completions: AtomicU32::new(0),
            phys_exp: AtomicU8::new(0),
            align_off: AtomicU8::new(0),
            min_io: AtomicU16::new(0),
            opt_io: AtomicU32::new(0),
            max_discard: AtomicU32::new(0),
            io_reqs: AtomicU64::new(0),
            flushes: AtomicU64::new(0),
            queue_vecs: [const { AtomicU64::new(0) }; MAX_VQ],
            #[cfg(feature = "kernel_tests")]
            inject_unsupp: AtomicU32::new(0),
        }
    }
}

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

fn r64(va: u64, off: u16) -> u64 {
    (r32(va, off) as u64) | ((r32(va, off + 4) as u64) << 32)
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

fn fail_armed(dev: &Device, common: u64, vecs: &[u8], nvec: usize) {
    irq_init::disable_msix(dev);
    let mut i = 0usize;
    while i < nvec {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "cleanup after an error the caller already returns: a vector that fails to free stays allocated, which nothing can act on (DESIGN §2.5)"
        )]
        let _ = irq_init::free_vector(vecs[i]);
        i += 1;
    }
    fail_status(common);
}

const QUEUE_VEC_LIVE: u64 = 1 << 63;

fn online_cpus() -> u16 {
    let n = per_cpu_init::online_mask().count_ones() as u16;
    n.max(1)
}

fn msix_table_size(dev: &Device) -> u16 {
    let Some(cap_off) = dev.caps.msix else {
        return 0;
    };
    let mut hw = pci_init::HwCfg;
    vibeos::pci::read_msix_cap(&mut hw, dev.addr, cap_off).table_size
}

/// Bring `dev` up as `blk`, the instance `inst` holds: fill it, give each
/// queue vector a reference to it, then register its disk.
fn setup(
    blk: &VirtioBlk,
    inst: &Instance,
    dev: &Device,
    caps: ModernCaps,
) -> Result<(), VirtioError> {
    let common_cap = caps.common.ok_or(VirtioError::NoCaps)?;
    let notify_cap = caps.notify.ok_or(VirtioError::NoCaps)?;
    let isr_cap = caps.isr.ok_or(VirtioError::NoCaps)?;
    let cfg_cap = caps.device.ok_or(VirtioError::NoCaps)?;
    let common = region(dev, common_cap).ok_or(VirtioError::NoCaps)?;
    let notify_base = region(dev, notify_cap).ok_or(VirtioError::NoCaps)?;
    let isr = region(dev, isr_cap).ok_or(VirtioError::NoCaps)?;
    let cfg = region(dev, cfg_cap).ok_or(VirtioError::NoCaps)?;
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
    let feat = match pick_features(device_feat) {
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

    let cap_512 = r64(cfg, CFG_CAPACITY);
    let cfg_bs = r32(cfg, CFG_BLK_SIZE);
    let blk_size = pick_blk_size(feat, cfg_bs);
    let Some(capacity) = logical_capacity(cap_512, blk_size) else {
        fail_status(common);
        return Err(VirtioError::Failed);
    };
    if capacity == 0 {
        fail_status(common);
        return Err(VirtioError::Failed);
    }
    if feat & F_TOPOLOGY != 0 {
        blk.phys_exp.store(r8(cfg, CFG_TOPOLOGY), Ordering::Release);
        blk.align_off
            .store(r8(cfg, CFG_TOPOLOGY + 1), Ordering::Release);
        blk.min_io
            .store(r16(cfg, CFG_TOPOLOGY + 2), Ordering::Release);
        blk.opt_io
            .store(r32(cfg, CFG_TOPOLOGY + 4), Ordering::Release);
    }
    if feat & F_DISCARD != 0 {
        blk.max_discard
            .store(r32(cfg, CFG_MAX_DISCARD_SECTORS), Ordering::Release);
    }
    let cfg_nq = r16(cfg, CFG_NUM_QUEUES);
    let common_nq = r16(common, COMMON_OFF_NUM_QUEUES);
    let offered = nq_from_config(feat, cfg_nq, common_nq);
    let nq = offered.min(online_cpus()).min(MAX_VQ as u16).max(1) as usize;

    let table_size = msix_table_size(dev);
    let per_q_msix = table_size as usize >= nq;
    let mut vecs = [0u8; MAX_VQ];
    // The CPU each of `vecs` was allocated on; a vector is valid only there.
    let mut vcpus = [0u32; MAX_VQ];
    let mut nvec = 0usize;

    w16(common, COMMON_OFF_MSIX_CFG, MSI_NO_VECTOR);

    if !per_q_msix {
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
        if irq_init::set_threaded(vec, Some(blk_top), blk_work, Some(inst.clone())).is_err() {
            #[expect(
                clippy::let_underscore_must_use,
                reason = "cleanup after an error the caller already returns: a vector that fails to free stays allocated, which nothing can act on (DESIGN §2.5)"
            )]
            let _ = irq_init::free_vector(vec);
            fail_status(common);
            return Err(VirtioError::Failed);
        }
        if let Err(e) = irq_init::enable_msix(dev, 0, vec, pc.apic_id.load(Ordering::Relaxed) as u8)
        {
            #[expect(
                clippy::let_underscore_must_use,
                reason = "cleanup after an error the caller already returns: a vector that fails to free stays allocated, which nothing can act on (DESIGN §2.5)"
            )]
            let _ = irq_init::free_vector(vec);
            fail_status(common);
            return match e {
                IrqError::NoRoute => Err(VirtioError::NoCaps),
                _ => Err(VirtioError::Failed),
            };
        }
        vecs[0] = vec;
        vcpus[0] = cpu;
        nvec = 1;
    }

    let Some(slots) = dma_init::alloc(DmaAlloc::dma32((N_SLOTS * SLOT_STRIDE) as u64)) else {
        fail_armed(dev, common, &vecs, nvec);
        return Err(VirtioError::Failed);
    };
    // SAFETY: `slots` was just allocated, `len()` bytes at `virt()`, and is
    // not yet shared with the device; established by `dma_init::alloc`.
    unsafe {
        core::ptr::write_bytes(slots.virt() as *mut u8, 0, slots.len() as usize);
    }

    let mut vqs: [Option<Vq>; MAX_VQ] = [None, None, None, None, None, None, None, None];
    let mut q0sz = 0u16;
    let mut qi = 0usize;
    while qi < nq {
        let cpu = if per_q_msix && per_cpu_init::is_online(qi as u32) {
            qi as u32
        } else if per_q_msix {
            0
        } else {
            irq_init::threaded_cpu()
        };
        if per_q_msix {
            let Some(pc) = per_cpu_init::cpu(cpu) else {
                dma_init::free(slots);
                let mut j = 0usize;
                while j < qi {
                    if let Some(v) = vqs[j].take() {
                        dma_init::free(v.qdma);
                    }
                    j += 1;
                }
                fail_armed(dev, common, &vecs, nvec);
                return Err(VirtioError::Failed);
            };
            let vec = match irq_init::allocate_vector(cpu) {
                Ok(v) => v,
                Err(_) => {
                    dma_init::free(slots);
                    let mut j = 0usize;
                    while j < qi {
                        if let Some(v) = vqs[j].take() {
                            dma_init::free(v.qdma);
                        }
                        j += 1;
                    }
                    fail_armed(dev, common, &vecs, nvec);
                    return Err(VirtioError::Failed);
                }
            };
            if irq_init::set_threaded(vec, Some(blk_top), blk_work, Some(inst.clone())).is_err() {
                #[expect(
                    clippy::let_underscore_must_use,
                    reason = "cleanup after an error the caller already returns: a vector that fails to free stays allocated, which nothing can act on (DESIGN §2.5)"
                )]
                let _ = irq_init::free_vector(vec);
                dma_init::free(slots);
                let mut j = 0usize;
                while j < qi {
                    if let Some(v) = vqs[j].take() {
                        dma_init::free(v.qdma);
                    }
                    j += 1;
                }
                fail_armed(dev, common, &vecs, nvec);
                return Err(VirtioError::Failed);
            }
            if irq_init::enable_msix(
                dev,
                qi as u16,
                vec,
                pc.apic_id.load(Ordering::Relaxed) as u8,
            )
            .is_err()
            {
                #[expect(
                    clippy::let_underscore_must_use,
                    reason = "cleanup after an error the caller already returns: a vector that fails to free stays allocated, which nothing can act on (DESIGN §2.5)"
                )]
                let _ = irq_init::free_vector(vec);
                dma_init::free(slots);
                let mut j = 0usize;
                while j < qi {
                    if let Some(v) = vqs[j].take() {
                        dma_init::free(v.qdma);
                    }
                    j += 1;
                }
                fail_armed(dev, common, &vecs, nvec);
                return Err(VirtioError::Failed);
            }
            vecs[nvec] = vec;
            vcpus[nvec] = cpu;
            nvec += 1;
        }

        w16(common, COMMON_OFF_QSEL, qi as u16);
        let hw_qs = r16(common, COMMON_OFF_QSIZE);
        let qsz = clamp_qsize(hw_qs);
        if qsz == 0 {
            dma_init::free(slots);
            let mut j = 0usize;
            while j < qi {
                if let Some(v) = vqs[j].take() {
                    dma_init::free(v.qdma);
                }
                j += 1;
            }
            fail_armed(dev, common, &vecs, nvec);
            return Err(VirtioError::BadQueue);
        }
        w16(common, COMMON_OFF_QSIZE, qsz);
        if qi == 0 {
            q0sz = qsz;
        }
        let Some(layout) = SplitLayout::new(qsz) else {
            dma_init::free(slots);
            let mut j = 0usize;
            while j < qi {
                if let Some(v) = vqs[j].take() {
                    dma_init::free(v.qdma);
                }
                j += 1;
            }
            fail_armed(dev, common, &vecs, nvec);
            return Err(VirtioError::BadQueue);
        };
        let Some(qdma) = dma_init::alloc(DmaAlloc::dma32(layout.total as u64)) else {
            dma_init::free(slots);
            let mut j = 0usize;
            while j < qi {
                if let Some(v) = vqs[j].take() {
                    dma_init::free(v.qdma);
                }
                j += 1;
            }
            fail_armed(dev, common, &vecs, nvec);
            return Err(VirtioError::Failed);
        };
        // SAFETY: as for `slots` above; established by `dma_init::alloc`.
        unsafe {
            core::ptr::write_bytes(qdma.virt() as *mut u8, 0, qdma.len() as usize);
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
        w16(
            common,
            COMMON_OFF_QMSIX,
            if per_q_msix { qi as u16 } else { 0 },
        );
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
            dma_init::free(slots);
            let mut j = 0usize;
            while j < qi {
                if let Some(v) = vqs[j].take() {
                    dma_init::free(v.qdma);
                }
                j += 1;
            }
            fail_armed(dev, common, &vecs, nvec);
            return Err(VirtioError::Notify);
        };
        vqs[qi] = Some(Vq {
            vq,
            qdma,
            doorbell,
            inflight: [FREE; MAX_QSIZE],
        });
        qi += 1;
    }

    // Allocate the driver state before the device goes live: a failure here
    // unwinds like any other setup error, and no published state names a
    // device without its `Blk` (DESIGN §4.4).
    let Ok(uninit) = TryBox::<Blk>::try_new_uninit() else {
        dma_init::free(slots);
        let mut j = 0usize;
        while j < nq {
            if let Some(v) = vqs[j].take() {
                dma_init::free(v.qdma);
            }
            j += 1;
        }
        fail_armed(dev, common, &vecs, nvec);
        return Err(VirtioError::NoMemory);
    };

    let st = r8(common, COMMON_OFF_STATUS);
    w8(common, COMMON_OFF_STATUS, st | STATUS_DRIVER_OK);

    blk.isr.store(isr, Ordering::Release);
    blk.features.store(feat, Ordering::Release);
    blk.blk_size.store(blk_size, Ordering::Release);
    blk.cap.store(capacity, Ordering::Release);
    blk.nq.store(nq as u8, Ordering::Release);
    blk.state
        .store(DeviceState::Ready.as_u8(), Ordering::Release);

    let boxed = uninit.write(Blk {
        q: Queue::new(),
        vqs,
        nq: nq as u8,
        slots,
        slot_used: [false; N_SLOTS],
        slot_req: [None; N_SLOTS],
        features: feat,
        blk_size,
        running: false,
    });
    *blk.st.lock() = Some(boxed);
    let mut i = 0usize;
    while i < MAX_VQ {
        let v = if i < nvec {
            QUEUE_VEC_LIVE | (u64::from(vcpus[i]) << 8) | u64::from(vecs[i])
        } else {
            0
        };
        blk.queue_vecs[i].store(v, Ordering::Release);
        i += 1;
    }
    blk.live.store(true, Ordering::Release);

    // The registration prints the `block: <name>` marker. A failure leaves
    // the instance live but its disk unregistered, so nothing mounts it.
    if let Err(e) = register(blk, inst) {
        crate::klog!(
            vibeos::log::Level::Error,
            "vibeOS: blk: {} not registered: {}",
            blk.name(),
            e.as_str()
        );
    }
    let mq = if feat & F_MQ != 0 { "mq" } else { "sq" };
    crate::marker!(
        "vibeOS: virtio: blk {} {} qsz={q0sz} nq={nq} {mq} feat={:#x} bs={blk_size} topo={}/{} discard={}",
        blk.name(),
        dev.addr,
        feat,
        blk.phys_exp.load(Ordering::Acquire),
        blk.opt_io.load(Ordering::Acquire),
        feat & F_DISCARD != 0
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
            Ok(()) | Err(ClaimError::Already) => {}
            Err(ClaimError::Overlap) => return Err(ProbeError::Busy),
            Err(ClaimError::Empty | ClaimError::BadIndex) => {
                return Err(ProbeError::NoResource);
            }
        }
    }
    Ok(())
}

struct BlkDriver;

static BLK_IDS: &[IdMatch] = &[
    IdMatch::vid_did(VENDOR_ID, DEV_BLK_MODERN),
    IdMatch::vid_did(VENDOR_ID, DEV_BLK_LEGACY),
];

static BLK_DRV: BlkDriver = BlkDriver;

impl Driver for BlkDriver {
    fn name(&self) -> &'static str {
        "virtio-blk"
    }
    fn ids(&self) -> &'static [IdMatch] {
        BLK_IDS
    }
    fn order(&self) -> u8 {
        41
    }
    /// Each function is its own instance, named `vda`, `vdb`, … in bind
    /// order. The instance exists before the device is touched, so each
    /// queue vector gets a reference to it; on a failure it holds no queue
    /// state and every DMA buffer was freed by `setup`.
    fn probe(&self, dev: &DevRef) -> Result<Option<Instance>, ProbeError> {
        let caps = virtio::read_modern_caps(&mut pci_init::HwCfg, dev.addr);
        if !caps.is_complete() || caps.device.is_none() {
            crate::marker!("vibeOS: virtio: blk missing modern caps");
            return Err(ProbeError::NoResource);
        }
        let mut name = [0u8; 4];
        let Some(name) = free_name(&mut name) else {
            crate::klog!(
                vibeos::log::Level::Warn,
                "vibeOS: virtio: blk {}: no free disk name",
                dev.addr
            );
            return Err(ProbeError::Busy);
        };
        claim_bars(dev, &caps)?;
        let inst = vibeos::dev::instance(VirtioBlk::new(dev.clone(), name))?;
        let blk = inst.downcast_ref::<VirtioBlk>().ok_or(ProbeError::Failed)?;
        match setup(blk, &inst, dev, caps) {
            Ok(()) => Ok(Some(inst)),
            Err(VirtioError::NoVersion1) => {
                crate::marker!("vibeOS: virtio: blk no VERSION_1");
                Err(ProbeError::Failed)
            }
            Err(VirtioError::NoMemory) => Err(ProbeError::NoMemory),
            Err(e) => {
                crate::marker!("vibeOS: virtio: blk probe {}", e.as_str());
                Err(ProbeError::Failed)
            }
        }
    }
    fn remove(&self, _dev: &DevRef) {}
}

pub fn init() {
    if !dev_init::register_driver(&BLK_DRV) {
        crate::klog!(
            vibeos::log::Level::Warn,
            "vibeOS: virtio: blk driver not registered: registry full"
        );
    }
}

impl VirtioBlk {
    /// The disk's name, `vda`, `vdb`, … in bind order.
    pub fn name(&self) -> &str {
        let n = self.name.get(..self.name_len as usize).unwrap_or(&[]);
        core::str::from_utf8(n).unwrap_or("vd?")
    }

    pub fn live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }

    pub fn state(&self) -> DeviceState {
        DeviceState::from_u8(self.state.load(Ordering::Acquire))
    }

    pub fn logical_block_size(&self) -> u32 {
        self.blk_size.load(Ordering::Acquire)
    }

    pub fn capacity_sectors(&self) -> u64 {
        self.cap.load(Ordering::Acquire)
    }

    pub fn num_queues(&self) -> u8 {
        self.nq.load(Ordering::Acquire)
    }

    pub fn io_reqs(&self) -> u64 {
        self.io_reqs.load(Ordering::Relaxed)
    }

    /// Replace the device status of the next `n` completions with
    /// `S_UNSUPP` (test-only).
    #[cfg(feature = "kernel_tests")]
    #[expect(
        dead_code,
        reason = "block_two_disk_instances, the proof test, injects a failure"
    )]
    pub fn inject_unsupp(&self, n: u32) {
        self.inject_unsupp.store(n, Ordering::Release);
    }

    /// Check a request and tie it to `w`. Hard IRQ must not call this.
    fn build(
        &self,
        op: Op,
        lba: u64,
        nsect: u32,
        ptr: usize,
        len: usize,
        w: &IoWaiter,
    ) -> Result<Request, BlockError> {
        if irq_init::in_hard_irq() {
            return Err(BlockError::Failed);
        }
        if !self.live() {
            return Err(BlockError::Failed);
        }
        if self.state() == DeviceState::Failed {
            return Err(BlockError::Failed);
        }
        let bs = self.logical_block_size() as usize;
        let cap = self.capacity_sectors();
        let mut req = Request::new(op, lba, nsect).with_waiter(w as *const IoWaiter as usize);
        match op {
            Op::Read | Op::Write => {
                if nsect == 0 || (nsect as usize).checked_mul(bs) != Some(len) {
                    return Err(BlockError::Inval);
                }
                if lba
                    .checked_add(nsect as u64)
                    .map(|e| e > cap)
                    .unwrap_or(true)
                {
                    return Err(BlockError::Inval);
                }
                req = req.with_seg(ptr, len);
            }
            Op::Flush => {
                if nsect != 0 || len != 0 {
                    return Err(BlockError::Inval);
                }
            }
            Op::Discard => {
                if len != 0 || nsect == 0 {
                    return Err(BlockError::Inval);
                }
                if lba
                    .checked_add(nsect as u64)
                    .map(|e| e > cap)
                    .unwrap_or(true)
                {
                    return Err(BlockError::Inval);
                }
            }
        }
        Ok(req)
    }

    fn start(&self, req: Request) -> Result<(), BlockError> {
        if self.submit_req(req)? {
            self.pump();
        }
        Ok(())
    }

    fn blocking(
        &self,
        op: Op,
        lba: u64,
        nsect: u32,
        ptr: usize,
        len: usize,
    ) -> Result<(), BlockError> {
        self.blocking_req(op, lba, nsect, ptr, len, false)
    }

    /// Submit and wait, yielding while the queue is full. `fua` marks a write.
    fn blocking_req(
        &self,
        op: Op,
        lba: u64,
        nsect: u32,
        ptr: usize,
        len: usize,
        fua: bool,
    ) -> Result<(), BlockError> {
        let mut spins = 0u32;
        loop {
            let w = IoWaiter::new();
            let mut req = self.build(op, lba, nsect, ptr, len, &w)?;
            if fua {
                req = req.with_fua();
            }
            match self.start(req) {
                Ok(()) => return w.wait(),
                Err(BlockError::QueueFull) => {
                    spins = spins.saturating_add(1);
                    if spins > 1_000_000 {
                        return Err(BlockError::QueueFull);
                    }
                    thread_init::yield_now();
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let bs = self.logical_block_size() as usize;
        if bs == 0 || !buf.len().is_multiple_of(bs) {
            return Err(BlockError::Inval);
        }
        let nsect = (buf.len() / bs) as u32;
        self.blocking(Op::Read, lba, nsect, buf.as_mut_ptr() as usize, buf.len())
    }

    pub fn write(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
        let bs = self.logical_block_size() as usize;
        if bs == 0 || !buf.len().is_multiple_of(bs) {
            return Err(BlockError::Inval);
        }
        let nsect = (buf.len() / bs) as u32;
        self.blocking(Op::Write, lba, nsect, buf.as_ptr() as usize, buf.len())
    }

    pub fn flush(&self) -> Result<(), BlockError> {
        self.blocking(Op::Flush, 0, 0, 0, 0)
    }

    /// Write `buf` at `lba` with `Fua`: durable when this returns `Ok`.
    /// virtio-blk has no FUA (DESIGN §10.4), so the queue sends a `Flush`.
    #[cfg(feature = "kernel_tests")]
    pub fn write_fua(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
        let bs = self.logical_block_size() as usize;
        if bs == 0 || !buf.len().is_multiple_of(bs) {
            return Err(BlockError::Inval);
        }
        let nsect = (buf.len() / bs) as u32;
        self.blocking_req(
            Op::Write,
            lba,
            nsect,
            buf.as_ptr() as usize,
            buf.len(),
            true,
        )
    }

    pub fn discard(&self, lba: u64, nsectors: u64) -> Result<(), BlockError> {
        if nsectors == 0 || nsectors > u32::MAX as u64 {
            return Err(BlockError::Inval);
        }
        self.blocking(Op::Discard, lba, nsectors as u32, 0, 0)
    }
}

/// Observers the in-guest tests read.
#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(dead_code, reason = "observers only the in-guest tests read")
)]
impl VirtioBlk {
    /// The PCI function this instance drives.
    pub fn dev(&self) -> &DevRef {
        &self.dev
    }

    pub fn features(&self) -> u64 {
        self.features.load(Ordering::Acquire)
    }

    pub fn has_mq(&self) -> bool {
        self.features() & F_MQ != 0 && self.num_queues() > 1
    }

    #[cfg_attr(
        feature = "kernel_tests",
        expect(
            dead_code,
            reason = "block_two_disk_instances, the proof test, reads it"
        )
    )]
    pub fn has_flush(&self) -> bool {
        self.features() & F_FLUSH != 0
    }

    pub fn has_discard(&self) -> bool {
        self.features() & F_DISCARD != 0
    }

    /// Runs of this disk's top half.
    pub fn top_hits(&self) -> u32 {
        self.top_hits.load(Ordering::Acquire)
    }

    /// Runs of this disk's bottom half.
    pub fn thread_hits(&self) -> u32 {
        self.thread_hits.load(Ordering::Acquire)
    }

    /// Requests this disk's device completed.
    pub fn completions(&self) -> u32 {
        self.completions.load(Ordering::Acquire)
    }

    /// `Flush` requests dispatched, emulated-`Fua` ones and those finished
    /// locally without `F_FLUSH` included.
    pub fn flushes(&self) -> u64 {
        self.flushes.load(Ordering::Relaxed)
    }

    /// A test LBA inside the Linux GPT partition (which starts at 512), not
    /// the GPT backup.
    pub fn persist_lba(&self) -> u64 {
        const LBA: u64 = 2048;
        let cap = self.capacity_sectors();
        if cap > LBA + 1 {
            LBA
        } else {
            cap.saturating_sub(1)
        }
    }
}

/// A disk's driver operations as the block registry's entry holds them: a
/// counted reference to the instance, which holds no `BlockRef`, so no
/// reference cycle forms.
struct VblkDev {
    inst: Instance,
}

impl VblkDev {
    fn blk(&self) -> Result<&VirtioBlk, BlockError> {
        self.inst
            .downcast_ref::<VirtioBlk>()
            .ok_or(BlockError::Failed)
    }
}

impl BlockDevice for VblkDev {
    fn logical_block_size(&self) -> u32 {
        self.blk().map_or(SECTOR, |b| b.logical_block_size())
    }
    fn capacity_sectors(&self) -> u64 {
        self.blk().map_or(0, |b| b.capacity_sectors())
    }
    fn state(&self) -> DeviceState {
        self.blk().map_or(DeviceState::Failed, |b| b.state())
    }
    fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        self.blk()?.read(lba, buf)
    }
    fn write(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
        self.blk()?.write(lba, buf)
    }
    fn flush(&self) -> Result<(), BlockError> {
        self.blk()?.flush()
    }
    fn discard(&self, lba: u64, nsectors: u64) -> Result<(), BlockError> {
        self.blk()?.discard(lba, nsectors)
    }
}

/// Register `blk`'s disk under its name in the block registry, through the
/// page cache.
fn register(blk: &VirtioBlk, inst: &Instance) -> Result<(), BlockError> {
    let ops = TryBox::<dyn BlockDevice>::try_new_unsize(VblkDev { inst: inst.clone() }, |b| b)
        .map_err(|_| BlockError::NoMem)?;
    crate::block::blockdev_init::register(
        blk.name().as_bytes(),
        Backing::Disk {
            ops,
            cache: Some(&crate::cache_init::PAGE_CACHE),
        },
    )
    .map(|_| ())
}

/// The first `vd<x>` name the block registry does not hold. Binding is
/// serial, so it stays free until the probe registers it.
fn free_name(out: &mut [u8; 4]) -> Option<&str> {
    let mut i = 0u8;
    while i < MAX_DISKS {
        let mut n = [0u8; 4];
        let taken = crate::block::blockdev_init::lookup(disk_name(i, &mut n)?.as_bytes()).is_some();
        if !taken {
            return disk_name(i, out);
        }
        i += 1;
    }
    None
}

/// Run `f` on each bound virtio-blk instance, in registry order, with the
/// registry unlocked; stop at the first `Some`. The driver keeps no list:
/// the device registry owns the instances.
fn find_disk<R>(mut f: impl FnMut(&VirtioBlk) -> Option<R>) -> Option<R> {
    let mut i = 0usize;
    while let Some(d) = dev_init::get(i) {
        i += 1;
        if dev_init::bound(&d) != Some(BLK_DRV.name()) {
            continue;
        }
        let Some(inst) = dev_init::instance(&d) else {
            continue;
        };
        if let Some(b) = inst.downcast_ref::<VirtioBlk>()
            && let Some(r) = f(b)
        {
            return Some(r);
        }
    }
    None
}

/// Run `f` on the disk named `name`; `None` when no bound instance has
/// that name.
#[cfg_attr(
    not(feature = "kernel_tests"),
    expect(
        dead_code,
        reason = "only the in-guest tests look a disk up by name yet"
    )
)]
pub(crate) fn with_disk<R>(name: &[u8], f: impl FnOnce(&VirtioBlk) -> R) -> Option<R> {
    let mut f = Some(f);
    find_disk(|b| {
        if b.name().as_bytes() != name {
            return None;
        }
        f.take().map(|f| f(b))
    })
}

/// Print one `vibeOS: blk: <name> …` line per bound instance.
pub(crate) fn shell_lines(f: &mut impl core::fmt::Write) -> core::fmt::Result {
    let mut res = Ok(());
    find_disk::<()>(|b| {
        if b.live() {
            res = writeln!(
                f,
                "vibeOS: blk: {} {} {} sectors {} nq={} io={}",
                b.name(),
                b.logical_block_size(),
                b.capacity_sectors(),
                b.state().as_str(),
                b.num_queues(),
                b.io_reqs()
            );
        }
        res.err().map(|_| ())
    });
    res
}
