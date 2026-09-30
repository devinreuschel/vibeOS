//! The IF-off tracer's kernel half (ROADMAP §10.3, INVARIANTS.md §2.9
//! rule 2). In an `irqoff` build it stamps the cycle counter where this
//! CPU's IF goes from 1 to 0 ([`off`]) and back to 1 ([`on`]), and records
//! each closed stretch against the site that turned IF off in
//! `vibeos::sched::irqoff`'s tables. Without the feature every hook is an
//! empty `#[inline(always)]` fn and the build holds no tracer state.
//!
//! The hooks take no lock, `IrqCell`, `InterruptGuard` or log line: they
//! run inside all of those. They run with IF=0 after a real transition, so
//! `arch::cpu_id_hint` is exact (INVARIANTS.md §2.9 rule 5), and never
//! before a CPU's first `sti`, which arms it, so no hook reads `gs:` on an
//! AP before its per-CPU base is loaded.

pub use vibeos::sched::irqoff::Site;

#[cfg(feature = "kernel_tests")]
use crate::arch::current::InterruptGuard;

/// A stretch starts at `site`: IF just went from 1 to 0 on this CPU.
#[inline(always)]
pub fn off(site: Site) {
    #[cfg(feature = "irqoff")]
    tracer::off(site);
    #[cfg(not(feature = "irqoff"))]
    let _ = site;
}

/// [`off`] at the caller's `Location`, for Rust code that ran `cli`.
#[inline(always)]
#[track_caller]
pub fn off_here() {
    #[cfg(feature = "irqoff")]
    tracer::off(Site::caller(core::panic::Location::caller()));
}

/// The open stretch ends: IF is about to go from 0 to 1 on this CPU.
#[inline(always)]
pub fn on() {
    #[cfg(feature = "irqoff")]
    tracer::on();
}

/// [`off`] for the asm stubs: `site` is a vector site (`0x100 | vector`)
/// or a syscall site. Defined in every build, so the stubs' `sym`
/// operands resolve; the stubs call it only in an `irqoff` build.
pub extern "C" fn vibeos_irqoff_off_site(site: u64) {
    off(Site::from_stub(site));
}

/// [`on`] for the asm stubs, as [`vibeos_irqoff_off_site`].
pub extern "C" fn vibeos_irqoff_on() {
    on();
}

/// Time spent while this lives is subtracted from the open stretch: one
/// of the waits rule 2 exempts (the shootdown and call-function waits, a
/// spinlock's spin).
#[must_use]
pub struct ExemptGuard {
    #[cfg(feature = "irqoff")]
    inner: tracer::Exempt,
}

/// Start an exempt window (see [`ExemptGuard`]).
#[inline(always)]
pub fn exempt() -> ExemptGuard {
    ExemptGuard {
        #[cfg(feature = "irqoff")]
        inner: tracer::Exempt::start(),
    }
}

impl Drop for ExemptGuard {
    #[inline(always)]
    fn drop(&mut self) {
        #[cfg(feature = "irqoff")]
        self.inner.end();
    }
}

/// An IF-off stretch held on purpose by a test or a test hook
/// (C-IRQOFF-GUARD): IF stays off while it lives, and the tracer never
/// counts the stretch it is part of as over the bound. `!Send` through
/// its `InterruptGuard`.
#[cfg(feature = "kernel_tests")]
pub struct DeliberateGuard {
    _irq: InterruptGuard,
}

/// Turn IF off (or keep it off) and mark this CPU's open stretch
/// deliberate, with `reason` for its `deliberate` line (C-IRQOFF-GUARD).
#[cfg(feature = "kernel_tests")]
#[track_caller]
pub fn deliberate(reason: &'static str) -> DeliberateGuard {
    let irq = InterruptGuard::enter();
    #[cfg(feature = "irqoff")]
    tracer::mark_deliberate(reason);
    #[cfg(not(feature = "irqoff"))]
    let _ = reason;
    DeliberateGuard { _irq: irq }
}

#[cfg(feature = "irqoff")]
mod tracer {
    use vibeos::arch::{CycleCounter, InterruptMask};
    use vibeos::atomic::statics::{AtomicBool, AtomicU64, Ordering};
    use vibeos::sched::irqoff::{SiteTable, Stretch, close};

    use super::Site;
    use crate::arch::current::Arch;
    use crate::serial::raw::HALTING;

    /// CPUs the tracer keeps a slot for.
    const CPUS_MAX: usize = vibeos::acpi::MAX_CPUS;

    /// One CPU's open stretch. Only its own CPU touches it, with IF=0; an
    /// NMI's hooks run only when the NMI interrupted IF=1 code, which is
    /// never inside a hook. Atomics make the array a `static`.
    pub(super) struct CpuTrace {
        armed: AtomicBool,
        /// The open stretch's site bits; 0 when none is open.
        site: AtomicU64,
        start: AtomicU64,
        exempt: AtomicU64,
        deliberate: AtomicBool,
    }

