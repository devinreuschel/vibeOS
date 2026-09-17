//! Kernel threads: TCB table, spawn, schedule, sleep. ROADMAP §3.1–§3.4 / §4.8.
//!
//! Global TCB table + timeouts under one SCHED lock. Per-CPU ready queues
//! (owner CPU only, IRQs off). Cross-CPU wake is inbox + IPI 0xFD, never
//! a remote runq lock. Never held across `switch_context`, heap free, or
//! KVA/stack reclaim.
#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]

use alloc::boxed::Box;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, Ordering};

use vibeos::ipi::{home_cpu, pick_cpu};
use vibeos::kva::DEFAULT_STACK_PAGES;
use vibeos::lock::RANK_SCHED;
use vibeos::paging::VirtAddr;
use vibeos::sched::{
    effective_deadline, enqueue_runnable, take_next, TimeoutQueue, SWEEP_TICKS,
};
use vibeos::thread::{
    apply_if_on_resume, prepare_thread, switch_context, CpuAffinity, CpuContext, KernelStack, Tcb,
    ThreadId, ThreadState, WaitOutcome, MAX_THREADS,
};
use vibeos::time::Instant;
use vibeos::wait::{self, WaitQueue};

use crate::kva_init::{self, GuardedStack};
use crate::per_cpu_init;
use crate::serial::Serial;
use crate::sync_init::SpinMutex;
use crate::time_init;
use crate::x86::InterruptGuard;

#[derive(Clone, Copy)]
pub struct ThreadHandle {
    id: ThreadId,
}

impl ThreadHandle {
    pub fn id(self) -> ThreadId {
        self.id
    }
}

pub(crate) struct Sched {
    slots: [Option<Box<Tcb>>; MAX_THREADS],
    timeouts: TimeoutQueue,
    places: [(u32, ThreadId); MAX_THREADS],
    place_n: usize,
}

impl Sched {
    const fn empty() -> Self {
        Self {
            slots: [const { None }; MAX_THREADS],
            timeouts: TimeoutQueue::empty(),
            places: [(0, ThreadId::NONE); MAX_THREADS],
            place_n: 0,
        }
    }

    fn get(&self, id: ThreadId) -> Option<&Tcb> {
        self.slots.get(id.0 as usize)?.as_deref()
    }

    fn get_mut(&mut self, id: ThreadId) -> Option<&mut Tcb> {
        self.slots.get_mut(id.0 as usize)?.as_deref_mut()
    }

    fn ptr(&mut self, id: ThreadId) -> *mut Tcb {
        match self.get_mut(id) {
            Some(t) => t as *mut Tcb,
            None => core::ptr::null_mut(),
        }
    }

    fn place(&mut self, cpu: u32, id: ThreadId) {
        if self.place_n >= MAX_THREADS {
            return;
        }
        self.places[self.place_n] = (cpu, id);
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
            // SAFETY: cookie is a WaitQueue living under SCHED (mutex / chan / …).
            unsafe { &mut *(cookie as *mut WaitQueue) }.remove(id);
        }
    }
}

static SCHED: SpinMutex<Sched> = SpinMutex::with_rank(Sched::empty(), RANK_SCHED);
static SPAWN_RR: AtomicU32 = AtomicU32::new(0);

extern "C" fn trampoline() {
    reap_zombies();
    let entry = unsafe { (*per_cpu_init::current_thread()).entry };
    entry();
    thread_exit();
}

