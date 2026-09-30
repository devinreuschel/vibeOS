use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::sched::stack_depth::{self, Deepest};
use vibeos::sched::take_next;
use vibeos::thread::{CpuAffinity, GuardedStack, MAX_THREADS, Tcb, ThreadId, ThreadState};

use super::{runnable_on, with_sched};

use crate::kva_init;
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
    // IF is off here (`thread_exit`'s switch section).
    let _hold = crate::sched::irqoff::deliberate("exit stall");
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
/// Per thread-table slot: set when its thread was moved, cleared by the
/// dequeue that runs it.
pub(in crate::sched) static ARRIVED: [AtomicBool; MAX_THREADS] =
    [const { AtomicBool::new(false) }; MAX_THREADS];

/// A `CpuAffinity::Any` kernel thread, a thread the requeue hook may
/// move, that does not run until [`queue_here`] queues it on a CPU. It
/// waits `Blocked` on no queue, with no deadline, so a stale run-queue
/// entry for its slot cannot run it (`thread_init::runnable_on`). Its
/// entry starts at `irq_nest` 1, IF off, on the CPU that first runs it:
/// no tick or IPI can switch it off, and the hook move it on, first.
pub fn spawn_parked_any(
    name: &'static str,
    entry: fn(),
) -> Result<super::ThreadHandle, super::SpawnError> {
    let h = super::spawn_inner(
        name,
        entry,
        CpuAffinity::Any,
        false,
        1,
        0,
        0,
        vibeos::kva::DEFAULT_STACK_PAGES,
    )?;
    with_sched(|s| {
        if let Some(t) = s.get_mut(h.id()) {
            t.state = ThreadState::Blocked { wq: 0 };
        }
    });
    Ok(h)
}

