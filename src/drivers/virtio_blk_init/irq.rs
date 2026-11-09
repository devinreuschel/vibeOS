use core::any::Any;
use core::sync::atomic::Ordering;

use super::issue::{PUMP_BATCH, copy_from_bounce, slot_base};
use super::*;

impl VirtioBlk {
    /// This disk's IDT vector allocated on `cpu`, valid only on that CPU.
    #[cfg(feature = "kernel_tests")]
    pub fn queue_vector(&self, cpu: u32) -> Option<u8> {
        self.queue_vecs.iter().find_map(|q| {
            // Acquire: pairs with the Release store in `setup`.
            let v = q.load(Ordering::Acquire);
            (v & QUEUE_VEC_LIVE != 0 && ((v & !QUEUE_VEC_LIVE) >> 8) as u32 == cpu)
                .then_some(v as u8)
        })
    }

    /// `st`, or `S_UNSUPP` while an injected failure is left (test-only).
    #[cfg(feature = "kernel_tests")]
    fn injected(&self, st: u8) -> u8 {
        // AcqRel, Acquire on failure: pairs with the Release store in `inject_unsupp`.
        let take = self
            .inject_unsupp
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1));
        if take.is_ok() {
            vibeos::virtio_blk::S_UNSUPP
        } else {
            st
        }
    }

    pub(super) fn harvest(&self) {
        // [`PUMP_BATCH`] completions at a time, same as [`pump`](Self::pump):
        // a `[Request; N_SLOTS]` local is ~4 KiB, and this runs on irqth's
        // 16 KiB stack under a top half (DESIGN §4.5).
        let mut bad = false;
        loop {
            let mut done: [Option<(Request, Result<(), BlockError>)>; PUMP_BATCH] =
                [None; PUMP_BATCH];
            let mut n = 0usize;
            {
                let mut g = self.st.lock();
                let Some(blk) = g.as_deref_mut() else {
                    return;
                };
                let mut qi = 0usize;
                while qi < blk.nq as usize && n < PUMP_BATCH && !bad {
                    if let Some(v) = blk.vqs[qi].as_mut() {
                        while n < PUMP_BATCH {
                            let u = match v.vq.get_used() {
                                Ok(Some(u)) => u,
                                Ok(None) => break,
                                Err(_) => {
                                    bad = true;
                                    break;
                                }
                            };
                            // The queue vouches for `id`: in flight and below its size.
                            let id = u.id as usize;
                            let si = v.inflight.get(id).copied().unwrap_or(FREE);
                            if let Some(e) = v.inflight.get_mut(id) {
                                *e = FREE;
                            }
                            if si == FREE || (si as usize) >= N_SLOTS {
                                bad = true;
                                break;
                            }
                            let si = si as usize;
                            let stride = blk.stride;
                            // Before the CPU reads the slot: a non-coherent
                            // device's slot is invalidated line by line.
                            take_slot::<Arch>(&blk.slots, si, stride);
                            // SAFETY: slot `si` is in flight on this queue, so the
                            // device wrote its status byte at `SLOT_STATUS`, synced
                            // for the CPU above; established by
                            // `virtio_blk_init::issue::slot_base`.
                            let st = unsafe { *slot_base(&blk.slots, si, stride).add(SLOT_STATUS) };
                            #[cfg(feature = "kernel_tests")]
                            let st = self.injected(st);
                            let res = map_status(st);
                            if let Some(req) = blk.slot_req[si].take() {
                                if res.is_ok() && req.bio.op == Op::Read {
                                    copy_from_bounce(&blk.slots, si, stride, &req);
                                }
                                done[n] = Some((req, res));
                                n += 1;
                            }
                            blk.slot_used[si] = false;
                            self.completions.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    qi += 1;
                }
            }
            let mut i = 0usize;
            while i < n {
                if let Some((req, res)) = done[i].take() {
                    self.finish(req, res);
                }
                i += 1;
            }
            if n == 0 || bad {
                break;
            }
        }
        if bad {
            // The device broke the used ring: nothing it says can be trusted.
            crate::klog_ratelimited!(
                1000,
                vibeos::log::Level::Error,
                "vibeOS: virtio-blk: {}: bad used ring, failing the device",
                self.name()
            );
            self.fail_rest();
            return;
        }
        self.pump();
    }
}

/// The instance a threaded half was set with.
fn instance_of(ctx: Option<&(dyn Any + Send + Sync)>) -> Option<&VirtioBlk> {
    ctx.and_then(|c| c.downcast_ref::<VirtioBlk>())
}

pub(super) fn blk_top(ctx: Option<&(dyn Any + Send + Sync)>) {
    let Some(b) = instance_of(ctx) else {
        return;
    };
    b.top_hits.fetch_add(1, Ordering::SeqCst);
    // Acquire: pairs with the Release store in `setup`.
    if b.mmio.load(Ordering::Acquire) {
        // Acquire: pairs with the Release store of `common` in `setup_mmio`.
        let base = b.common.load(Ordering::Acquire);
        if base != 0 {
            crate::virtio_mmio_init::ack_isr(base);
        }
    } else {
        // Acquire: pairs with the Release store in `setup`.
        let isr = b.isr.load(Ordering::Acquire);
        if isr != 0 {
            // Reading the ISR status acknowledges the interrupt; the value
            // itself is not needed (virtio 1.x §4.1.4.5).
            let _ = crate::virtio_init::read_isr8(isr);
        }
    }
}

pub(super) fn blk_work(ctx: Option<&(dyn Any + Send + Sync)>) {
    let Some(b) = instance_of(ctx) else {
        return;
    };
    b.thread_hits.fetch_add(1, Ordering::SeqCst);
    b.harvest();
}
