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

use crate::arch::current::UserFrame;
use vibeos::arch::ContextSwitch;
use vibeos::desc::UserSegs;
use vibeos::ipi::{home_cpu, pick_cpu};
use vibeos::kalloc::{AllocError, TryBox, TryVec};
use vibeos::kva::DEFAULT_STACK_PAGES;
use vibeos::limits::{self, PID_MAX};
use vibeos::lock::RANK_SCHED;
use vibeos::per_cpu::PerCpu;
use vibeos::proc::pid::{IdIndex, PidAlloc};
use vibeos::sched::{TimeoutQueue, effective_deadline, enqueue_runnable, take_next};
#[cfg(target_arch = "x86_64")]
use vibeos::thread::prepare_thread;
use vibeos::thread::{
    CpuAffinity, CpuContext, Fxsave, MAX_THREADS, OnCpu, Tcb, ThreadId, ThreadState, WaitOutcome,
};
use vibeos::time::Instant;
use vibeos::wait::{WaitLink, WaitLinks, WaitQueue};

use crate::arch::current::{Arch, InterruptGuard};
use crate::cell::BootCell;
use crate::kva_init::{self, GuardedStack};
use crate::per_cpu_init;
use crate::sync_init::{self, SleepCtx, SpinMutex};
use crate::time_init;

mod ap;
mod boot;
mod idle;
mod sweep;
mod table;
mod user;
pub use ap::{abandon_unstarted, adopt_ap_idle};
#[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
pub(crate) use boot::BOOT_STACK_PAGES;
#[cfg(target_arch = "x86_64")]
pub(crate) use boot::bootstrap_stack;
pub use boot::init_bootstrap;
pub use idle::halt_if_idle;
pub use sweep::start_sweep;
#[cfg(all(feature = "kernel_tests", target_arch = "aarch64"))]
pub(crate) use table::report_stack_depth;
#[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
pub(crate) use table::scan_live_stacks;
pub(crate) use table::table_root;
#[cfg(feature = "kernel_tests")]
pub(crate) use table::table_usage;
#[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
pub(crate) use table::{RunTsc, run_tsc_snapshot, timeouts_capacity};
use table::{dead_reusable, slot_reusable};
pub use table::{each_thread, init_tables};
pub use user::{reset_user_segs, set_tls_base, set_user_segs};

// The syscall layer's hooks (DESIGN §1.2), which `syscall_init::init_bsp`
// sets before the scheduler runs a second thread.
/// The hardware side of a context switch (FPU, RSP0, CR3):
/// `syscall_init::on_switch`. Unset, a switch changes none of them.
static SWITCH_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Install the context-switch hook, which has `syscall_init::on_switch`'s
/// `# Safety` contract.
pub fn set_switch_hooks(on_switch: unsafe fn(&mut PerCpu, *mut Tcb, *mut Tcb)) {
    // Release: pairs with the Acquire load in `on_switch`.
    SWITCH_HOOK.store(on_switch as *mut (), Ordering::Release);
}