/// Make [`spawn_parked_any`]'s thread `id` Ready on this CPU's run queue.
pub fn queue_here(id: ThreadId) {
    let me = super::current_cpu();
    with_sched(|s| {
        let Some(t) = s.get_mut(id) else {
            return;
        };
        t.state = ThreadState::Ready;
        t.cpu = me;
        s.place(me, id);
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

/// Thread-table slot `slot`'s thread was moved. Under SCHED.
pub(super) fn moved(slot: usize) {
    if let Some(a) = ARRIVED.get(slot) {
        a.store(true, Ordering::Release);
    }
    REQUEUES.fetch_add(1, Ordering::Relaxed);
}

/// Whether slot `slot`'s thread arrived by a move and has not run since;
/// clears it. Under SCHED.
pub(super) fn take_arrived(slot: usize) -> bool {
    ARRIVED
        .get(slot)
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

/// Hold `id`'s next blocking wait between the store that queues it on the
/// wait queue and its `schedule`, IF=0, until [`arm_late_wake`]'s held
/// wake lets it go or 2 s pass: the `with_sched` whose section leaves it
/// `Blocked` holds it once SCHED has dropped. The thread calls it on
/// itself before it blocks.
pub fn arm_wait_window(id: ThreadId) {
    WINDOW_GO.store(false, Ordering::Relaxed);
    WINDOW_HELD.store(false, Ordering::Relaxed);
    WINDOW_TID.store(id.raw(), Ordering::Release);
}

/// Whether [`arm_wait_window`]'s thread is held now.
pub fn wait_window_held() -> bool {
    WINDOW_HELD.load(Ordering::Acquire)
}

/// Called by `with_sched` before it takes SCHED: IF off for each section
/// of [`arm_wait_window`]'s thread until the returned guard drops, after
/// the section's places are delivered. SCHED's own guard would turn IF
/// back on as it drops, and a tick between that and the hold would switch
/// the thread off `Blocked`, so that it reached the hold only once woken.
pub(super) fn window_enter() -> Option<WindowGuard> {
    let armed = WINDOW_TID.load(Ordering::Acquire);
    (armed != u32::MAX && armed == super::current_id().raw()).then(|| WindowGuard {
        _irq: crate::sched::irqoff::deliberate("late-wake window hold"),
    })
}

/// [`window_enter`]'s guard: on drop it runs [`wait_window`], then turns
/// IF back on.
pub(super) struct WindowGuard {
    _irq: crate::sched::irqoff::DeliberateGuard,
}

impl Drop for WindowGuard {
    fn drop(&mut self) {
        wait_window();
    }
}

/// Holds the thread, IF off under [`window_enter`]'s guard, when its
/// `with_sched` section left it `Blocked`: the wait's queueing is done and
/// its `schedule` not yet. The hold serves other CPUs' shootdowns and
/// calls, as every IF-off spin does, so none of them waits it out.
fn wait_window() {
    let me = super::current_id();
    if !matches!(try_state(me), Some(ThreadState::Blocked { .. })) {
        return;
    }
    let me = me.raw();
    if WINDOW_TID
        .compare_exchange(me, u32::MAX, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    WINDOW_HELD.store(true, Ordering::Release);
    let end =
        time_init::read_tsc().saturating_add(LATE_WAKE_MS.saturating_mul(time_init::tsc_per_ms()));
    while !WINDOW_GO.load(Ordering::Acquire) && time_init::read_tsc() < end {
        crate::ipi_init::service_incoming();
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
    // With IF=0 the stall is a deliberate IF-off stretch; with IF=1 it
    // holds none and takes no guard.
    let _hold = (!crate::arch::current::interrupts_enabled())
        .then(|| crate::sched::irqoff::deliberate("late-wake place stall"));
    let end =
        time_init::read_tsc().saturating_add(LATE_WAKE_MS.saturating_mul(time_init::tsc_per_ms()));
    while time_init::read_tsc() < end {
        if exited(id) {
            PLACE_LATE.store(true, Ordering::Release);
            return;
        }
        core::hint::spin_loop();
    }
}

#[allow(
    clippy::expect_used,
    reason = "invariant: a test asks only about a thread it keeps from being reaped, and a thread's tid names its TCB until a spawn reuses its Dead slot (`thread_init::spawn_inner`)"
)]
pub fn state(id: ThreadId) -> ThreadState {
    super::SCHED.lock().get(id).expect("unknown thread").state
}

pub fn try_state(id: ThreadId) -> Option<ThreadState> {
    super::SCHED.lock().get(id).map(|t| t.state)
}

/// Whether `id`'s thread has exited: its TCB is Dead, or a spawn has
/// reused its Dead slot, after which the tid names no thread. For an id a
/// spawn returned.
pub fn exited(id: ThreadId) -> bool {
    matches!(try_state(id), None | Some(ThreadState::Dead))
}

#[allow(
    clippy::expect_used,
    reason = "invariant: a test asks only about a thread it keeps from being reaped, and a thread's tid names its TCB until a spawn reuses its Dead slot (`thread_init::spawn_inner`)"
)]
pub fn name(id: ThreadId) -> &'static str {
    super::SCHED.lock().get(id).expect("unknown thread").name
}

#[allow(
    clippy::expect_used,
    reason = "invariant: a test asks only about a thread it keeps from being reaped, and a thread's tid names its TCB until a spawn reuses its Dead slot (`thread_init::spawn_inner`)"
)]
pub fn cpu_of(id: ThreadId) -> u32 {
    super::SCHED.lock().get(id).expect("unknown thread").cpu
}

/// C-REQUEUE-HOOK: while `sched::ktest::set_requeue_next_cpu` is on, a user
/// thread or a `CpuAffinity::Any` kernel thread that this CPU dequeues
/// while another thread is current moves to the next online CPU instead
/// of running here, once per slice it runs. A preempted thread that is the
/// only runnable one here stays queued while this CPU runs idle, whose
/// dequeue then moves it. Never the running thread, an idle thread, or a
/// pinned kernel thread; the move goes out through `place` after the
/// SCHED lock drops, before any switch. Each entry dequeued after a move
/// goes through the hook too, so a movable thread queued behind a moved
/// one moves as well.
pub(super) fn requeue_next_cpu(
    mut next: ThreadId,
    cur: ThreadId,
    idle: ThreadId,
    me: u32,
) -> ThreadId {
    if !requeue_on() {
        return next;
    }
    let Some(target) = next_online_cpu(me) else {
        return next;
    };
    // A stale entry's thread is not runnable here and does not move:
    // `schedule_inner` drops the entry.
    let movable = |t: &Tcb| runnable_on(t, me) && (t.pid != 0 || t.affinity == CpuAffinity::Any);
    loop {
        if next == idle {
            return next;
        }
        if next == cur {
            if with_sched(|s| s.get(cur).is_some_and(movable)) {
                per_cpu_init::with_current(|cpu| cpu.runq.push_back(cur));
                return idle;
            }
            return next;
        }
        let did_move = with_sched(|s| {
            let Some(slot) = s.slot_of(next) else {
                return false;
            };
            if take_arrived(slot) {
                return false;
            }
            let Some(t) = s.get_mut(next) else {
                return false;
            };
            if !movable(t) {
                return false;
            }
            t.cpu = target;
            if t.pid != 0 {
                t.affinity = CpuAffinity::Pinned(target);
            }
            // Before the place: `with_sched` hands the thread to `target` as
            // its lock drops, and a dequeue there that finds no arrival would
            // move it again.
            moved(slot);
            s.place(target, next);
            true
        });
        if !did_move {
            return next;
        }
        next = per_cpu_init::with_current(|cpu| take_next(&mut cpu.runq, idle));
    }
}

