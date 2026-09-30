//! In-guest tests of the FAT and vibefs volume instances' busy flags
//! (invariant I236).

use core::sync::atomic::{AtomicBool, Ordering};

use crate::fat_init::{self, FatVolume};
use crate::ktest::Outcome;
use crate::vibefs_init::{self, VibeVolume};

/// Take a volume's busy flag as another holder would.
fn hold(busy: &AtomicBool) -> bool {
    busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// `drop_slot` on a used volume whose busy flag this thread holds as
/// another holder would: `grab` gives up after 1,000,000 yields, the error
/// comes back, and the volume keeps both flags. Then the flag is dropped.
fn busy_keeps(
    used: &AtomicBool,
    busy: &AtomicBool,
    drop_slot: impl FnOnce() -> bool,
) -> Result<(), &'static str> {
    if !used.load(Ordering::Acquire) {
        return Err("volume not in use");
    }
    if !hold(busy) {
        return Err("volume busy");
    }
    let dropped = drop_slot();
    let kept = used.load(Ordering::Acquire) && busy.load(Ordering::Acquire);
    busy.store(false, Ordering::Release);
    if dropped {
        return Err("drop_slot succeeded on a volume another thread holds");
    }
    if !kept {
        return Err("drop_slot changed a volume whose grab failed");
    }
    Ok(())
}

/// A spare FAT and a spare vibefs volume, which no other thread reaches,
/// each held busy while `drop_slot` runs on it.
pub(crate) fn test_fs_drop_slot_busy_keeps_slot() -> Outcome {
    let Ok(fat) = fat_init::spare_volume() else {
        return Outcome::Fail("no spare FAT volume");
    };
    let Some(f) = fat.downcast_ref::<FatVolume>() else {
        return Outcome::Fail("spare FAT volume type");
    };
    if let Err(e) = busy_keeps(&f.used, &f.busy, || fat_init::drop_slot(f).is_ok()) {
        return Outcome::Fail(e);
    }
    let Ok(vibe) = vibefs_init::spare_volume() else {
        return Outcome::Fail("no spare vibefs volume");
    };
    let Some(v) = vibe.downcast_ref::<VibeVolume>() else {
        return Outcome::Fail("spare vibefs volume type");
    };
    match busy_keeps(&v.used, &v.busy, || vibefs_init::drop_slot(v).is_ok()) {
        Ok(()) => Outcome::Ok,
        Err(e) => Outcome::Fail(e),
    }
}
