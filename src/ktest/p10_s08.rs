//! In-guest tests of P10-S08, Stack reclaim on the owning CPU, fallible spawn, frame baselines (DESIGN §8.2).

use vibeos::pmm::{Frames, MAX_ORDER};
use vibeos::proc::wait_exited;

use super::user::{self, DEFAULT, Image, user_code};
use super::{Outcome, Test, test};
use crate::pmm_init;
use crate::thread_init::{self, SpawnError};
use crate::x86;

pub(super) const TESTS: &[Test] = &[
    test("spawn_stack_oom", spawn_stack_oom).deadline(10_000),
    test("fork_oom", fork_oom).deadline(10_000),
];

/// The free-frame count at a quiescent point (ROADMAP §10.2, F074): the
/// shared warm-up has run (once per boot, from whichever caller comes
/// first), no thread but the caller and the idle threads is runnable, and
/// no dead thread's stack is still on its way back. Every frame-accounting
/// test takes its `before` and `after` from here.
pub(crate) fn quiescent_free_frames() -> usize {
    super::quiesce_frames();
    if !quiesce() {
        crate::marker!("vibeOS: ktest:   quiesce: threads did not settle");
    }
    super::free_frames()
}

/// Wait, bounded, until no thread but this one and the idle threads is
/// Ready or Running, then reclaim the stacks dead threads left. False if
/// the threads did not settle in time.
fn quiesce() -> bool {
    let settled = super::settle_threads();
    thread_init::reap_zombies();
    settled
}

fn dying_entry() {}

/// Most blocks [`spawn_stack_oom`] holds while the buddy is drained.
const OOM_HOLD: usize = 96;
/// One default kernel stack's frames: fewer than this left, a spawn fails.
const STACK_FRAMES: usize = vibeos::kva::DEFAULT_STACK_PAGES;

fn buddy_free() -> usize {
    pmm_init::with_buddy(|b| b.stats().free_frames)
}

/// Take blocks from the buddy, highest order first, until fewer than
/// [`STACK_FRAMES`] frames are free. False if `held` filled first.
fn drain_buddy(held: &mut [Option<Frames>; OOM_HOLD]) -> bool {
    let mut n = 0usize;
    let mut order = MAX_ORDER as u8;
    loop {
        if buddy_free() < STACK_FRAMES {
            return true;
        }
        match pmm_init::with_buddy(|b| b.alloc(order)) {
            Some(f) => {
                let Some(slot) = held.get_mut(n) else {
                    pmm_init::with_buddy(|b| b.free(f));
                    return false;
                };
                *slot = Some(f);
                n += 1;
            }
            None if order == 0 => return true,
            None => order -= 1,
        }
    }
}

fn release_buddy(held: &mut [Option<Frames>; OOM_HOLD]) {
    pmm_init::with_buddy(|b| {
        for slot in held.iter_mut() {
            if let Some(f) = slot.take() {
                b.free(f);
            }
        }
    });
}

/// With fewer than one kernel stack's frames free, a kernel-thread spawn
/// returns `SpawnError::NoMemory` and the kernel stays up (ROADMAP §10.10,
/// F010).
fn spawn_stack_oom() -> Outcome {
    let base = quiescent_free_frames();
    let mut held: [Option<Frames>; OOM_HOLD] = [const { None }; OOM_HOLD];
    // IF off on this CPU keeps the drained window short.
    let (drained, r) = {
        let _g = x86::InterruptGuard::enter();
        let drained = drain_buddy(&mut held);
        let r = if drained {
            Some(thread_init::spawn("oom", dying_entry))
        } else {
            None
        };
        release_buddy(&mut held);
        (drained, r)
    };
    if !drained {
        return Outcome::Fail("buddy not drained: hold array full");
    }
    match r {
        Some(Err(SpawnError::NoMemory)) => {}
        Some(Err(e)) => return crate::fail_fmt!("spawn: {}", e.as_str()),
        Some(Ok(_)) | None => return Outcome::Fail("spawn succeeded with no free stack frames"),
    }
    let after = quiescent_free_frames();
    if after != base {
        return crate::fail_fmt!("frames {base} -> {after}");
    }
    Outcome::Ok
}

// fork(): exit 0 when it returns -ENOMEM, 1 when it returns a pid; a
// child exits 2.
user_code!(
    FORK_ENOMEM,
    "
    mov eax, 57
    syscall
    mov edi, 2
    test rax, rax
    jz 1f
    xor edi, edi
    cmp rax, -12
    je 1f
    mov edi, 1
1:
    mov eax, 60
    syscall
    ud2
    "
);

/// Run [`FORK_ENOMEM`] with the next fork's kernel stack failing.
fn fork_armed() -> Result<(), Outcome> {
    thread_init::testing::fail_next_fork_stack();
    let st = match user::run(&Image::Code(FORK_ENOMEM, DEFAULT), &["fork-oom"]) {
        Ok(st) => st,
        Err(e) => return Err(crate::fail_fmt!("spawn: {}", e.as_str())),
    };
    if st != wait_exited(0) {
        return Err(crate::fail_fmt!(
            "status {st:#x}, want exited 0 (fork gave ENOMEM)"
        ));
    }
    Ok(())
}

/// A `fork` whose kernel-stack allocation fails, after `clone_full` has
/// run, returns `ENOMEM` and frees what it took (ROADMAP §10.10, F010).
fn fork_oom() -> Outcome {
    // The first run warms what a process start maps for good.
    if let Err(o) = fork_armed() {
        return o;
    }
    let base = quiescent_free_frames();
    if let Err(o) = fork_armed() {
        return o;
    }
    let after = quiescent_free_frames();
    if after != base {
        return crate::fail_fmt!("frames {base} -> {after}");
    }
    Outcome::Ok
}
