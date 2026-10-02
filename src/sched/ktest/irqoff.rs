//! In-guest tests of the IF-off tracer (`irqoff` and kernel_tests only).
//! Rows: the parent `ktest.rs`'s `TESTS`.

use vibeos::arch::{CycleCounter, InterruptMask};

use crate::arch::current::{Arch, InterruptGuard};
use crate::ktest::Outcome;
use crate::sched::irqoff::{self, BOUND_NS, Site, testing};

/// Cycles in twice the bound, or `None` before the counter is measured.
fn twice_bound_cycles() -> Option<u64> {
    let freq = Arch::freq_hz()?;
    let c = u128::from(2 * BOUND_NS) * u128::from(freq) / 1_000_000_000;
    u64::try_from(c).ok()
}

/// Spin for `cycles` of the cycle counter.
fn spin(cycles: u64) {
    let t0 = Arch::now();
    while Arch::now().wrapping_sub(t0) < cycles {
        core::hint::spin_loop();
    }
}

/// Whether `site` names `line` of this file.
fn is_line(site: Site, line: u32) -> bool {
    site.location()
        .is_some_and(|l| l.file() == file!() && l.line() == line)
}

/// A plain `InterruptGuard` held for twice the bound is one over-bound
/// stretch whose site is the guard's own `file:line` (ROADMAP §10.3).
pub(crate) fn irqoff_logs_long_stretch() -> Outcome {
    let Some(cycles) = twice_bound_cycles() else {
        return Outcome::Skip("cycle counter not measured");
    };
    if !Arch::enabled() {
        return Outcome::Fail("registry runs with IF off");
    }
    let cap = testing::capture();
    let (g, line) = (InterruptGuard::enter(), line!());
    spin(cycles);
    drop(g);
    let last = testing::last();
    drop(cap);
    match last {
        None => Outcome::Fail("no stretch captured"),
        Some((_, site, _)) if !is_line(site, line) => {
            crate::fail_fmt!("longest stretch at {site}, not line {line}")
        }
        Some((_, _, true)) => Outcome::Fail("stretch marked deliberate"),
        Some((ns, _, false)) if ns <= BOUND_NS => {
            crate::fail_fmt!("stretch {ns} ns, not over {BOUND_NS}")
        }
        Some(_) => Outcome::Ok,
    }
}

/// A report writes its lines to serial outside the log ring, so the
/// reports the reporter thread prints every 100 ms cannot push the boot
/// lines out of it (`log_boot_captured`): a report that has a moved site
/// to print leaves no `site` line in the ring.
pub(crate) fn irqoff_report_skips_ring() -> Outcome {
    if !Arch::enabled() {
        return Outcome::Fail("registry runs with IF off");
    }
    // A stretch at this line's site, so the report below has a line.
    drop(InterruptGuard::enter());
    irqoff::report();
    if crate::log_init::contains_msg("irqoff: site ") {
        return Outcome::Fail("an irqoff report line is in the log ring");
    }
    Outcome::Ok
}

/// The same hold under `irqoff::deliberate` is marked deliberate, so never
/// over the bound (C-IRQOFF-GUARD).
pub(crate) fn irqoff_deliberate_is_exempt() -> Outcome {
    let Some(cycles) = twice_bound_cycles() else {
        return Outcome::Skip("cycle counter not measured");
    };
    if !Arch::enabled() {
        return Outcome::Fail("registry runs with IF off");
    }
    let cap = testing::capture();
    let (g, line) = (irqoff::deliberate("irqoff test hold"), line!());
    spin(cycles);
    drop(g);
    let last = testing::last();
    drop(cap);
    match last {
        None => Outcome::Fail("no stretch captured"),
        Some((_, site, _)) if !is_line(site, line) => {
            crate::fail_fmt!("longest stretch at {site}, not line {line}")
        }
        Some((_, _, false)) => Outcome::Fail("stretch not marked deliberate"),
        Some((ns, _, true)) if ns <= BOUND_NS => {
            crate::fail_fmt!("stretch {ns} ns, not over {BOUND_NS}")
        }
        Some(_) => Outcome::Ok,
    }
}
