//! Kernel threads: TCB table, spawn, schedule, sleep. ROADMAP §3.1–§3.4 / §4.8.
//!
//! Global TCB table + timeouts under one SCHED lock. Per-CPU ready queues
//! (owner CPU only, IRQs off). Cross-CPU wake is inbox + IPI 0xFD, never
//! a remote runq lock. Never held across `switch_context`, heap free, or
//! KVA/stack reclaim.
//!
//! A dead thread's kernel stack stays with the CPU that ran it: `thread_exit`
//! parks it in that CPU's `PerCpu.dead_stack`, and [`finish_switch`], the
//! switch tail that runs on that CPU once `switch_context` has moved it off
//! the stack, puts it in the CPU's stack cache or on its dead list, which
//! the CPU's workqueue worker unmaps and frees with IF=1 (DESIGN §4.5).

use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};

use vibeos::ipi::{home_cpu, pick_cpu};
use vibeos::kalloc::TryBox;
use vibeos::kva::DEFAULT_STACK_PAGES;
use vibeos::limits::PID_MAX;
use vibeos::lock::RANK_SCHED;
use vibeos::per_cpu::PerCpu;
use vibeos::proc::pid::{IdIndex, PidAlloc};
use vibeos::sched::{SWEEP_TICKS, TimeoutQueue, effective_deadline, enqueue_runnable, take_next};
use vibeos::syscall::UserFrame;
use vibeos::thread::{
    CpuAffinity, CpuContext, Fxsave, MAX_THREADS, OnCpu, Tcb, ThreadId, ThreadState, WaitOutcome,
    apply_if_on_resume, prepare_thread,
};
use vibeos::time::Instant;
use vibeos::wait::{self, WaitQueue};

use crate::kva_init::{self, GuardedStack};
use crate::per_cpu_init;
use crate::sync_init::{self, SleepCtx, SpinMutex};
use crate::time_init;
use crate::x86::InterruptGuard;

mod boot;
#[cfg(feature = "kernel_tests")]
pub(crate) use boot::BOOT_STACK_PAGES;
pub(crate) use boot::bootstrap_stack;
pub use boot::init_bootstrap;

// The syscall layer's hooks (DESIGN §1.2), which `syscall_init::init_bsp`
// sets before the scheduler runs a second thread.
/// The hardware side of a context switch (FPU, RSP0, CR3):
/// `syscall_init::on_switch`. Unset, a switch changes none of them.
static SWITCH_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
/// A new thread's FP image: `syscall_init::fpu_template`. Unset, a thread
/// starts with `Fxsave::empty()`.
static FPU_TEMPLATE_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Install the context-switch and FP-template hooks.
pub fn set_switch_hooks(
    on_switch: fn(&mut PerCpu, *mut Tcb, *mut Tcb),
    fpu_template: fn() -> Fxsave,
) {
    // Release: pairs with the Acquire loads in `on_switch` and
    // `fpu_template`.
    SWITCH_HOOK.store(on_switch as *mut (), Ordering::Release);
    FPU_TEMPLATE_HOOK.store(fpu_template as *mut (), Ordering::Release);
}

fn on_switch(cpu: &mut PerCpu, old: *mut Tcb, new: *mut Tcb) {
    // Acquire: pairs with the Release store in `set_switch_hooks`.
    let p = SWITCH_HOOK.load(Ordering::Acquire);
    if p.is_null() {
        return;
    }
    // SAFETY: invariant: a non-null `SWITCH_HOOK` holds a
    // `fn(&mut PerCpu, *mut Tcb, *mut Tcb)`; established by
    // `thread_init::set_switch_hooks`, its only store.
    let f = unsafe { core::mem::transmute::<*mut (), fn(&mut PerCpu, *mut Tcb, *mut Tcb)>(p) };
    f(cpu, old, new);
}

fn fpu_template() -> Fxsave {
    // Acquire: pairs with the Release store in `set_switch_hooks`.
    let p = FPU_TEMPLATE_HOOK.load(Ordering::Acquire);
    if p.is_null() {
        return Fxsave::empty();
    }
    // SAFETY: invariant: a non-null `FPU_TEMPLATE_HOOK` holds a
    // `fn() -> Fxsave`; established by `thread_init::set_switch_hooks`,
    // its only store.
    let f = unsafe { core::mem::transmute::<*mut (), fn() -> Fxsave>(p) };
    f()
}

/// Wake this CPU's workqueue worker to free its dead stacks:
/// `work_init::kick_dead_stacks`, which `work_init::init` sets (DESIGN
/// §1.2). Unset, nothing is woken; the worker frees the list when it
/// starts.
static KICK_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Install the dead-stack kick.
pub fn set_kick_hook(f: fn()) {
    // Release: pairs with the Acquire load in `kick_dead_stacks`.
    KICK_HOOK.store(f as *mut (), Ordering::Release);
}

fn kick_dead_stacks() {
    // Acquire: pairs with the Release store in `set_kick_hook`.
    let p = KICK_HOOK.load(Ordering::Acquire);
    if p.is_null() {
        return;
    }
    // SAFETY: invariant: a non-null `KICK_HOOK` holds a `fn()`;
    // established by `thread_init::set_kick_hook`, its only store.
    let f = unsafe { core::mem::transmute::<*mut (), fn()>(p) };
    f();
}

/// Why a `spawn*` call made no thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnError {
    /// Every TCB slot holds a live thread.
    NoSlot,
    /// The kernel stack could not be allocated.
    NoMemory,
}

impl SpawnError {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoSlot => "no thread slot",
            Self::NoMemory => "no memory for a kernel stack",
        }
    }
}

#[derive(Clone, Copy)]
pub struct ThreadHandle {
    id: ThreadId,
}

impl ThreadHandle {
    pub fn id(self) -> ThreadId {
        self.id
    }
}

/// Entries in the tid-to-slot index: twice the thread table, so probe runs
/// stay short.
const TID_INDEX_CAP: usize = (2 * MAX_THREADS).next_power_of_two();

pub(crate) struct Sched {
    slots: [Option<TryBox<Tcb>>; MAX_THREADS],
    /// Tids, shared with pids (`proc_init`): a tid is never a slot index.
    pids: PidAlloc,
    /// Each live TCB's tid to its slot in `slots`.
    index: IdIndex<TID_INDEX_CAP>,
    timeouts: TimeoutQueue,
    /// Wakes recorded under SCHED, placed after it drops: (cpu, id, slot).
    places: [(u32, ThreadId, u32); MAX_THREADS],
    place_n: usize,
    /// The context the current `with_sched` section was entered from, which
    /// `begin_wait` checks: it runs under SCHED with IF off and cannot read
    /// IF or `HELD` itself.
    waiter: SleepCtx,
}

