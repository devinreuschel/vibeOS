//! In-guest tests of P10-S23, Flake root cause II and a harness that retries nothing (DESIGN §8.2).

use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use super::{Outcome, Test, alloc_frames_owned, free_frames_owned, spin_until_ns, test};
use crate::kva_init;
use crate::per_cpu_init;
use crate::thread_init;
use crate::time_init;
use crate::x86;

pub(super) const TESTS: &[Test] = &[test("shootdown_ack_while_busy", shootdown_ack_while_busy)];

// ---------------------------------------------------------------------------
// shootdown_ack_while_busy (ROADMAP §10.2, F011, F075)

/// How long the registry stays busy, in ns of `now_ns` time.
const BUSY_NS: u64 = 2_000_000_000;
/// A shootdown cycle this long is the traced `ipi: ack timeout` shape.
const SLOW_NS: u64 = 1_000_000_000;
/// Rounds one shooter runs at most.
const MAX_ROUNDS: u32 = 10_000;

/// Set while the registry dumps the log ring and spins.
static BUSY: AtomicBool = AtomicBool::new(false);
/// Set when the shooters are to stop.
static STOP: AtomicBool = AtomicBool::new(false);
/// Shooters that have started.
static STARTED: AtomicU32 = AtomicU32::new(0);
/// Shooters that have finished.
static DONE: AtomicU32 = AtomicU32::new(0);
/// Shooters that finished a round while `BUSY` was still set.
static OVERLAP: AtomicU32 = AtomicU32::new(0);
/// Shooters that stopped on an allocation or `vmap` error.
static ERRS: AtomicU32 = AtomicU32::new(0);
/// Longest `vmap` plus `vunmap` round any shooter saw, in ns.
static MAX_NS: AtomicU64 = AtomicU64::new(0);

/// One CPU's shooter: `vmap` and `vunmap` one page per round, each of which
/// shoots it down on every other CPU and waits for their acks.
fn shooter() {
    STARTED.fetch_add(1, Ordering::AcqRel);
    let mut overlapped = false;
    let mut rounds = 0u32;
    while !STOP.load(Ordering::Acquire) && rounds < MAX_ROUNDS {
        rounds += 1;
        let Some(f) = alloc_frames_owned(0) else {
            ERRS.fetch_add(1, Ordering::AcqRel);
            break;
        };
        let t0 = time_init::now_ns();
        // A failed `vmap` has already freed its frames (`kva_init::vmap`).
        let Ok(v) = kva_init::vmap(f) else {
            ERRS.fetch_add(1, Ordering::AcqRel);
            break;
        };
        let f = kva_init::vunmap(v);
        let took = time_init::now_ns().saturating_sub(t0);
        free_frames_owned(f);
        MAX_NS.fetch_max(took, Ordering::AcqRel);
        if !overlapped && BUSY.load(Ordering::Acquire) {
            overlapped = true;
            OVERLAP.fetch_add(1, Ordering::AcqRel);
        }
    }
    DONE.fetch_add(1, Ordering::AcqRel);
}

/// ROADMAP §10.2 (F011, F075): the traced `-smp 4` `ipi: ack timeout`.
/// The registry dumps the whole log ring to the console and then spins
/// CPU-bound for 2 s, at the IF it runs at and without polling
/// `service_incoming`, while every other online CPU loops `vmap`/`vunmap`
/// shootdowns. No shootdown cycle may take 1 s.
fn shootdown_ack_while_busy() -> Outcome {
    let me = per_cpu_init::current().cpu_id;
    let others = per_cpu_init::online_mask() & !(1u64 << me);
    if others == 0 {
        return Outcome::Skip("no AP");
    }
    let shooters = others.count_ones();
    BUSY.store(false, Ordering::Release);
    STOP.store(false, Ordering::Release);
    STARTED.store(0, Ordering::Release);
    DONE.store(0, Ordering::Release);
    OVERLAP.store(0, Ordering::Release);
    ERRS.store(0, Ordering::Release);
    MAX_NS.store(0, Ordering::Release);

    BUSY.store(true, Ordering::Release);
    let mut spawned = 0u32;
    let mut mask = others;
    while mask != 0 {
        let cpu = mask.trailing_zeros();
        mask &= mask - 1;
        if thread_init::spawn_on("s23-shooter", shooter, cpu).is_err() {
            break;
        }
        spawned += 1;
    }
    if spawned != shooters {
        BUSY.store(false, Ordering::Release);
        STOP.store(true, Ordering::Release);
        let _ = spin_until_ns(|| DONE.load(Ordering::Acquire) == spawned, 5_000_000_000);
        return crate::fail_fmt!("spawn_on failed after {} of {} shooters", spawned, shooters);
    }
    let started = spin_until_ns(|| STARTED.load(Ordering::Acquire) == shooters, 500_000_000);

    // The traced busy stretch: a full dmesg replay through the console,
    // then CPU-bound work that never polls for IPIs. No guard is held.
    let if_on = x86::interrupts_enabled();
    let t0 = time_init::now_ns();
    let dumped = crate::shell_init::dispatch_line("dmesg trace");
    while time_init::now_ns().saturating_sub(t0) < BUSY_NS {
        spin_loop();
    }
    BUSY.store(false, Ordering::Release);
    STOP.store(true, Ordering::Release);

    let finished = spin_until_ns(|| DONE.load(Ordering::Acquire) == shooters, 5_000_000_000);
    if !finished {
        return Outcome::Fail("shooters unfinished");
    }
    if !started {
        return Outcome::Fail("shooters did not start");
    }
    if dumped.is_err() {
        return Outcome::Fail("dmesg trace");
    }
    if ERRS.load(Ordering::Acquire) > 0 {
        return crate::fail_fmt!("{} shooters hit a vmap error", ERRS.load(Ordering::Acquire));
    }
    let max = MAX_NS.load(Ordering::Acquire);
    if max >= SLOW_NS {
        return crate::fail_fmt!(
            "shootdown waited {} ms, IF {} on cpu{}",
            max / 1_000_000,
            if if_on { "on" } else { "off" },
            me
        );
    }
    if OVERLAP.load(Ordering::Acquire) < shooters {
        return Outcome::Fail("shooter idle during busy window");
    }
    if !super::settle_threads() {
        return Outcome::Fail("shooters did not settle");
    }
    Outcome::Ok
}
