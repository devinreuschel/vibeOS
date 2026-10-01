//! The fill API (ROADMAP §10.6): the one way the kernel writes an address
//! space that no thread runs, the ELF loader's segments, zero fill, TLS and
//! initial stack, and `fork`'s copy until §12.3.
//!
//! Every entry point takes a `&mut NewSpace` or returns one, and only
//! `addr_space_init::create` makes a `NewSpace`, so syscall code, which
//! reaches a running space only as `&Space` through a scoped guard, cannot
//! hand one here; its copies to and from the caller go through the user
//! accessors (`uaccess_init`). The drivers in `vibeos::proc::fill` hold
//! `PT` for one batch's lookup at a time, at most one leaf table, and write
//! the batch's frames through the physmap with `PT` dropped and IF on.

use vibeos::addr_space::{AsError, Backing, UserPerms};
use vibeos::proc::fill::{self, Batch, FillError, PtHold};

use crate::addr_space_init::{self, NewSpace, Space};
use crate::paging_init;

/// `PT`, as the drivers take it: one hold per batch.
struct KernelPt;

impl PtHold for KernelPt {
    fn hold<R>(&mut self, batch: Batch, f: impl FnOnce() -> R) -> R {
        paging_init::with_pt(|_pt| {
            #[cfg(feature = "kernel_tests")]
            testing::note_hold(batch);
            #[cfg(not(feature = "kernel_tests"))]
            let _ = batch;
            f()
        })
    }

    fn filled(&mut self, _batch: Batch) {
        #[cfg(feature = "kernel_tests")]
        testing::note_boundary(crate::arch::current::interrupts_enabled());
    }
}

/// Every entry point runs with IF on, so IF is on between batches
/// (DESIGN §2.9 rule 2): `execve`, `fork`, and the kernel's spawns all call
/// from thread context after `irq: enabled`.
fn assert_if_on() {
    debug_assert!(
        crate::arch::current::interrupts_enabled(),
        "fill: called with IF off"
    );
}

/// Map `[va, va+len)` of `space` with zeroed frames, as one region with
/// `perms`, through `addr_space_init`'s chunked install.
/// Its error is the space's, so `execve` returns what the install says
/// (`ENOMEM` for any, as it always has).
pub fn map(space: &mut NewSpace, va: u64, len: u64, perms: UserPerms) -> Result<(), AsError> {
    assert_if_on();
    // SAFETY: `map_anon` checks the range is in the user half and clear of
    // every region before it maps anything, and maps zeroed buddy frames;
    // established by `addr_space_init::map_anon`.
    unsafe { addr_space_init::map_anon(space, va, len, perms) }
}

/// Zero `[va, va+len)` of `space`, which `map` mapped.
pub fn zero(space: &mut NewSpace, va: u64, len: u64) -> Result<(), FillError> {
    assert_if_on();
    let mm = space.mm();
    fill::zero(&*mm, &mut KernelPt, va, len)
}

/// Write `src` at `va` of `space`, which `map` mapped; read-only pages
/// included.
pub fn write(space: &mut NewSpace, va: u64, src: &[u8]) -> Result<(), FillError> {
    assert_if_on();
    let mm = space.mm();
    fill::write(&*mm, &mut KernelPt, va, src)
}

/// `fork`'s copy of `src`: a new space with its regions, break and bytes
/// in new frames. Takes `src`'s `mm` lock and then the new space's; nothing
/// else reaches an unpublished space, so the pair cannot deadlock. A
/// failure drops the new space, whose last `users` put frees what it holds.
pub fn clone_full(src: &Space) -> Result<NewSpace, FillError> {
    assert_if_on();
    let dst = addr_space_init::create()?;
    let core = dst.core();
    let from = src.mm();
    let mut to = dst.mm();
    fill::clone_layout(&from, &mut to, &core, |to, va, len, perms, core| {
        // SAFETY: `clone_layout` maps each of `src`'s regions once into the
        // new space, where regions never overlap, and `map_anon_mm` maps
        // zeroed buddy frames; established by `addr_space_init::map_anon_mm`.
        unsafe { addr_space_init::map_anon_mm(to, core, va, len, perms) }
    })?;
    for r in from.regions().filter(|r| r.backing == Backing::Anonymous) {
        fill::copy(&from, &to, &mut KernelPt, r.start, r.len)?;
    }
    drop(to);
    drop(from);
    drop(core);
    Ok(dst)
}

/// The fill API's statistics (`kernel_tests` only): how its holds of `PT`
/// went since [`testing::reset`].
#[cfg(feature = "kernel_tests")]
pub(crate) mod testing {
    use core::sync::atomic::{AtomicU64, Ordering};

    use vibeos::proc::fill::{Batch, LEAF_SPAN};

    static HOLDS: AtomicU64 = AtomicU64::new(0);
    static MAX_PAGES: AtomicU64 = AtomicU64::new(0);
    static CROSSED: AtomicU64 = AtomicU64::new(0);
    static IF_OFF: AtomicU64 = AtomicU64::new(0);

    /// What [`stats`] reports.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct Stats {
        /// Holds of `PT` the drivers took.
        pub holds: u64,
        /// The most pages one hold covered.
        pub max_pages: u64,
        /// Holds whose batch crossed a leaf-table boundary.
        pub crossed: u64,
        /// Boundaries between batches that ran with IF off.
        pub if_off: u64,
    }

    pub(super) fn note_hold(b: Batch) {
        HOLDS.fetch_add(1, Ordering::Relaxed);
        MAX_PAGES.fetch_max(b.pages as u64, Ordering::Relaxed);
        let last = b.va + (b.pages.saturating_sub(1) as u64) * vibeos::paging::PAGE_SIZE_4K;
        if b.va / LEAF_SPAN != last / LEAF_SPAN {
            CROSSED.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn note_boundary(if_on: bool) {
        if !if_on {
            IF_OFF.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Zero the counters.
    pub(crate) fn reset() {
        for c in [&HOLDS, &MAX_PAGES, &CROSSED, &IF_OFF] {
            c.store(0, Ordering::Relaxed);
        }
    }

    pub(crate) fn stats() -> Stats {
        Stats {
            holds: HOLDS.load(Ordering::Relaxed),
            max_pages: MAX_PAGES.load(Ordering::Relaxed),
            crossed: CROSSED.load(Ordering::Relaxed),
            if_off: IF_OFF.load(Ordering::Relaxed),
        }
    }
}
