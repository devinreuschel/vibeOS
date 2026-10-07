use super::vq::{kick, pick_vq};
use super::*;

pub(super) const N_SLOTS: usize = 16;
pub(super) const BOUNCE: usize = 8192;
pub(super) const SLOT_META: usize = 64;
pub(super) const SLOT_STRIDE: usize = SLOT_META + BOUNCE;
pub(super) struct Blk {
    pub(super) q: Queue,
    pub(super) vqs: [Option<Vq>; MAX_VQ],
    pub(super) nq: u8,
    pub(super) slots: DmaBuffer,
    pub(super) slot_used: [bool; N_SLOTS],
    pub(super) slot_req: [Option<Request>; N_SLOTS],
    pub(super) features: u64,
    pub(super) blk_size: u32,
    pub(super) running: bool,
}

/// Slot `si`'s first byte. `si < N_SLOTS`, so the slot lies inside `slots`,
/// which `setup` allocates at `N_SLOTS * SLOT_STRIDE` bytes.
pub(super) fn slot_base(slots: &DmaBuffer, si: usize) -> *mut u8 {
    (slots.virt() as usize + si * SLOT_STRIDE) as *mut u8
}

pub(super) fn slot_dev(slots: &DmaBuffer, si: usize, off: usize) -> u64 {
    slots.device().as_u64() + (si * SLOT_STRIDE + off) as u64
}

pub(super) fn copy_to_bounce(slots: &DmaBuffer, si: usize, req: &Request) {
    // SAFETY: slot `si`'s `SLOT_META` header bytes precede its bounce area
    // inside `slots`; established by `virtio_blk_init::issue::slot_base`.
    let mut dst = unsafe { slot_base(slots, si).add(SLOT_META) };
    let mut i = 0u8;
    while i < req.nseg {
        let s = req.segs[i as usize];
        // SAFETY: invariant I55: the segment's `len` bytes stay valid and
        // untouched until the request completes, and `issue` checked that
        // the segments total at most `BOUNCE`, the bounce area's size;
        // established by `virtio_blk_init::VirtioBlk::build`.
        unsafe {
            core::ptr::copy_nonoverlapping(s.ptr as *const u8, dst, s.len);
            dst = dst.add(s.len);
        }
        i += 1;
    }
}

pub(super) fn copy_from_bounce(slots: &DmaBuffer, si: usize, req: &Request) {
    // SAFETY: as in `copy_to_bounce`; established by
    // `virtio_blk_init::issue::slot_base`.
    let mut src = unsafe { slot_base(slots, si).add(SLOT_META) };
    let mut i = 0u8;
    while i < req.nseg {
        let s = req.segs[i as usize];
        // SAFETY: invariant I55, as in `copy_to_bounce`; established by
        // `virtio_blk_init::VirtioBlk::build`.
        unsafe {
            core::ptr::copy_nonoverlapping(src, s.ptr as *mut u8, s.len);
            src = src.add(s.len);
        }
        i += 1;
    }
}

