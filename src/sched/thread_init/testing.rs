use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::thread::{MAX_THREADS, ThreadId, ThreadState};

use crate::per_cpu_init;
use crate::time_init;

/// CPU whose exits [`exit_stall`] holds, or `u32::MAX`.
static STALL_CPU: AtomicU32 = AtomicU32::new(u32::MAX);
/// Exits left to hold.
static STALL_LEFT: AtomicU32 = AtomicU32::new(0);
/// How long each held exit spins, in ms of TSC time.
static STALL_MS: AtomicU64 = AtomicU64::new(0);

/// Hold the next `exits` thread exits on `cpu` for `ms` of TSC time
/// each, between the store that makes the exiting thread's stack
/// reclaimable and the switch off it. The hold spins with IF=0 and
/// services no IPI.
pub fn arm_exit_stall(cpu: u32, exits: u32, ms: u64) {
    STALL_CPU.store(cpu, Ordering::Relaxed);
    STALL_MS.store(ms, Ordering::Relaxed);
    STALL_LEFT.store(exits, Ordering::Release);
}

pub fn disarm_exit_stall() {
    STALL_LEFT.store(0, Ordering::Release);
    STALL_CPU.store(u32::MAX, Ordering::Relaxed);
}

/// Called by `thread_exit` just before it switches away.
pub(super) fn exit_stall() {
    if STALL_CPU.load(Ordering::Relaxed) != super::current_cpu() {
        return;
    }
    if STALL_LEFT
        .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
        .is_err()
    {
        return;
    }
    let cycles = STALL_MS
        .load(Ordering::Relaxed)
        .saturating_mul(time_init::tsc_per_ms());
    let end = time_init::read_tsc().saturating_add(cycles);
    while time_init::read_tsc() < end {
        core::hint::spin_loop();
    }
}

/// One-shot: the next spawn from a process context fails as if its
/// kernel stack could not be allocated.
pub(super) static FAIL_FORK_STACK: AtomicBool = AtomicBool::new(false);

/// Move this CPU's cached stacks onto its dead list and wake its worker,
/// which unmaps and frees them.
pub fn drain_local_stack_cache() {
    let kick = crate::per_cpu_init::with_current(|cpu| {
        let mut any = false;
        while let Some(stack) = cpu.stack_cache.take() {
            super::CACHED_STACK_FRAMES.fetch_sub(stack.pages(), Ordering::AcqRel);
            super::STACKS_IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
            crate::kva_init::park_on_list(&mut cpu.dead_list, stack);
            any = true;
        }
        any
    });
    if kick {
        crate::work_init::kick_dead_stacks();
    }
}

/// Put `stack`, which nothing runs on, on this CPU's dead list and wake
/// its worker, as the switch tail does with a stack the cache refuses.
pub fn park_on_local_list(stack: crate::kva_init::GuardedStack) {
    super::STACKS_IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
    crate::per_cpu_init::with_current(|cpu| {
        crate::kva_init::park_on_list(&mut cpu.dead_list, stack);
    });
    crate::work_init::kick_dead_stacks();
}

/// Make the next `spawn*` made on behalf of a process (a `fork`) return
/// `SpawnError::NoMemory` before it allocates anything. One-shot.
pub fn fail_next_fork_stack() {
    FAIL_FORK_STACK.store(true, Ordering::Release);
}

/// C-REQUEUE-HOOK's switch, which `sched::ktest::set_requeue_next_cpu`
/// sets.
pub(in crate::sched) static REQUEUE: AtomicBool = AtomicBool::new(false);
/// Moves the hook has made since boot (`sched::ktest::requeues`).
pub(in crate::sched) static REQUEUES: AtomicU64 = AtomicU64::new(0);
/// Set when a thread was moved, cleared by the dequeue that runs it.
pub(in crate::sched) static ARRIVED: [AtomicBool; MAX_THREADS] =
    [const { AtomicBool::new(false) }; MAX_THREADS];

/// Let `id`, spawned pinned and not yet run, run on any CPU: a thread
/// the requeue hook may move, queued where it was spawned.
pub fn unpin(id: ThreadId) {
    super::with_sched(|s| {
        if let Some(t) = s.get_mut(id) {
            t.affinity = vibeos::thread::CpuAffinity::Any;
        }
    });
}

pub(super) fn requeue_on() -> bool {
    REQUEUE.load(Ordering::Acquire)
}

/// The first online CPU after `me`, wrapping; `None` with one CPU.
pub(super) fn next_online_cpu(me: u32) -> Option<u32> {
    let mask = per_cpu_init::online_mask();
    (1..64u32)
        .map(|d| me.wrapping_add(d) % 64)
        .find(|&c| mask & (1u64 << c) != 0)
}

