//! In-guest tests of P10-S07, Address-space and mapping soundness (DESIGN §8.2).

use core::mem::ManuallyDrop;
use core::ptr;
use core::sync::atomic::Ordering;

use vibeos::kva::{KVA_END, KVA_START, PAGE_SIZE};
use vibeos::paging::VirtAddr;
use vibeos::thread::ThreadState;

use super::{
    Outcome, Test, alloc_frames_owned, free_frames_owned, settle_threads, spawn_thread,
    spin_until_ns, test,
};
use crate::sync_init::Semaphore;
use crate::{
    addr_space_init, arch, kva_init, paging_init, per_cpu_init, thread_init, time_init, x86,
};

pub(super) const TESTS: &[Test] = &[
    test("current_mapper_holds_pt", current_mapper_holds_pt),
    test("teardown_live_root_asserts", teardown_live_root_asserts),
    test("vmap_32_frames_unmapped", vmap_32_frames_unmapped),
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

// ---------------------------------------------------------------------------
// vmap_32_frames_unmapped (ROADMAP §10.3, F018, F107)

/// Frames in the test's block: `MAX_UNMAP`, the most `vmap` takes.
const VMAP_ORDER: u8 = 5;
const VMAP_PAGES: u64 = 1 << VMAP_ORDER;

/// A 32-frame `vmap` maps the handle's own frames, and `vunmap` unmaps all
/// 32 pages and returns exactly the span it mapped to the KVA free list.
fn vmap_32_frames_unmapped() -> Outcome {
    // No other thread runs and no dead stack waits to be freed, so the KVA
    // use below moves only for this test's span.
    settle_threads();
    let Some(f) = alloc_frames_owned(VMAP_ORDER) else {
        return Outcome::Fail("no order-5 block");
    };
    let used0 = kva_init::stats().used;
    let v = match kva_init::vmap(f) {
        Ok(v) => v,
        Err(_) => return Outcome::Fail("vmap"),
    };
    let base = v.base().as_u64();
    if v.len() != VMAP_PAGES * PAGE_SIZE {
        free_frames_owned(kva_init::vunmap(v));
        return Outcome::Fail("len is not 32 pages");
    }
    if base < KVA_START || base.saturating_add(v.len()) > KVA_END {
        free_frames_owned(kva_init::vunmap(v));
        return Outcome::Fail("base outside the KVA window");
    }
    let mut i = 0u64;
    while i < VMAP_PAGES {
        // SAFETY: page `i` of the span `vmap` just mapped writable, which
        // nothing else uses until `vunmap` below; established here.
        unsafe { ((base + i * PAGE_SIZE) as *mut u64).write_volatile(i) };
        i += 1;
    }
    let frames = kva_init::vunmap(v);
    let mut still = 0u64;
    let mut wrong = 0u64;
    let mut i = 0u64;
    while i < VMAP_PAGES {
        if paging_init::translate(VirtAddr(base + i * PAGE_SIZE)).is_some() {
            still += 1;
        }
        let hhdm = paging_init::HHDM_BASE + frames.base() + i * PAGE_SIZE;
        // SAFETY: frame `i` of the block this test holds, read through the
        // physmap, which covers all RAM (DESIGN §4.1); established here.
        if unsafe { (hhdm as *const u64).read_volatile() } != i {
            wrong += 1;
        }
        i += 1;
    }
    let used1 = kva_init::stats().used;
    free_frames_owned(frames);
    if still != 0 {
        return crate::fail_fmt!("{still} of 32 pages still mapped after vunmap");
    }
    if wrong != 0 {
        return crate::fail_fmt!("{wrong} frames did not hold what their page wrote");
    }
    if used1 != used0 {
        return crate::fail_fmt!("kva used {used0} before vmap, {used1} after vunmap");
    }
    Outcome::Ok
}