fn thread_exit() -> ! {
    // IF-on-resume leaves IF set. Hold it off across Dead → defer_free →
    // schedule so a tick cannot preempt a Dead thread still on-CPU.
    let _irq = InterruptGuard::enter();
    unsafe {
        let p = per_cpu_init::current_thread();
        (*p).state = ThreadState::Dead;
        if let Some(ks) = (*p).stack.take() {
            kva_init::defer_free(GuardedStack {
                guard: VirtAddr(ks.guard),
                pages: ks.pages,
            });
        }
    }
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
            let ticks = per_cpu_init::current().ticks;
            if ticks % SWEEP_TICKS == 0 {
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
            let _ = writeln!(Serial, "vibeOS: sched: overdue tid {}", overdue[i].raw());
            i += 1;
        }
    }

    let next = {
        let cpu = per_cpu_init::current_mut();
        enqueue_runnable(&mut cpu.runq, cur, idle, cur_state);
        cpu.runq.remove(idle);
        take_next(&mut cpu.runq, idle)
    };

    let (old_ptr, new_ptr, old_id, new_id) = with_sched(|s| {
        if let Some(t) = s.get_mut(next) {
            t.state = ThreadState::Running;
            t.cpu = me;
        }
        relink(s);
        (s.ptr(cur), s.ptr(next), cur, next)
    });

    if old_id == new_id || old_ptr.is_null() || new_ptr.is_null() {
        if !from_irq {
            reap_zombies();
        }
        return;
    }
    assert!(!new_ptr.is_null(), "schedule: next vanished");
    switch_now(old_ptr, new_ptr);
    if !from_irq {
        reap_zombies();
    }
}

fn switch_now(old_ptr: *mut Tcb, new_ptr: *mut Tcb) {
    let now = time_init::read_tsc();
    let cpu = per_cpu_init::current_mut();
    let delta = now.wrapping_sub(cpu.slice_tsc);
    cpu.slice_tsc = now;
    cpu.switches = cpu.switches.wrapping_add(1);
    unsafe {
        (*old_ptr).run_tsc = (*old_ptr).run_tsc.wrapping_add(delta);
        (*old_ptr).switches = (*old_ptr).switches.wrapping_add(1);
        if old_ptr == cpu.idle {
            cpu.idle_tsc = cpu.idle_tsc.wrapping_add(delta);
        }
        (*old_ptr).irq_nest = cpu.irq_nest;
        cpu.irq_nest = (*new_ptr).irq_nest;
        apply_if_on_resume(&mut (*new_ptr).context.rflags, cpu.irq_nest);
        per_cpu_init::set_current_thread(new_ptr);
        switch_context(&mut (*old_ptr).context, &(*new_ptr).context);
    }
}

fn relink(s: &mut Sched) {
    let cpu = per_cpu_init::current_mut();
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
}

pub(crate) fn reap_zombies() {
    // Stacks only. Dead TCBs stay queryable (`state == Dead`) until spawn
    // reuses the slot. Never unmap a stack we are running on.
    kva_init::drain_deferred();
}

/// `sti; hlt` with a closed lost-wakeup window: cli, drain inbox, recheck
/// runq, then `sti; hlt` as one pair. ROADMAP §4.8 / DESIGN §7.8.
pub fn halt_if_idle() {
    loop {
        unsafe {
            core::arch::asm!("cli", options(nomem, nostack, preserves_flags));
        }
        crate::ipi_init::drain_inbox();
        if !per_cpu_init::current().runq.is_empty() {
            unsafe {
                core::arch::asm!("sti", options(nomem, nostack, preserves_flags));
            }
            return;
        }
        unsafe {
            core::arch::asm!("sti; hlt", options(nomem, nostack));
        }
    }
}

/// Wrap the current boot context as thread 0. After [`per_cpu_init::init_bsp`].
///
/// # Safety
/// Single-CPU, `GS_BASE` live, not already initialized.
pub unsafe fn init_bootstrap() {
    let mut tcb = Box::new(Tcb {
        id: ThreadId::BOOTSTRAP,
        name: "bootstrap",
        state: ThreadState::Running,
        stack: None,
        context: CpuContext::empty(),
        entry: bootstrap_entry,
        next: None,
        prev: None,
        affinity: CpuAffinity::Pinned(0),
        cpu: 0,
        irq_nest: 0,
        switches: 0,
        run_tsc: 0,
        wait_outcome: WaitOutcome::Woken,
    });
    let ptr = &mut *tcb as *mut Tcb;
    {
        let mut s = SCHED.lock();
        assert!(s.slots[0].is_none(), "bootstrap twice");
        s.slots[0] = Some(tcb);
    }
    let cpu = per_cpu_init::current_mut();
    cpu.current = ptr;
    cpu.idle = ptr;
    cpu.idle_id = ThreadId::BOOTSTRAP;
    cpu.ready_head = core::ptr::null_mut();
}

fn bootstrap_entry() {
    panic!("bootstrap entry called");
}

