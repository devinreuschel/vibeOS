//! In-guest tests of the ktest registry itself: its rows, its failure
//! messages and its helpers. Rows: the list in crate::ktest.

use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};

use vibeos::paging::PAGE_SIZE_4K;
use vibeos::thread::ThreadState;

use crate::ktest::{
    FAIL_MSG_BYTES, FailMsg, Outcome, SUITES, TESTS, alloc_frames, cpu_remote, dealloc_frames,
    service_incoming_guarded, spawn_thread, spawn_thread_on, test,
};
use crate::per_cpu_init;
use crate::thread_init::{self, ThreadHandle};
use crate::time_init;

pub(crate) fn test_ktest_rows() -> Outcome {
    let mut seen = 0usize;
    for (si, suite) in SUITES.iter().enumerate() {
        for (ri, t) in suite.iter().enumerate() {
            seen += 1;
            if t.deadline_ms == 0 {
                return crate::fail_fmt!("zero deadline on {}", t.name);
            }
            for (sj, other) in SUITES.iter().enumerate().skip(si) {
                let from = if sj == si { ri + 1 } else { 0 };
                if other[from..].iter().any(|o| o.name == t.name) {
                    return crate::fail_fmt!("duplicate test name {}", t.name);
                }
            }
        }
    }
    if seen < TESTS.len() {
        return Outcome::Fail("SUITES does not hold the legacy list");
    }
    let d = test("d", test_ktest_rows);
    if d.deadline_ms != 10_000 || d.once || d.opt_in {
        return Outcome::Fail("test() defaults");
    }
    let b = d.deadline(20_000).once().opt_in();
    if b.deadline_ms != 20_000 || !b.once || !b.opt_in {
        return Outcome::Fail("builder did not set deadline/once/opt_in");
    }
    Outcome::Ok
}

struct FailingDisplay;

impl fmt::Display for FailingDisplay {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        Err(fmt::Error)
    }
}

pub(crate) fn test_ktest_fail_fmt() -> Outcome {
    let m = FailMsg::from_args(format_args!("n={}", 7));
    if m.as_str() != "n=7" {
        return Outcome::Fail("n={} did not round-trip");
    }
    // 199 spaces then `x`: 200 formatted bytes.
    let m = FailMsg::from_args(format_args!("{:>200}", "x"));
    if m.as_str().len() != FAIL_MSG_BYTES || m.as_str().bytes().any(|c| c != b' ') {
        return Outcome::Fail("200 bytes did not cut to 120");
    }
    // 119 `x` then a 2-byte `é`: the character goes whole.
    let m = FailMsg::from_args(format_args!("{:x>119}{}", "", 'é'));
    if m.as_str().len() != 119 || m.as_str().bytes().any(|c| c != b'x') {
        return Outcome::Fail("split a character at the cut");
    }
    let m = FailMsg::from_args(format_args!("a{}", FailingDisplay));
    if m.as_str() != "a <fmt error>" {
        return Outcome::Fail("Display error not marked");
    }
    match crate::fail_fmt!("id {}", 3) {
        Outcome::FailFmt(m) if m.as_str() == "id 3" => Outcome::Ok,
        _ => Outcome::Fail("fail_fmt! did not build FailFmt"),
    }
}

static HELPER_RAN: AtomicBool = AtomicBool::new(false);

fn helper_entry() {
    HELPER_RAN.store(true, Ordering::SeqCst);
}

/// Wait up to 1 s for `helper_entry` to run in `h` and `h` to die. The
/// thread may land on this CPU, so yield as well as serve IPIs.
fn helper_ran_and_died(h: ThreadHandle) -> bool {
    let t0 = time_init::now_ns();
    loop {
        let dead = matches!(
            thread_init::try_state(h.id()),
            Some(ThreadState::Dead) | None
        );
        if HELPER_RAN.load(Ordering::SeqCst) && dead {
            return true;
        }
        if time_init::now_ns().saturating_sub(t0) > 1_000_000_000 {
            return false;
        }
        thread_init::yield_now();
        service_incoming_guarded();
        core::hint::spin_loop();
    }
}

pub(crate) fn test_ktest_helpers() -> Outcome {
    let Some(pa) = alloc_frames(2) else {
        return Outcome::Fail("alloc_frames(2)");
    };
    let aligned = pa.as_u64() % (4 * PAGE_SIZE_4K) == 0;
    // SAFETY: `pa` is the order-2 block `alloc_frames(2)` returned above,
    // freed once; established here.
    unsafe { dealloc_frames(pa, 2) };
    if !aligned {
        return Outcome::Fail("order-2 block not 16 KiB aligned");
    }
    if cpu_remote(0).is_none() {
        return Outcome::Fail("cpu_remote(0) is None");
    }
    if cpu_remote(per_cpu_init::cpu_count() as u32).is_some() {
        return Outcome::Fail("cpu_remote(cpu_count) is Some");
    }
    HELPER_RAN.store(false, Ordering::SeqCst);
    if !helper_ran_and_died(spawn_thread("ktest-helper", helper_entry)) {
        return Outcome::Fail("spawn_thread entry did not run and exit");
    }
    HELPER_RAN.store(false, Ordering::SeqCst);
    if !helper_ran_and_died(spawn_thread_on("ktest-helper0", helper_entry, 0)) {
        return Outcome::Fail("spawn_thread_on(0) entry did not run and exit");
    }
    Outcome::Ok
}
