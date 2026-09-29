//! How a CPU's worker frees its dead-stack list (kernel_tests only),
//! re-exported from `sched::ktest`.

use vibeos::ipi::SHOOT_RANGES;
use vibeos::kva::DEFAULT_STACK_PAGES;

use super::WAIT_NS;
use crate::ktest::Outcome;
use crate::kva_init;
use crate::per_cpu_init;
use crate::thread_init;
use crate::time_init;
use crate::x86;

/// Stacks [`dead_list_batched_rounds`] parks on CPU 0's dead list.
const PARKED_STACKS: usize = 64;

/// A worker frees its CPU's dead list with one shootdown round per
/// `vibeos::ipi::SHOOT_RANGES` stacks, not one per page: each round waits for
/// every other CPU's ack while an exit sends none, so per-page rounds let a
/// burst of exits outrun the worker, and the dead stacks, their frames and
/// fresh KVA pile up (`lifetime_stack_reclaim`). 64 default stacks parked
/// at once on CPU 0 are freed in at most 4 rounds.
pub(crate) fn dead_list_batched_rounds() -> Outcome {
    if per_cpu_init::online_mask().count_ones() < 2 {
        return Outcome::Skip("needs 2 cpus");
    }
    if thread_init::current_cpu() != 0 {
        return Outcome::Fail("registry not on cpu0");
    }
    if !crate::ktest::quiesce() {
        return Outcome::Fail("threads did not settle");
    }
    let mut stacks: [Option<kva_init::GuardedStack>; PARKED_STACKS] =
        [const { None }; PARKED_STACKS];
    for slot in stacks.iter_mut() {
        match kva_init::alloc_guarded_stack(DEFAULT_STACK_PAGES) {
            Ok(s) => *slot = Some(s),
            Err(_) => {
                for s in stacks.iter_mut().filter_map(Option::take) {
                    kva_init::free_stack(s);
                }
                return Outcome::Fail("stack alloc failed");
            }
        }
    }
    let r0 = crate::irq::ktest::rounds_sent();
    {
        // IF off: this CPU's worker cannot run until every stack is on
        // the list, so it takes all of them at once.
        let _g = x86::InterruptGuard::enter();
        for s in stacks.iter_mut().filter_map(Option::take) {
            thread_init::testing::park_on_local_list(s);
        }
    }
    let t0 = time_init::now_ns();
    while thread_init::stacks_in_flight() != 0 {
        if time_init::now_ns().saturating_sub(t0) > WAIT_NS {
            return Outcome::Fail("parked stacks not freed");
        }
        thread_init::yield_now();
    }
    let rounds = crate::irq::ktest::rounds_sent().wrapping_sub(r0);
    let most = PARKED_STACKS.div_ceil(SHOOT_RANGES) as u64;
    if rounds > most {
        return crate::fail_fmt!(
            "{rounds} shootdown rounds for {PARKED_STACKS} stacks, want <= {most}"
        );
    }
    Outcome::Ok
}
