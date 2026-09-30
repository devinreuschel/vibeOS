//! The `hang_test` build's hang (ROADMAP §10.7): once `smp: done`, one CPU
//! prints `vibeOS: hang_test: armed`, holds a spinlock the others then wait
//! on, and spins with interrupts off, so every CPU hangs and
//! `make test-forensics` takes a core of a real hang and checks the core
//! tool's report on it, the way the panic e2e checks the panic path.
//! Test-only (AGENTS.md rule 9): the `hang_test` feature, which no published
//! ISO enables.
//!
//! [`arm`], [`hold`] and [`wait`] never inline, so the report's signature
//! is `sig: timeout @ …hang_test::hold < …hang_test::arm < …boot_rest` in
//! every build, and each waiter's backtrace lists `hang_test::wait`.

use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::lock::RANK_DEVICE;
use vibeos::log::Level;

use crate::per_cpu_init;
use crate::sync_init::SpinMutex;
use crate::thread_init;

/// The lock the holder takes and the waiters spin on. Device rank, below
/// the serial lock, so the holder still prints its marker under it (as
/// `panic_test`'s held lock is).
static HANG: SpinMutex<()> = SpinMutex::with_rank((), RANK_DEVICE);

/// Set (Release) once the holder has `HANG`; each waiter reads it
/// (Acquire) before it takes the lock.
static ARMED: AtomicBool = AtomicBool::new(false);

/// Never set. `hold` spins until it is, an exit the compiler cannot rule
/// out, so `arm` drops its guard after the call: the call stays a call,
/// never a tail jump, and `arm`'s frame stays in the backtrace.
static RELEASE: AtomicBool = AtomicBool::new(false);

/// Spawn one waiter pinned to each other online CPU, take `HANG` (IF=0 from
/// here on), publish `ARMED`, print the armed marker and hold the lock for
/// good: it never returns. Called once, on the boot thread, after
/// `smp: done`; a boot with one CPU arms too.
#[inline(never)]
pub(crate) fn arm() {
    let online = per_cpu_init::online_mask();
    for cpu in 1..64u32 {
        if online & (1u64 << cpu) == 0 {
            continue;
        }
        // A waiter that cannot spawn leaves its CPU idle, and the tier
        // fails on the missing `hang_test::wait` frame.
        if let Err(e) = thread_init::spawn_on("hang-wait", wait, cpu) {
            crate::klog!(
                Level::Warn,
                "vibeOS: hang_test: spawn on cpu {} failed: {}",
                cpu,
                e.as_str()
            );
        }
    }
    // After the spawns, which take the heap and SCHED locks, ranked below
    // `HANG`; held for good, since `hold` never returns.
    let _held = HANG.lock();
    // Release: pairs with the Acquire load in `wait`.
    ARMED.store(true, Ordering::Release);
    crate::marker!(vibeos::marker::HANG_TEST_ARMED);
    hold();
}

/// Spin with `HANG` held and interrupts off, forever.
#[inline(never)]
fn hold() {
    // Relaxed: nothing is published through `RELEASE`, which no code sets.
    while !RELEASE.load(Ordering::Relaxed) {
        core::hint::spin_loop();
    }
}

/// A waiter: once the holder is armed, take `HANG`, which is never
/// released, so the lock spins with interrupts off for good.
#[inline(never)]
fn wait() {
    // Acquire: pairs with the Release store in `arm`.
    while !ARMED.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    let _never = HANG.lock();
}
