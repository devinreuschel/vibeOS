use core::any::Any;

use super::issue::{copy_from_bounce, slot_base};
use super::*;

impl VirtioBlk {
    /// This disk's vector allocated on `cpu`, valid only on that CPU.
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
        let mut done: [Option<(Request, Result<(), BlockError>)>; N_SLOTS] = [None; N_SLOTS];
        let mut n = 0usize;
        {
            let mut g = self.st.lock();
            let Some(blk) = g.as_deref_mut() else {
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
                        blk.slots.sync_for_cpu::<Arch>();
                        // SAFETY: slot `si` is in flight on this queue, so the
                        // device wrote its status byte at offset 16, synced for
                        // the CPU above; established by
                        // `virtio_blk_init::issue::slot_base`.
                        let st = unsafe { *slot_base(&blk.slots, si).add(16) };
                        #[cfg(feature = "kernel_tests")]
                        let st = self.injected(st);
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
    let isr = b.isr.load(Ordering::Acquire);
    if isr != 0 {
        // Reading the ISR status acknowledges the interrupt; the value
        // itself is not needed (virtio 1.x §4.1.4.5).
        let _ = r8(isr, 0);
    }
}

pub(super) fn blk_work(ctx: Option<&(dyn Any + Send + Sync)>) {
    let Some(b) = instance_of(ctx) else {
        return;
    };
    b.thread_hits.fetch_add(1, Ordering::SeqCst);
    b.harvest();
}
