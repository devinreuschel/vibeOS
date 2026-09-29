//! In-guest tests of the FAT and vibefs volume slots (invariant I236).

use core::sync::atomic::{AtomicBool, Ordering};

use crate::fat_init;
use crate::ktest::Outcome;
use crate::vibefs_init;

/// A claimed slot: its id and its `used` and `busy` flags.
type Claimed = (u8, &'static AtomicBool, &'static AtomicBool);

/// Claim the first free FAT slot past slot 0 as `mount_dev` does, under
/// `ALLOC`, and hold its busy flag.
fn claim_fat() -> Option<Claimed> {
    let _g = fat_init::ALLOC.lock();
    let (id, s) = fat_init::SLOTS
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, s)| !s.used.load(Ordering::Acquire))?;
    s.busy.store(true, Ordering::Release);
    s.used.store(true, Ordering::Release);
    Some((id as u8, &s.used, &s.busy))
}

/// [`claim_fat`] for a vibefs slot.
fn claim_vibefs() -> Option<Claimed> {
    let _g = vibefs_init::ALLOC.lock();
    let (id, s) = vibefs_init::SLOTS
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, s)| !s.used.load(Ordering::Acquire))?;
    s.busy.store(true, Ordering::Release);
    s.used.store(true, Ordering::Release);
    Some((id as u8, &s.used, &s.busy))
}

/// `drop_slot` on the used slot `c`, whose busy flag this thread holds as
/// another holder would: `grab` gives up after 1,000,000 yields, the error
/// comes back, and the slot keeps both flags. Then the slot is freed.
/// `Ok(false)` when there was no slot to claim.
fn busy_keeps(c: Option<Claimed>, drop_slot: fn(u8) -> bool) -> Result<bool, &'static str> {
    let Some((id, used, busy)) = c else {
        return Ok(false);
    };
    let dropped = drop_slot(id);
    let kept = used.load(Ordering::Acquire) && busy.load(Ordering::Acquire);
    used.store(false, Ordering::Release);
    busy.store(false, Ordering::Release);
    if dropped {
        return Err("drop_slot succeeded on a slot another thread holds");
    }
    if !kept {
        return Err("drop_slot changed a slot whose grab failed");
    }
    Ok(true)
}

pub(crate) fn test_fs_drop_slot_busy_keeps_slot() -> Outcome {
    let fat = match busy_keeps(claim_fat(), |id| fat_init::drop_slot(id).is_ok()) {
        Ok(ran) => ran,
        Err(e) => return Outcome::Fail(e),
    };
    let vibe = match busy_keeps(claim_vibefs(), |id| vibefs_init::drop_slot(id).is_ok()) {
        Ok(ran) => ran,
        Err(e) => return Outcome::Fail(e),
    };
    if !fat && !vibe {
        return Outcome::Skip("no free FAT or vibefs slot");
    }
    Outcome::Ok
}