/// Each thread-table slot's tid, `u32::MAX` for none: what a wake-inbox
/// bit (a slot) names when its CPU drains it ([`tid_of_slot`]). Stored with
/// Release under SCHED ([`publish_slot`]) whenever a slot takes a TCB.
static SLOT_TID: [AtomicU32; MAX_THREADS] = [const { AtomicU32::new(u32::MAX) }; MAX_THREADS];

/// Record that `slot` now holds thread `id`. Under SCHED.
fn publish_slot(slot: usize, id: ThreadId) {
    if let Some(t) = SLOT_TID.get(slot) {
        // Release: pairs with the Acquire load in `tid_of_slot`.
        t.store(id.0, Ordering::Release);
    }
}

/// The tid in thread-table slot `slot`, for `ipi_init::drain_inbox`. A slot
/// with a bit in some inbox holds a Ready thread, which cannot die before
/// it runs, so the slot is not reused while the bit is set.
pub(crate) fn tid_of_slot(slot: usize) -> Option<ThreadId> {
    // Acquire: pairs with the Release store in `publish_slot`.
    let raw = SLOT_TID.get(slot)?.load(Ordering::Acquire);
    (raw != u32::MAX).then_some(ThreadId(raw))
}

impl Sched {
    const fn empty() -> Self {
        Self {
            slots: [const { None }; MAX_THREADS],
            pids: PidAlloc::new(PID_MAX),
            index: IdIndex::new(),
            timeouts: TimeoutQueue::empty(),
            places: [(0, ThreadId::NONE, 0); MAX_THREADS],
            place_n: 0,
            waiter: SleepCtx::UNCHECKED,
        }
    }

    fn get(&self, id: ThreadId) -> Option<&Tcb> {
        let slot = self.index.get(id.raw())?;
        self.slots.get(slot)?.as_deref().filter(|t| t.id == id)
    }

    fn get_mut(&mut self, id: ThreadId) -> Option<&mut Tcb> {
        let slot = self.index.get(id.raw())?;
        self.slots
            .get_mut(slot)?
            .as_deref_mut()
            .filter(|t| t.id == id)
    }

    /// A new id from the pid and tid allocator, with one use; `None` when
    /// every id is in use.
    pub(crate) fn alloc_id(&mut self) -> Option<u32> {
        self.pids.alloc()
    }

    /// Add a use to `id`: a user thread's tid is its process's pid, which
    /// the process holds already. False when `id` is out of range or its
    /// use count is full.
    #[must_use]
    pub(crate) fn hold_id(&mut self, id: u32) -> bool {
        self.pids.hold(id)
    }

    /// Whether `id` has a use: `proc_init::alloc_pid` asks it for init's
    /// pid, which `init_bootstrap` holds until init takes it over.
    pub(crate) fn id_in_use(&self, id: u32) -> bool {
        self.pids.in_use(id)
    }

    /// Drop a use of `id`; at zero the id is free for a later `alloc_id`.
    #[allow(
        clippy::panic,
        reason = "invariant: every carrier drops exactly the use it took (`alloc_id`/`hold_id`), so a use is left to drop"
    )]
    pub(crate) fn free_id(&mut self, id: u32) {
        assert!(self.pids.free(id), "pid: {id} freed with no use");
    }

    /// A new thread's tid: a user thread's (`pid != 0`) is its process's
    /// pid, with a use of its own; a kernel thread's is a new id.
    fn new_tid(&mut self, pid: u32) -> Option<u32> {
        if pid == 0 {
            self.alloc_id()
        } else {
            self.hold_id(pid).then_some(pid)
        }
    }

    /// Point `id` at `slot` in the index and publish it for the inbox.
    fn bind(&mut self, id: ThreadId, slot: usize) {
        // Invariant: the index holds one entry per live TCB, at most
        // `MAX_THREADS`, and has twice that room; `id` is below `PID_MAX`.
        assert!(self.index.insert(id.raw(), slot), "tid index full");
        publish_slot(slot, id);
    }

    /// Retire slot `slot`'s old tid `old`: out of the index and the timeout
    /// queue, then back to the allocator.
    fn unbind(&mut self, old: ThreadId) {
        if self.index.remove(old.raw()).is_some() {
            self.timeouts.remove(old);
            self.free_id(old.raw());
        }
    }

    fn ptr(&mut self, id: ThreadId) -> *mut Tcb {
        match self.get_mut(id) {
            Some(t) => t as *mut Tcb,
            None => core::ptr::null_mut(),
        }
    }

    /// `id`'s thread-table slot.
    fn slot_of(&self, id: ThreadId) -> Option<usize> {
        let slot = self.index.get(id.raw())?;
        match self.slots.get(slot)?.as_deref() {
            Some(t) if t.id == id => Some(slot),
            _ => None,
        }
    }

    fn place(&mut self, cpu: u32, id: ThreadId) {
        if self.place_n >= MAX_THREADS {
            return;
        }
        let Some(slot) = self.slot_of(id) else {
            return;
        };
        self.places[self.place_n] = (cpu, id, slot as u32);
        self.place_n += 1;
    }

    fn place_home(&mut self, id: ThreadId) {
        let (aff, last) = match self.get(id) {
            Some(t) => (t.affinity, t.cpu),
            None => return,
        };
        let cpu = home_cpu(aff, last, per_cpu_init::online_mask());
        if let Some(t) = self.get_mut(id) {
            t.cpu = cpu;
        }
        self.place(cpu, id);
    }

    /// Enqueue wait → Blocked. Still holding SCHED. Caller drops, then `schedule`.
    pub(crate) fn begin_wait(&mut self, wq: &mut WaitQueue, deadline: Instant) {
        sync_init::assert_not_hard_irq();
        self.waiter.check();
        let id = current_id();
        wq.enqueue(id);
        self.timeouts.insert(id, deadline);
        let cookie = wq.cookie();
        if let Some(t) = self.get_mut(id) {
            t.state = ThreadState::Blocked { wq: cookie };
            t.wait_outcome = WaitOutcome::Woken;
        }
    }

    pub(crate) fn wake_one(&mut self, wq: &mut WaitQueue) -> Option<ThreadId> {
        let id = wait::take_one(wq, &mut self.timeouts)?;
        if let Some(t) = self.get_mut(id) {
            t.state = ThreadState::Ready;
            t.wait_outcome = WaitOutcome::Woken;
        }
        self.place_home(id);
        Some(id)
    }

    pub(crate) fn wake_all(&mut self, wq: &mut WaitQueue) -> usize {
        let mut n = 0usize;
        while self.wake_one(wq).is_some() {
            n += 1;
        }
        n
    }

    fn unlink_wait(&mut self, id: ThreadId) {
        let cookie = match self.get(id).map(|t| t.state) {
            Some(ThreadState::Blocked { wq }) => wq,
            _ => 0,
        };
        if cookie != 0 {
            // SAFETY: invariant: a Blocked thread's `wq` cookie is the address
            // of the `WaitQueue` it waits on, which lives in a lock's model
            // that is touched only under SCHED, held here, and stays put
            // while a thread waits on it; established by
            // `thread_init::Sched::begin_wait`.
            unsafe { &mut *(cookie as *mut WaitQueue) }.remove(id);
        }
    }
}

