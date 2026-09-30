//! The File API's in-guest test hooks (AGENTS.md rule 9): atomics only,
//! and no wait here is longer than 10,000 `yield_now` calls. Arming one
//! installs them all in `fs_init` (`install_hooks`); unarmed, each does
//! nothing. The fs in-guest tests' path wrappers are here too.

use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::fs::{FsError, MAX_PATH};
use vibeos::kalloc::TryVec;
use vibeos::limits::MAX_OPEN_FILES;

use crate::file_init;
use crate::fs_init;
use crate::thread_init;

static WRITE_YIELD: AtomicBool = AtomicBool::new(false);
static HOLD: AtomicBool = AtomicBool::new(false);
static HELD: AtomicBool = AtomicBool::new(false);
static RELEASE: AtomicBool = AtomicBool::new(false);
static OPEN_RACE: AtomicBool = AtomicBool::new(false);

/// The most `yield_now` calls any wait here makes.
const MAX_YIELDS: u32 = 10_000;

/// Each `file_init::write` yields once between its backend I/O and
/// its write-back to the open-file table.
pub(super) fn set_write_yield(on: bool) {
    install_hooks();
    WRITE_YIELD.store(on, Ordering::Release);
}

/// The next `file_init::write` waits between its backend I/O and its
/// write-back until [`release_write`].
pub(super) fn hold_next_write() {
    install_hooks();
    RELEASE.store(false, Ordering::Release);
    HELD.store(false, Ordering::Release);
    HOLD.store(true, Ordering::Release);
}

/// Whether the held write has reached its wait.
pub(super) fn write_held() -> bool {
    HELD.load(Ordering::Acquire)
}

/// Let the held write go on.
pub(super) fn release_write() {
    RELEASE.store(true, Ordering::Release);
}

/// `file_init::open` with `O_CREAT` creates the file itself between
/// its walk and its create, as another opener would.
pub(super) fn set_open_race(on: bool) {
    install_hooks();
    OPEN_RACE.store(on, Ordering::Release);
}

/// The `Vfs` File API's `open_race` hook.
fn open_race() -> bool {
    OPEN_RACE.load(Ordering::Acquire)
}

/// The `Vfs` File API's `write_window` hook, with the VFS lock
/// dropped.
fn write_window() {
    if HOLD.swap(false, Ordering::AcqRel) {
        HELD.store(true, Ordering::Release);
        let mut n = 0u32;
        while !RELEASE.load(Ordering::Acquire) && n < MAX_YIELDS {
            thread_init::yield_now();
            n += 1;
        }
        HELD.store(false, Ordering::Release);
    }
    if WRITE_YIELD.load(Ordering::Acquire) {
        thread_init::yield_now();
    }
}

/// `Vfs::open` opens so far.
pub(super) fn open_counts() -> u32 {
    fs_init::with(|v| v.stats.opens)
}

/// Each open-file slot's `(used, refs, gen)`, in a heap table allocated
/// before the VFS lock; `None` when it cannot be.
pub(super) fn table() -> Option<TryVec<(bool, u16, u16)>> {
    let mut t = vibeos::limits::table(MAX_OPEN_FILES, || (false, 0, 0)).ok()?;
    let n = fs_init::with(|v| v.file_table(&mut t));
    t.truncate(n);
    Some(t)
}

/// `path` made absolute against the working directory.
fn abs(path: &[u8]) -> Result<([u8; MAX_PATH], usize), FsError> {
    file_init::join_cwd(path)
}

pub(super) fn symlink_path(path: &[u8], target: &[u8]) -> Result<(), FsError> {
    let (b, n) = abs(path)?;
    fs_init::api().symlink(None, &b[..n], target)
}

pub(super) fn link_path(old: &[u8], new: &[u8]) -> Result<(), FsError> {
    let (ob, on) = abs(old)?;
    let (nb, nn) = abs(new)?;
    fs_init::api().link(None, &ob[..on], &nb[..nn])
}

pub(super) fn truncate_path(path: &[u8], size: u64) -> Result<(), FsError> {
    let (b, n) = abs(path)?;
    fs_init::api().truncate(None, &b[..n], size)
}

/// Point `fs_init`'s File API hooks at `write_window` and
/// `open_race`, the only stores to them.
fn install_hooks() {
    // Release: pairs with the Acquire loads in `fs_init::hooks`.
    fs_init::WRITE_WINDOW.store(write_window as fn() as *mut (), Ordering::Release);
    fs_init::OPEN_RACE.store(open_race as fn() -> bool as *mut (), Ordering::Release);
}