    impl CpuTrace {
        const fn new() -> Self {
            Self {
                armed: AtomicBool::new(false),
                site: AtomicU64::new(0),
                start: AtomicU64::new(0),
                exempt: AtomicU64::new(0),
                deliberate: AtomicBool::new(false),
            }
        }
    }

    pub(super) static CPUS: [CpuTrace; CPUS_MAX] = [const { CpuTrace::new() }; CPUS_MAX];
    /// Every site's totals (`vibeos::sched::irqoff::SiteTable`).
    pub(super) static SITES: SiteTable<512> = SiteTable::new();

    /// This CPU's slot, when the hooks may run: not halting, IF off, and
    /// an id the table covers.
    #[inline(always)]
    fn slot() -> Option<&'static CpuTrace> {
        // Relaxed: a late view only delays the stop by one hook.
        if HALTING.load(Ordering::Relaxed) || Arch::enabled() {
            return None;
        }
        CPUS.get(crate::arch::cpu_id_hint() as usize)
    }

    fn site_of(bits: u64) -> Site {
        // SAFETY: invariant: a `CpuTrace::site` holds 0 or `Site::bits`;
        // established by `sched::irqoff::off` (its tracer body), the only
        // nonzero store.
        unsafe { Site::from_bits(bits) }
    }

    // Relaxed throughout: each `CpuTrace` is its own CPU's, used with IF=0.

    pub(super) fn off(site: Site) {
        let Some(cpu) = slot() else {
            return;
        };
        if !cpu.armed.load(Ordering::Relaxed) {
            return;
        }
        cpu.start.store(Arch::now(), Ordering::Relaxed);
        cpu.exempt.store(0, Ordering::Relaxed);
        cpu.deliberate.store(false, Ordering::Relaxed);
        let prev = cpu.site.swap(site.bits(), Ordering::Relaxed);
        if prev != 0 {
            // IF came back on with no `on` stamp (a `catch` longjmp skips
            // guard drops): drop the old stretch's time, count it.
            if let Some(s) = SITES.get_or_insert(site_of(prev)) {
                s.unmatched.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub(super) fn on() {
        let Some(cpu) = slot() else {
            return;
        };
        if !cpu.armed.swap(true, Ordering::Relaxed) {
            // Bring-up: the first `sti` arms this CPU (rule 2's exemption).
            return;
        }
        let bits = cpu.site.swap(0, Ordering::Relaxed);
        if bits == 0 {
            return;
        }
        let now = Arch::now();
        let Some(freq) = Arch::freq_hz() else {
            return;
        };
        let s = Stretch {
            start: cpu.start.load(Ordering::Relaxed),
            site: site_of(bits),
            exempt: cpu.exempt.load(Ordering::Relaxed),
            deliberate: cpu.deliberate.load(Ordering::Relaxed),
        };
        let c = close(&s, now, freq);
        if let Some(st) = SITES.get_or_insert(s.site) {
            st.record(c, s.deliberate);
        }
    }

    #[cfg(feature = "kernel_tests")]
    pub(super) fn mark_deliberate(reason: &'static str) {
        let Some(cpu) = slot() else {
            return;
        };
        let bits = cpu.site.load(Ordering::Relaxed);
        if bits == 0 {
            return;
        }
        cpu.deliberate.store(true, Ordering::Relaxed);
        if let Some(st) = SITES.get_or_insert(site_of(bits)) {
            st.set_reason(reason);
        }
    }

    /// An exempt window: the CPU and stretch it began in, and the counter
    /// then. `cpu == u32::MAX` when no stretch was open.
    pub(super) struct Exempt {
        cpu: u32,
        start: u64,
        t0: u64,
    }

    impl Exempt {
        pub(super) fn start() -> Self {
            let none = Self {
                cpu: u32::MAX,
                start: 0,
                t0: 0,
            };
            let Some(cpu) = slot() else {
                return none;
            };
            if cpu.site.load(Ordering::Relaxed) == 0 {
                return none;
            }
            Self {
                cpu: crate::arch::cpu_id_hint(),
                start: cpu.start.load(Ordering::Relaxed),
                t0: Arch::now(),
            }
        }

        /// Add the window to the stretch it began in, if that is still
        /// this CPU's open one.
        pub(super) fn end(&self) {
            if self.cpu == u32::MAX {
                return;
            }
            let Some(cpu) = slot() else {
                return;
            };
            if crate::arch::cpu_id_hint() != self.cpu
                || cpu.site.load(Ordering::Relaxed) == 0
                || cpu.start.load(Ordering::Relaxed) != self.start
            {
                return;
            }
            let spent = Arch::now().wrapping_sub(self.t0);
            let e = cpu.exempt.load(Ordering::Relaxed);
            cpu.exempt.store(e.saturating_add(spent), Ordering::Relaxed);
        }
    }
}
