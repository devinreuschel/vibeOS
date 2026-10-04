//! The kernel's one block-device table (C-BLOCKREF, DEVICES.md §12.1): a
//! `vibeos::block::blockdev::Registry` under a ranked lock, and the
//! `DiskSeq` its ids come from.
//!
//! Nothing is allocated or freed under [`REG`]: [`register`] builds its
//! entry before it takes the lock, and every handle a lookup returns is a
//! clone taken under it, dropped by the caller after it unlocks.

use vibeos::block::blockdev::{Backing, BlockName, BlockRef, DiskSeq, Registry};
use vibeos::block::{BlockError, write_marker};
use vibeos::dev::Instance;
use vibeos::lock::RANK_DEVICE;

use crate::sync_init::SpinMutex;

static REG: SpinMutex<Registry> = SpinMutex::with_rank(Registry::new(), RANK_DEVICE);
static SEQ: DiskSeq = DiskSeq::new();

/// Register block device `name` over `dev` under a fresh id and print its
/// `vibeOS: block: <name> <n> sectors` marker. `Inval` for a bad name,
/// `Exists` when the name is taken, `NoMem` when the table or the heap is
/// full, `Gone` when a partition's disk is not registered.
pub fn register(name: &[u8], dev: Backing) -> Result<BlockRef, BlockError> {
    let name = BlockName::new(name)?;
    let id = SEQ.next()?;
    let r = BlockRef::try_new(id, name, dev)?;
    let nsect = r.capacity_sectors()?;
    // A clone goes in, so a refused insert drops only a count under the
    // lock, and `r`'s own count, perhaps the last, goes after it.
    REG.lock().insert(r.clone())?;
    #[expect(
        clippy::let_underscore_must_use,
        reason = "a write to Serial cannot fail (DESIGN §2.5)"
    )]
    let _ = crate::serial::write_line_with(|w| write_marker(w, r.name().as_str(), nsect));
    Ok(r)
}

/// A handle to the device named `name`.
pub fn lookup(name: &[u8]) -> Option<BlockRef> {
    REG.lock().lookup(name)
}

/// A handle to the device with id `id`.
pub fn lookup_id(id: u64) -> Option<BlockRef> {
    REG.lock().lookup_id(id)
}

/// Clone every registered handle into `out`, in table order; returns how
/// many. The caller drops them with the table unlocked.
pub fn snapshot(out: &mut [Option<BlockRef>]) -> usize {
    REG.lock().snapshot(out)
}

/// Take `dev` and its children out of the table, then close each one's gate
/// with the table unlocked (DEVICES.md §12.2 rule 5). Test builds only:
/// nothing unplugs a disk yet.
#[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
pub fn unregister(dev: &BlockRef) -> Result<(), BlockError> {
    use vibeos::block::MAX_BLOCKDEVS;
    let mut out: [Option<BlockRef>; MAX_BLOCKDEVS] = [const { None }; MAX_BLOCKDEVS];
    let n = REG.lock().unpublish(dev, &mut out)?;
    for r in out.iter().take(n).flatten() {
        r.kill()?;
    }
    Ok(())
}

/// Make `h`, a filesystem's volume instance, the holder of `dev`, which
/// owns it from then on. `Exists` when `dev` has one, `Gone` when it is
/// not registered.
pub fn set_holder(dev: &BlockRef, h: Instance) -> Result<(), BlockError> {
    REG.lock().set_holder(dev, h)
}

/// A reference to `dev`'s holder.
pub fn holder(dev: &BlockRef) -> Option<Instance> {
    REG.lock().holder(dev)
}

/// Take `dev`'s holder out; the caller drops it with the table unlocked.
pub fn take_holder(dev: &BlockRef) -> Option<Instance> {
    REG.lock().take_holder(dev)
}
