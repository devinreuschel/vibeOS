//! The stalled AP's frame record for the stalled-AP test (kernel_tests,
//! x86_64; ROADMAP §11.4, F032). `smp_init`'s stall path records the frames
//! its bring-up allocated, and the test reads them back.

use crate::cell::IrqCell;
use crate::per_cpu_init;
use crate::smp::smp_init::ApFrames;

/// The frames the stalled AP's bring-up allocated, and how many of them were
/// free when its stall path ended, written on the boot CPU at its timeout.
struct StallRecord {
    frames: ApFrames,
    free_at_stall: usize,
}

static STALL_FRAMES: IrqCell<StallRecord> = IrqCell::new(StallRecord {
    frames: ApFrames::new(),
    free_at_stall: 0,
});

/// Whether the block at `base` of `order` is free now: on a buddy free list
/// (the buddy lock, rank buddy, taken and released here), or in this CPU's
/// stack cache (IF off, no lock). Each CPU's stack cache is its own, so only
/// the calling CPU's is read.
fn frame_is_free(base: u64, order: u8) -> bool {
    crate::pmm_init::with_buddy(|b| b.block_is_free(base, order))
        || per_cpu_init::with_current(|c| c.stack_cache.holds_frame(base))
}

/// Record the stalled AP's frames, and how many of them are free at this
/// point. Runs before `note_stalled_leak`, whose Release store publishes it.
pub(crate) fn record_stalled_frames(frames: &ApFrames) {
    let free_at_stall = frames
        .recorded()
        .filter(|&(base, order)| frame_is_free(base, order))
        .count();
    STALL_FRAMES.with(|s| {
        *s = StallRecord {
            frames: *frames,
            free_at_stall,
        };
    });
}

/// How many frames the stalled AP's bring-up recorded, the first of them
/// that is free now, and how many were free when the stall path recorded
/// them. Call after `stalled_ap_leaked` returns true.
pub fn stalled_ap_frames() -> (usize, Option<u64>, usize) {
    STALL_FRAMES.with(|s| {
        let free = s
            .frames
            .recorded()
            .find(|&(base, order)| frame_is_free(base, order))
            .map(|(base, _)| base);
        (s.frames.len, free, s.free_at_stall)
    })
}