pub fn spawn(name: &'static str, entry: fn()) -> ThreadHandle {
    spawn_inner(name, entry, CpuAffinity::Any, true, 0)
}

/// Pin to this CPU. In-guest tests that `switch_to` / `yield_now` a
/// worker must use this: `ktest::run` holds IF off on the BSP, so an
/// `Any` spawn would RR onto an AP the BSP cannot switch to.
///
/// Copies `irq_nest` so `switch_to` does not `sti` the worker (incoming
/// nest 0 would enable IF; a tick then preempts the cooperative chain).
pub fn spawn_here(name: &'static str, entry: fn()) -> ThreadHandle {
    spawn_inner(
        name,
        entry,
        CpuAffinity::Pinned(current_cpu()),
        true,
        per_cpu_init::irq_nest(),
    )
}

pub fn spawn_on(name: &'static str, entry: fn(), cpu: u32) -> ThreadHandle {
    spawn_inner(name, entry, CpuAffinity::Pinned(cpu), true, 0)
}

pub(crate) fn spawn_idle(entry: fn()) -> ThreadHandle {
    spawn_inner("idle", entry, CpuAffinity::Pinned(0), false, 0)
}

/// AP idle: running on `stack` already. No synthetic frame, not on the FIFO.
pub fn adopt_ap_idle(cpu_id: u32, stack: GuardedStack) -> Option<ThreadId> {
    let ks = KernelStack {
        guard: stack.guard.as_u64(),
        pages: stack.pages,
    };
    let mut tcb = Box::new(Tcb {
        id: ThreadId(0),
        name: "idle",
        state: ThreadState::Running,
        stack: Some(ks),
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
    });
    let id = with_sched(|s| {
        let slot = s.slots.iter().position(|x| x.is_none())?;
        let id = ThreadId(slot as u32);
        tcb.id = id;
        s.slots[slot] = Some(tcb);
        Some(id)
    })?;
    Some(id)
}

/// Timeout path: TCB never ran. Return the stack so the caller can free it.
pub fn abandon_ap_idle(id: ThreadId) -> Option<GuardedStack> {
    with_sched(|s| {
        s.timeouts.remove(id);
        let t = s.get_mut(id)?;
        t.state = ThreadState::Dead;
        let ks = t.stack.take()?;
        Some(GuardedStack {
            guard: VirtAddr(ks.guard),
            pages: ks.pages,
        })
    })
}

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

fn spawn_inner(
    name: &'static str,
    entry: fn(),
    affinity: CpuAffinity,
    enqueue: bool,
    irq_nest: u32,
) -> ThreadHandle {
    let stack = kva_init::alloc_guarded_stack(DEFAULT_STACK_PAGES).expect("thread stack");
    let top = stack.top().as_u64();
    assert!(top % 16 == 0, "kva stack top not 16-aligned");
    let ks = KernelStack {
        guard: stack.guard.as_u64(),
        pages: stack.pages,
    };
    let tramp = trampoline as *const () as u64;
    let cur = current_id();
    let idle = per_cpu_init::current().idle_id;
    let cpu = choose_cpu(affinity);

    // Dead slot: rewrite the Box. 2000 spawn/exit must not churn the
    // heap (one extra mapped page shows up as a leaked frame).
    if let Some(id) = with_sched(|s| {
        let slot = s.slots.iter().position(|x| match x.as_ref() {
            Some(t) => t.state == ThreadState::Dead && t.id != cur && t.id != idle,
            None => false,
        })?;
        let id = ThreadId(slot as u32);
        s.timeouts.remove(id);
        let tcb = s.slots[slot].as_mut().expect("dead slot");
        assert!(tcb.stack.is_none(), "dead tcb still owns stack");
        fill_tcb(tcb, name, entry, affinity, cpu, ks, top, tramp, irq_nest);
        if enqueue {
            s.place(cpu, id);
        }
        Some(id)
    }) {
        return ThreadHandle { id };
    }

    let mut tcb = Box::new(Tcb {
        id: ThreadId(0),
        name,
        state: ThreadState::Ready,
        stack: Some(ks),
        context: CpuContext::empty(),
        entry,
        next: None,
        prev: None,
        affinity,
        cpu,
        irq_nest,
        switches: 0,
        run_tsc: 0,
        wait_outcome: WaitOutcome::Woken,
    });
    prepare_thread(&mut tcb.context, top, tramp);
    unsafe { (tcb.context.rsp as *mut u64).write_volatile(0) };

    let id = with_sched(|s| {
        let slot = s
            .slots
            .iter()
            .position(|x| x.is_none())
            .expect("thread table full");
        let id = ThreadId(slot as u32);
        s.timeouts.remove(id);
        tcb.id = id;
        s.slots[slot] = Some(tcb);
        if enqueue {
            s.place(cpu, id);
        }
        id
    });
    ThreadHandle { id }
}