static SCHED: SpinMutex<Sched> = SpinMutex::with_rank(Sched::empty(), RANK_SCHED);
static SPAWN_RR: AtomicU32 = AtomicU32::new(0);
/// Frames of the stacks every CPU's stack cache holds. A global atomic, not
/// a `PerCpu` field, so no CPU reads another's `PerCpu`.
static CACHED_STACK_FRAMES: AtomicUsize = AtomicUsize::new(0);
/// Dead threads' stacks parked in a `PerCpu.dead_stack` slot or on a dead
/// list: taken from their thread and not yet cached or freed.
static STACKS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// A new thread's first code. It starts at `irq_nest` + 1, IF=0 (the
/// first-run level `spawn_inner` adds), so a tick cannot switch away before
/// the switch tail has run; it drops that level, and turns IF on when the
/// nest reaches 0, before `entry`.
extern "C" fn trampoline() {
    finish_switch();
    per_cpu_init::irq_nest_leave();
    if per_cpu_init::irq_nest() == 0 {
        crate::x86::sti();
    }
    // SAFETY: invariant I9: the current thread's `Tcb` stays in `SCHED`,
    // and `entry` is written only before the thread first runs; established
    // by `thread_init::switch_now`, which sets the current thread.
    let entry = unsafe { (*per_cpu_init::current_thread()).entry };
    entry();
    thread_exit();
}

pub fn exit_current() -> ! {
    thread_exit();
}

#[allow(
    clippy::panic,
    reason = "invariant I9: a Dead thread is never placed on a run queue, so `schedule` never returns to it (`thread_init::runnable_on`)"
)]
fn thread_exit() -> ! {
    // IF-on-resume leaves IF set. Hold it off from the park to the switch:
    // a tick cannot preempt a thread whose stack is parked, and the switch
    // tail that takes the stack runs on this CPU after the switch.
    let _irq = InterruptGuard::enter();
    let p = per_cpu_init::current_thread();
    // SAFETY: `p` is this CPU's running thread (`per_cpu_init::set_current_thread`
    // in `thread_init::switch_now`), whose `Tcb` stays in `SCHED` (invariant
    // I9) and whose `stack` only this thread and `thread_init::spawn_inner`
    // write, and a spawn reuses the slot only once the state below is Dead.
    let stack = unsafe { (*p).stack.take() };
    if let Some(stack) = stack {
        // The store that makes the stack reclaimable. Only this CPU's
        // `finish_switch` takes it, after `switch_context` has moved this
        // CPU off it (invariant I10).
        per_cpu_init::with_current(|cpu| {
            assert!(
                cpu.dead_stack.is_none(),
                "thread_exit: dead-stack slot full"
            );
            cpu.dead_stack = Some(stack);
        });
        STACKS_IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
    }
    // Under SCHED, as every other state store; the slot stays unreusable
    // while `on_cpu` is set, until this CPU's switch tail clears it.
    with_sched(|_| {
        // SAFETY: invariant I9: `p` is this CPU's running thread, whose `Tcb`
        // stays in `SCHED`; SCHED, held here, orders this store with the
        // scan in `thread_init::spawn_inner`.
        unsafe { (*p).state = ThreadState::Dead };
    });
    #[cfg(feature = "kernel_tests")]
    testing::exit_stall();
    schedule();
    panic!("dead thread resumed");
}

/// Voluntary path. Returns when this thread is scheduled again.
pub fn yield_now() {
    schedule_inner(false);
}

pub fn schedule() {
    schedule_inner(false);
}

/// Timer / reschedule-IPI path. EOI already done. May switch; reaping
/// stays off the IRQ path.
pub fn schedule_preempt() {
    schedule_inner(true);
}

/// IF-on-resume is applied to the incoming TCB before `popfq`. See
/// [`apply_if_on_resume`]. `InterruptGuard` stays on the outgoing stack.
fn schedule_inner(from_irq: bool) {
    let _irq = InterruptGuard::enter();
    if !from_irq {
        sync_init::assert_not_hard_irq();
    }
    crate::ipi_init::drain_inbox();
    let now = Instant {
        ns: time_init::now_ns(),
    };
    let idle = per_cpu_init::current().idle_id;
    let cur = current_id();
    let me = per_cpu_init::current().cpu_id;

    let mut overdue = [ThreadId::NONE; 4];
    let mut n_overdue = 0usize;

    let cur_state = with_sched(|s| {
        let mut expired = [ThreadId::NONE; MAX_THREADS];
        let n = s.timeouts.pop_expired_into(now, &mut expired);
        let mut i = 0;
        while i < n {
            let id = expired[i];
            s.unlink_wait(id);
            if let Some(t) = s.get_mut(id) {
                match t.state {
                    ThreadState::Sleeping { .. } | ThreadState::Blocked { .. } => {
                        t.state = ThreadState::Ready;
                        t.wait_outcome = WaitOutcome::Timeout;
                    }
                    ThreadState::Ready | ThreadState::Running | ThreadState::Dead => {}
                }
            }
            s.place_home(id);
            i += 1;
        }

        if !from_irq {
            let ticks = per_cpu_init::current().remote.ticks.load(Ordering::Relaxed);
            if ticks.is_multiple_of(SWEEP_TICKS) {
                for t in s.timeouts.overdue(now) {
                    if n_overdue < overdue.len() {
                        overdue[n_overdue] = t.id;
                        n_overdue += 1;
                    }
                }
            }
        }

        let st = s.get(cur).map(|t| t.state).unwrap_or(ThreadState::Dead);
        if let Some(t) = s.get_mut(cur) {
            match t.state {
                ThreadState::Running => t.state = ThreadState::Ready,
                ThreadState::Ready
                | ThreadState::Sleeping { .. }
                | ThreadState::Blocked { .. }
                | ThreadState::Dead => {}
            }
        }
        st
    });

    if n_overdue != 0 {
        let mut i = 0;
        while i < n_overdue {
            crate::marker!("vibeOS: sched: overdue tid {}", overdue[i].raw());
            i += 1;
        }
    }

    let mut next = per_cpu_init::with_current(|cpu| {
        enqueue_runnable(&mut cpu.runq, cur, idle, cur_state);
        cpu.runq.remove(idle);
        take_next(&mut cpu.runq, idle)
    });

    // A run-queue entry says only that some wake put the thread here. The
    // wake's push reaches this CPU after its waker has dropped SCHED, so it
    // can land after the thread has already resumed through its own
    // `schedule`, which found it `Ready`, and has since blocked again or
    // exited; the entry is then stale. SCHED decides: run a thread only
    // while it is `Ready` and placed on this CPU, and drop any other entry.
    let (old_ptr, new_ptr, old_id, new_id) = loop {
        #[cfg(feature = "kernel_tests")]
        let cand = testing::requeue_next_cpu(next, cur, idle, me);
        #[cfg(not(feature = "kernel_tests"))]
        let cand = next;
        let picked = with_sched(|s| {
            if cand != idle && !s.get(cand).is_some_and(|t| runnable_on(t, me)) {
                return None;
            }
            if let Some(t) = s.get_mut(cand) {
                t.state = ThreadState::Running;
                t.cpu = me;
            }
            relink(s);
            Some((s.ptr(cur), s.ptr(cand), cur, cand))
        });
        match picked {
            Some(p) => break p,
            None => next = per_cpu_init::with_current(|cpu| take_next(&mut cpu.runq, idle)),
        }
    };

    if old_id == new_id || old_ptr.is_null() || new_ptr.is_null() {
        return;
    }
    assert!(!new_ptr.is_null(), "schedule: next vanished");
    switch_now(old_ptr, new_ptr);
    finish_switch();
}

