//! In-guest tests of P10-S08, Stack reclaim on the owning CPU, fallible spawn, frame baselines (DESIGN §8.2).

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::pmm::{Frames, MAX_ORDER};
use vibeos::proc::wait_exited;
use vibeos::thread::{MAX_THREADS, ThreadId, ThreadState};

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
    test("exit_burst", exit_burst).deadline(60_000),
    test("lifetime_dead_slot_on_cpu", lifetime_dead_slot_on_cpu).deadline(180_000),
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
/// Ready or Running and no dead thread's stack sits in a CPU's dead-stack
/// slot or on its dead list. False if that did not happen in time.
fn quiesce() -> bool {
    super::settle_threads()
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
    // A cached stack would let the spawn succeed with the buddy empty.
    thread_init::testing::drain_local_stack_cache();
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

// Spin about 10 million iterations (several 10 ms quanta under TCG), then
// exit(0).
user_code!(
    SPIN_EXIT0,
    "
    mov ecx, 10000000
1:
    dec ecx
    jnz 1b
    xor edi, edi
    mov eax, 60
    syscall
    ud2
    "
);

/// Processes [`exit_burst`] starts.
const BURST: usize = 16;
/// CPU the burst runs on.
const BURST_CPU: u32 = 1;

static BURST_PIDS: [AtomicU32; BURST] = [const { AtomicU32::new(0) }; BURST];
static BURST_SPAWNED: AtomicU32 = AtomicU32::new(0);
static BURST_FAILED: AtomicBool = AtomicBool::new(false);
static BURST_DONE: AtomicBool = AtomicBool::new(false);

/// On [`BURST_CPU`]: start the burst there, since a process's thread is
/// pinned to the CPU that spawns it.
fn burst_spawner() {
    for slot in BURST_PIDS.iter() {
        match user::spawn(&Image::Code(SPIN_EXIT0, DEFAULT), &["burst"]) {
            Ok(pid) => {
                slot.store(pid, Ordering::Relaxed);
                BURST_SPAWNED.fetch_add(1, Ordering::AcqRel);
            }
            Err(_) => {
                BURST_FAILED.store(true, Ordering::Release);
                break;
            }
        }
    }
    BURST_DONE.store(true, Ordering::Release);
}

/// Every switch tail empties the dead-stack slot (ROADMAP §10.10, F010):
/// 16 processes on one CPU exit back to back, each switching to a sibling
/// resumed from timer preemption, and the kernel stays up.
fn exit_burst() -> Outcome {
    if !per_cpu_init::is_online(BURST_CPU) {
        return Outcome::Skip("needs 2 cpus");
    }
    BURST_SPAWNED.store(0, Ordering::Release);
    BURST_FAILED.store(false, Ordering::Release);
    BURST_DONE.store(false, Ordering::Release);
    if let Err(e) = thread_init::spawn_on("burst", burst_spawner, BURST_CPU) {
        return crate::fail_fmt!("spawn: {}", e.as_str());
    }
    let t0 = time_init::now_ns();
    while !BURST_DONE.load(Ordering::Acquire) {
        if time_init::now_ns().saturating_sub(t0) > WAIT_NS {
            return Outcome::Fail("burst spawner did not finish");
        }
        thread_init::yield_now();
    }
    let n = BURST_SPAWNED.load(Ordering::Acquire) as usize;
    let mut bad = 0u32;
    for slot in BURST_PIDS.iter().take(n) {
        if user::wait(slot.load(Ordering::Relaxed)) != wait_exited(0) {
            bad += 1;
        }
    }
    if BURST_FAILED.load(Ordering::Acquire) {
        return crate::fail_fmt!("only {n} of {BURST} processes started");
    }
    if bad != 0 {
        return crate::fail_fmt!("{bad} of {BURST} processes did not exit 0");
    }
    Outcome::Ok
}

/// Dead slots left for CPU 0's exits once the fillers hold the rest.
const FREE_SLOTS: u32 = 4;
/// Spawns each spawner on CPUs 1 to 3 makes.
const SPAWNS_PER_CPU: u32 = 400;

static FILL_LIVE: AtomicU32 = AtomicU32::new(0);
/// Fillers told to exit, taken one at a time.
static FILL_RELEASE: AtomicU32 = AtomicU32::new(0);
static FILL_ALL: AtomicBool = AtomicBool::new(false);
static SPAWN_GO: AtomicBool = AtomicBool::new(false);
static SPAWNERS_DONE: AtomicU32 = AtomicU32::new(0);
static SPAWN_BAD: AtomicBool = AtomicBool::new(false);
/// Per TCB slot: spawns that returned it, and runs of a child in it.
static SPAWNED: [AtomicU32; MAX_THREADS] = [const { AtomicU32::new(0) }; MAX_THREADS];
static RAN: [AtomicU32; MAX_THREADS] = [const { AtomicU32::new(0) }; MAX_THREADS];

/// Hold a TCB slot, asleep on CPU 0, until released.
fn filler_entry() {
    FILL_LIVE.fetch_add(1, Ordering::AcqRel);
    loop {
        if FILL_ALL.load(Ordering::Acquire)
            || FILL_RELEASE
                .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                .is_ok()
        {
            break;
        }
        thread_init::sleep_ms(2);
    }
    FILL_LIVE.fetch_sub(1, Ordering::AcqRel);
}

fn child_entry() {
    if let Some(r) = RAN.get(thread_init::current_id().0 as usize) {
        r.fetch_add(1, Ordering::AcqRel);
    }
}

/// On CPUs 1 to 3: spawn children onto CPU 0 as fast as slots come free.
fn slot_spawner() {
    while !SPAWN_GO.load(Ordering::Acquire) {
        thread_init::yield_now();
    }
    let t0 = time_init::now_ns();
    let mut n = 0u32;
    while n < SPAWNS_PER_CPU {
        match thread_init::spawn_on("child", child_entry, 0) {
            Ok(h) => {
                if let Some(s) = SPAWNED.get(h.id().0 as usize) {
                    s.fetch_add(1, Ordering::AcqRel);
                }
                n += 1;
            }
            Err(SpawnError::NoSlot) => thread_init::yield_now(),
            Err(SpawnError::NoMemory) => {
                SPAWN_BAD.store(true, Ordering::Release);
                break;
            }
        }
        if time_init::now_ns().saturating_sub(t0) > 6 * WAIT_NS {
            SPAWN_BAD.store(true, Ordering::Release);
            break;
        }
    }
    SPAWNERS_DONE.fetch_add(1, Ordering::AcqRel);
}

fn wait_for(pred: impl Fn() -> bool, ns: u64) -> bool {
    let t0 = time_init::now_ns();
    while !pred() {
        if time_init::now_ns().saturating_sub(t0) > ns {
            return false;
        }
        thread_init::sleep_ms(1);
    }
    true
}

/// Let every filler go and wait for them, bounded.
fn release_fillers() -> bool {
    FILL_ALL.store(true, Ordering::Release);
    wait_for(|| FILL_LIVE.load(Ordering::Acquire) == 0, WAIT_NS)
}

/// A spawn reuses a `Dead` TCB slot only once its CPU has switched off it
/// (ROADMAP §10.10, F012): fillers hold every slot but four, so the only
/// Dead slots are CPU 0's exits, held open for 10 ms on the first 100;
/// spawners on CPUs 1 to 3 respawn into them, and every child runs its
/// entry exactly once.
fn lifetime_dead_slot_on_cpu() -> Outcome {
    if !tail_cpus_online() {
        return Outcome::Skip("needs 4 cpus");
    }
    for (s, r) in SPAWNED.iter().zip(RAN.iter()) {
        s.store(0, Ordering::Relaxed);
        r.store(0, Ordering::Relaxed);
    }
    FILL_LIVE.store(0, Ordering::Release);
    FILL_RELEASE.store(0, Ordering::Release);
    FILL_ALL.store(false, Ordering::Release);
    SPAWN_GO.store(false, Ordering::Release);
    SPAWNERS_DONE.store(0, Ordering::Release);
    SPAWN_BAD.store(false, Ordering::Release);

    // The spawners take their slots before the fillers take the rest.
    let mut spawners = 0u32;
    for cpu in 1..TAIL_CPUS {
        if let Err(e) = thread_init::spawn_on("slot-spawner", slot_spawner, cpu) {
            SPAWN_GO.store(true, Ordering::Release);
            SPAWN_BAD.store(true, Ordering::Release);
            let _done = wait_for(
                || SPAWNERS_DONE.load(Ordering::Acquire) == spawners,
                WAIT_NS,
            );
            return crate::fail_fmt!("spawner: {}", e.as_str());
        }
        spawners += 1;
    }
    // Fill every free slot, then let four fillers exit on CPU 0.
    let mut fillers = 0u32;
    loop {
        match thread_init::spawn_on("filler", filler_entry, 0) {
            Ok(_) => fillers += 1,
            Err(SpawnError::NoSlot) => break,
            Err(SpawnError::NoMemory) => {
                SPAWN_GO.store(true, Ordering::Release);
                let _released = release_fillers();
                return Outcome::Fail("filler spawn: no memory");
            }
        }
    }
    let fill_target = fillers;
    if !wait_for(|| FILL_LIVE.load(Ordering::Acquire) == fill_target, WAIT_NS) {
        SPAWN_GO.store(true, Ordering::Release);
        let _released = release_fillers();
        return Outcome::Fail("fillers did not start");
    }
    FILL_RELEASE.store(FREE_SLOTS.min(fillers), Ordering::Release);
    let left = fill_target.saturating_sub(FREE_SLOTS);
    if !wait_for(|| FILL_LIVE.load(Ordering::Acquire) == left, WAIT_NS) {
        SPAWN_GO.store(true, Ordering::Release);
        let _released = release_fillers();
        return Outcome::Fail("fillers did not exit");
    }

    thread_init::testing::arm_exit_stall(0, STALL_EXITS, STALL_MS);
    SPAWN_GO.store(true, Ordering::Release);
    let done = wait_for(
        || SPAWNERS_DONE.load(Ordering::Acquire) == spawners,
        6 * WAIT_NS,
    );
    let sum = |a: &[AtomicU32]| a.iter().map(|x| x.load(Ordering::Acquire)).sum::<u32>();
    let ran = wait_for(|| sum(&RAN) >= sum(&SPAWNED), WAIT_NS);
    thread_init::testing::disarm_exit_stall();
    let released = release_fillers();
    if !done {
        return Outcome::Fail("spawners did not finish");
    }
    if SPAWN_BAD.load(Ordering::Acquire) {
        return Outcome::Fail("a spawner gave up");
    }
    for (slot, (s, r)) in SPAWNED.iter().zip(RAN.iter()).enumerate() {
        let (s, r) = (s.load(Ordering::Acquire), r.load(Ordering::Acquire));
        if s != r {
            return crate::fail_fmt!("slot {slot}: spawned {s}, ran {r}");
        }
    }
    if !ran {
        return Outcome::Fail("children did not run");
    }
    if !released {
        return Outcome::Fail("fillers did not exit at the end");
    }
    Outcome::Ok
}
