//! Kernel threads: TCB table, spawn, schedule, sleep. ROADMAP §3.1–§3.4.
//!
//! One `SCHED` lock (IRQ-aware `SpinMutex`). Never held across
//! `switch_context`, heap free, or KVA/stack reclaim.
#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]

use alloc::boxed::Box;
use core::fmt::Write;

use vibeos::kva::DEFAULT_STACK_PAGES;
use vibeos::paging::VirtAddr;
use vibeos::sched::{
    BlockedList, ReadyQueue, SWEEP_TICKS, TimeoutQueue, effective_deadline, enqueue_runnable,
    take_next,
};
use vibeos::thread::{
    CpuAffinity, CpuContext, KernelStack, MAX_THREADS, Tcb, ThreadId, ThreadState,
    apply_if_on_resume, prepare_thread, switch_context,
};
use vibeos::time::Instant;

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

struct Sched {
    slots: [Option<Box<Tcb>>; MAX_THREADS],
    ready: ReadyQueue,
    timeouts: TimeoutQueue,
    blocked: BlockedList,
}

impl Sched {
    const fn empty() -> Self {
        Self {
            slots: [const { None }; MAX_THREADS],
            ready: ReadyQueue::empty(),
            timeouts: TimeoutQueue::empty(),
            blocked: BlockedList::empty(),
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
}

static SCHED: SpinMutex<Sched> = SpinMutex::new(Sched::empty());

extern "C" fn trampoline() {
    reap_zombies();
    let entry = unsafe { (*per_cpu_init::current_thread()).entry };
    entry();
    thread_exit();
}

fn thread_exit() -> ! {
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

/// Timer path. EOI already done. May switch; reaping stays off the IRQ path.
pub fn schedule_preempt() {
    schedule_inner(true);
}

/// IF-on-resume is applied to the incoming TCB before `popfq`. See
/// [`apply_if_on_resume`]. `InterruptGuard` stays on the outgoing stack.
fn schedule_inner(from_irq: bool) {
    let _irq = InterruptGuard::enter();
    let now = Instant {
        ns: time_init::now_ns(),
    };
    let idle = per_cpu_init::current().idle_id;
    let cur = current_id();

    let mut overdue = [ThreadId::NONE; 4];
    let mut n_overdue = 0usize;

    let (old_ptr, new_ptr, old_id, new_id) = {
        let mut s = SCHED.lock();
        let mut expired = [ThreadId::NONE; MAX_THREADS];
        let n = s.timeouts.pop_expired_into(now, &mut expired);
        let mut i = 0;
        while i < n {
            let id = expired[i];
            s.blocked.remove(id);
            if let Some(t) = s.get_mut(id) {
                match t.state {
                    ThreadState::Sleeping { .. } | ThreadState::Blocked => {
                        t.state = ThreadState::Ready;
                    }
                    ThreadState::Ready | ThreadState::Running | ThreadState::Dead => {}
                }
            }
            s.ready.push_back(id);
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

        let cur_state = s.get(cur).map(|t| t.state).unwrap_or(ThreadState::Dead);
        enqueue_runnable(&mut s.ready, cur, idle, cur_state);
        if let Some(t) = s.get_mut(cur) {
            match t.state {
                ThreadState::Running => t.state = ThreadState::Ready,
                ThreadState::Ready
                | ThreadState::Sleeping { .. }
                | ThreadState::Blocked
                | ThreadState::Dead => {}
            }
        }

        s.ready.remove(idle);
        let next = take_next(&mut s.ready, idle);
        if let Some(t) = s.get_mut(next) {
            t.state = ThreadState::Running;
            t.cpu = 0;
        }
        relink(&mut s);
        let old_ptr = s.ptr(cur);
        let new_ptr = s.ptr(next);
        (old_ptr, new_ptr, cur, next)
    };

    if n_overdue != 0 {
        let mut i = 0;
        while i < n_overdue {
            let _ = writeln!(Serial, "vibeOS: sched: overdue tid {}", overdue[i].raw());
            i += 1;
        }
    }

    if old_id == new_id || old_ptr.is_null() || new_ptr.is_null() {
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
    let n = s.ready.len();
    let mut ids = [ThreadId::NONE; MAX_THREADS];
    let mut i = 0;
    while i < n {
        ids[i] = s.ready.at(i);
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
    let head = match s.ready.front() {
        Some(id) => s.ptr(id),
        None => core::ptr::null_mut(),
    };
    if let Some(c) = per_cpu_init::try_current_mut() {
        c.ready_head = head;
    }
}

fn reap_zombies() {
    // Stacks only. Dead TCBs stay queryable (`state == Dead`) until spawn
    // reuses the slot. Never unmap a stack we are running on.
    kva_init::drain_deferred();
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
    spawn_inner(name, entry, CpuAffinity::Any, true)
}

pub(crate) fn spawn_idle(entry: fn()) -> ThreadHandle {
    spawn_inner("idle", entry, CpuAffinity::Pinned(0), false)
}

fn spawn_inner(
    name: &'static str,
    entry: fn(),
    affinity: CpuAffinity,
    enqueue: bool,
) -> ThreadHandle {
    let stack = kva_init::alloc_guarded_stack(DEFAULT_STACK_PAGES).expect("thread stack");
    let top = stack.top().as_u64();
    assert!(top % 16 == 0, "kva stack top not 16-aligned");
    let ks = KernelStack {
        guard: stack.guard.as_u64(),
        pages: stack.pages,
    };

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
        cpu: 0,
        irq_nest: 0,
        switches: 0,
        run_tsc: 0,
    });
    prepare_thread(&mut tcb.context, top, trampoline as *const () as u64);
    unsafe { (tcb.context.rsp as *mut u64).write_volatile(0) };

    let cur = current_id();
    let idle = per_cpu_init::current().idle_id;
    let (id, old) = {
        let mut s = SCHED.lock();
        let slot = s
            .slots
            .iter()
            .position(|x| x.is_none())
            .or_else(|| {
                s.slots.iter().position(|x| match x.as_ref() {
                    Some(t) => {
                        t.state == ThreadState::Dead && t.id != cur && t.id != idle
                    }
                    None => false,
                })
            })
            .expect("thread table full");
        let id = ThreadId(slot as u32);
        s.ready.remove(id);
        s.timeouts.remove(id);
        s.blocked.remove(id);
        let old = s.slots[slot].take();
        tcb.id = id;
        s.slots[slot] = Some(tcb);
        if enqueue {
            s.ready.push_back(id);
        }
        relink(&mut s);
        (id, old)
    };
    drop(old);
    ThreadHandle { id }
}

pub fn sleep_ms(ms: u64) {
    let ns = time_init::now_ns().saturating_add(ms.saturating_mul(1_000_000));
    park(Some(Instant { ns }), false);
}

/// Park until `deadline` (or a far-future sentinel). Slice C WaitQueue
/// uses the same timeout structure; `blocked` is the stub list.
pub fn park(deadline: Option<Instant>, blocked: bool) {
    let d = effective_deadline(deadline);
    let id = current_id();
    {
        let mut s = SCHED.lock();
        if let Some(t) = s.get_mut(id) {
            t.state = if blocked {
                ThreadState::Blocked
            } else {
                ThreadState::Sleeping { deadline: d }
            };
        }
        s.ready.remove(id);
        if blocked {
            s.blocked.push_back(id);
        }
        s.timeouts.insert(id, d);
        relink(&mut s);
    }
    schedule();
}

/// Test helper. Same nest-swap as `schedule`.
pub fn switch_to(id: ThreadId) {
    let _irq = InterruptGuard::enter();
    let old_id = current_id();
    assert!(old_id != id, "switch_to self");
    let idle = per_cpu_init::current().idle_id;

    let (old_ptr, new_ptr) = {
        let mut s = SCHED.lock();
        let new = s.ptr(id);
        assert!(!new.is_null(), "switch_to unknown id");
        unsafe {
            assert!((*new).state != ThreadState::Dead, "switch_to dead thread");
        }
        s.ready.remove(id);
        let cur_state = s.get(old_id).map(|t| t.state).unwrap_or(ThreadState::Dead);
        enqueue_runnable(&mut s.ready, old_id, idle, cur_state);
        if let Some(t) = s.get_mut(old_id) {
            if t.state != ThreadState::Dead && old_id != idle {
                t.state = ThreadState::Ready;
            }
        }
        if let Some(t) = s.get_mut(id) {
            t.state = ThreadState::Running;
            t.cpu = 0;
        }
        relink(&mut s);
        (s.ptr(old_id), s.ptr(id))
    };
    switch_now(old_ptr, new_ptr);
    reap_zombies();
}

pub fn current_id() -> ThreadId {
    let p = per_cpu_init::current_thread();
    assert!(!p.is_null(), "no current thread");
    unsafe { (*p).id }
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

#[allow(dead_code)]
pub fn run_tsc(id: ThreadId) -> u64 {
    SCHED.lock().get(id).expect("unknown thread").run_tsc
}

pub fn tcb_ptr(id: ThreadId) -> *mut Tcb {
    SCHED.lock().ptr(id)
}
