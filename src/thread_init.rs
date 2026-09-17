//! Kernel threads: TCB table, spawn, voluntary switch. ROADMAP §3.1–§3.2.
//!
//! Slice A: no ready queue. `schedule` is a stub that switches a dead
//! thread back to bootstrap. Slice B owns real `schedule` / idle / reap.
#![cfg_attr(not(feature = "kernel_tests"), allow(dead_code))]

use alloc::boxed::Box;

use vibeos::kva::DEFAULT_STACK_PAGES;
use vibeos::thread::{
    prepare_thread, switch_context, CpuAffinity, CpuContext, ThreadId, ThreadState,
};

use crate::kva_init::{self, GuardedStack};
use crate::per_cpu_init;
use crate::sync_init::SpinMutex;
use crate::x86::InterruptGuard;

pub const MAX_THREADS: usize = 64;

#[derive(Clone, Copy)]
pub struct ThreadHandle {
    id: ThreadId,
}

impl ThreadHandle {
    pub fn id(self) -> ThreadId {
        self.id
    }
}

#[allow(dead_code)] // next/prev/affinity/stack held for layout and so the stack is not unmapped
pub struct Tcb {
    pub id: ThreadId,
    pub name: &'static str,
    pub state: ThreadState,
    pub stack: Option<GuardedStack>,
    pub context: CpuContext,
    pub entry: fn(),
    /// Intrusive run-queue link. Slice B fills this; phase 4 is per-CPU.
    pub next: Option<ThreadId>,
    pub prev: Option<ThreadId>,
    pub affinity: CpuAffinity,
    pub cpu: u32,
    pub switches: u64,
}

struct Table {
    slots: [Option<Box<Tcb>>; MAX_THREADS],
}

impl Table {
    const fn empty() -> Self {
        Self {
            slots: [const { None }; MAX_THREADS],
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

static TABLE: SpinMutex<Table> = SpinMutex::new(Table::empty());

extern "C" fn trampoline() {
    let entry = unsafe {
        let p = per_cpu_init::current_thread() as *const Tcb;
        (*p).entry
    };
    entry();
    thread_exit();
}

fn thread_exit() -> ! {
    unsafe {
        let p = per_cpu_init::current_thread() as *mut Tcb;
        (*p).state = ThreadState::Dead;
    }
    schedule();
}

/// Slice A stub. No ready queue (Slice B). A returning thread is Dead;
/// switch back to bootstrap. Bootstrap must not return from its entry.
pub fn schedule() -> ! {
    let cur = current_id();
    if cur == ThreadId::BOOTSTRAP {
        panic!("schedule: bootstrap (Slice A stub; Slice B owns the ready queue)");
    }
    switch_to(ThreadId::BOOTSTRAP);
    panic!("schedule: dead thread resumed");
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
        switches: 0,
    });
    let ptr = &mut *tcb as *mut Tcb;
    {
        let mut tbl = TABLE.lock();
        assert!(tbl.slots[0].is_none(), "bootstrap twice");
        tbl.slots[0] = Some(tcb);
    }
    per_cpu_init::set_current_thread(ptr as *mut u8);
}

fn bootstrap_entry() {
    panic!("bootstrap entry called");
}

pub fn spawn(name: &'static str, entry: fn()) -> ThreadHandle {
    let stack = kva_init::alloc_guarded_stack(DEFAULT_STACK_PAGES).expect("thread stack");
    let top = stack.top().as_u64();
    assert!(top % 16 == 0, "kva stack top not 16-aligned");

    let mut tcb = Box::new(Tcb {
        id: ThreadId(0),
        name,
        state: ThreadState::Ready,
        stack: Some(stack),
        context: CpuContext::empty(),
        entry,
        next: None,
        prev: None,
        affinity: CpuAffinity::Any,
        cpu: 0,
        switches: 0,
    });
    prepare_thread(&mut tcb.context, top, trampoline as *const () as u64);
    unsafe { (tcb.context.rsp as *mut u64).write_volatile(0) };

    let id = {
        let mut tbl = TABLE.lock();
        let slot = tbl
            .slots
            .iter()
            .position(|s| s.is_none())
            .expect("thread table full");
        let id = ThreadId(slot as u32);
        tcb.id = id;
        tbl.slots[slot] = Some(tcb);
        id
    };
    ThreadHandle { id }
}

/// Voluntary switch. Test helper; Slice B's `schedule` picks from a queue.
pub fn switch_to(id: ThreadId) {
    let _irq = InterruptGuard::enter();
    let old_id = current_id();
    assert!(old_id != id, "switch_to self");

    let (old_ptr, new_ptr) = {
        let mut tbl = TABLE.lock();
        let new = tbl.ptr(id);
        assert!(!new.is_null(), "switch_to unknown id");
        unsafe {
            assert!(
                (*new).state != ThreadState::Dead,
                "switch_to dead thread"
            );
            let old = tbl.ptr(old_id);
            assert!(!old.is_null());
            if (*old).state != ThreadState::Dead {
                (*old).state = ThreadState::Ready;
            }
            (*old).switches = (*old).switches.wrapping_add(1);
            (*new).state = ThreadState::Running;
            (*new).cpu = 0;
        }
        (tbl.ptr(old_id), new)
    };

    per_cpu_init::set_current_thread(new_ptr as *mut u8);
    per_cpu_init::current_mut().switches = per_cpu_init::current().switches.wrapping_add(1);

    unsafe {
        switch_context(&mut (*old_ptr).context, &(*new_ptr).context);
    }
}

pub fn current_id() -> ThreadId {
    let p = per_cpu_init::current_thread() as *const Tcb;
    assert!(!p.is_null(), "no current thread");
    unsafe { (*p).id }
}

#[allow(dead_code)]
pub fn current_tcb() -> &'static mut Tcb {
    let p = per_cpu_init::current_thread() as *mut Tcb;
    assert!(!p.is_null(), "no current thread");
    unsafe { &mut *p }
}

pub fn state(id: ThreadId) -> ThreadState {
    TABLE.lock().get(id).expect("unknown thread").state
}

pub fn name(id: ThreadId) -> &'static str {
    TABLE.lock().get(id).expect("unknown thread").name
}