pub(super) fn alloc_slot(blk: &mut Blk) -> Option<usize> {
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

pub(super) fn descs_for(op: Op) -> u16 {
    match op {
        Op::Read | Op::Write | Op::Discard => 3,
        Op::Flush => 2,
    }
}

pub(super) enum Issued {
    Device { qi: usize, kick: bool },
    Local(Request, Result<(), BlockError>),
    Full(Request),
}

/// Requests one [`pump`](Self::pump) or `harvest` pass holds before it drops
/// the queue lock to wake waiters. Small, so the pass's frame stays far
/// below a top half's 4 KiB share of a kernel stack: `pump` runs on a
/// submitter's stack under the FAT write path, and `harvest` on irqth
/// (DESIGN §4.5).
pub(super) const PUMP_BATCH: usize = 2;

impl VirtioBlk {
    pub(super) fn issue(&self, blk: &mut Blk, req: Request) -> Issued {
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
        // SAFETY: slot `si` is free (`alloc_slot` just marked it), so the device
        // owns none of its `SLOT_STRIDE` bytes; established by
        // `virtio_blk_init::issue::alloc_slot`.
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
            // Acquire: pairs with the Release store in `setup`.
            let maxd = self.max_discard.load(Ordering::Acquire);
            if maxd != 0 && n512 > maxd {
                blk.slot_used[si] = false;
                return Issued::Local(req, Err(BlockError::Inval));
            }
            let mut disc = [0u8; 16];
            pack_discard(&mut disc, sector, n512, 0);
            // SAFETY: as above: bytes 32..48 of the free slot `si`; established
            // by `virtio_blk_init::issue::alloc_slot`.
            unsafe {
                core::ptr::copy_nonoverlapping(disc.as_ptr(), base.add(32), 16);
            }
        }

        let hdr_d = slot_dev(&blk.slots, si, 0);
        let st_d = slot_dev(&blk.slots, si, 16);
        let data_d = slot_dev(&blk.slots, si, SLOT_META);
        let disc_d = slot_dev(&blk.slots, si, 32);
        blk.slots.sync_for_device::<Arch>();

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
        let old = v.vq.last_avail();
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
        v.qdma.sync_for_device::<Arch>();
        let kick = v.vq.should_kick(old);
        Issued::Device { qi, kick }
    }

    /// Dispatch what `blk.q` picks until it is idle or a virtqueue is full.
    /// A request finished here (`Local`) is completed or aborted under the queue lock,
    /// and its waiters wake after the lock drops, [`PUMP_BATCH`] at a time.
    pub(super) fn pump(&self) {
        loop {
            let mut kicks = [0u64; MAX_VQ];
            let mut notify32 = [false; MAX_VQ];
            let mut want = [false; MAX_VQ];
            let mut local: [Option<(Request, Result<(), BlockError>)>; PUMP_BATCH] =
                [None; PUMP_BATCH];
            let mut nlocal = 0usize;
            let mut again = false;
            {
                let mut g = self.st.lock();
                let Some(blk) = g.as_deref_mut() else {
                    return;
                };
                loop {
                    if nlocal == PUMP_BATCH {
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
                    let seq = req.seq;
                    match self.issue(blk, req) {
                        Issued::Device { qi, kick } => {
                            // Relaxed: a count; pairs with nothing.
                            self.io_reqs.fetch_add(1, Ordering::Relaxed);
                            if req.bio.op == Op::Flush {
                                // Relaxed: a count; pairs with nothing.
                                self.flushes.fetch_add(1, Ordering::Relaxed);
                            }
                            if kick && let Some(v) = blk.vqs[qi].as_ref() {
                                kicks[qi] = v.doorbell;
                                notify32[qi] = v.notify32;
                                want[qi] = true;
                            }
                        }
                        Issued::Local(req, res) => {
                            if req.bio.op == Op::Flush && res.is_ok() {
                                // Relaxed: a count; pairs with nothing.
                                self.flushes.fetch_add(1, Ordering::Relaxed);
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
                    kick(kicks[i], i as u16, notify32[i]);
                }
                i += 1;
            }
            if !again {
                return;
            }
        }
    }

    /// Whether the device has set `DEVICE_NEEDS_RESET` (virtio 1.2
    /// §2.1.1). Until ROADMAP §12.5's error handler resets it, that is the
    /// one failure that fails the device.
    pub(super) fn needs_reset(&self) -> bool {
        // Acquire: pairs with the Release store in `setup`.
        let common = self.common.load(Ordering::Acquire);
        if common == 0 {
            return false;
        }
        // Acquire: pairs with the Release store in `setup_mmio`.
        let st = if self.mmio.load(Ordering::Acquire) {
            crate::virtio_mmio_init::status(common)
        } else {
            r8(common, COMMON_OFF_STATUS)
        };
        exhausted_fails_device(st)
    }

    /// [`fail_rest`](Self::fail_rest), only when [`needs_reset`](Self::needs_reset).
    fn fail_if_needs_reset(&self) {
        if self.needs_reset() {
            self.fail_rest();
        }
    }

    pub(super) fn fail_rest(&self) {
        // Release: pairs with the Acquire load in `state`.
        self.state
            .store(DeviceState::Failed.as_u8(), Ordering::Release);
        loop {
            let mut dump = [None; DRAIN_BATCH];
            let n = {
                let mut g = self.st.lock();
                let Some(blk) = g.as_deref_mut() else {
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

    /// Retire `req`'s dispatch under the queue lock, drop it, then wake. A retry goes
    /// back on the queue for [`harvest`](Self::harvest)'s closing [`pump`](Self::pump).
    pub(super) fn finish(&self, mut req: Request, res: Result<(), BlockError>) {
        let seq = req.seq;
        let mut g = self.st.lock();
        let Some(blk) = g.as_deref_mut() else {
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
                let failed = blk.q.failed;
                drop(g);
                if requeued.is_err() {
                    // A full queue fails this request alone, with its own
                    // error; `Failed` only when the queue already failed.
                    let err = if failed { BlockError::Failed } else { e };
                    block_init::complete_waiters(&req, Err(err));
                    self.fail_if_needs_reset();
                }
            }
            // A spent budget, or an error no retry helps, fails the
            // request alone (DESIGN §10.3).
            Err(e) => {
                blk.q.abort(seq);
                drop(g);
                block_init::complete_waiters(&req, Err(e));
                self.fail_if_needs_reset();
            }
        }
    }

    pub(super) fn submit_req(&self, req: Request) -> Result<bool, BlockError> {
        let mut g = self.st.lock();
        let blk = g.as_deref_mut().ok_or(BlockError::Failed)?;
        blk.q.submit(req)?;
        if blk.running {
            Ok(false)
        } else {
            blk.running = true;
            Ok(true)
        }
    }
}

#[cfg(feature = "kernel_tests")]
/// Async submit. `buf` lives until `w` completes. Hard IRQ must not call this.
pub fn submit(
    blk: &VirtioBlk,
    op: Op,
    lba: u64,
    nsect: u32,
    ptr: usize,
    len: usize,
    w: &IoWaiter,
) -> Result<(), BlockError> {
    if blk.submit_req(blk.build(op, lba, nsect, ptr, len, w)?)? {
        blk.pump();
    }
    Ok(())
}
