//! In-guest tests of P10-S08, Stack reclaim on the owning CPU, fallible spawn, frame baselines (DESIGN §8.2).

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::pmm::{Frames, MAX_ORDER};
use vibeos::proc::wait_exited;
use vibeos::thread::{ThreadId, ThreadState};

use super::user::{self, DEFAULT, Image, user_code};
use super::{Outcome, Test, test};
use crate::ipi_init;
use crate::per_cpu_init;
use crate::pmm_init;
use crate::thread_init::{self, SpawnError};
use crate::time_init;
use crate::x86;

pub(super) const TESTS: &[Test] = &[
    test("spawn_stack_oom", spawn_stack_oom).deadline(10_000),
    test("fork_oom", fork_oom).deadline(10_000),
    test("lifetime_stack_reclaim", lifetime_stack_reclaim).deadline(120_000),
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

/// CPUs the switch-tail tests need: CPU 0 exits, CPUs 1 to 3 churn or spawn.
const TAIL_CPUS: u32 = 4;
/// Thread exits [`lifetime_stack_reclaim`] makes on CPU 0.
const RECLAIM_EXITS: usize = 10_000;
/// Threads spawned per batch, each batch waited `Dead`.
const EXIT_BATCH: usize = 16;
/// Exits held open by the exit-stall hook, and for how long each.
const STALL_EXITS: u32 = 100;
const STALL_MS: u64 = 10;
/// Churn threads per churning CPU.
const CHURN_PER_CPU: u32 = 2;
/// Bound on waiting for one batch to die or the churn threads to stop.
const WAIT_NS: u64 = 10_000_000_000;

static CHURN_STOP: AtomicBool = AtomicBool::new(false);
static CHURN_DONE: AtomicU32 = AtomicU32::new(0);
/// Churn switches, per CPU.
static CHURN: [AtomicU64; TAIL_CPUS as usize] = [const { AtomicU64::new(0) }; TAIL_CPUS as usize];

fn tail_cpus_online() -> bool {
    per_cpu_init::online_mask().count_ones() >= TAIL_CPUS
}

/// Yield to its sibling on the same CPU until told to stop: every yield
/// runs a switch tail on that CPU.
fn churn_entry() {
    let cpu = thread_init::current_cpu() as usize;
    while !CHURN_STOP.load(Ordering::Acquire) {
        if let Some(c) = CHURN.get(cpu) {
            c.fetch_add(1, Ordering::Relaxed);
        }
        thread_init::yield_now();
    }
    CHURN_DONE.fetch_add(1, Ordering::AcqRel);
}

/// Stop the churn threads and wait, bounded, for them to finish.
fn stop_churn(started: u32) -> bool {
    CHURN_STOP.store(true, Ordering::Release);
    let t0 = time_init::now_ns();
    while CHURN_DONE.load(Ordering::Acquire) < started {
        if time_init::now_ns().saturating_sub(t0) > WAIT_NS {
            return false;
        }
        thread_init::yield_now();
    }
    true
}

/// Spawn `EXIT_BATCH` threads on CPU 0 that return at once, and wait for
/// every one to be `Dead`. A full thread table is retried after a yield.
fn exit_batch() -> Result<(), Outcome> {
    let mut ids = [ThreadId::NONE; EXIT_BATCH];
    let mut n = 0usize;
    let t0 = time_init::now_ns();
    while n < EXIT_BATCH {
        match thread_init::spawn_on("exit", dying_entry, 0) {
            Ok(h) => {
                ids[n] = h.id();
                n += 1;
            }
            Err(SpawnError::NoSlot) => thread_init::yield_now(),
            Err(e) => return Err(crate::fail_fmt!("spawn: {}", e.as_str())),
        }
        if time_init::now_ns().saturating_sub(t0) > WAIT_NS {
            return Err(Outcome::Fail("no thread slot came free"));
        }
    }
    loop {
        if ids
            .iter()
            .all(|&id| thread_init::try_state(id) == Some(ThreadState::Dead))
        {
            return Ok(());
        }
        if time_init::now_ns().saturating_sub(t0) > WAIT_NS {
            return Err(Outcome::Fail("batch did not die"));
        }
        thread_init::yield_now();
    }
}

/// A dead thread's kernel stack is reused or freed only by the CPU that
/// ran it, after it has switched off it (ROADMAP §10.10, F012): 10,000
/// exits on CPU 0, the first 100 held open between the store that makes
/// the stack reclaimable and the switch, while CPUs 1 to 3 run switch
/// tails; no switch tail sends a shootdown and the frames come back.
fn lifetime_stack_reclaim() -> Outcome {
    if !tail_cpus_online() {
        return Outcome::Skip("needs 4 cpus");
    }
    if thread_init::current_cpu() != 0 {
        return Outcome::Fail("registry not on cpu0");
    }
    let base = quiescent_free_frames();
    let t0 = ipi_init::testing::shootdowns_from_tail();
    CHURN_STOP.store(false, Ordering::Release);
    CHURN_DONE.store(0, Ordering::Release);
    for c in CHURN.iter() {
        c.store(0, Ordering::Relaxed);
    }
    let mut started = 0u32;
    for cpu in 1..TAIL_CPUS {
        for _ in 0..CHURN_PER_CPU {
            if let Err(e) = thread_init::spawn_on("churn", churn_entry, cpu) {
                let _stopped = stop_churn(started);
                return crate::fail_fmt!("churn spawn: {}", e.as_str());
            }
            started += 1;
        }
    }
    thread_init::testing::arm_exit_stall(0, STALL_EXITS, STALL_MS);
    let mut exits = 0usize;
    let mut r = Ok(());
    while exits < RECLAIM_EXITS {
        r = exit_batch();
        if r.is_err() {
            break;
        }
        exits += EXIT_BATCH;
    }
    thread_init::testing::disarm_exit_stall();
    let stopped = stop_churn(started);
    if let Err(o) = r {
        return o;
    }
    if !stopped {
        return Outcome::Fail("churn threads did not stop");
    }
    for (cpu, c) in CHURN.iter().enumerate().skip(1) {
        if c.load(Ordering::Relaxed) == 0 {
            return crate::fail_fmt!("cpu{cpu} churn did not advance");
        }
    }
    let tail = ipi_init::testing::shootdowns_from_tail().wrapping_sub(t0);
    if tail != 0 {
        return crate::fail_fmt!("{tail} shootdowns sent from a switch tail");
    }
    let after = quiescent_free_frames();
    if after != base {
        return crate::fail_fmt!("frames {base} -> {after} after {exits} exits");
    }
    Outcome::Ok
}