/// Whether `t` may run on CPU `cpu` now: `Ready`, and placed there. A
/// thread is placed on one CPU at a time (`Sched::place_home`, `spawn_inner`,
/// the requeue hook), and `schedule_inner` sets it `Running` under SCHED
/// before the switch, so no other CPU's stale entry can run it meanwhile.
fn runnable_on(t: &Tcb, cpu: u32) -> bool {
    t.state == ThreadState::Ready && t.cpu == cpu
}

fn switch_now(old_ptr: *mut Tcb, new_ptr: *mut Tcb) {
    // SAFETY: invariant I9: the callers (`thread_init::schedule_inner`,
    // `thread_init::switch_to`) pass two live entries of `SCHED`, and `id`
    // changes only while a slot is Dead; established by
    // `thread_init::spawn_inner`.
    let (from, to) = unsafe { ((*old_ptr).id.0, (*new_ptr).id.0) };
    vibeos::trace!(Switch, u64::from(from), u64::from(to));
    #[cfg(feature = "kernel_tests")]
    if sync_init::held_mask() != 0 {
        // SAFETY: invariant I9: `new_ptr` is a live entry of `SCHED` that
        // `schedule_inner` or `switch_to` set Running for this CPU, and `id`
        // changes only while its slot is Dead; established by
        // `thread_init::spawn_inner`.
        testing::refuse_switch(unsafe { (*new_ptr).id });
    }
    sync_init::assert_switch_clean();
    let now = time_init::read_tsc();
    per_cpu_init::with_current_switch(|cpu| {
        // For the switch tail that runs next on this CPU.
        cpu.tail_prev = old_ptr;
        let delta = now.wrapping_sub(cpu.slice_tsc);
        cpu.slice_tsc = now;
        // Single writer: only this CPU stores its `switches`.
        let switches = cpu.remote.switches.load(Ordering::Relaxed);
        cpu.remote
            .switches
            .store(switches.wrapping_add(1), Ordering::Relaxed);
        // SAFETY: invariant I9: both TCBs stay in `SCHED`; `old_ptr` is this
        // CPU's running thread and `new_ptr` the one `schedule_inner` or
        // `switch_to` set Running for this CPU under SCHED, so no other CPU
        // writes these fields until the switch tail clears `on_cpu`;
        // established by `thread_init::schedule_inner`.
        unsafe {
            (*old_ptr).run_tsc = (*old_ptr).run_tsc.wrapping_add(delta);
            (*old_ptr).switches = (*old_ptr).switches.wrapping_add(1);
            if old_ptr == cpu.idle {
                cpu.idle_tsc = cpu.idle_tsc.wrapping_add(delta);
            }
            (*old_ptr).irq_nest = cpu.irq_nest.load(Ordering::Relaxed);
            cpu.irq_nest.store((*new_ptr).irq_nest, Ordering::Relaxed);
            apply_if_on_resume(
                &mut (*new_ptr).context.rflags,
                cpu.irq_nest.load(Ordering::Relaxed),
            );
            per_cpu_init::set_current_thread(cpu, new_ptr);
            (*new_ptr).on_cpu.set();
            on_switch(cpu, old_ptr, new_ptr);
        }
    });
    // SAFETY: both TCBs are live entries of `SCHED` that this CPU runs
    // or was given, established at `thread_init::schedule_inner` and
    // `thread_init::switch_to`, and IF=0 under their `InterruptGuard`,
    // which spans the switch; the `&mut PerCpu` above has ended (DESIGN
    // §7.5).
    unsafe {
        <crate::arch::current::Arch as vibeos::arch::ContextSwitch>::switch(
            &mut (*old_ptr).context,
            &(*new_ptr).context,
        )
    };
}

fn relink(s: &mut Sched) {
    per_cpu_init::with_current(|cpu| {
        let n = cpu.runq.len();
        let mut ids = [ThreadId::NONE; MAX_THREADS];
        let mut i = 0;
        while i < n {
            ids[i] = cpu.runq.at(i);
            i += 1;
        }
        i = 0;
        while i < n {
            let prev = if i == 0 { None } else { Some(ids[i - 1]) };
            let next = if i + 1 == n { None } else { Some(ids[i + 1]) };
            if let Some(t) = s.get_mut(ids[i]) {
                t.prev = prev;
                t.next = next;
            }
            i += 1;
        }
        let front = cpu.runq.front();
        let head = match front {
            Some(id) => s.ptr(id),
            None => core::ptr::null_mut(),
        };
        cpu.ready_head = head;
    });
}

/// The switch tail: runs on this CPU after every `switch_context` returns,
/// on both `schedule_inner` paths (the preempt one included), in
/// `switch_to`, and first thing in `trampoline`, with IF=0. It empties
/// this CPU's dead-stack slot: the stack goes into this CPU's stack cache,
/// or onto its dead list, whose worker it wakes. It never unmaps,
/// allocates, or sends a shootdown.
pub(crate) fn finish_switch() {
    debug_assert!(
        !crate::x86::interrupts_enabled(),
        "finish_switch with IF on"
    );
    #[cfg(feature = "kernel_tests")]
    crate::irq::ktest::tail_enter();
    #[cfg(feature = "kernel_tests")]
    testing::scan_dead_slot();
    let (prev, kick) = per_cpu_init::with_current(|cpu| {
        let prev = core::mem::replace(&mut cpu.tail_prev, core::ptr::null_mut());
        let Some(stack) = cpu.dead_stack.take() else {
            return (prev, false);
        };
        let pages = stack.pages();
        let refused = if pages == DEFAULT_STACK_PAGES {
            cpu.stack_cache.put(stack).err()
        } else {
            Some(stack)
        };
        match refused {
            None => {
                CACHED_STACK_FRAMES.fetch_add(pages, Ordering::AcqRel);
                STACKS_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
                (prev, false)
            }
            Some(stack) => {
                kva_init::park_on_list(&mut cpu.dead_list, stack);
                (prev, true)
            }
        }
    });
    if kick {
        kick_dead_stacks();
    }
    #[cfg(feature = "kernel_tests")]
    crate::irq::ktest::tail_leave();
    if !prev.is_null() {
        // SAFETY: `prev` is the TCB this CPU switched off (stored in
        // `tail_prev` by `thread_init::switch_now`), a live entry of `SCHED`
        // (invariant I9); `switch_context` has returned, so every save into
        // it is done. This Release store (`OnCpu::clear`) is this CPU's
        // last access to it: after it `spawn_inner` may rewrite the slot
        // (AGENTS rule 5).
        unsafe { (*prev).on_cpu.clear() };
    }
}

