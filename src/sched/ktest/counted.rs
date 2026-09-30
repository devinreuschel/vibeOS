//! In-guest test of counted objects' deferred release (ROADMAP §10.4,
//! DESIGN §2.11 rule 6). Rows: the parent `ktest.rs`'s `TESTS`.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use vibeos::kalloc::TryArc;
use vibeos::lock::RANK_DEVICE;

use crate::ktest::{Outcome, second_cpu, spawn_thread_on, spin_until_ns};
use crate::sync_init::SpinMutex;
use crate::thread_init;
use crate::time_init;
use crate::work_init;

/// Set while the test's spinlock section is open.
static CDR_SECTION_OPEN: AtomicBool = AtomicBool::new(false);
/// The probe's drop ran.
static CDR_RAN: AtomicBool = AtomicBool::new(false);
/// What the probe's drop saw: the section open, IF, and its thread.
static CDR_SAW_OPEN: AtomicBool = AtomicBool::new(false);
static CDR_SAW_IF: AtomicBool = AtomicBool::new(false);
static CDR_SAW_TID: AtomicU32 = AtomicU32::new(u32::MAX);
/// The body thread's result: 0 running, 1 ok, else an index into
/// `CDR_FAILS` plus 2.
static CDR_RESULT: AtomicU32 = AtomicU32::new(0);

const CDR_FAILS: [&str; 8] = [
    "deferred-release list never idle",
    "TryArc allocation failed",
    "ranked section: released while the section was open",
    "ranked section: not released within 2 s",
    "ranked section: released with IF=0",
    "ranked section: released on the dropping thread",
    "rank-0 section: not released within 2 s",
    "rank-0 section: released with IF=0 or on the dropping thread",
];

struct CdrProbe;

impl Drop for CdrProbe {
    fn drop(&mut self) {
        CDR_SAW_OPEN.store(CDR_SECTION_OPEN.load(Ordering::SeqCst), Ordering::SeqCst);
        CDR_SAW_IF.store(crate::arch::current::interrupts_enabled(), Ordering::SeqCst);
        CDR_SAW_TID.store(thread_init::current_id().0, Ordering::SeqCst);
        CDR_RAN.store(true, Ordering::SeqCst);
    }
}

/// Drop the last reference to a fresh probe inside a section of a
/// spinlock of `rank`, then wait up to 2 s for its release. `Err` indexes
/// `CDR_FAILS`.
fn cdr_case(rank: u8) -> Result<(), usize> {
    CDR_RAN.store(false, Ordering::SeqCst);
    CDR_SAW_TID.store(u32::MAX, Ordering::SeqCst);
    let a = TryArc::try_new(CdrProbe).map_err(|_| 1usize)?;
    let b = a.clone();
    drop(a);
    let lock = SpinMutex::with_rank((), rank);
    {
        let _g = lock.lock();
        CDR_SECTION_OPEN.store(true, Ordering::SeqCst);
        drop(b);
        // Give a remote worker time to run anything queued at once.
        let t0 = time_init::now_ns();
        while time_init::now_ns().saturating_sub(t0) < 5_000_000 {
            core::hint::spin_loop();
        }
        CDR_SECTION_OPEN.store(false, Ordering::SeqCst);
    }
    let ranked = rank != 0;
    if !spin_until_ns(|| CDR_RAN.load(Ordering::SeqCst), 2_000_000_000) {
        return Err(if ranked { 3 } else { 6 });
    }
    let me = thread_init::current_id().0;
    let on_worker = CDR_SAW_TID.load(Ordering::SeqCst) != me;
    let if_on = CDR_SAW_IF.load(Ordering::SeqCst);
    if !ranked {
        // An item queued at once may run on a remote worker while the
        // section is still open, so only the context is checked.
        return if if_on && on_worker { Ok(()) } else { Err(7) };
    }
    if CDR_SAW_OPEN.load(Ordering::SeqCst) {
        return Err(2);
    }
    if !if_on {
        return Err(4);
    }
    if !on_worker {
        return Err(5);
    }
    Ok(())
}

fn cdr_body() {
    let cpu = thread_init::current_cpu() as usize;
    let r = if !spin_until_ns(|| work_init::testing::release_idle(cpu), 2_000_000_000) {
        Err(0)
    } else {
        // A: under RANK_DEVICE no release item can be queued before the
        // section ends; this CPU's next tick queues it. B: a rank-0 section
        // queues it at once.
        cdr_case(RANK_DEVICE).and_then(|()| cdr_case(0))
    };
    let code = match r {
        Ok(()) => 1,
        Err(i) => i as u32 + 2,
    };
    CDR_RESULT.store(code, Ordering::SeqCst);
}

pub(crate) fn test_counted_deferred_release() -> Outcome {
    CDR_RESULT.store(0, Ordering::SeqCst);
    let cpu = second_cpu().unwrap_or(0);
    let _h = spawn_thread_on("cdr", cdr_body, cpu);
    if !spin_until_ns(|| CDR_RESULT.load(Ordering::SeqCst) != 0, 8_000_000_000) {
        return Outcome::Fail("counted_deferred_release: body did not finish");
    }
    match CDR_RESULT.load(Ordering::SeqCst) {
        1 => Outcome::Ok,
        c => Outcome::Fail(
            (c as usize)
                .checked_sub(2)
                .and_then(|i| CDR_FAILS.get(i))
                .copied()
                .unwrap_or("counted_deferred_release: bad result"),
        ),
    }
}
