//! In-guest test of the FAT and vibefs volume locks (ROADMAP §10.4, F060):
//! a retire waits for the volume's holder instead of failing.

use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::dev::Instance;
use vibeos::lock::RANK_DEVICE;

use crate::fat_init::{self, FatVolume};
use crate::ktest::{Outcome, sleep_until};
use crate::sync_init::SpinMutex;
use crate::thread_init;
use crate::vibefs_init::{self, VibeVolume};

/// The volume the dropper thread retires.
static SPARE: SpinMutex<Option<Instance>> = SpinMutex::with_rank(None, RANK_DEVICE);
/// Set when the dropper's `drop_slot` returned.
static DONE: AtomicBool = AtomicBool::new(false);

/// Retire the volume in [`SPARE`], then set [`DONE`].
fn dropper() {
    let v = SPARE.lock().take();
    if let Some(i) = v {
        if let Some(f) = i.downcast_ref::<FatVolume>() {
            fat_init::drop_slot(f);
        } else if let Some(b) = i.downcast_ref::<VibeVolume>() {
            vibefs_init::drop_slot(b);
        }
    }
    DONE.store(true, Ordering::Release);
}

/// While `hold` holds the volume's lock, a thread's `drop_slot` on it is
/// still waiting 50 ms later and the volume is still in use; once the
/// holder lets go it finishes and the volume is retired.
fn waits(
    inst: &Instance,
    used: &AtomicBool,
    hold: impl FnOnce(&dyn Fn() -> Result<(), &'static str>) -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    if !used.load(Ordering::Acquire) {
        return Err("volume not in use");
    }
    DONE.store(false, Ordering::Release);
    *SPARE.lock() = Some(inst.clone());
    hold(&|| {
        thread_init::spawn("s59drop", dropper).map_err(|_| "spawn")?;
        thread_init::sleep_ms(50);
        if DONE.load(Ordering::Acquire) || !used.load(Ordering::Acquire) {
            return Err("drop_slot did not wait for the volume's holder");
        }
        Ok(())
    })?;
    if !sleep_until(|| DONE.load(Ordering::Acquire), 2_000) {
        return Err("drop_slot did not finish after the holder let go");
    }
    if used.load(Ordering::Acquire) {
        return Err("volume still in use after drop_slot");
    }
    Ok(())
}

/// A spare FAT and a spare vibefs volume, which no other thread reaches,
/// each held by this thread while another retires it.
pub(crate) fn test_fs_drop_slot_waits_for_holder() -> Outcome {
    let Ok(fat) = fat_init::spare_volume() else {
        return Outcome::Fail("no spare FAT volume");
    };
    let Some(f) = fat.downcast_ref::<FatVolume>() else {
        return Outcome::Fail("spare FAT volume type");
    };
    if let Err(e) = waits(&fat, &f.used, |g| fat_init::hold(f, g)) {
        return Outcome::Fail(e);
    }
    let Ok(vibe) = vibefs_init::spare_volume() else {
        return Outcome::Fail("no spare vibefs volume");
    };
    let Some(v) = vibe.downcast_ref::<VibeVolume>() else {
        return Outcome::Fail("spare vibefs volume type");
    };
    match waits(&vibe, &v.used, |g| vibefs_init::hold(v, g)) {
        Ok(()) => Outcome::Ok,
        Err(e) => Outcome::Fail(e),
    }
}