/// Take this CPU's whole dead list, for its worker. Owner CPU, IF=0.
/// Returns the list head, 0 when empty.
pub(crate) fn take_dead_stacks() -> u64 {
    per_cpu_init::with_current(|cpu| core::mem::replace(&mut cpu.dead_list, 0))
}

/// Free this CPU's dead list now, as its worker would: IF=1 only
/// (`kva_init::free_parked`). True if a stack was freed.
fn reclaim_dead_stacks_here() -> bool {
    if !crate::x86::interrupts_enabled() {
        return false;
    }
    let head = take_dead_stacks();
    if head == 0 {
        return false;
    }
    let n = kva_init::free_parked(head);
    stacks_reclaimed(n);
    n > 0
}

/// The worker has freed `n` stacks it took with [`take_dead_stacks`].
pub(crate) fn stacks_reclaimed(n: usize) {
    STACKS_IN_FLIGHT.fetch_sub(n, Ordering::AcqRel);
}

/// Frames the stack caches of every CPU hold.
#[cfg(feature = "kernel_tests")]
pub fn cached_stack_frames() -> usize {
    CACHED_STACK_FRAMES.load(Ordering::Acquire)
}

/// Dead threads' stacks not yet cached or freed.
#[cfg(feature = "kernel_tests")]
pub fn stacks_in_flight() -> usize {
    STACKS_IN_FLIGHT.load(Ordering::Acquire)
}

/// A default-size stack from this CPU's cache, zeroed, or `None`.
fn cached_stack() -> Option<GuardedStack> {
    let stack = per_cpu_init::with_current(|cpu| cpu.stack_cache.take())?;
    CACHED_STACK_FRAMES.fetch_sub(stack.pages(), Ordering::AcqRel);
    let len = stack.pages() * vibeos::paging::PAGE_SIZE_4K as usize;
    // SAFETY: the stack's `pages` pages above `base` are mapped writable
    // and owned by this handle alone: no thread runs on a cached stack
    // (invariant I10, established at `thread_init::finish_switch`).
    unsafe { core::ptr::write_bytes(stack.base().as_u64() as *mut u8, 0, len) };
    #[cfg(feature = "kernel_tests")]
    testing::refill_cached(&stack);
    Some(stack)
}

/// Keep a stack a failed spawn did not use: this CPU's cache if it has
/// room and the stack is default-size, else free it.
fn return_stack(stack: GuardedStack) {
    let pages = stack.pages();
    let refused = if pages == DEFAULT_STACK_PAGES {
        per_cpu_init::with_current(|cpu| cpu.stack_cache.put(stack).err())
    } else {
        Some(stack)
    };
    match refused {
        None => {
            CACHED_STACK_FRAMES.fetch_add(pages, Ordering::AcqRel);
        }
        Some(stack) => kva_init::free_stack(stack),
    }
}

/// `sti; hlt` with a closed lost-wakeup window: cli, drain inbox, recheck
/// runq, then `sti; hlt` as one pair. ROADMAP §4.8 / DESIGN §7.8.
pub fn halt_if_idle() {
    loop {
        // SAFETY: `cli` touches only IF; the `sti` below or the idle loop's
        // next pass turns it back on; established here.
        unsafe {
            core::arch::asm!("cli", options(nostack, preserves_flags));
        }
        crate::ipi_init::drain_inbox();
        if !per_cpu_init::current().runq.is_empty() {
            // SAFETY: `sti` restores the IF=1 this idle loop runs with;
            // established here.
            unsafe {
                core::arch::asm!("sti", options(nostack, preserves_flags));
            }
            return;
        }
        // SAFETY: `sti; hlt` as one pair: the interrupt shadow keeps a
        // wake-up IPI from landing between them (DESIGN §7.8); established
        // here.
        unsafe {
            core::arch::asm!("sti; hlt", options(nomem, nostack));
        }
    }
}

pub fn spawn(name: &'static str, entry: fn()) -> Result<ThreadHandle, SpawnError> {
    spawn_inner(
        name,
        entry,
        CpuAffinity::Any,
        true,
        0,
        0,
        0,
        DEFAULT_STACK_PAGES,
    )
}

/// Pin to this CPU: `switch_to` reaches only this CPU's run queue, so an
/// in-guest test that `switch_to`s a worker must spawn it here.
///
/// Copies the caller's `irq_nest`. A worker started from the IF=1 in-guest
/// registry runs with IF on; one started under a test's own guard runs with
/// IF off, so `switch_to` does not `sti` it and a tick cannot preempt a
/// cooperative `switch_to` chain.
#[cfg(feature = "kernel_tests")]
pub fn spawn_here(name: &'static str, entry: fn()) -> Result<ThreadHandle, SpawnError> {
    spawn_inner(
        name,
        entry,
        CpuAffinity::Pinned(current_cpu()),
        true,
        per_cpu_init::irq_nest(),
        0,
        0,
        DEFAULT_STACK_PAGES,
    )
}

pub fn spawn_on(name: &'static str, entry: fn(), cpu: u32) -> Result<ThreadHandle, SpawnError> {
    spawn_inner(
        name,
        entry,
        CpuAffinity::Pinned(cpu),
        true,
        0,
        0,
        0,
        DEFAULT_STACK_PAGES,
    )
}

/// Stack size and CPU for [`spawn_opts`].
#[cfg(feature = "kernel_tests")]
#[derive(Clone, Copy)]
pub struct SpawnOpts {
    /// Stack pages, without the guard page. `kva_init::alloc_guarded_stack`
    /// takes at most 32.
    pub stack_pages: usize,
    /// `Some(c)` pins the thread to CPU `c`, which must be online; `None`
    /// lets it run on any CPU.
    pub cpu: Option<u32>,
}