fn fill_tcb(
    tcb: &mut Tcb,
    name: &'static str,
    entry: fn(),
    affinity: CpuAffinity,
    cpu: u32,
    ks: KernelStack,
    top: u64,
    tramp: u64,
    irq_nest: u32,
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
    prepare_thread(&mut tcb.context, top, tramp);
    unsafe { (tcb.context.rsp as *mut u64).write_volatile(0) };
}

pub fn sleep_ms(ms: u64) {
    let ns = time_init::now_ns().saturating_add(ms.saturating_mul(1_000_000));
    park(Some(Instant { ns }));
}

/// Park until `deadline` (or a far-future sentinel). Sleep path only.
pub fn park(deadline: Option<Instant>) {
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
pub fn with_sched_lock<R>(f: impl FnOnce() -> R) -> R {
    let _g = SCHED.lock();
    f()
}

pub(crate) fn with_sched<R>(f: impl FnOnce(&mut Sched) -> R) -> R {
    let (r, places, n) = {
        let mut s = SCHED.lock();
        let r = f(&mut s);
        let n = s.place_n;
        let p = s.places;
        s.place_n = 0;
        (r, p, n)
    };
    let mut i = 0;
    while i < n {
        crate::ipi_init::place_ready(places[i].0, places[i].1);
        i += 1;
    }
    r
}

pub fn last_wait_outcome() -> WaitOutcome {
    let p = per_cpu_init::current_thread();
    assert!(!p.is_null(), "no current thread");
    unsafe { (*p).wait_outcome }
}

/// Test helper. Local CPU only — the target must already sit on this
/// runq (spawn_here). Same nest-swap as `schedule`.
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
    {
        let cpu = per_cpu_init::current_mut();
        cpu.runq.remove(id);
        enqueue_runnable(&mut cpu.runq, old_id, idle, cur_state);
    }
    let (old_ptr, new_ptr) = with_sched(|s| {
        if let Some(t) = s.get_mut(old_id) {
            if t.state != ThreadState::Dead && old_id != idle {
                t.state = ThreadState::Ready;
            }
        }
        if let Some(t) = s.get_mut(id) {
            t.state = ThreadState::Running;
            t.cpu = me;
        }
        relink(s);
        (s.ptr(old_id), s.ptr(id))
    });
    switch_now(old_ptr, new_ptr);
    reap_zombies();
}

pub fn current_id() -> ThreadId {
    let p = per_cpu_init::current_thread();
    assert!(!p.is_null(), "no current thread");
    unsafe { (*p).id }
}

pub fn current_cpu() -> u32 {
    per_cpu_init::current().cpu_id
}

#[allow(dead_code)]
pub fn current_tcb() -> &'static mut Tcb {
    let p = per_cpu_init::current_thread();
    assert!(!p.is_null(), "no current thread");
    unsafe { &mut *p }
}

pub fn state(id: ThreadId) -> ThreadState {
    SCHED.lock().get(id).expect("unknown thread").state
}

pub fn try_state(id: ThreadId) -> Option<ThreadState> {
    SCHED.lock().get(id).map(|t| t.state)
}

pub fn name(id: ThreadId) -> &'static str {
    SCHED.lock().get(id).expect("unknown thread").name
}

pub fn cpu_of(id: ThreadId) -> u32 {
    SCHED.lock().get(id).expect("unknown thread").cpu
}

#[allow(dead_code)]
pub fn run_tsc(id: ThreadId) -> u64 {
    SCHED.lock().get(id).expect("unknown thread").run_tsc
}

pub fn tcb_ptr(id: ThreadId) -> *mut Tcb {
    SCHED.lock().ptr(id)
}
