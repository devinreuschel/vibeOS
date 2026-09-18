//! VFS bring-up. ROADMAP §8.1.
//!
//! Static tables behind RANK_DEVICE (DESIGN §2.1: device-rank peer,
//! not a seventh global lock). Dummy ramfs at `/` so later FAT/pseudo
//! can mount. No serial marker.

use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::fs::{FsError, PathRef, Vfs};
use vibeos::lock::RANK_DEVICE;

use crate::sync_init::SpinMutex;

static VFS: SpinMutex<Vfs> = SpinMutex::with_rank(Vfs::new(), RANK_DEVICE);
static LIVE: AtomicBool = AtomicBool::new(false);

pub fn init() {
    let ok = {
        let mut g = VFS.lock();
        g.mount_root().is_ok()
    };
    LIVE.store(ok, Ordering::Release);
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn live() -> bool {
    LIVE.load(Ordering::Acquire)
}

#[cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]
pub fn with<R>(f: impl FnOnce(&mut Vfs) -> R) -> R {
    let mut g = VFS.lock();
    f(&mut g)
}

#[allow(dead_code)]
pub fn root() -> Result<PathRef, FsError> {
    if !live() {
        return Err(FsError::Io);
    }
    with(|v| v.root())
}