/// Spawn a Ready kernel thread with `opts`' stack size and CPU. It starts
/// with `irq_nest` 0, so it runs with IF on, as [`spawn`]'s threads do.
/// `opts.stack_pages` must be at most 32 (`kva_init`'s limit) and
/// `opts.cpu`, when set, an online CPU.
#[cfg(feature = "kernel_tests")]
pub fn spawn_opts(
    name: &'static str,
    entry: fn(),
    opts: SpawnOpts,
) -> Result<ThreadHandle, SpawnError> {
    let affinity = match opts.cpu {
        Some(c) => CpuAffinity::Pinned(c),
        None => CpuAffinity::Any,
    };
    spawn_inner(name, entry, affinity, true, 0, 0, 0, opts.stack_pages)
}

pub(crate) fn spawn_idle(entry: fn()) -> Result<ThreadHandle, SpawnError> {
    spawn_inner(
        "idle",
        entry,
        CpuAffinity::Pinned(0),
        false,
        0,
        0,
        0,
        DEFAULT_STACK_PAGES,
    )
}

/// User process thread. Not runnable until [`make_ready`]. `frame` is
/// the user frame its first return to ring 3 leaves from (DESIGN §5.10):
/// it goes at the top of the new stack, and the switch context starts
/// below it, so `entry` ends in `syscall_init::first_return`.
#[allow(
    clippy::panic,
    reason = "invariant: `spawn_inner` gives every thread it builds a stack, and a thread not yet `make_ready` never runs, so nothing takes it (`thread_init::spawn_inner`)"
)]
pub fn spawn_user(
    name: &'static str,
    entry: fn(),
    pid: u32,
    cr3: u64,
    frame: &UserFrame,
) -> Result<ThreadHandle, SpawnError> {
    let h = spawn_inner(
        name,
        entry,
        CpuAffinity::Pinned(current_cpu()),
        false,
        0,
        pid,
        cr3,
        DEFAULT_STACK_PAGES,
    )?;
    let tramp = trampoline as *const () as u64;
    with_sched(|s| {
        let tcb = s.get_mut(h.id());
        let top = tcb
            .as_ref()
            .and_then(|t| t.stack.as_ref())
            .map(|st| st.top().as_u64());
        let (Some(tcb), Some(top)) = (tcb, top) else {
            panic!("spawn_user: thread {} has no stack", h.id().0);
        };
        let at = top - USER_FRAME_BYTES as u64;
        // SAFETY: invariant I25: `[top - 168, top)` is the user frame of
        // this thread's own kernel stack, mapped and unused: the thread is
        // not runnable before `make_ready`, and nothing else refers to its
        // stack; established by `thread_init::spawn_inner`.
        unsafe { (at as *mut UserFrame).write(*frame) };
        // The pad word below the frame, as the syscall entry leaves it.
        prepare_thread(&mut tcb.context, at - 8, tramp);
        // SAFETY: invariant: `context.rsp` is 8 bytes below the pad word,
        // inside this thread's unused stack; established here.
        unsafe { (tcb.context.rsp as *mut u64).write_volatile(0) };
    });
    Ok(h)
}

const USER_FRAME_BYTES: usize = core::mem::size_of::<UserFrame>();

pub fn make_ready(id: ThreadId) {
    with_sched(|s| {
        if let Some(t) = s.get_mut(id) {
            if t.state == ThreadState::Dead {
                return;
            }
            t.state = ThreadState::Ready;
        }
        s.place_home(id);
    });
}

/// AP idle: running on `stack` already. No synthetic frame, not on the FIFO.
/// `Err(stack)` hands the stack back when no TCB slot is free.
#[allow(
    clippy::result_large_err,
    reason = "the stack comes back by value so the caller frees it once; a Box would allocate on the failure path"
)]
pub fn adopt_ap_idle(cpu_id: u32, stack: GuardedStack) -> Result<ThreadId, GuardedStack> {
    // Built without the stack, which the AP is running on: a failed
    // allocation must not drop it.
    let tcb = TryBox::try_new(Tcb {
        id: ThreadId(0),
        name: "idle",
        state: ThreadState::Running,
        on_cpu: OnCpu::new_set(),
        stack: None,
        context: CpuContext::empty(),
        entry: ap_idle_entry,
        next: None,
        prev: None,
        affinity: CpuAffinity::Pinned(cpu_id),
        cpu: cpu_id,
        irq_nest: 0,
        switches: 0,
        run_tsc: 0,
        wait_outcome: WaitOutcome::Woken,
        as_cr3: 0,
        fpu: fpu_template(),
        fp_cpu: None,
        syscall_count: 0,
        pid: 0,
        no_reclaim: AtomicU32::new(0),
    });
    let Ok(mut tcb) = tcb else {
        return Err(stack);
    };
    let placed = with_sched(|s| {
        let Some(slot) = s.slots.iter().position(|x| x.is_none()) else {
            // Dropped after SCHED is released: no heap free under it.
            return Err((tcb, stack));
        };
        let Some(raw) = s.alloc_id() else {
            return Err((tcb, stack));
        };
        let id = ThreadId(raw);
        tcb.id = id;
        tcb.stack = Some(stack);
        s.slots[slot] = Some(tcb);
        s.bind(id, slot);
        Ok(id)
    });
    match placed {
        Ok(id) => Ok(id),
        Err((tcb, stack)) => {
            drop(tcb);
            Err(stack)
        }
    }
}

/// Timeout path: TCB never ran. Return the stack so the caller can free it.
pub fn abandon_ap_idle(id: ThreadId) -> Option<GuardedStack> {
    with_sched(|s| {
        s.timeouts.remove(id);
        let t = s.get_mut(id)?;
        t.state = ThreadState::Dead;
        t.stack.take()
    })
}

#[allow(
    clippy::panic,
    reason = "invariant: an AP idle TCB is adopted Running and never started, so nothing enters its `entry` (`thread_init::adopt_ap_idle`)"
)]
fn ap_idle_entry() {
    panic!("ap idle entry called");
}

fn choose_cpu(affinity: CpuAffinity) -> u32 {
    let mask = per_cpu_init::online_mask();
    let mut rr = SPAWN_RR.load(Ordering::Relaxed);
    let cpu = pick_cpu(affinity, mask, &mut rr);
    SPAWN_RR.store(rr, Ordering::Relaxed);
    cpu
}

