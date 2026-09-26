//! In-guest tests of P10-S07, Address-space and mapping soundness (DESIGN §8.2).

use core::mem::ManuallyDrop;
use core::ptr;
use core::sync::atomic::Ordering;

use vibeos::paging::VirtAddr;
use vibeos::thread::ThreadState;

use super::{Outcome, Test, spawn_thread, spin_until_ns, test};
use crate::sync_init::Semaphore;
use crate::{addr_space_init, arch, paging_init, per_cpu_init, thread_init, time_init, x86};

pub(super) const TESTS: &[Test] = &[
    test("current_mapper_holds_pt", current_mapper_holds_pt),
    test("teardown_live_root_asserts", teardown_live_root_asserts),
];

/// Yield until `pred` holds or `ms` pass.
fn wait_ms(pred: impl Fn() -> bool, ms: u64) -> bool {
    let t0 = time_init::now_ns();
    while !pred() {
        if time_init::now_ns().saturating_sub(t0) > ms.saturating_mul(1_000_000) {
            return false;
        }
        thread_init::yield_now();
    }
    true
}

// ---------------------------------------------------------------------------
// current_mapper_holds_pt (ROADMAP §10.3, F018)

/// PT is held while a `current_mapper` guard lives and free once it drops.
/// Another CPU may take PT briefly after the drop, so the free check polls.
fn current_mapper_holds_pt() -> Outcome {
    let g = paging_init::current_mapper();
    let held = !paging_init::pt_lock_free();
    let walks = g
        .translate(VirtAddr(current_mapper_holds_pt as *const () as u64))
        .is_some();
    drop(g);
    if !held {
        return Outcome::Fail("PT free while a MapperGuard lives");
    }
    if !walks {
        return Outcome::Fail("guard's mapper does not translate kernel text");
    }
    if !spin_until_ns(paging_init::pt_lock_free, 100_000_000) {
        return Outcome::Fail("PT still held after drop");
    }
    Outcome::Ok
}

// ---------------------------------------------------------------------------
// teardown_live_root_asserts (ROADMAP §10.3, F018, F106)

/// The parked thread waits here until the test has cleared its `as_cr3`.
static PARK: Semaphore = Semaphore::new(0);

fn parked() {
    PARK.acquire();
}

/// `teardown` of a root that a parked TCB's `as_cr3` still names hits its
/// assertion (invariant I128) instead of freeing the tables.
fn teardown_live_root_asserts() -> Outcome {
    let Some(space) = addr_space_init::create() else {
        return Outcome::Fail("create");
    };
    let root = space.root().as_u64();
    let th = spawn_thread("s07-park", parked);
    let id = th.id();
    if !wait_ms(
        || {
            matches!(
                thread_init::try_state(id),
                Some(ThreadState::Blocked { .. })
            )
        },
        1_000,
    ) {
        PARK.release();
        addr_space_init::teardown(space);
        return Outcome::Fail("parked thread did not block");
    }
    thread_init::set_pid_cr3(id, 0, root);
    let keep = ManuallyDrop::new(space);
    let nest0 = per_cpu_init::irq_nest();
    let if0 = x86::interrupts_enabled();
    let hit = arch::catch::catch_panic(|| {
        // SAFETY: the assertion is `teardown`'s first act, so on a hit this
        // copy dies unused in the frame the longjmp abandons; on a miss
        // `keep` is never used again. Exactly one copy frees; established
        // here.
        let dup = unsafe { ptr::read(&*keep) };
        addr_space_init::teardown(dup);
    });
    per_cpu_init::current()
        .irq_nest
        .store(nest0, Ordering::Relaxed);
    thread_init::set_pid_cr3(id, 0, 0);
    PARK.release();
    let died = wait_ms(
        || matches!(thread_init::try_state(id), None | Some(ThreadState::Dead)),
        2_000,
    );
    if !hit {
        return Outcome::Fail("teardown freed a root a parked TCB names");
    }
    addr_space_init::teardown(ManuallyDrop::into_inner(keep));
    if x86::interrupts_enabled() != if0 {
        return Outcome::Fail("IF changed across the caught assertion");
    }
    if !died {
        return Outcome::Fail("parked thread did not exit");
    }
    Outcome::Ok
}
