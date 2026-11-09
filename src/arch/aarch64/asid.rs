//! aarch64 ASIDs: the kernel half of `vibeos::mm::asid` (ROADMAP §11.2,
//! PORTABILITY §11.2). Every TTBR0 load goes through [`load_ttbr0`].
//!
//! With ASIDs on, a user root is loaded with its space's ASID in TTBR0's
//! bits 63:48 (`TCR_EL1.A1` clear) and no invalidate: the allocator keeps
//! each ASID naming one space on each CPU between that CPU's flushes
//! (invariant I39), and a CPU a rollover marked runs `tlbi vmalle1`,
//! `dsb nsh`, `isb` before it loads an ASID of the new generation. ASID 0 is
//! the empty user root's, and a root with no context (a test's) also loads
//! as ASID 0, followed by a local invalidate of ASID 0 whenever another
//! ASID-0 root was loaded since the last one. With ASIDs off every TTBR0
//! load is ASID 0 and is followed by that invalidate.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};

use vibeos::kalloc::TryBox;
use vibeos::log::Level;
use vibeos::machine::MAX_CPUS;
use vibeos::mm::asid::{self, AsidAlloc};

use super::cpu;
use crate::cell::BootCell;

/// TTBR0's ASID field (`TCR_EL1.A1` clear).
const ASID_SHIFT: u64 = 48;

/// The in-guest test the `kernel_tests` cap serves: a boot whose
/// `vibeos.ktest=` selects it runs with [`asid::TEST_CAP_BITS`]-bit ASIDs.
#[cfg(feature = "kernel_tests")]
pub(crate) const CAP_TEST: &str = "asid_procs_4bit";

/// The allocator, set once on the boot CPU before any user root loads and
/// before `smp: done`; unset means ASIDs are off.
static ALLOC: BootCell<TryBox<AsidAlloc>> = BootCell::new();

/// Per CPU: an ASID-0 root other than the empty one was loaded since this
/// CPU last invalidated ASID 0.
static ASID0_DIRTY: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// A space's ASID word, generation above ASID (`vibeos::asid::AsidWord`),
/// 0 until its first load. Every CPU that runs the space shares it.
pub struct SpaceCtx {
    word: AtomicU64,
}

impl SpaceCtx {
    pub const fn new() -> Self {
        Self {
            word: AtomicU64::new(0),
        }
    }

    /// Drop this space's translation of user VA `va` from every CPU's TLB,
    /// by the space's own ASID (Linux arm64's `ASID(mm)`), whether or not
    /// this CPU has it loaded. With no ASID yet (or ASIDs off) its entries
    /// can only be ASID 0 on a CPU that has it loaded (`loaded` here).
    ///
    /// The fence orders the read of the word after the caller's PTE store:
    /// either this read sees the ASID a concurrent switch-in stores, or that
    /// switch's walk, after the `dsb ishst` in [`load_ttbr0`], sees the new
    /// PTE. A space keeps its number while any CPU has it loaded (the
    /// rollover reserves it), so the number read is the one its entries carry.
    pub fn flush_page(&self, va: u64, loaded: bool) {
        let asid = match ALLOC.try_get() {
            Some(alloc) => {
                // SeqCst: `dmb ish`, the PTE store before the word's load;
                // pairs with the `dsb ishst` before `load_ttbr0`'s TTBR0 write.
                fence(Ordering::SeqCst);
                // Relaxed: pairs with nothing; the fence above orders it.
                alloc.asid_of(self.word.load(Ordering::Relaxed))
            }
            None => 0,
        };
        if asid != 0 || loaded {
            cpu::tlbi_user_va(va, asid as u16);
        }
    }
}

/// The ASID width this boot's allocator uses, or `None` with ASIDs off.
#[cfg(feature = "kernel_tests")]
pub(crate) fn bits() -> Option<u32> {
    ALLOC.try_get().map(|a| a.bits())
}

/// The allocator's generation, shifted above its ASID bits; 0 with ASIDs
/// off. A test counts rollovers by its change.
#[cfg(feature = "kernel_tests")]
pub(crate) fn generation() -> u64 {
    ALLOC.try_get().map_or(0, |a| a.generation())
}

/// The `kernel_tests` cap: [`asid::TEST_CAP_BITS`] when the boot's
/// selection runs [`CAP_TEST`].
fn test_cap() -> Option<u32> {
    #[cfg(feature = "kernel_tests")]
    {
        let sel = vibeos::ktest::Selection::parse(crate::boot::cmdline().get("vibeos.ktest"));
        if sel.selects(CAP_TEST, false) {
            return Some(asid::TEST_CAP_BITS);
        }
    }
    None
}

/// Build the allocator and log its width, or why ASIDs are off, in which
/// case every TTBR0 switch is followed by a local ASID-0 flush. Boot CPU,
/// once, after `per_cpu_init::init_bsp` and with TTBR0 on the empty root,
/// before any user root loads.
///
/// # Safety
/// Single-CPU, before `smp: done` (`BootCell::set`'s contract).
pub unsafe fn init_bsp() {
    let asid16 = cpu::asid16();
    let cap = test_cap();
    let bits = asid::width(asid16, cap);
    let ncpus = crate::per_cpu_init::cpu_count() as u32;
    let usable = (1u32 << bits) - 1;
    match AsidAlloc::try_boxed(bits, ncpus) {
        Some(a) => {
            // SAFETY: invariant I22: one write before `smp: done` with no
            // reader yet (this fn's `# Safety`), BootCell's set contract;
            // established here: the boot CPU runs this once.
            unsafe { ALLOC.set(a) };
            crate::klog!(
                Level::Info,
                "vibeOS: asid: {bits}-bit, {usable} usable for {ncpus} cpus (TCR_EL1.AS={}{})",
                u8::from(asid16),
                if cap.is_some() {
                    ", kernel_tests cap"
                } else {
                    ""
                }
            );
        }
        None if usable <= ncpus => crate::klog!(
            Level::Info,
            "vibeOS: asid: off: {} ({usable} <= {ncpus})",
            asid::DISABLED_REASON
        ),
        None => crate::klog!(
            Level::Warn,
            "vibeOS: asid: off: no memory for the allocator"
        ),
    }
    // Whatever Limine and the TTBR1 takeover left tagged with any ASID goes
    // before the first ASID is handed out; each AP's entry stub does the same.
    cpu::tlbi_local_all();
}

