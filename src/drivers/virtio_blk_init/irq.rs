use super::issue::{copy_from_bounce, finish, slot_base};
use super::*;

/// The virtio-blk vector allocated on `cpu`, valid only on that CPU.
#[cfg(feature = "kernel_tests")]
pub fn queue_vector(cpu: u32) -> Option<u8> {
    QUEUE_VECS.iter().find_map(|q| {
        let v = q.load(Ordering::Acquire);
        (v & QUEUE_VEC_LIVE != 0 && ((v & !QUEUE_VEC_LIVE) >> 8) as u32 == cpu).then_some(v as u8)
    })
}

pub(super) fn harvest() {
    let mut done: [Option<(Request, Result<(), BlockError>)>; N_SLOTS] = [None; N_SLOTS];
    let mut n = 0usize;
    {
        let mut g = BLK.lock();
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

pub(super) fn blk_top(_ctx: Option<&(dyn core::any::Any + Send + Sync)>) {
    TOP_HITS.fetch_add(1, Ordering::SeqCst);
    let isr = ISR_VA.load(Ordering::Acquire);
    if isr != 0 {
        // Reading the ISR status acknowledges the interrupt; the value
        // itself is not needed (virtio 1.x §4.1.4.5).
        let _ = r8(isr, 0);
    }
}

pub(super) fn blk_work(_ctx: Option<&(dyn core::any::Any + Send + Sync)>) {
    THREAD_HITS.fetch_add(1, Ordering::SeqCst);
    harvest();
}
