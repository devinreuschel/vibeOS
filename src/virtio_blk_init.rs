//! virtio-blk driver. ROADMAP §7.2.
//!
//! Modern transport from Phase 6. One VQ per CPU when `F_MQ` is offered;
//! otherwise a single request queue. Completions run on the threaded IRQ
//! (DESIGN §2.2 / §5.4). Kick barriers are the Phase 6 `dma_wmb` story.
//! Status bytes live in DMA, not on the submitter stack.

#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]

use alloc::boxed::Box;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};

use vibeos::block::{
    self, BlockError, Completion, DeviceState, MAX_QUEUE, Op, Queue, Request, write_marker,
};
use vibeos::dev::{Device, Driver, IdMatch, ProbeError};
use vibeos::dma::{self, DmaAlloc, DmaBuffer};
use vibeos::irq::IrqError;
use vibeos::lock::RANK_DEVICE;
use vibeos::pci::MAX_BARS;
use vibeos::virtio::{
    self, COMMON_OFF_DF, COMMON_OFF_DFSEL, COMMON_OFF_DR, COMMON_OFF_DRSEL, COMMON_OFF_MSIX_CFG,
    COMMON_OFF_NUM_QUEUES, COMMON_OFF_QDESC, COMMON_OFF_QDEVICE, COMMON_OFF_QDRIVER,
    COMMON_OFF_QENABLE, COMMON_OFF_QMSIX, COMMON_OFF_QNOTIFY, COMMON_OFF_QSEL, COMMON_OFF_QSIZE,
    COMMON_OFF_STATUS, DESC_F_WRITE, DEV_BLK_LEGACY, DEV_BLK_MODERN, DescBuf, F_EVENT_IDX,
    MSI_NO_VECTOR, ModernCaps, PciCap, STATUS_ACKNOWLEDGE, STATUS_DRIVER, STATUS_DRIVER_OK,
    STATUS_FEATURES_OK, SplitLayout, SplitQueue, VENDOR_ID, VirtioError, notify_addr,
};
use vibeos::virtio_blk::{
    CFG_BLK_SIZE, CFG_CAPACITY, CFG_DISCARD_ALIGN, CFG_MAX_DISCARD_SECTORS, CFG_MAX_DISCARD_SEG,
    CFG_NUM_QUEUES, CFG_TOPOLOGY, F_DISCARD, F_FLUSH, F_MQ, F_TOPOLOGY, NAME, SECTOR, T_DISCARD,
    T_FLUSH, T_IN, T_OUT, logical_capacity, map_status, nq_from_config, pack_discard, pack_header,
    pick_blk_size, pick_features, sector_for_lba,
};

use crate::block_init::{self, IoWaiter};
use crate::dev_init;
use crate::dma_init;
use crate::irq_init;
use crate::pci_init;
use crate::per_cpu_init;
use crate::serial::Serial;
use crate::sync_init::SpinMutex;
use crate::thread_init;

const MAX_VQ: usize = 8;
const MAX_QSIZE: usize = 64;
const N_SLOTS: usize = 16;
const BOUNCE: usize = 8192;
const SLOT_META: usize = 64;
const SLOT_STRIDE: usize = SLOT_META + BOUNCE;
const FREE: u8 = 0xFF;

struct Vq {
    vq: SplitQueue,
    qdma: DmaBuffer,
    doorbell: u64,
    #[allow(dead_code)]
    vec: u8,
    inflight: [u8; MAX_QSIZE],
}

struct Blk {
    q: Queue,
    vqs: [Option<Vq>; MAX_VQ],
    nq: u8,
    slots: DmaBuffer,
    slot_used: [bool; N_SLOTS],
    slot_req: [Option<Request>; N_SLOTS],
    features: u64,
    blk_size: u32,
    running: bool,
}