/// Build a Ready (or, `enqueue` false, parked) thread. The stack comes
/// first, then a TCB slot: a `Dead` one is rewritten under SCHED, else a new
/// `Tcb` box goes into an empty one. Neither the box nor the stack is
/// allocated or freed under SCHED, which ranks above HEAP, PT and BUDDY.
#[allow(clippy::too_many_arguments)] // TCB fields and stack size chosen at spawn
fn spawn_inner(
    name: &'static str,
    entry: fn(),
    affinity: CpuAffinity,
    enqueue: bool,
    irq_nest: u32,
    pid: u32,
    as_cr3: u64,
    stack_pages: usize,
) -> Result<ThreadHandle, SpawnError> {
    #[cfg(feature = "kernel_tests")]
    if current_pid() != 0 && testing::FAIL_FORK_STACK.swap(false, Ordering::AcqRel) {
        return Err(SpawnError::NoMemory);
    }
    let cached = if stack_pages == DEFAULT_STACK_PAGES {
        cached_stack()
    } else {
        None
    };
    let stack = match cached {
        Some(s) => s,
        None => match kva_init::alloc_guarded_stack(stack_pages) {
            Ok(s) => s,
            // Once: an exit burst can leave this CPU's dead list holding
            // enough stacks for KVA to refuse (`Kva::alloc`'s live cap).
            Err(_) if reclaim_dead_stacks_here() => {
                kva_init::alloc_guarded_stack(stack_pages).map_err(|_| SpawnError::NoMemory)?
            }
            Err(_) => return Err(SpawnError::NoMemory),
        },
    };
    let top = stack.top().as_u64();
    assert!(top.is_multiple_of(16), "kva stack top not 16-aligned");
    // Taken by whichever path below installs it in a TCB.
    let mut stack = Some(stack);
    let tramp = trampoline as *const () as u64;
    let cpu = choose_cpu(affinity);
    // The first-run level: `trampoline` runs the switch tail with IF=0 and
    // then drops it.
    let first_nest = irq_nest + 1;

    // Dead slot: rewrite the Box. 2000 spawn/exit must not churn the
    // heap (one extra mapped page shows up as a leaked frame). A Dead
    // thread whose CPU has not finished switching off it still has
    // `on_cpu` set; its CPU clears it with Release (`finish_switch`).
    // The Dead TCB's old tid leaves the index and the timeout queue and
    // goes back to the allocator; the new thread takes a new tid.
    let reused = with_sched(|s| {
        let slot = s.slots.iter().position(|x| match x.as_ref() {
            Some(t) => t.state == ThreadState::Dead && t.on_cpu.is_clear(),
            None => false,
        })?;
        let old = s.slots[slot].as_deref()?.id;
        let ks = stack.take()?;
        let Some(raw) = s.new_tid(pid) else {
            stack = Some(ks);
            return Some(Err(SpawnError::NoSlot));
        };
        let id = ThreadId(raw);
        s.unbind(old);
        let tcb = s.slots[slot].as_deref_mut()?;
        assert!(tcb.stack.is_none(), "dead tcb still owns stack");
        tcb.id = id;
        fill_tcb(
            tcb, name, entry, affinity, cpu, ks, top, tramp, first_nest, pid, as_cr3,
        );
        s.bind(id, slot);
        if enqueue {
            s.place(cpu, id);
        }
        Some(Ok(id))
    });
    match reused {
        Some(Ok(id)) => return Ok(ThreadHandle { id }),
        Some(Err(e)) => {
            if let Some(ks) = stack.take() {
                return_stack(ks);
            }
            return Err(e);
        }
        None => {}
    }

    // Built without the stack, so a failed allocation drops no stack; the
    // stack goes back as a full table's does (DESIGN §4.4).
    let tcb = TryBox::try_new(Tcb {
        id: ThreadId(0),
        name,
        state: ThreadState::Ready,
        on_cpu: OnCpu::new(),
        stack: None,
        context: CpuContext::empty(),
        entry,
        next: None,
        prev: None,
        affinity,
        cpu,
        irq_nest: first_nest,
        switches: 0,
        run_tsc: 0,
        wait_outcome: WaitOutcome::Woken,
        as_cr3,
        fpu: fpu_template(),
        fp_cpu: None,
        syscall_count: 0,
        pid,
        no_reclaim: AtomicU32::new(0),
    });
    let mut tcb = match tcb {
        Ok(t) => t,
        Err(_) => {
            if let Some(ks) = stack.take() {
                return_stack(ks);
            }
            return Err(SpawnError::NoMemory);
        }
    };
    tcb.stack = stack;
    prepare_thread(&mut tcb.context, top, tramp);
    // SAFETY: `prepare_thread` put `context.rsp` 8 bytes below `top`,
    // inside the thread's own stack, mapped and not yet run on; established
    // by `vibeos::thread::prepare_thread`.
    unsafe { (tcb.context.rsp as *mut u64).write_volatile(0) };

    let placed = with_sched(|s| {
        let Some(slot) = s.slots.iter().position(|x| x.is_none()) else {
            // Dropped after SCHED is released: no heap free under it.
            return Err(tcb);
        };
        let Some(raw) = s.new_tid(pid) else {
            return Err(tcb);
        };
        let id = ThreadId(raw);
        tcb.id = id;
        s.slots[slot] = Some(tcb);
        s.bind(id, slot);
        if enqueue {
            s.place(cpu, id);
        }
        Ok(id)
    });
    match placed {
        Ok(id) => Ok(ThreadHandle { id }),
        Err(mut tcb) => {
            if let Some(ks) = tcb.stack.take() {
                return_stack(ks);
            }
            drop(tcb);
            Err(SpawnError::NoSlot)
        }
    }
}

/// A write to `tcb.fpu` makes the saved image the thread's state: no
/// CPU's registers hold it any more, so its next return to user mode
/// loads what was written (DESIGN §7.5, C-FPBIND).
pub fn fp_invalidate(tcb: &mut Tcb) {
    vibeos::fpu::invalidate(&mut tcb.fp_cpu);
}

#[allow(clippy::too_many_arguments)] // TCB fields filled at spawn
fn fill_tcb(
    tcb: &mut Tcb,
    name: &'static str,
    entry: fn(),
    affinity: CpuAffinity,
    cpu: u32,
    ks: GuardedStack,
    top: u64,
    tramp: u64,
    irq_nest: u32,
    pid: u32,
    as_cr3: u64,
) {
    tcb.name = name;
    tcb.state = ThreadState::Ready;
    tcb.stack = Some(ks);
    tcb.entry = entry;
    tcb.next = None;
    tcb.prev = None;
    tcb.affinity = affinity;
    tcb.cpu = cpu;
    tcb.irq_nest = irq_nest;
    tcb.switches = 0;
    tcb.run_tsc = 0;
    tcb.wait_outcome = WaitOutcome::Woken;
    tcb.as_cr3 = as_cr3;
    tcb.fpu = fpu_template();
    // A reused TCB address: no CPU's `fp_owner` may match it.
    fp_invalidate(tcb);
    tcb.syscall_count = 0;
    tcb.pid = pid;
    prepare_thread(&mut tcb.context, top, tramp);
    // SAFETY: `prepare_thread` put `context.rsp` 8 bytes below `top`,
    // inside the thread's own stack, mapped and not yet run on; established
    // by `vibeos::thread::prepare_thread`.
    unsafe { (tcb.context.rsp as *mut u64).write_volatile(0) };
}

pub fn sleep_ms(ms: u64) {
    let ns = time_init::now_ns().saturating_add(ms.saturating_mul(1_000_000));
    park(Some(Instant { ns }));
}