/// Load `root` into this CPU's TTBR0: with its space's ASID when ASIDs are
/// on and `ctx` names the space's [`SpaceCtx`], else as ASID 0. Returns
/// the TTBR0 value written. IRQs masked (the switch, or `with_current`).
///
/// # Safety
/// `root` is the empty user root or a complete user root that stays
/// allocated while loaded (invariant I44). `ctx` is 0 or the address of
/// the `SpaceCtx` of `root`'s live `SpaceCore`, which a TCB names beside
/// `root` or the caller holds (`Tcb.as_ctx`).
pub unsafe fn load_ttbr0(cpu_id: u32, root: u64, ctx: usize) -> u64 {
    let empty = crate::paging_init::empty_user_root();
    let alloc = ALLOC.try_get();
    if let Some(alloc) = alloc
        && ctx != 0
        && root != empty
    {
        let ptr = core::ptr::with_exposed_provenance::<SpaceCtx>(ctx);
        // SAFETY: invariant I44: `ctx` is the `SpaceCtx` of the live space
        // whose root a TCB or the caller names (this fn's `# Safety`), its
        // provenance exposed from the core's allocation, and it is used only
        // as `&` (its word is an atomic); established by
        // `addr_space_init::SpaceCore::ctx_addr`.
        let space = unsafe { &*ptr };
        let w = asid::switch(alloc, cpu_id, &space.word);
        if w != 0 {
            // A rollover since this CPU's last flush: drop every entry
            // before any ASID of the new generation is loaded (I39).
            if alloc.take_flush(cpu_id) {
                cpu::tlbi_local_all();
                #[cfg(feature = "kernel_tests")]
                shadow::flushed(cpu_id);
            }
            #[cfg(feature = "kernel_tests")]
            shadow::loaded(cpu_id, alloc.asid_of(w), ctx);
            let ttbr0 = root | (u64::from(alloc.asid_of(w)) << ASID_SHIFT);
            cpu::dsb_ishst();
            // SAFETY: invariant I44: `root` stays allocated while loaded,
            // and the ASID is one the allocator gave this space (I39);
            // established by `addr_space_init::SpaceCore`'s drop.
            unsafe { cpu::write_ttbr0(ttbr0) };
            return ttbr0;
        }
    }
    // SAFETY: invariant I44: `root` stays allocated while loaded;
    // established by `addr_space_init::SpaceCore`'s drop.
    unsafe { cpu::write_ttbr0(root) };
    let slot = ASID0_DIRTY.get(cpu_id as usize);
    // Relaxed: pairs with nothing; only this CPU touches its slot, with
    // IRQs masked.
    let dirty = slot.is_none_or(|d| d.swap(root != empty, Ordering::Relaxed));
    if alloc.is_none() || dirty || root != empty {
        cpu::tlbi_asid0_local();
    }
    root
}

/// What each CPU's TLB may hold under the low ASIDs, for the in-guest proof
/// of invariant I39 (`asid_procs_4bit`): the space each of ASIDs 0 to 15 has
/// named on that CPU since its last local flush. TCG drops its soft TLB
/// whenever the TTBR0 ASID changes, so a missing rollover flush shows in
/// guest memory only on hardware or HVF; this record shows it on TCG too.
/// A space is its `SpaceCtx` address, which a later space may reuse: that
/// can hide an alias, never invent one.
#[cfg(feature = "kernel_tests")]
mod shadow {
    use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use vibeos::machine::MAX_CPUS;

    const SLOTS: usize = 16;
    static SEEN: [[AtomicUsize; SLOTS]; MAX_CPUS] =
        [const { [const { AtomicUsize::new(0) }; SLOTS] }; MAX_CPUS];
    static ALIASES: AtomicU64 = AtomicU64::new(0);

    /// CPU `cpu` ran `tlbi vmalle1`.
    pub(super) fn flushed(cpu: u32) {
        if let Some(row) = SEEN.get(cpu as usize) {
            for s in row {
                // Relaxed: only CPU `cpu` writes its row, IRQs masked; pairs with nothing.
                s.store(0, Ordering::Relaxed);
            }
        }
    }

    /// CPU `cpu` loads `asid` for the space whose context is `ctx`.
    pub(super) fn loaded(cpu: u32, asid: u32, ctx: usize) {
        let Some(s) = SEEN.get(cpu as usize).and_then(|r| r.get(asid as usize)) else {
            return;
        };
        // Relaxed: only CPU `cpu` writes its row, IRQs masked; pairs with nothing.
        let prev = s.swap(ctx, Ordering::Relaxed);
        if prev != 0 && prev != ctx {
            // Relaxed: a count the test reads after its spawners' Release; pairs with nothing.
            ALIASES.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Loads so far that gave an ASID a second space on one CPU between
    /// that CPU's flushes.
    pub(crate) fn aliases() -> u64 {
        // Relaxed: as `loaded`'s add; pairs with nothing.
        ALIASES.load(Ordering::Relaxed)
    }
}

#[cfg(feature = "kernel_tests")]
pub(crate) use shadow::aliases;