static BLK: SpinMutex<Option<Box<Blk>>> = SpinMutex::with_rank(None, RANK_DEVICE);
static ISR_VA: AtomicU64 = AtomicU64::new(0);
static LIVE: AtomicBool = AtomicBool::new(false);
static STATE: AtomicU8 = AtomicU8::new(0);
static FEATURES: AtomicU64 = AtomicU64::new(0);
static BLK_SIZE: AtomicU32 = AtomicU32::new(SECTOR);
static CAP: AtomicU64 = AtomicU64::new(0);
static NQ: AtomicU8 = AtomicU8::new(0);
static TOP_HITS: AtomicU32 = AtomicU32::new(0);
static THREAD_HITS: AtomicU32 = AtomicU32::new(0);
static COMPLETIONS: AtomicU32 = AtomicU32::new(0);
static PHYS_EXP: AtomicU8 = AtomicU8::new(0);
static ALIGN_OFF: AtomicU8 = AtomicU8::new(0);
static MIN_IO: AtomicU16 = AtomicU16::new(0);
static OPT_IO: AtomicU32 = AtomicU32::new(0);
static MAX_DISCARD: AtomicU32 = AtomicU32::new(0);
static IO_REQS: AtomicU64 = AtomicU64::new(0);
static FLUSHES: AtomicU64 = AtomicU64::new(0);

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