/// The incoming thread of the last switch `sync_init::assert_switch_clean`
/// refused, or `ThreadId::NONE`.
static REFUSED_SWITCH: AtomicU32 = AtomicU32::new(u32::MAX);

/// Called by `switch_now` just before a switch that holds a ranked lock.
pub(super) fn refuse_switch(id: ThreadId) {
    REFUSED_SWITCH.store(id.0, Ordering::Relaxed);
}

/// The thread a refused switch was about to run, which `schedule_inner` has
/// already set Running and taken off this CPU's run queue: a test that
/// catches the refusal `switch_to`s it to undo that. Clears the record.
pub fn take_refused_switch() -> Option<ThreadId> {
    let id = ThreadId(REFUSED_SWITCH.swap(ThreadId::NONE.0, Ordering::Relaxed));
    if id.is_none() { None } else { Some(id) }
}

/// Fill a stack `cached_stack` took from this CPU's cache again, before a
/// new thread's first frame goes on it (DESIGN §4.5, TESTING §8.2).
pub(super) fn refill_cached(stack: &GuardedStack) {
    // SAFETY: `refill_stack`'s contract; a cached stack has no thread on
    // it and `cached_stack` owns the handle alone (invariant I10,
    // established at `sched::thread_init::finish_switch`).
    unsafe { kva_init::refill_stack(stack) };
}

/// The switch tail's scan of this CPU's dead-stack slot, before the stack
/// is cached, zeroed or linked onto the dead list: its depth is recorded
/// for the thread that just switched off it, whose `Tcb` stays intact
/// until `on_cpu` clears (TESTING §8.2).
pub(super) fn scan_dead_slot() {
    let found = per_cpu_init::with_current(|cpu| {
        let s = cpu.dead_stack.as_ref()?;
        Some((s.base().as_u64(), s.pages(), cpu.tail_prev))
    });
    let Some((base, pages, prev)) = found else {
        return;
    };
    let words = pages * WORDS_PER_PAGE;
    // SAFETY: the slot's stack is mapped (`mm::kva_init::alloc_guarded_stack`)
    // and stays so: only this CPU's switch tail, running here, takes it
    // from the slot (invariant I10, established at
    // `sched::thread_init::thread_exit`).
    let used = unsafe { stack_depth::used_volatile(base as *const u64, words) };
    let (tid, name) = if prev.is_null() {
        (0, "?")
    } else {
        // SAFETY: invariant I9: `tail_prev` is the TCB this CPU just
        // switched off, a live entry of `SCHED` until the Release store
        // of its `on_cpu` later in this switch tail; established by
        // `sched::thread_init::switch_now`.
        unsafe { ((*prev).id.0, (*prev).name) }
    };
    crate::sched::ktest::record(Deepest {
        size: words * 8,
        used,
        tid,
        name,
    });
}

/// Scan every live thread's stack, one `SCHED` section per slot, and hand
/// each measurement to `f` with the lock dropped (TESTING §8.2).
pub fn scan_live_stacks(mut f: impl FnMut(Deepest)) {
    let mut i = 0usize;
    while i < MAX_THREADS {
        let d = with_sched(|s| {
            let t = s.slots.get(i)?.as_deref()?;
            if t.state == ThreadState::Dead {
                return None;
            }
            let st = t.stack.as_ref()?;
            let words = st.pages() * WORDS_PER_PAGE;
            // SAFETY: a thread that is not Dead keeps its stack mapped
            // while SCHED is held: `thread_exit` stores Dead under SCHED
            // before its switch hands the stack to reclaim (invariant I10,
            // established at `sched::thread_init::thread_exit`).
            let used =
                unsafe { stack_depth::used_volatile(st.base().as_u64() as *const u64, words) };
            Some(Deepest {
                size: words * 8,
                used,
                tid: t.id.0,
                name: t.name,
            })
        });
        if let Some(d) = d {
            f(d);
        }
        i += 1;
    }
}

const WORDS_PER_PAGE: usize = vibeos::paging::PAGE_SIZE_4K as usize / 8;