pub(super) fn moved(id: ThreadId) {
    if let Some(a) = ARRIVED.get(id.raw() as usize) {
        a.store(true, Ordering::Release);
    }
    REQUEUES.fetch_add(1, Ordering::Relaxed);
}

/// Whether `id` arrived by a move and has not run since; clears it.
pub(super) fn take_arrived(id: ThreadId) -> bool {
    ARRIVED
        .get(id.raw() as usize)
        .is_some_and(|a| a.swap(false, Ordering::AcqRel))
}

/// Most either late-wake hold spins, in ms of TSC time.
const LATE_WAKE_MS: u64 = 2_000;
/// Thread whose next wait [`wait_window`] holds, or `u32::MAX`.
static WINDOW_TID: AtomicU32 = AtomicU32::new(u32::MAX);
/// Set while [`wait_window`] holds its thread.
static WINDOW_HELD: AtomicBool = AtomicBool::new(false);
/// Lets the thread [`wait_window`] holds go on.
static WINDOW_GO: AtomicBool = AtomicBool::new(false);
/// Thread whose next wake [`place_stall`] delivers late, or `u32::MAX`.
static PLACE_TID: AtomicU32 = AtomicU32::new(u32::MAX);
/// Set when [`place_stall`] delivered its wake after the thread died.
static PLACE_LATE: AtomicBool = AtomicBool::new(false);

/// Hold `id`'s next blocking wait (`sync_init::wait_resume`) between the
/// store that queues it on the wait queue and its `schedule`, IF=0,
/// until [`arm_late_wake`]'s held wake lets it go or 2 s pass. The
/// thread calls it on itself before it blocks, and blocks with IF off: a
/// tick after the queueing and before the hold would switch it off
/// `Blocked`, and it would reach the hold only once woken.
pub fn arm_wait_window(id: ThreadId) {
    WINDOW_GO.store(false, Ordering::Relaxed);
    WINDOW_HELD.store(false, Ordering::Relaxed);
    WINDOW_TID.store(id.raw(), Ordering::Release);
}

/// Whether [`arm_wait_window`]'s thread is held now.
pub fn wait_window_held() -> bool {
    WINDOW_HELD.load(Ordering::Acquire)
}

/// Called by `sync_init::wait_resume` before it schedules.
pub fn wait_window() {
    let me = super::current_id().raw();
    if WINDOW_TID
        .compare_exchange(me, u32::MAX, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    // IF=0 so no tick switches the held thread off before the wake.
    let _irq = crate::x86::InterruptGuard::enter();
    WINDOW_HELD.store(true, Ordering::Release);
    let end =
        time_init::read_tsc().saturating_add(LATE_WAKE_MS.saturating_mul(time_init::tsc_per_ms()));
    while !WINDOW_GO.load(Ordering::Acquire) && time_init::read_tsc() < end {
        core::hint::spin_loop();
    }
    WINDOW_HELD.store(false, Ordering::Release);
}

/// Deliver the next wake of `id` late: the waker, after `with_sched`
/// has dropped SCHED and before it pushes `id` to its CPU, lets
/// [`arm_wait_window`]'s thread go and spins until `id` is `Dead` or 2 s
/// pass, as a waker preempted at that point would while `id` runs on.
pub fn arm_late_wake(id: ThreadId) {
    PLACE_LATE.store(false, Ordering::Relaxed);
    PLACE_TID.store(id.raw(), Ordering::Release);
}

/// Whether [`arm_late_wake`]'s wake went out after its thread died.
pub fn late_wake_after_death() -> bool {
    PLACE_LATE.load(Ordering::Acquire)
}

pub fn disarm_late_wake() {
    PLACE_TID.store(u32::MAX, Ordering::Release);
    WINDOW_TID.store(u32::MAX, Ordering::Release);
    WINDOW_GO.store(true, Ordering::Release);
}

/// Called by `with_sched` before it delivers each place.
pub(super) fn place_stall(id: ThreadId) {
    if PLACE_TID
        .compare_exchange(id.raw(), u32::MAX, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    WINDOW_GO.store(true, Ordering::Release);
    let end =
        time_init::read_tsc().saturating_add(LATE_WAKE_MS.saturating_mul(time_init::tsc_per_ms()));
    while time_init::read_tsc() < end {
        if super::try_state(id) == Some(ThreadState::Dead) {
            PLACE_LATE.store(true, Ordering::Release);
            return;
        }
        core::hint::spin_loop();
    }
}