fn clamp_qsize(hw: u16) -> u16 {
    let n = hw.min(MAX_QSIZE as u16);
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

fn fail_armed(dev: &Device, common: u64, vecs: &[u8], nvec: usize) {
    irq_init::disable_msix(dev);
    let mut i = 0usize;
    while i < nvec {
        let _ = irq_init::free_vector(vecs[i]);
        i += 1;
    }
    fail_status(common);
}

fn slot_base(slots: &DmaBuffer, si: usize) -> *mut u8 {
    (slots.virt() as usize + si * SLOT_STRIDE) as *mut u8
}

fn slot_dev(slots: &DmaBuffer, si: usize, off: usize) -> u64 {
    slots.device().as_u64() + (si * SLOT_STRIDE + off) as u64
}

fn copy_to_bounce(slots: &DmaBuffer, si: usize, req: &Request) {
    let mut dst = unsafe { slot_base(slots, si).add(SLOT_META) };
    let mut i = 0u8;
    while i < req.nseg {
        let s = req.segs[i as usize];
        unsafe {
            core::ptr::copy_nonoverlapping(s.ptr as *const u8, dst, s.len);
            dst = dst.add(s.len);
        }
        i += 1;
    }
}

fn copy_from_bounce(slots: &DmaBuffer, si: usize, req: &Request) {
    let mut src = unsafe { slot_base(slots, si).add(SLOT_META) };
    let mut i = 0u8;
    while i < req.nseg {
        let s = req.segs[i as usize];
        unsafe {
            core::ptr::copy_nonoverlapping(src, s.ptr as *mut u8, s.len);
            src = src.add(s.len);
        }
        i += 1;
    }
}

fn alloc_slot(blk: &mut Blk) -> Option<usize> {
    let mut i = 0usize;
    while i < N_SLOTS {
        if !blk.slot_used[i] {
            blk.slot_used[i] = true;
            return Some(i);
        }
        i += 1;
    }
    None
}

fn prefer_vq() -> usize {
    let cpu = per_cpu_init::try_current().map(|c| c.cpu_id).unwrap_or(0);
    cpu as usize
}

fn vq_has_room(v: &Vq, need: u16) -> bool {
    v.vq.num_free >= need
}

fn pick_vq(blk: &Blk, need: u16) -> Option<usize> {
    let nq = blk.nq as usize;
    if nq == 0 {
        return None;
    }
    let pref = prefer_vq() % nq;
    if let Some(v) = blk.vqs[pref].as_ref()
        && vq_has_room(v, need)
    {
        return Some(pref);
    }
    let mut i = 0usize;
    while i < nq {
        if let Some(v) = blk.vqs[i].as_ref()
            && vq_has_room(v, need)
        {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn descs_for(op: Op) -> u16 {
    match op {
        Op::Read | Op::Write | Op::Discard => 3,
        Op::Flush => 2,
    }
}

fn kick(doorbell: u64) {
    dma::dma_wmb();
    unsafe {
        core::ptr::write_volatile(doorbell as *mut u16, 0u16);
    }
}

enum Issued {
    Device { qi: usize, kick: bool },
    Local(Request, Result<(), BlockError>),
    Full(Request),
}

fn issue(blk: &mut Blk, req: Request) -> Issued {
    match req.bio.op {
        Op::Flush if blk.features & F_FLUSH == 0 => return Issued::Local(req, Ok(())),
        Op::Discard if blk.features & F_DISCARD == 0 => {
            return Issued::Local(req, Err(BlockError::Inval));
        }
        Op::Read | Op::Write | Op::Flush | Op::Discard => {}
    }
    let need = descs_for(req.bio.op);
    let Some(qi) = pick_vq(blk, need) else {
        return Issued::Full(req);
    };
    let Some(si) = alloc_slot(blk) else {
        return Issued::Full(req);
    };
    let Some(sector) = sector_for_lba(req.bio.lba, blk.blk_size) else {
        blk.slot_used[si] = false;
        return Issued::Local(req, Err(BlockError::Inval));
    };
    if req.bio.op.needs_buf() {
        let want = (req.bio.nsect as usize).saturating_mul(blk.blk_size as usize);
        if req.bytes() != want || want == 0 || want > BOUNCE {
            blk.slot_used[si] = false;
            return Issued::Local(req, Err(BlockError::Inval));
        }
    }

    let typ = match req.bio.op {
        Op::Read => T_IN,
        Op::Write => T_OUT,
        Op::Flush => T_FLUSH,
        Op::Discard => T_DISCARD,
    };

    let base = slot_base(&blk.slots, si);
    unsafe {
        core::ptr::write_bytes(base, 0, SLOT_STRIDE);
        let mut hdr = [0u8; 16];
        pack_header(&mut hdr, typ, sector);
        core::ptr::copy_nonoverlapping(hdr.as_ptr(), base, 16);
        *base.add(16) = 0xFF;
    }
    if req.bio.op == Op::Write {
        copy_to_bounce(&blk.slots, si, &req);
    }
    if req.bio.op == Op::Discard {
        let n512 = match sector_for_lba(req.bio.nsect as u64, blk.blk_size) {
            Some(n) if n <= u32::MAX as u64 => n as u32,
            _ => {
                blk.slot_used[si] = false;
                return Issued::Local(req, Err(BlockError::Inval));
            }
        };
        let maxd = MAX_DISCARD.load(Ordering::Acquire);
        if maxd != 0 && n512 > maxd {
            blk.slot_used[si] = false;
            return Issued::Local(req, Err(BlockError::Inval));
        }
        let mut disc = [0u8; 16];
        pack_discard(&mut disc, sector, n512, 0);
        unsafe {
            core::ptr::copy_nonoverlapping(disc.as_ptr(), base.add(32), 16);
        }
    }

    let hdr_d = slot_dev(&blk.slots, si, 0);
    let st_d = slot_dev(&blk.slots, si, 16);
    let data_d = slot_dev(&blk.slots, si, SLOT_META);
    let disc_d = slot_dev(&blk.slots, si, 32);
    blk.slots.sync_for_device();

    let chain: [DescBuf; 3];
    let nchain: usize;
    match req.bio.op {
        Op::Read => {
            chain = [
                DescBuf {
                    addr: hdr_d,
                    len: 16,
                    flags: 0,
                },
                DescBuf {
                    addr: data_d,
                    len: req.bytes() as u32,
                    flags: DESC_F_WRITE,
                },
                DescBuf {
                    addr: st_d,
                    len: 1,
                    flags: DESC_F_WRITE,
                },
            ];
            nchain = 3;
        }
        Op::Write => {
            chain = [
                DescBuf {
                    addr: hdr_d,
                    len: 16,
                    flags: 0,
                },
                DescBuf {
                    addr: data_d,
                    len: req.bytes() as u32,
                    flags: 0,
                },
                DescBuf {
                    addr: st_d,
                    len: 1,
                    flags: DESC_F_WRITE,
                },
            ];
            nchain = 3;
        }
        Op::Flush => {
            chain = [
                DescBuf {
                    addr: hdr_d,
                    len: 16,
                    flags: 0,
                },
                DescBuf {
                    addr: st_d,
                    len: 1,
                    flags: DESC_F_WRITE,
                },
                DescBuf {
                    addr: 0,
                    len: 0,
                    flags: 0,
                },
            ];
            nchain = 2;
        }
        Op::Discard => {
            chain = [
                DescBuf {
                    addr: hdr_d,
                    len: 16,
                    flags: 0,
                },
                DescBuf {
                    addr: disc_d,
                    len: 16,
                    flags: 0,
                },
                DescBuf {
                    addr: st_d,
                    len: 1,
                    flags: DESC_F_WRITE,
                },
            ];
            nchain = 3;
        }
    }

    let v = match blk.vqs[qi].as_mut() {
        Some(v) => v,
        None => {
            blk.slot_used[si] = false;
            return Issued::Full(req);
        }
    };
    let old = v.vq.last_avail;
    let head = match v.vq.add_chain(&chain[..nchain]) {
        Ok(h) => h,
        Err(_) => {
            blk.slot_used[si] = false;
            return Issued::Full(req);
        }
    };
    if (head as usize) < MAX_QSIZE {
        v.inflight[head as usize] = si as u8;
    }
    blk.slot_req[si] = Some(req);
    v.vq.publish();
    v.qdma.sync_for_device();
    let kick = v.vq.should_kick(old);
    Issued::Device { qi, kick }
}

/// Dispatch what `blk.q` picks until it is idle or a virtqueue is full.
/// A request finished here (`Local`) is completed or aborted under `BLK`,
/// and its waiters wake after the lock drops.
fn pump() {
    loop {
        let mut kicks = [0u64; MAX_VQ];
        let mut want = [false; MAX_VQ];
        let mut local: [Option<(Request, Result<(), BlockError>)>; MAX_QUEUE] = [None; MAX_QUEUE];
        let mut nlocal = 0usize;
        let mut again = false;
        {
            let mut g = BLK.lock();
            let Some(blk) = g.as_mut() else {
                return;
            };
            loop {
                if nlocal == MAX_QUEUE {
                    again = true;
                    break;
                }
                let req = match blk.q.pick() {
                    Some(r) => r,
                    None => {
                        blk.running = false;
                        break;
                    }
                };
                let seq = u64::from(req.seq);
                match issue(blk, req) {
                    Issued::Device { qi, kick } => {
                        IO_REQS.fetch_add(1, Ordering::Relaxed);
                        if req.bio.op == Op::Flush {
                            FLUSHES.fetch_add(1, Ordering::Relaxed);
                        }
                        if kick && let Some(v) = blk.vqs[qi].as_ref() {
                            kicks[qi] = v.doorbell;
                            want[qi] = true;
                        }
                    }
                    Issued::Local(req, res) => {
                        if req.bio.op == Op::Flush && res.is_ok() {
                            FLUSHES.fetch_add(1, Ordering::Relaxed);
                        }
                        let report = match res {
                            Ok(()) => blk.q.complete(seq) == Completion::Report,
                            Err(_) => {
                                blk.q.abort(seq);
                                true
                            }
                        };
                        if report {
                            local[nlocal] = Some((req, res));
                            nlocal += 1;
                        }
                    }
                    Issued::Full(req) => {
                        if blk.q.requeue(req).is_err() {
                            local[nlocal] = Some((req, Err(BlockError::Failed)));
                            nlocal += 1;
                        }
                        break;
                    }
                }
            }
        }
        let mut i = 0usize;
        while i < nlocal {
            if let Some((req, res)) = local[i].take() {
                block_init::complete_waiters(&req, res);
            }
            i += 1;
        }
        i = 0;
        while i < MAX_VQ {
            if want[i] {
                kick(kicks[i]);
            }
            i += 1;
        }
        if !again {
            return;
        }
    }
}

fn fail_rest() {
    STATE.store(DeviceState::Failed.as_u8(), Ordering::Release);
    loop {
        let mut dump = [None; MAX_QUEUE];
        let n = {
            let mut g = BLK.lock();
            let Some(blk) = g.as_mut() else {
                return;
            };
            blk.q.fail();
            blk.q.drain(&mut dump)
        };
        if n == 0 {
            return;
        }
        let mut i = 0usize;
        while i < n {
            if let Some(r) = dump[i] {
                block_init::complete_waiters(&r, Err(BlockError::Failed));
            }
            i += 1;
        }
    }
}

/// Retire `req`'s dispatch under `BLK`, drop it, then wake. A retry goes
/// back on the queue for [`harvest`]'s closing [`pump`].
fn finish(mut req: Request, res: Result<(), BlockError>) {
    let seq = u64::from(req.seq);
    let mut g = BLK.lock();
    let Some(blk) = g.as_mut() else {
        drop(g);
        block_init::complete_waiters(&req, Err(BlockError::Failed));
        return;
    };
    match res {
        Ok(()) => {
            let c = blk.q.complete(seq);
            drop(g);
            if c == Completion::Report {
                block_init::complete_waiters(&req, Ok(()));
            }
        }
        Err(e) if e.retryable() && req.retries_left > 0 => {
            req.retries_left -= 1;
            let requeued = blk.q.requeue(req);
            drop(g);
            if requeued.is_err() {
                block_init::complete_waiters(&req, Err(BlockError::Failed));
                fail_rest();
            }
        }
        Err(e) if e.retryable() => {
            blk.q.abort(seq);
            drop(g);
            block_init::complete_waiters(&req, Err(BlockError::Failed));
            fail_rest();
        }
        Err(e) => {
            blk.q.abort(seq);
            drop(g);
            block_init::complete_waiters(&req, Err(e));
        }
    }
}

fn harvest() {
    let mut done: [Option<(Request, Result<(), BlockError>)>; N_SLOTS] = [None; N_SLOTS];
    let mut n = 0usize;
    {
        let mut g = BLK.lock();
        let Some(blk) = g.as_mut() else {
            return;
        };
        let mut qi = 0usize;
        while qi < blk.nq as usize {
            if let Some(v) = blk.vqs[qi].as_mut() {
                while let Some(u) = v.vq.get_used() {
                    let id = u.id as usize;
                    let si = if id < MAX_QSIZE {
                        let s = v.inflight[id];
                        v.inflight[id] = FREE;
                        s
                    } else {
                        FREE
                    };
                    if si == FREE || (si as usize) >= N_SLOTS {
                        continue;
                    }
                    let si = si as usize;
                    blk.slots.sync_for_cpu();
                    let st = unsafe { *slot_base(&blk.slots, si).add(16) };
                    let res = map_status(st);
                    if let Some(req) = blk.slot_req[si].take() {
                        if res.is_ok() && req.bio.op == Op::Read {
                            copy_from_bounce(&blk.slots, si, &req);
                        }
                        if n < N_SLOTS {
                            done[n] = Some((req, res));
                            n += 1;
                        }
                    }
                    blk.slot_used[si] = false;
                    COMPLETIONS.fetch_add(1, Ordering::SeqCst);
                }
            }
            qi += 1;
        }
    }
    let mut i = 0usize;
    while i < n {
        if let Some((req, res)) = done[i].take() {
            finish(req, res);
        }
        i += 1;
    }
    pump();
}

fn blk_top() {
    TOP_HITS.fetch_add(1, Ordering::SeqCst);
    let isr = ISR_VA.load(Ordering::Acquire);
    if isr != 0 {
        let _ = r8(isr, 0);
    }
}

fn blk_work() {
    THREAD_HITS.fetch_add(1, Ordering::SeqCst);
    harvest();
}

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

fn setup(dev: &mut Device, caps: ModernCaps) -> Result<(), VirtioError> {
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
        PHYS_EXP.store(r8(cfg, CFG_TOPOLOGY), Ordering::Release);
        ALIGN_OFF.store(r8(cfg, CFG_TOPOLOGY + 1), Ordering::Release);
        MIN_IO.store(r16(cfg, CFG_TOPOLOGY + 2), Ordering::Release);
        OPT_IO.store(r32(cfg, CFG_TOPOLOGY + 4), Ordering::Release);
    }
    if feat & F_DISCARD != 0 {
        MAX_DISCARD.store(r32(cfg, CFG_MAX_DISCARD_SECTORS), Ordering::Release);
        let _ = (r32(cfg, CFG_MAX_DISCARD_SEG), r32(cfg, CFG_DISCARD_ALIGN));
    }
    let cfg_nq = r16(cfg, CFG_NUM_QUEUES);
    let common_nq = r16(common, COMMON_OFF_NUM_QUEUES);
    let offered = nq_from_config(feat, cfg_nq, common_nq);
    let nq = offered.min(online_cpus()).min(MAX_VQ as u16).max(1) as usize;

    let table_size = msix_table_size(dev);
    let per_q_msix = table_size as usize >= nq;
    let mut vecs = [0u8; MAX_VQ];
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
        if irq_init::set_threaded(vec, Some(blk_top), blk_work).is_err() {
            let _ = irq_init::free_vector(vec);
            fail_status(common);
            return Err(VirtioError::Failed);
        }
        if let Err(e) = irq_init::enable_msix(dev, 0, vec, pc.apic_id.load(Ordering::Relaxed) as u8)
        {
            let _ = irq_init::free_vector(vec);
            fail_status(common);
            return match e {
                IrqError::NoRoute => Err(VirtioError::NoCaps),
                _ => Err(VirtioError::Failed),
            };
        }
        vecs[0] = vec;
        nvec = 1;
    }

    let Some(slots) = dma_init::alloc(DmaAlloc::dma32((N_SLOTS * SLOT_STRIDE) as u64)) else {
        fail_armed(dev, common, &vecs, nvec);
        return Err(VirtioError::Failed);
    };
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
        let vec = if per_q_msix {
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
            if irq_init::set_threaded(vec, Some(blk_top), blk_work).is_err() {
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
            nvec += 1;
            vec
        } else {
            vecs[0]
        };

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
        unsafe {
            core::ptr::write_bytes(qdma.virt() as *mut u8, 0, qdma.len() as usize);
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
            vec,
            inflight: [FREE; MAX_QSIZE],
        });
        qi += 1;
    }

    let st = r8(common, COMMON_OFF_STATUS);
    w8(common, COMMON_OFF_STATUS, st | STATUS_DRIVER_OK);

    ISR_VA.store(isr, Ordering::Release);
    FEATURES.store(feat, Ordering::Release);
    BLK_SIZE.store(blk_size, Ordering::Release);
    CAP.store(capacity, Ordering::Release);
    NQ.store(nq as u8, Ordering::Release);
    STATE.store(DeviceState::Ready.as_u8(), Ordering::Release);

    let boxed = Box::new(Blk {
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
    *BLK.lock() = Some(boxed);
    LIVE.store(true, Ordering::Release);

    let _ = write_marker(&mut Serial, NAME, capacity);
    let _ = writeln!(Serial);
    let mq = if feat & F_MQ != 0 { "mq" } else { "sq" };
    crate::marker!(
        "vibeOS: virtio: blk {NAME} {} qsz={q0sz} nq={nq} {mq} feat={:#x} bs={blk_size} topo={}/{} discard={}",
        dev.addr,
        feat,
        PHYS_EXP.load(Ordering::Acquire),
        OPT_IO.load(Ordering::Acquire),
        feat & F_DISCARD != 0
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
    fn probe(&self, dev: &mut Device) -> Result<(), ProbeError> {
        if LIVE.load(Ordering::Acquire) {
            crate::marker!("vibeOS: virtio: blk already bound");
            return Err(ProbeError::Failed);
        }
        let caps = virtio::read_modern_caps(&mut pci_init::HwCfg, dev.addr);
        if !caps.is_complete() || caps.device.is_none() {
            crate::marker!("vibeOS: virtio: blk missing modern caps");
            return Err(ProbeError::NoResource);
        }
        claim_bars(dev, &caps);
        match setup(dev, caps) {
            Ok(()) => Ok(()),
            Err(VirtioError::NoVersion1) => {
                crate::marker!("vibeOS: virtio: blk no VERSION_1");
                Err(ProbeError::Failed)
            }
            Err(e) => {
                crate::marker!("vibeOS: virtio: blk probe {}", e.as_str());
                Err(ProbeError::Failed)
            }
        }
    }
    fn remove(&self, _dev: &mut Device) {}
}

pub fn init() {
    let _ = dev_init::register_driver(&BLK_DRV);
}

pub fn live() -> bool {
    LIVE.load(Ordering::Acquire)
}

pub fn state() -> DeviceState {
    DeviceState::from_u8(STATE.load(Ordering::Acquire))
}

pub fn name() -> &'static str {
    NAME
}

pub fn logical_block_size() -> u32 {
    BLK_SIZE.load(Ordering::Acquire)
}

pub fn capacity_sectors() -> u64 {
    CAP.load(Ordering::Acquire)
}

pub fn features() -> u64 {
    FEATURES.load(Ordering::Acquire)
}

pub fn num_queues() -> u8 {
    NQ.load(Ordering::Acquire)
}

pub fn has_mq() -> bool {
    features() & F_MQ != 0 && num_queues() > 1
}

pub fn has_flush() -> bool {
    features() & F_FLUSH != 0
}

pub fn has_discard() -> bool {
    features() & F_DISCARD != 0
}

pub fn top_hits() -> u32 {
    TOP_HITS.load(Ordering::Acquire)
}

pub fn thread_hits() -> u32 {
    THREAD_HITS.load(Ordering::Acquire)
}

pub fn completions() -> u32 {
    COMPLETIONS.load(Ordering::Acquire)
}

pub fn persist_lba() -> u64 {
    // Inside the Linux GPT partition (starts at 512). Not GPT backup.
    const LBA: u64 = 2048;
    let cap = capacity_sectors();
    if cap > LBA + 1 {
        LBA
    } else {
        cap.saturating_sub(1)
    }
}

pub fn io_reqs() -> u64 {
    IO_REQS.load(Ordering::Relaxed)
}

fn submit_req(req: Request) -> Result<bool, BlockError> {
    let mut g = BLK.lock();
    let blk = g.as_mut().ok_or(BlockError::Failed)?;
    blk.q.submit(req)?;
    if blk.running {
        Ok(false)
    } else {
        blk.running = true;
        Ok(true)
    }
}

/// Check a request and tie it to `w`. Hard IRQ must not call this.
fn build(
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
    if !LIVE.load(Ordering::Acquire) {
        return Err(BlockError::Failed);
    }
    if DeviceState::from_u8(STATE.load(Ordering::Acquire)) == DeviceState::Failed {
        return Err(BlockError::Failed);
    }
    let bs = logical_block_size() as usize;
    let cap = capacity_sectors();
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

fn start(req: Request) -> Result<(), BlockError> {
    if submit_req(req)? {
        pump();
    }
    Ok(())
}

/// Async submit. `buf` lives until `w` completes. Hard IRQ must not call this.
pub fn submit(
    op: Op,
    lba: u64,
    nsect: u32,
    ptr: usize,
    len: usize,
    w: &IoWaiter,
) -> Result<(), BlockError> {
    start(build(op, lba, nsect, ptr, len, w)?)
}

fn blocking(op: Op, lba: u64, nsect: u32, ptr: usize, len: usize) -> Result<(), BlockError> {
    blocking_req(op, lba, nsect, ptr, len, false)
}

/// Submit and wait, yielding while the queue is full. `fua` marks a write.
fn blocking_req(
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
        let mut req = build(op, lba, nsect, ptr, len, &w)?;
        if fua {
            req = req.with_fua();
        }
        match start(req) {
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

pub fn read(lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
    let bs = logical_block_size() as usize;
    if bs == 0 || !buf.len().is_multiple_of(bs) {
        return Err(BlockError::Inval);
    }
    let nsect = (buf.len() / bs) as u32;
    blocking(Op::Read, lba, nsect, buf.as_mut_ptr() as usize, buf.len())
}

pub fn write(lba: u64, buf: &[u8]) -> Result<(), BlockError> {
    let bs = logical_block_size() as usize;
    if bs == 0 || !buf.len().is_multiple_of(bs) {
        return Err(BlockError::Inval);
    }
    let nsect = (buf.len() / bs) as u32;
    blocking(Op::Write, lba, nsect, buf.as_ptr() as usize, buf.len())
}

pub fn flush() -> Result<(), BlockError> {
    blocking(Op::Flush, 0, 0, 0, 0)
}

/// Write `buf` at `lba` with `Fua`: durable when this returns `Ok`.
/// virtio-blk has no FUA (DESIGN §10.4), so the queue sends a `Flush`.
pub fn write_fua(lba: u64, buf: &[u8]) -> Result<(), BlockError> {
    let bs = logical_block_size() as usize;
    if bs == 0 || !buf.len().is_multiple_of(bs) {
        return Err(BlockError::Inval);
    }
    let nsect = (buf.len() / bs) as u32;
    blocking_req(
        Op::Write,
        lba,
        nsect,
        buf.as_ptr() as usize,
        buf.len(),
        true,
    )
}

/// `Flush` requests dispatched to vda, emulated-`Fua` ones and those
/// finished locally without `F_FLUSH` included.
pub fn flushes() -> u64 {
    FLUSHES.load(Ordering::Relaxed)
}

pub fn discard(lba: u64, nsectors: u64) -> Result<(), BlockError> {
    if nsectors == 0 || nsectors > u32::MAX as u64 {
        return Err(BlockError::Inval);
    }
    blocking(Op::Discard, lba, nsectors as u32, 0, 0)
}

struct Vda;

impl block::BlockDevice for Vda {
    fn name(&self) -> &'static str {
        NAME
    }
    fn logical_block_size(&self) -> u32 {
        logical_block_size()
    }
    fn capacity_sectors(&self) -> u64 {
        capacity_sectors()
    }
    fn state(&self) -> DeviceState {
        state()
    }
    fn read(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        read(lba, buf)
    }
    fn write(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
        write(lba, buf)
    }
    fn flush(&self) -> Result<(), BlockError> {
        flush()
    }
    fn discard(&self, lba: u64, nsectors: u64) -> Result<(), BlockError> {
        discard(lba, nsectors)
    }
}

pub fn device() -> Option<&'static dyn block::BlockDevice> {
    if live() { Some(&Vda) } else { None }
}

pub fn shell_line(f: &mut impl core::fmt::Write) -> core::fmt::Result {
    if !live() {
        return Ok(());
    }
    writeln!(
        f,
        "vibeOS: blk: {NAME} {} {} sectors {} nq={} io={}",
        logical_block_size(),
        capacity_sectors(),
        state().as_str(),
        num_queues(),
        io_reqs()
    )
}