/// Park until `deadline` (or a far-future sentinel). Sleep path only.
pub fn park(deadline: Option<Instant>) {
    sync_init::might_sleep();
    let d = effective_deadline(deadline);
    let id = current_id();
    with_sched(|s| {
        if let Some(t) = s.get_mut(id) {
            t.state = ThreadState::Sleeping { deadline: d };
        }
        s.timeouts.insert(id, d);
    });
    schedule();
}

/// Hold SCHED for `f`. IF is off for the whole call (IRQ-aware lock).
#[cfg(feature = "kernel_tests")]
pub fn with_sched_lock<R>(f: impl FnOnce() -> R) -> R {
    let _g = SCHED.lock();
    f()
}

pub(crate) fn with_sched<R>(f: impl FnOnce(&mut Sched) -> R) -> R {
    let ctx = sync_init::sleep_ctx();
    // Dropped last, once the places are delivered.
    #[cfg(feature = "kernel_tests")]
    let _window = testing::window_enter();
    let (r, places, n) = {
        let mut s = SCHED.lock();
        s.waiter = ctx;
        let r = f(&mut s);
        let n = s.place_n;
        let p = s.places;
        s.place_n = 0;
        (r, p, n)
    };
    let mut i = 0;
    while i < n {
        #[cfg(feature = "kernel_tests")]
        testing::place_stall(places[i].1);
        let (cpu, id, slot) = places[i];
        crate::ipi_init::place_ready(cpu, id, slot as usize);
        i += 1;
    }
    r
}

/// Why the current thread's last wait ended. Valid after `schedule`
/// returns from a wait.
pub fn last_wait_outcome() -> WaitOutcome {
    let p = per_cpu_init::current_thread();
    assert!(!p.is_null(), "no current thread");
    // SAFETY: invariant I9: the current thread's `Tcb` stays in `SCHED`,
    // and only this thread and the SCHED holder that wakes it write
    // `wait_outcome`, before it runs again; established by
    // `thread_init::Sched::wake_one`.
    unsafe { (*p).wait_outcome }
}

/// Test helper. Local CPU only — the target must already sit on this
/// runq (spawn_here). Same nest-swap as `schedule`.
#[cfg(feature = "kernel_tests")]
#[allow(
    clippy::expect_used,
    reason = "invariant I9: a TCB slot once filled is never emptied, so an id `spawn_here` returned always names one (`thread_init::spawn_inner`)"
)]
pub fn switch_to(id: ThreadId) {
    let _irq = InterruptGuard::enter();
    crate::ipi_init::drain_inbox();
    let old_id = current_id();
    assert!(old_id != id, "switch_to self");
    let idle = per_cpu_init::current().idle_id;
    let me = per_cpu_init::current().cpu_id;

    {
        let s = SCHED.lock();
        let t = s.get(id).expect("switch_to unknown id");
        assert!(t.state != ThreadState::Dead, "switch_to dead thread");
        assert!(t.cpu == me, "switch_to remote cpu");
    }

    let cur_state = {
        let s = SCHED.lock();
        s.get(old_id).map(|t| t.state).unwrap_or(ThreadState::Dead)
    };
    per_cpu_init::with_current(|cpu| {
        cpu.runq.remove(id);
        enqueue_runnable(&mut cpu.runq, old_id, idle, cur_state);
    });
    let (old_ptr, new_ptr) = with_sched(|s| {
        if let Some(t) = s.get_mut(old_id)
            && t.state != ThreadState::Dead
            && old_id != idle
        {
            t.state = ThreadState::Ready;
        }
        if let Some(t) = s.get_mut(id) {
            t.state = ThreadState::Running;
            t.cpu = me;
        }
        relink(s);
        (s.ptr(old_id), s.ptr(id))
    });
    switch_now(old_ptr, new_ptr);
    finish_switch();
}

pub fn current_id() -> ThreadId {
    let p = per_cpu_init::current_thread();
    assert!(!p.is_null(), "no current thread");
    // SAFETY: invariant I9: the current thread's `Tcb` stays in `SCHED`, and
    // `id` changes only while its slot is Dead, never while it runs;
    // established by `thread_init::spawn_inner`.
    unsafe { (*p).id }
}

pub fn current_pid() -> u32 {
    let p = per_cpu_init::current_thread();
    if p.is_null() {
        0
    } else {
        // SAFETY: invariant I9: the current thread's `Tcb` stays in `SCHED`,
        // and `pid` is written only under SCHED by `set_pid_cr3` or a spawn,
        // a word-sized store this read cannot tear; established by
        // `thread_init::set_pid_cr3`.
        unsafe { (*p).pid }
    }
}

pub fn set_pid_cr3(id: ThreadId, pid: u32, cr3: u64) {
    with_sched(|s| {
        if let Some(t) = s.get_mut(id) {
            t.pid = pid;
            t.as_cr3 = cr3;
        }
    });
}

/// The first TCB, Dead ones included, whose saved root is `root`. Reads
/// only; `addr_space_init::teardown` asks it before it frees a root.
pub(crate) fn tcb_naming_root(root: u64) -> Option<ThreadId> {
    with_sched(|s| {
        s.slots
            .iter()
            .flatten()
            .find(|t| t.as_cr3 & vibeos::paging::PTE_ADDR_MASK == root)
            .map(|t| t.id)
    })
}

pub fn current_cpu() -> u32 {
    per_cpu_init::current().cpu_id
}

#[cfg(feature = "kernel_tests")]
pub fn current_tcb() -> *mut Tcb {
    let p = per_cpu_init::current_thread();
    assert!(!p.is_null(), "no current thread");
    p
}

#[cfg(feature = "kernel_tests")]
pub use testing::{cpu_of, exited, name, state, try_state};

pub fn tcb_ptr(id: ThreadId) -> *mut Tcb {
    SCHED.lock().ptr(id)
}

#[derive(Clone, Copy)]
pub struct ThreadInfo {
    pub id: ThreadId,
    pub name: &'static str,
    pub state: ThreadState,
    pub cpu: u32,
}

/// Snapshot under SCHED, then drop the lock. `ps` must not hold SCHED
/// across console writes.
pub fn snapshot(out: &mut [ThreadInfo]) -> usize {
    with_sched(|s| {
        let mut n = 0usize;
        let mut i = 0usize;
        while i < MAX_THREADS && n < out.len() {
            if let Some(t) = s.slots[i].as_ref() {
                out[n] = ThreadInfo {
                    id: t.id,
                    name: t.name,
                    state: t.state,
                    cpu: t.cpu,
                };
                n += 1;
            }
            i += 1;
        }
        n
    })
}

/// Hooks the in-guest tests arm (DESIGN §8.2). `kernel_tests` builds only.
#[cfg(feature = "kernel_tests")]
pub mod testing;