/// Run the switch hook.
///
/// # Safety
/// `syscall_init::on_switch`'s contract: `old` and `new` are null or live
/// TCBs, `new` runs next on this CPU, IF=0, and `cpu` is this CPU's
/// `PerCpu`.
unsafe fn on_switch(cpu: &mut PerCpu, old: *mut Tcb, new: *mut Tcb) {
    // Acquire: pairs with the Release store in `set_switch_hooks`.
    let p = SWITCH_HOOK.load(Ordering::Acquire);
    if p.is_null() {
        return;
    }
    // SAFETY: invariant: a non-null `SWITCH_HOOK` holds an
    // `unsafe fn(&mut PerCpu, *mut Tcb, *mut Tcb)`; established by
    // `thread_init::set_switch_hooks`, its only store.
    let f =
        unsafe { core::mem::transmute::<*mut (), unsafe fn(&mut PerCpu, *mut Tcb, *mut Tcb)>(p) };
    // SAFETY: the hook's contract is this fn's `# Safety`, which
    // `thread_init::switch_now` establishes.
    unsafe { f(cpu, old, new) };
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
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnError {
    /// Every TCB slot holds a live thread.
    NoSlot,
    /// The kernel stack could not be allocated.
    NoMemory,
}

/// Linux's errno for a thread `fork` or a new process could not get: `EAGAIN` for a full thread
/// table, `ENOMEM` for a kernel stack.
impl From<SpawnError> for vibeos::kerror::KError {
    fn from(e: SpawnError) -> Self {
        match e {
            SpawnError::NoSlot => Self::Again,
            SpawnError::NoMemory => Self::NoMem,
        }
    }
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

/// The scheduler's tables. `slots`, `places`, `links` and `timeouts` are
/// heap tables of `limits::MAX_THREADS` entries, which [`init_tables`]
/// allocates before `init_bootstrap` and nothing grows, moves or frees
/// after (ROADMAP §10.4, D1); [`Sched::empty`] holds none.
pub(crate) struct Sched {
    slots: TryVec<Option<TryBox<Tcb>>>,
    /// Tids, shared with pids (`proc_init`): a tid is never a slot index.
    pids: PidAlloc,
    /// Each live TCB's tid to its slot in `slots`.
    index: IdIndex<TID_INDEX_CAP>,
    timeouts: TimeoutQueue,
    /// Each slot's thread's links in the wait queue it is on
    /// (`wait::WaitLinks`).
    links: TryVec<WaitLink>,
    /// Wakes recorded under SCHED, placed after it drops: (cpu, id, slot),
    /// `place_head..place_n` still to place.
    places: TryVec<(u32, ThreadId, u32)>,
    place_head: usize,
    place_n: usize,
    /// The context the current `with_sched` section was entered from, which
    /// `begin_wait` checks: it runs under SCHED with IF off and cannot read
    /// IF or `HELD` itself.
    waiter: SleepCtx,
}

/// Each thread-table slot's tid, `u32::MAX` for none: what a wake-inbox
/// bit (a slot) names when its CPU drains it ([`tid_of_slot`]). Stored with
/// Release under SCHED ([`publish_slot`]) whenever a slot takes a TCB. A
/// table of `limits::MAX_THREADS` words, set by [`init_tables`].
static SLOT_TID: BootCell<TryVec<AtomicU32>> = BootCell::new();

/// Record that `slot` now holds thread `id`. Under SCHED.
fn publish_slot(slot: usize, id: ThreadId) {
    if let Some(t) = SLOT_TID.try_get().and_then(|v| v.get(slot)) {
        // Release: pairs with the Acquire load in `tid_of_slot`.
        t.store(id.0, Ordering::Release);
    }
}

/// The tid in thread-table slot `slot`, for `ipi_init::drain_inbox`. A
/// slot's inbox bit can outlive the wake that set it: the push lands after
/// the waker drops SCHED, by which time the thread may have run, exited,
/// and had its slot reused. The drain then queues the slot's new thread,
/// which `schedule_inner` runs only while it is `Ready` and placed on that
/// CPU; a thread spawned parked is not `Ready` until [`make_ready`].
pub(crate) fn tid_of_slot(slot: usize) -> Option<ThreadId> {
    // Acquire: pairs with the Release store in `publish_slot`.
    let raw = SLOT_TID.try_get()?.get(slot)?.load(Ordering::Acquire);
    (raw != u32::MAX).then_some(ThreadId(raw))
}

impl Sched {
    const fn empty() -> Self {
        Self {
            slots: TryVec::new(),
            pids: PidAlloc::new(PID_MAX),
            index: IdIndex::new(),
            timeouts: TimeoutQueue::empty(),
            links: TryVec::new(),
            places: TryVec::new(),
            place_head: 0,
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
        if let Some(l) = self.links.get_mut(slot) {
            *l = WaitLink::NONE;
        }
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
        let Some(slot) = self.slot_of(id) else {
            return;
        };
        // A placed thread is Ready and cannot be woken again before its
        // place is delivered and it runs, so each pending entry is a
        // distinct live thread and `places`, one entry per slot, has room.
        if let Some(p) = self.places.get_mut(self.place_n) {
            *p = (cpu, id, slot as u32);
            self.place_n += 1;
        }
    }

    /// Move up to `out.len()` recorded wakes into `out`, oldest first; how
    /// many.
    fn take_places(&mut self, out: &mut [(u32, ThreadId, u32)]) -> usize {
        let mut n = 0usize;
        while n < out.len() && self.place_head < self.place_n {
            if let (Some(o), Some(p)) = (out.get_mut(n), self.places.get(self.place_head)) {
                *o = *p;
            }
            self.place_head += 1;
            n += 1;
        }
        if self.place_head == self.place_n {
            self.place_head = 0;
            self.place_n = 0;
        }
        n
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
        wq.enqueue(id, self);
        self.timeouts.insert(id, deadline);
        let cookie = wq.cookie();
        if let Some(t) = self.get_mut(id) {
            t.state = ThreadState::Blocked {
                wq: cookie,
                deadline,
            };
            t.wait_outcome = WaitOutcome::Woken;
        }
    }

    pub(crate) fn wake_one(&mut self, wq: &mut WaitQueue) -> Option<ThreadId> {
        let id = wq.dequeue(self)?;
        self.timeouts.remove(id);
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
            Some(ThreadState::Blocked { wq, .. }) => wq,
            _ => 0,
        };
        if cookie != 0 {
            // The cookie's provenance was exposed from `&mut WaitQueue`
            // (`WaitQueue::cookie`), so the rebuilt pointer may write.
            let wq = core::ptr::with_exposed_provenance_mut::<WaitQueue>(cookie);
            // SAFETY: invariant: a Blocked thread's `wq` cookie is the address
            // of the `WaitQueue` it waits on, which lives in a lock's model
            // that is touched only under SCHED, held here, and stays put
            // while a thread waits on it; established by
            // `thread_init::Sched::begin_wait`.
            unsafe { &mut *wq }.remove(id, self);
        }
    }
}

/// A thread's wait-queue link is its slot's entry in `links`.
impl WaitLinks for Sched {
    fn link(&mut self, id: ThreadId) -> Option<&mut WaitLink> {
        let slot = self.slot_of(id)?;
        self.links.get_mut(slot)
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
        crate::arch::current::irq_enable();
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
        // AcqRel: pairs with the Acquire load in `stacks_in_flight` and the other updates.
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
    #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
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

/// Expired timeouts `schedule_inner` takes per pass, and recorded wakes
/// `with_sched` places per lock hold: small batches, so neither frame
/// scales with `limits::MAX_THREADS`.
const EXPIRE_BATCH: usize = 32;
const PLACE_BATCH: usize = 32;

/// IF-on-resume is applied to the incoming TCB before `popfq` /
/// `msr daif`. See [`vibeos::arch::ContextSwitch::resume_with_irqs`].
/// `InterruptGuard` stays on the outgoing stack.
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

    let cur_state = with_sched(|s| {
        // In batches, so the frame stays small on top of a preempted
        // syscall body's stack whatever the thread count.
        let mut expired = [ThreadId::NONE; EXPIRE_BATCH];
        loop {
            let n = s.timeouts.pop_expired_into(now, &mut expired);
            for &id in expired.iter().take(n) {
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
            }
            if n < EXPIRE_BATCH {
                break;
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
        #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
        let cand = testing::requeue_next_cpu(next, cur, idle, me);
        #[cfg(not(all(feature = "kernel_tests", target_arch = "x86_64")))]
        let cand = next;
        let picked = with_sched(|s| {
            if cand != idle && !s.get(cand).is_some_and(|t| runnable_on(t, me)) {
                return None;
            }
            if let Some(t) = s.get_mut(cand) {
                t.state = ThreadState::Running;
                t.cpu = me;
            }
            Some((s.ptr(cur), s.ptr(cand), cur, cand))
        });
        match picked {
            Some(p) => break p,
            None => next = per_cpu_init::with_current(|cpu| take_next(&mut cpu.runq, idle)),
        }
    };

    if old_id == new_id || old_ptr.is_null() || new_ptr.is_null() {
        // Nothing else to run: a new quantum, so the next tick does not
        // preempt again at once.
        let now = time_init::read_tsc();
        per_cpu_init::with_current(|cpu| cpu.quantum_tsc = now);
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
    #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
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
        cpu.quantum_tsc = now;
        // Relaxed: only this CPU stores its `switches`; pairs with nothing.
        let switches = cpu.remote.switches.load(Ordering::Relaxed);
        // Relaxed: as the load; pairs with nothing.
        cpu.remote
            .switches
            .store(switches.wrapping_add(1), Ordering::Relaxed);
        // SAFETY: invariant I9: both TCBs stay in `SCHED`; `old_ptr` is this
        // CPU's running thread and `new_ptr` the one `schedule_inner` or
        // `switch_to` set Running for this CPU under SCHED, so no other CPU
        // writes these fields until the switch tail clears `on_cpu`. The
        // same facts, IF=0 under the callers' guard, and `cpu` being this
        // CPU's `PerCpu` from `with_current_switch` meet `on_switch`'s
        // `# Safety`. Established by `thread_init::schedule_inner`.
        unsafe {
            (*old_ptr).run_tsc = (*old_ptr).run_tsc.wrapping_add(delta);
            (*old_ptr).switches = (*old_ptr).switches.wrapping_add(1);
            if old_ptr == cpu.idle {
                cpu.idle_tsc = cpu.idle_tsc.wrapping_add(delta);
            }
            // Relaxed: only this CPU changes its `irq_nest`; pairs with nothing.
            (*old_ptr).irq_nest = cpu.irq_nest.load(Ordering::Relaxed);
            // Relaxed: as above; pairs with nothing.
            cpu.irq_nest.store((*new_ptr).irq_nest, Ordering::Relaxed);
            // Relaxed: as above; pairs with nothing.
            Arch::resume_with_irqs(
                &mut (*new_ptr).context,
                cpu.irq_nest.load(Ordering::Relaxed) == 0,
            );
            per_cpu_init::set_current_thread(cpu, new_ptr);
            (*new_ptr).on_cpu.set();
            // A user thread's ring-3 DS, ES, FS and GS (DESIGN §7.5). Save
            // the outgoing TLS base before `on_switch` and before a
            // selector load, which can zero `FS_BASE`.
            if (*old_ptr).pid != 0 {
                (*old_ptr).user_segs = crate::arch::gdt::read_user_segs();
                (*old_ptr).tls_base = crate::arch::current::user_tls();
            }
            on_switch(cpu, old_ptr, new_ptr);
            if (*new_ptr).pid != 0 {
                crate::arch::gdt::load_user_segs((*new_ptr).user_segs);
                // SAFETY: `tls_base` is 0 or the image's thread pointer,
                // a user address `setup_tls` chose; established by
                // `user_init::setup_tls` and `on_switch`.
                crate::arch::current::set_user_tls((*new_ptr).tls_base);
            }
        }
    });
    // The switch asm turns IF on for a thread whose `irq_nest` is 0.
    // SAFETY: invariant I9, as above: `new_ptr` is a live entry of `SCHED`
    // this CPU was given; established by `thread_init::spawn_inner`.
    if unsafe { (*new_ptr).irq_nest } == 0 {
        crate::sched::irqoff::on();
    }
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

/// The switch tail: runs on this CPU after every `switch_context` returns,
/// on both `schedule_inner` paths (the preempt one included), in
/// `switch_to`, and first thing in `trampoline`, with IF=0. It empties
/// this CPU's dead-stack slot: the stack goes into this CPU's stack cache,
/// or onto its dead list, whose worker it wakes. It never unmaps,
/// allocates, or sends a shootdown.
pub(crate) fn finish_switch() {
    debug_assert!(
        !crate::arch::current::interrupts_enabled(),
        "finish_switch with IF on"
    );
    #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
    crate::irq::ktest::tail_enter();
    #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
    testing::scan_dead_slot();
    let (prev, kick) = per_cpu_init::with_current(|cpu| {
        let prev = core::mem::replace(&mut cpu.tail_prev, core::ptr::null_mut());
        (prev, cpu.dead_stack.is_some() && retire_dead_stack(cpu))
    });
    if kick {
        kick_dead_stacks();
    }
    #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
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
    if !crate::arch::current::interrupts_enabled() {
        return false;
    }
    let head = take_dead_stacks();
    if head == 0 {
        return false;
    }
    // SAFETY: invariant I10, established at `thread_init::finish_switch`:
    // `head` is this CPU's whole dead list, which only its switch tail
    // fills with stacks no CPU runs on.
    let n = unsafe { kva_init::free_parked(head) };
    stacks_reclaimed(n);
    n > 0
}

/// The worker has freed `n` stacks it took with [`take_dead_stacks`].
pub(crate) fn stacks_reclaimed(n: usize) {
    // AcqRel: pairs with the Acquire load in `stacks_in_flight` and the other updates.
    STACKS_IN_FLIGHT.fetch_sub(n, Ordering::AcqRel);
}

/// The switch tail's work on `cpu`'s dead-stack slot, which holds a stack:
/// move it into `cpu`'s stack cache, or onto its dead list when the cache
/// refuses it; true for the dead list, whose worker the caller wakes. Out
/// of line, and moved slot to slot: a `GuardedStack` is 520 bytes, and the
/// copies of one that `finish_switch` once held in its own frame sat at the
/// bottom of every thread that blocks, under its deepest frames, and under
/// every preempting interrupt (ROADMAP §10.2).
#[inline(never)]
fn retire_dead_stack(cpu: &mut PerCpu) -> bool {
    let Some(pages) = cpu.dead_stack.as_ref().map(GuardedStack::pages) else {
        return false;
    };
    if pages == DEFAULT_STACK_PAGES && cpu.stack_cache.put_from(&mut cpu.dead_stack) {
        // AcqRel: pairs with the Acquire load in `cached_stack_frames` and the other updates.
        CACHED_STACK_FRAMES.fetch_add(pages, Ordering::AcqRel);
        // AcqRel: pairs with the Acquire load in `stacks_in_flight` and the other updates.
        STACKS_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        return false;
    }
    // SAFETY: invariant I10, established at `thread_init::finish_switch`:
    // its switch tail calls this after `switch_context` has left the slot's
    // stack, and `dead_list` is this CPU's dead list.
    unsafe { kva_init::park_slot_on_list(&mut cpu.dead_list, &mut cpu.dead_stack) };
    true
}

/// Frames the stack caches of every CPU hold.
#[cfg(feature = "kernel_tests")]
pub fn cached_stack_frames() -> usize {
    // Acquire: pairs with each AcqRel update of `CACHED_STACK_FRAMES`.
    CACHED_STACK_FRAMES.load(Ordering::Acquire)
}

/// Dead threads' stacks not yet cached or freed.
#[cfg(feature = "kernel_tests")]
pub fn stacks_in_flight() -> usize {
    // Acquire: pairs with each AcqRel update of `STACKS_IN_FLIGHT`.
    STACKS_IN_FLIGHT.load(Ordering::Acquire)
}

/// A default-size stack from this CPU's cache, zeroed, or `None`.
fn cached_stack() -> Option<GuardedStack> {
    let stack = per_cpu_init::with_current(|cpu| cpu.stack_cache.take())?;
    // AcqRel: pairs with the Acquire load in `cached_stack_frames` and the other updates.
    CACHED_STACK_FRAMES.fetch_sub(stack.pages(), Ordering::AcqRel);
    let len = stack.pages() * vibeos::paging::PAGE_SIZE_4K as usize;
    // SAFETY: the stack's `pages` pages above `base` are mapped writable
    // and owned by this handle alone: no thread runs on a cached stack
    // (invariant I10, established at `thread_init::finish_switch`).
    unsafe { core::ptr::write_bytes(stack.base().as_u64() as *mut u8, 0, len) };
    #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
    testing::refill_cached(&stack);
    #[cfg(all(feature = "kernel_tests", target_arch = "aarch64"))]
    // SAFETY: invariant I10, established at `thread_init::finish_switch`.
    unsafe {
        kva_init::refill_stack(&stack);
    }
    Some(stack)
}

/// Keep a stack a failed spawn did not use: this CPU's cache if it has
/// room and the stack is default-size, else free it.
pub(crate) fn return_stack(stack: GuardedStack) {
    let pages = stack.pages();
    let refused = if pages == DEFAULT_STACK_PAGES {
        per_cpu_init::with_current(|cpu| cpu.stack_cache.put(stack).err())
    } else {
        Some(stack)
    };
    match refused {
        None => {
            // AcqRel: pairs with the Acquire load in `cached_stack_frames` and the other updates.
            CACHED_STACK_FRAMES.fetch_add(pages, Ordering::AcqRel);
        }
        Some(stack) => kva_init::free_stack(stack),
    }
}

fn prepare_kernel_context(ctx: &mut CpuContext, top: u64, tramp: u64) {
    #[cfg(target_arch = "x86_64")]
    {
        prepare_thread(ctx, top, tramp);
        // SAFETY: `prepare_thread` wrote `rsp` 8 bytes below `top` on this
        // thread's stack; established by `vibeos::thread::prepare_thread`.
        unsafe { (ctx.stack_ptr() as *mut u64).write_volatile(0) };
    }
    #[cfg(target_arch = "aarch64")]
    Arch::prepare(ctx, top, tramp);
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

/// A thread pinned to CPU `cpu`, parked: not runnable until
/// [`make_ready`], so no CPU runs it before then and `cpu` need not be
/// online yet (AP bring-up spawns a CPU's workers before it starts it).
/// [`abandon_unstarted`] retires one that never started.
pub fn spawn_parked_on(
    name: &'static str,
    entry: fn(),
    cpu: u32,
) -> Result<ThreadHandle, SpawnError> {
    spawn_inner(
        name,
        entry,
        CpuAffinity::Pinned(cpu),
        false,
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
    root: u64,
    frame: &UserFrame,
) -> Result<ThreadHandle, SpawnError> {
    let h = spawn_inner(
        name,
        entry,
        CpuAffinity::Pinned(current_cpu()),
        false,
        0,
        pid,
        root,
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
        // SAFETY: invariant I25: `[top - USER_FRAME_BYTES, top)` is the
        // user frame of this thread's own kernel stack, mapped and unused:
        // the thread is not runnable before `make_ready`, and nothing else
        // refers to its stack; established by `thread_init::spawn_inner`.
        unsafe { (at as *mut UserFrame).write(*frame) };
        #[cfg(target_arch = "aarch64")]
        Arch::prepare(&mut tcb.context, at, tramp);
        #[cfg(target_arch = "x86_64")]
        {
            // The pad word below the frame, as the syscall entry leaves it.
            prepare_thread(&mut tcb.context, at - 8, tramp);
            // SAFETY: invariant: `context.rsp` is 8 bytes below the pad
            // word, inside this thread's unused stack; established here.
            unsafe { (tcb.context.stack_ptr() as *mut u64).write_volatile(0) };
        }
    });
    Ok(h)
}

const USER_FRAME_BYTES: usize = core::mem::size_of::<UserFrame>();

/// A thread spawned parked ([`spawn_parked_on`], [`spawn_user`]) until
/// [`make_ready`]: blocked on no queue, with no deadline, so no wake and no
/// timeout readies it, and no stale run-queue or inbox entry for its slot
/// runs it ([`tid_of_slot`]).
const PARKED: ThreadState = ThreadState::Blocked {
    wq: 0,
    deadline: vibeos::sched::FAR_DEADLINE,
};

/// Make thread `id`, spawned parked, runnable on its home CPU. A parked
/// thread retired before it started (`abandon_unstarted`) stays Dead.
pub fn make_ready(id: ThreadId) {
    with_sched(|s| {
        let Some(t) = s.get_mut(id) else {
            return;
        };
        if t.state == ThreadState::Dead {
            return;
        }
        // Invariant: each parked spawn's owner makes it ready once, and
        // nothing else changes a parked thread's state (`thread_init::PARKED`).
        assert!(t.state == PARKED, "make_ready: thread {} not parked", id.0);
        t.state = ThreadState::Ready;
        s.place_home(id);
    });
}

fn choose_cpu(affinity: CpuAffinity) -> u32 {
    let mask = per_cpu_init::online_mask();
    // Relaxed: a round-robin hint; a lost update only repeats a CPU; pairs with nothing.
    let mut rr = SPAWN_RR.load(Ordering::Relaxed);
    let cpu = pick_cpu(affinity, mask, &mut rr);
    // Relaxed: as the load; pairs with nothing.
    SPAWN_RR.store(rr, Ordering::Relaxed);
    cpu
}

/// Build a Ready (or, `enqueue` false, [`PARKED`]) thread. The stack comes
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
    // AcqRel: pairs with the Release store in `testing::fail_next_fork_stack`.
    #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
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
        let slot = s
            .slots
            .iter()
            .position(|x| x.as_deref().is_some_and(dead_reusable))?;
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
        if !enqueue {
            tcb.state = PARKED;
        }
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
        state: if enqueue { ThreadState::Ready } else { PARKED },
        on_cpu: OnCpu::new(),
        stack: None,
        context: CpuContext::empty(),
        entry,
        affinity,
        cpu,
        irq_nest: first_nest,
        switches: 0,
        run_tsc: 0,
        wait_outcome: WaitOutcome::Woken,
        as_cr3,
        fpu: Fxsave::new_thread(),
        fp_cpu: None,
        user_segs: UserSegs::NULL,
        tls_base: 0,
        syscall_count: vibeos::atomic::AtomicU64::new(0),
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
    prepare_kernel_context(&mut tcb.context, top, tramp);

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
    tcb.affinity = affinity;
    tcb.cpu = cpu;
    tcb.irq_nest = irq_nest;
    tcb.switches = 0;
    tcb.run_tsc = 0;
    tcb.wait_outcome = WaitOutcome::Woken;
    tcb.as_cr3 = as_cr3;
    tcb.fpu = Fxsave::new_thread();
    // A reused TCB address: no CPU's `fp_owner` may match it.
    fp_invalidate(tcb);
    tcb.user_segs = UserSegs::NULL;
    // Relaxed: a statistic, reset before the thread first runs; pairs with nothing.
    tcb.syscall_count.store(0, Ordering::Relaxed);
    tcb.pid = pid;
    prepare_kernel_context(&mut tcb.context, top, tramp);
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
#[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
pub fn with_sched_lock<R>(f: impl FnOnce() -> R) -> R {
    let _g = SCHED.lock();
    f()
}

/// Run `f` under SCHED, then place the wakes it recorded. IF stays off
/// from before SCHED is taken until every recorded wake is placed, so a
/// caller that `f` left `Blocked` is not switched off with those wakes
/// still in this frame's batch (DESIGN §2.9 rule 1, F034).
pub(crate) fn with_sched<R>(f: impl FnOnce(&mut Sched) -> R) -> R {
    // The entering context, which `begin_wait` checks: read before the
    // guard turns IF off.
    let ctx = sync_init::sleep_ctx();
    let _irq = InterruptGuard::enter();
    // Dropped last, once the places are delivered.
    #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
    let _window = testing::window_enter();
    // The wakes `f` recorded, placed after SCHED drops, at most
    // `PLACE_BATCH` per lock hold: the rest wait in `places`, where this
    // call, or any other SCHED holder's, takes them next.
    let mut batch = [(0u32, ThreadId::NONE, 0u32); PLACE_BATCH];
    let (r, mut n) = {
        let mut s = SCHED.lock();
        s.waiter = ctx;
        let r = f(&mut s);
        let n = s.take_places(&mut batch);
        (r, n)
    };
    #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
    testing::preempt_before_places(n);
    loop {
        for &(cpu, id, slot) in batch.iter().take(n) {
            #[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
            testing::place_stall(id);
            crate::ipi_init::place_ready(cpu, id, slot as usize);
        }
        if n < PLACE_BATCH {
            break;
        }
        n = SCHED.lock().take_places(&mut batch);
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
#[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
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
        (s.ptr(old_id), s.ptr(id))
    });
    switch_now(old_ptr, new_ptr);
    finish_switch();
}

/// The running thread's id, through [`crate::arch::current_tcb`]'s one
/// load, at any IF (DESIGN §2.9 rule 5).
pub fn current_id() -> ThreadId {
    let p = crate::arch::current_tcb();
    assert!(!p.is_null(), "no current thread");
    // SAFETY: invariant I9: the current thread's `Tcb` stays in `SCHED`, and
    // `id` changes only while its slot is Dead, never while it runs;
    // established by `thread_init::spawn_inner`.
    unsafe { (*p).id }
}

/// The running thread's pid (0 with no current thread), through
/// [`crate::arch::current_tcb`]'s one load, at any IF.
pub fn current_pid() -> u32 {
    let p = crate::arch::current_tcb();
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

pub fn set_pid_cr3(id: ThreadId, pid: u32, root: u64) {
    with_sched(|s| {
        if let Some(t) = s.get_mut(id) {
            t.pid = pid;
            t.as_cr3 = root;
        }
    });
}

/// Add each live thread's syscall count to its process's entry of `sums`,
/// `(pid, sum)` pairs sorted by pid (`vibeos::proc::sum_syscalls`), in one
/// pass over the thread table under SCHED. Dead TCBs are skipped.
pub fn sum_syscalls(sums: &mut [(u32, u64)]) {
    with_sched(|s| {
        let threads = s
            .slots
            .iter()
            .flatten()
            .filter(|t| t.state != ThreadState::Dead)
            // Relaxed: the count is a statistic and pairs with nothing.
            .map(|t| (t.pid, t.syscall_count.load(Ordering::Relaxed)));
        vibeos::proc::sum_syscalls(sums, threads);
    });
}

/// The first TCB, Dead ones included, whose saved root is `root`. Reads
/// only; the root's free (`addr_space_init::SpaceCore`) asks it first.
pub(crate) fn tcb_naming_root(root: u64) -> Option<ThreadId> {
    with_sched(|s| {
        s.slots
            .iter()
            .flatten()
            // A saved root is a table address, as `spawn_user` and
            // `set_pid_cr3` store it.
            .find(|t| t.as_cr3 == root)
            .map(|t| t.id)
    })
}

/// The CPU this thread runs on: exact while IF=0, a hint with IF=1
/// (`arch::cpu_id_hint`), which is enough to place a new thread. A caller
/// that needs the exact id reads it with IF=0.
pub fn current_cpu() -> u32 {
    crate::arch::cpu_id_hint()
}

#[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
pub use testing::{
    cpu_of, exited, ktest_drop_timeout, ktest_last_overdue, ktest_preempt_before_places,
    ktest_sweeps, name, state, try_state,
};

pub fn tcb_ptr(id: ThreadId) -> *mut Tcb {
    SCHED.lock().ptr(id)
}

/// Hooks the in-guest tests arm (DESIGN §8.2). `kernel_tests` builds only.
#[cfg(all(feature = "kernel_tests", target_arch = "x86_64"))]
pub mod testing;
