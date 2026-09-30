//! In-guest tests for sched (kernel_tests only). Rows: [`TESTS`].

mod counted;
mod depth;
mod hooks;
mod reclaim;
mod registry;
mod requeue;
mod sleep;
pub(crate) use counted::test_counted_deferred_release;
pub(crate) use depth::{
    record, report, stack_depth_exit_scan, stack_depth_planted, wait_exit_depth,
};
pub(crate) use hooks::{RequeueGuard, requeues, set_requeue_next_cpu, work_live};
pub(crate) use reclaim::dead_list_batched_rounds;
pub(crate) use registry::{test_ktest_fail_fmt, test_ktest_helpers, test_ktest_rows};
pub(crate) use requeue::test_requeue_moves_each_dequeue;
pub(crate) use sleep::{
    block_in_hard_irq_asserts, in_hard_irq_top_bottom, lock_across_switch_asserts,
    sleep_under_spinlock_asserts,
};

use alloc::boxed::Box;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::apic::TimerMode;
use vibeos::kva::DEFAULT_STACK_PAGES;
use vibeos::paging::PAGE_SIZE_4K;
use vibeos::pmm::{Frames, MAX_ORDER};
use vibeos::proc::{SIGKILL, wait_exited, wait_signaled};
use vibeos::syscall::SYS_KILL;
use vibeos::thread::{MAX_THREADS, ThreadId, ThreadState};

use crate::apic_init;
use crate::ktest::user::{self, DEFAULT, Image, user_code};
use crate::ktest::{
    FrameCount, Outcome, Test, dying_entry, quiescent_free_frames, registry_tid, second_cpu,
    sleep_until, spin_until_ns, test,
};
use crate::kva_init;
use crate::paging_init;
use crate::per_cpu_init;
use crate::pmm_init;
use crate::proc_init;
use crate::sched_init;
use crate::thread_init::{self, SpawnError};
use crate::time_init;
use crate::work_init;
use crate::x86;

static SENTINEL: AtomicU64 = AtomicU64::new(0);

fn sentinel_entry() {
    SENTINEL.store(0xC0FFEE, Ordering::SeqCst);
}

pub(crate) fn test_spawn_sentinel() -> Outcome {
    let _g = x86::InterruptGuard::enter();
    SENTINEL.store(0, Ordering::SeqCst);
    let nest0 = per_cpu_init::irq_nest();
    let Ok(h) = thread_init::spawn_here("sentinel", sentinel_entry) else {
        return Outcome::Fail("spawn");
    };
    if h.id() == ThreadId::BOOTSTRAP {
        return Outcome::Fail("spawned bootstrap id");
    }
    thread_init::switch_to(h.id());
    if SENTINEL.load(Ordering::SeqCst) != 0xC0FFEE {
        return Outcome::Fail("sentinel not written");
    }
    if thread_init::name(h.id()) != "sentinel" {
        return Outcome::Fail("name lost");
    }
    if thread_init::current_id() != registry_tid() {
        return Outcome::Fail("did not return to the registry");
    }
    if thread_init::state(h.id()) != ThreadState::Dead {
        return Outcome::Fail("returned thread not dead");
    }
    if per_cpu_init::irq_nest() != nest0 {
        return Outcome::Fail("irq_nest leaked across spawn");
    }
    Outcome::Ok
}

static STEPS: AtomicU64 = AtomicU64::new(0);

static A_ID: AtomicU32 = AtomicU32::new(0);

static B_ID: AtomicU32 = AtomicU32::new(0);

fn thread_a() {
    STEPS.fetch_add(1, Ordering::SeqCst);
    thread_init::switch_to(ThreadId(B_ID.load(Ordering::SeqCst)));
    STEPS.fetch_add(1, Ordering::SeqCst);
}

fn thread_b() {
    STEPS.fetch_add(1, Ordering::SeqCst);
    thread_init::switch_to(registry_tid());
}

pub(crate) fn test_switch_two_threads() -> Outcome {
    let _g = x86::InterruptGuard::enter();
    STEPS.store(0, Ordering::SeqCst);
    let nest0 = per_cpu_init::irq_nest();
    let Ok(a) = thread_init::spawn_here("a", thread_a) else {
        return Outcome::Fail("spawn");
    };
    let Ok(b) = thread_init::spawn_here("b", thread_b) else {
        return Outcome::Fail("spawn");
    };
    A_ID.store(a.id().raw(), Ordering::SeqCst);
    B_ID.store(b.id().raw(), Ordering::SeqCst);
    thread_init::switch_to(a.id());
    if STEPS.load(Ordering::SeqCst) != 2 {
        return Outcome::Fail("expected a then b (2 steps)");
    }
    if thread_init::state(a.id()) != ThreadState::Ready {
        return Outcome::Fail("a should still be ready");
    }
    thread_init::switch_to(a.id());
    if STEPS.load(Ordering::SeqCst) != 3 {
        return Outcome::Fail("a did not resume");
    }
    if thread_init::state(a.id()) != ThreadState::Dead {
        return Outcome::Fail("a not dead after return");
    }
    if per_cpu_init::irq_nest() != nest0 {
        return Outcome::Fail("irq_nest leaked across switch");
    }
    Outcome::Ok
}

static YIELD_FLAG: AtomicU64 = AtomicU64::new(0);

fn yielder_entry() {
    YIELD_FLAG.store(1, Ordering::SeqCst);
    thread_init::yield_now();
    YIELD_FLAG.store(2, Ordering::SeqCst);
}

pub(crate) fn test_yield_now_switches() -> Outcome {
    let _g = x86::InterruptGuard::enter();
    YIELD_FLAG.store(0, Ordering::SeqCst);
    let Ok(_h) = thread_init::spawn_here("yielder", yielder_entry) else {
        return Outcome::Fail("spawn");
    };
    thread_init::yield_now();
    if YIELD_FLAG.load(Ordering::SeqCst) != 1 {
        return Outcome::Fail("yielder did not run");
    }
    thread_init::yield_now();
    if YIELD_FLAG.load(Ordering::SeqCst) != 2 {
        return Outcome::Fail("yielder did not resume");
    }
    Outcome::Ok
}

pub(crate) fn test_sleep_ms_50() -> Outcome {
    let t0 = time_init::uptime_ms();
    let u0 = time_init::now_us();
    thread_init::sleep_ms(50);
    let dt = time_init::uptime_ms().saturating_sub(t0);
    let du = time_init::now_us().saturating_sub(u0) / 1_000;
    if (50..=100).contains(&dt) {
        return Outcome::Ok;
    }
    // TCG: ticks coalesce under SMP; sleep is now_ns. Keep 50–100 on
    // invariant TSC.
    if !crate::time_init::tsc_invariant() && (40..=400).contains(&du) && (1..=400).contains(&dt) {
        return Outcome::Ok;
    }
    crate::marker!("vibeOS: ktest:   sleep_ms dt={dt} du={du}");
    Outcome::Fail("sleep_ms not 50-100ms")
}

static PREEMPT_A: AtomicU64 = AtomicU64::new(0);

static PREEMPT_B: AtomicU64 = AtomicU64::new(0);

static PREEMPT_STOP: AtomicBool = AtomicBool::new(false);

fn preempt_a() {
    while !PREEMPT_STOP.load(Ordering::Relaxed) {
        PREEMPT_A.fetch_add(1, Ordering::Relaxed);
        core::hint::spin_loop();
    }
}

fn preempt_b() {
    while !PREEMPT_STOP.load(Ordering::Relaxed) {
        PREEMPT_B.fetch_add(1, Ordering::Relaxed);
        core::hint::spin_loop();
    }
}

/// How long `preempt_two_threads` waits for both workers to run, and then
/// for both to exit, in 1 ms sleeps.
const PREEMPT_WAIT_MS: u32 = 500;

/// Two CPU-bound workers that never yield, pinned to one AP: both count
/// only if the timer preempts one for the other (ROADMAP §10.2, F142).
pub(crate) fn test_preempt_two_threads() -> Outcome {
    let here = thread_init::current_cpu();
    let mask = per_cpu_init::online_mask();
    let Some(ap) = (1..64u32).find(|&c| c != here && mask & (1u64 << c) != 0) else {
        return Outcome::Skip("no AP");
    };
    PREEMPT_A.store(0, Ordering::SeqCst);
    PREEMPT_B.store(0, Ordering::SeqCst);
    PREEMPT_STOP.store(false, Ordering::SeqCst);
    let a = thread_init::spawn_on("preempt-a", preempt_a, ap);
    let b = thread_init::spawn_on("preempt-b", preempt_b, ap);
    let mut ran = false;
    if a.is_ok() && b.is_ok() {
        let mut waited = 0u32;
        while waited < PREEMPT_WAIT_MS {
            if PREEMPT_A.load(Ordering::Relaxed) > 0 && PREEMPT_B.load(Ordering::Relaxed) > 0 {
                ran = true;
                break;
            }
            thread_init::sleep_ms(1);
            waited += 1;
        }
    }
    PREEMPT_STOP.store(true, Ordering::SeqCst);
    let (na, nb) = (
        PREEMPT_A.load(Ordering::Relaxed),
        PREEMPT_B.load(Ordering::Relaxed),
    );
    // Neither worker may outlive the test: it would hold the AP for the
    // tests after it.
    let mut waited = 0u32;
    let live = |h: &Result<thread_init::ThreadHandle, _>| {
        h.as_ref().is_ok_and(|h| !thread_init::exited(h.id()))
    };
    while (live(&a) || live(&b)) && waited < PREEMPT_WAIT_MS {
        thread_init::sleep_ms(1);
        waited += 1;
    }
    crate::ktest_info!("a={na} b={nb} on cpu{ap}");
    if a.is_err() || b.is_err() {
        return Outcome::Fail("spawn_on");
    }
    if live(&a) || live(&b) {
        return Outcome::Fail("workers did not exit");
    }
    if !ran {
        return Outcome::Fail("no preemption");
    }
    Outcome::Ok
}

pub(crate) fn test_idle_runs() -> Outcome {
    let t0 = sched_init::idle_tsc();
    thread_init::sleep_ms(20);
    let t1 = sched_init::idle_tsc();
    if t1 > t0 {
        Outcome::Ok
    } else {
        crate::marker!("vibeOS: ktest:   idle_tsc {t0} -> {t1}");
        Outcome::Fail("idle did not run")
    }
}

pub(crate) fn test_reap_returns_frames() -> Outcome {
    let before = quiescent_free_frames();
    {
        let _g = x86::InterruptGuard::enter();
        let Ok(h) = thread_init::spawn_here("dying", dying_entry) else {
            return Outcome::Fail("spawn");
        };
        thread_init::yield_now();
        if thread_init::current_id() != registry_tid() {
            return Outcome::Fail("did not return to the registry");
        }
        if !thread_init::exited(h.id()) {
            return Outcome::Fail("returned thread not dead");
        }
    }
    let after = quiescent_free_frames();
    if after != before {
        crate::marker!("vibeOS: ktest:   frames {before} -> {after}");
        return Outcome::Fail("reap did not restore frames");
    }
    Outcome::Ok
}

const REAP_MANY: usize = 16;

/// Two waves of dying threads, the first while the registry sleeps (the
/// last death switches to idle), the second while it runs (the last death
/// resumes it or idle from the preempt path); every stack comes back
/// whichever switch tail took it (ROADMAP §10.2, F074; §10.10).
pub(crate) fn test_reap_many_via_idle() -> Outcome {
    let before = quiescent_free_frames();
    let mut ids = [ThreadId::NONE; REAP_MANY];

    let mut i = 0;
    while i < REAP_MANY {
        let Ok(h) = thread_init::spawn_here("dying", dying_entry) else {
            return Outcome::Fail("spawn");
        };
        ids[i] = h.id();
        i += 1;
    }
    thread_init::sleep_ms(30);
    i = 0;
    while i < REAP_MANY {
        if !thread_init::exited(ids[i]) {
            return Outcome::Fail("parked wave not dead");
        }
        i += 1;
    }

    i = 0;
    while i < REAP_MANY {
        let Ok(h) = thread_init::spawn_here("dying", dying_entry) else {
            return Outcome::Fail("spawn");
        };
        ids[i] = h.id();
        i += 1;
    }
    let t0 = time_init::uptime_ms();
    loop {
        let mut n = 0usize;
        i = 0;
        while i < REAP_MANY {
            if thread_init::exited(ids[i]) {
                n += 1;
            }
            i += 1;
        }
        if n == REAP_MANY {
            break;
        }
        if time_init::uptime_ms().saturating_sub(t0) > 200 {
            return Outcome::Fail("running wave not dead");
        }
        core::hint::spin_loop();
    }

    let after = quiescent_free_frames();
    if after != before {
        crate::marker!("vibeOS: ktest:   frames {before} -> {after}");
        return Outcome::Fail("reap did not restore frames");
    }
    Outcome::Ok
}

pub(crate) fn test_sched_lock_timer_irq() -> Outcome {
    let nest0 = per_cpu_init::irq_nest();
    // The registry is pinned, so the hint is its CPU, whose remote view any
    // IF reads (DESIGN §7.5).
    let Some(me) = per_cpu_init::cpu(thread_init::current_cpu()) else {
        return Outcome::Fail("no remote view");
    };
    let t0 = me.ticks.load(Ordering::Relaxed);
    let wall0 = time_init::uptime_ms();
    loop {
        if me.ticks.load(Ordering::Relaxed) != t0 {
            break;
        }
        if time_init::uptime_ms().saturating_sub(wall0) > 200 {
            return Outcome::Fail("no ticks before lock");
        }
        core::hint::spin_loop();
    }
    // Read under the lock, with IF off: a tick between a read before the
    // lock and the lock's `cli` is not one that ran under SCHED.
    let (inner, held) = thread_init::with_sched_lock(|| {
        let held = me.ticks.load(Ordering::Relaxed);
        if x86::interrupts_enabled() {
            return (Outcome::Fail("SCHED left IF on"), held);
        }
        time_init::busy_wait_ms(20);
        if x86::interrupts_enabled() {
            return (Outcome::Fail("IF on during hold"), held);
        }
        if me.ticks.load(Ordering::Relaxed) != held {
            return (Outcome::Fail("timer ran under SCHED"), held);
        }
        (Outcome::Ok, held)
    });
    match inner {
        Outcome::Ok => {}
        other => return other,
    }
    match apic_init::timer_mode() {
        // SAFETY: a software `int` to the timer vector enters its IDT stub
        // as a tick would, with SCHED free again; established by
        // `apic_init::timer_mode`, which names the live timer's vector.
        TimerMode::Pit => unsafe {
            core::arch::asm!("int $0x20");
        },
        // SAFETY: as above, for the LAPIC timer's vector; established by
        // `apic_init::timer_mode`.
        TimerMode::TscDeadline | TimerMode::Periodic => unsafe {
            core::arch::asm!("int $0xF0");
        },
    }
    if me.ticks.load(Ordering::Relaxed) <= held {
        return Outcome::Fail("forced timer IRQ did not run");
    }
    if per_cpu_init::irq_nest() != nest0 {
        return Outcome::Fail("irq_nest leaked");
    }
    Outcome::Ok
}

const SPAWN_EXIT_N: usize = 2000;

fn spawn_until_dead(name: &'static str) -> Outcome {
    let _g = x86::InterruptGuard::enter();
    let Ok(h) = thread_init::spawn_here(name, dying_entry) else {
        return Outcome::Fail("spawn");
    };
    thread_init::yield_now();
    if !thread_init::exited(h.id()) {
        thread_init::yield_now();
    }
    if !thread_init::exited(h.id()) {
        Outcome::Fail("returned thread not dead")
    } else {
        Outcome::Ok
    }
}

pub(crate) fn test_spawn_exit_thousands() -> Outcome {
    let before = quiescent_free_frames();
    let mut i = 0usize;
    while i < SPAWN_EXIT_N {
        match spawn_until_dead("die") {
            Outcome::Ok => {}
            other => return other,
        }
        i += 1;
    }
    let after = quiescent_free_frames();
    if after != before {
        let h = crate::heap_init::stats();
        let k = kva_init::stats();
        crate::marker!(
            "vibeOS: ktest:   frames {before} -> {after} n={SPAWN_EXIT_N} heap {}/{} kva {}",
            h.used,
            h.capacity,
            k.used
        );
        return Outcome::Fail("spawn/exit leaked frames");
    }
    Outcome::Ok
}

static XCPU_FLAG: AtomicU64 = AtomicU64::new(0);

static XCPU_CPU: AtomicU32 = AtomicU32::new(0xFFFF);

fn xcpu_entry() {
    // Pinned, so the hint is the CPU this thread runs on.
    XCPU_CPU.store(thread_init::current_cpu(), Ordering::SeqCst);
    XCPU_FLAG.store(1, Ordering::SeqCst);
}

pub(crate) fn test_cross_cpu_spawn() -> Outcome {
    let Some(ap) = second_cpu() else {
        return Outcome::Skip("no AP");
    };
    XCPU_FLAG.store(0, Ordering::SeqCst);
    XCPU_CPU.store(0xFFFF, Ordering::SeqCst);
    let Ok(h) = thread_init::spawn_on("xcpu", xcpu_entry, ap) else {
        return Outcome::Fail("spawn");
    };
    if !spin_until_ns(|| XCPU_FLAG.load(Ordering::SeqCst) != 0, 500_000_000) {
        return Outcome::Fail("AP thread did not run");
    }
    if XCPU_CPU.load(Ordering::SeqCst) != ap {
        return Outcome::Fail("thread ran on wrong cpu");
    }
    if !spin_until_ns(|| thread_init::exited(h.id()), 500_000_000) {
        return Outcome::Fail("AP thread did not exit");
    }
    if thread_init::cpu_of(h.id()) != ap {
        return Outcome::Fail("tcb.cpu != ap");
    }
    Outcome::Ok
}

static WQ_HITS: AtomicU32 = AtomicU32::new(0);

fn wq_mark(arg: usize) {
    let _b = Box::new(arg as u8);
    WQ_HITS.fetch_add(arg as u32, Ordering::SeqCst);
}

pub(crate) fn test_workqueue() -> Outcome {
    if !work_live() {
        return Outcome::Fail("work not live");
    }
    WQ_HITS.store(0, Ordering::SeqCst);
    if !work_init::enqueue(wq_mark, 3) {
        return Outcome::Fail("enqueue");
    }
    if !spin_until_ns(|| WQ_HITS.load(Ordering::SeqCst) == 3, 2_000_000_000) {
        return Outcome::Fail("no worker");
    }
    Outcome::Ok
}

/// The registry's stack size ROADMAP §10.2 names.
const REGISTRY_STACK_BYTES: u64 = 64 * 1024;

/// How long [`ktest_context`] yields for its worker.
const WORKER_WAIT_NS: u64 = 1_000_000_000;

const CTX_IF_OFF: u32 = 1 << 0;

const CTX_NEST: u32 = 1 << 1;

const CTX_STACK: u32 = 1 << 2;

static CTX_RESULT: AtomicU32 = AtomicU32::new(0);

static CTX_DONE: AtomicBool = AtomicBool::new(false);

/// The calling thread's context as a [`CTX_IF_OFF`] / [`CTX_NEST`] /
/// [`CTX_STACK`] mask: IF on, `irq_nest` 0, and RSP inside a stack from
/// `alloc_guarded_stack` of `stack_bytes`, whose guard page is unmapped.
fn context_bits(stack_bytes: u64) -> u32 {
    let mut bits = 0;
    if !x86::interrupts_enabled() {
        bits |= CTX_IF_OFF;
    }
    if per_cpu_init::irq_nest() != 0 {
        bits |= CTX_NEST;
    }
    // SAFETY: a running thread's `Tcb` stays in the TCB table
    // (`thread_init::SCHED`) until it exits, and only
    // `thread_init::spawn_inner` and `thread_init::thread_exit` write its
    // `stack`, neither of which runs for this thread while it runs here.
    let stack = unsafe { &(*crate::arch::current_tcb()).stack };
    let guarded = match stack {
        Some(ks) => {
            let rsp = x86::read_rsp();
            rsp > ks.guard().as_u64() + PAGE_SIZE_4K
                && rsp <= ks.top().as_u64()
                && paging_init::translate(ks.guard()).is_none()
                && (ks.pages() as u64) * PAGE_SIZE_4K == stack_bytes
        }
        None => false,
    };
    if !guarded {
        bits |= CTX_STACK;
    }
    bits
}

fn ctx_worker() {
    CTX_RESULT.store(
        context_bits(DEFAULT_STACK_PAGES as u64 * PAGE_SIZE_4K),
        Ordering::Relaxed,
    );
    CTX_DONE.store(true, Ordering::Release);
}

/// The registry, and a `spawn_here` worker it starts, run with IF on,
/// `irq_nest` 0, and a guarded KVA stack (ROADMAP §10.2, F075).
pub(crate) fn ktest_context() -> Outcome {
    if !x86::interrupts_enabled() {
        return Outcome::Fail("registry IF off");
    }
    if per_cpu_init::irq_nest() != 0 {
        return Outcome::Fail("registry irq_nest not 0");
    }
    let bits = context_bits(REGISTRY_STACK_BYTES);
    if bits & CTX_STACK != 0 {
        // Tell the two stack failures apart.
        // SAFETY: as in `context_bits`: a running thread's `Tcb` stays in
        // the TCB table, and only `thread_init::spawn_inner` and
        // `thread_init::thread_exit` write its `stack`.
        let stack = unsafe { &(*crate::arch::current_tcb()).stack };
        return match stack {
            Some(ks) if (ks.pages() as u64) * PAGE_SIZE_4K != REGISTRY_STACK_BYTES => {
                Outcome::Fail("registry stack not 64 KiB")
            }
            _ => Outcome::Fail("registry stack not guarded"),
        };
    }

    CTX_RESULT.store(0, Ordering::Relaxed);
    CTX_DONE.store(false, Ordering::Relaxed);
    if thread_init::spawn_here("ktest-ctx-w", ctx_worker).is_err() {
        return Outcome::Fail("spawn");
    }
    let t0 = time_init::now_ns();
    while !CTX_DONE.load(Ordering::Acquire) {
        if time_init::now_ns().saturating_sub(t0) > WORKER_WAIT_NS {
            return Outcome::Fail("worker did not run");
        }
        thread_init::yield_now();
    }
    let w = CTX_RESULT.load(Ordering::Relaxed);
    if w & CTX_IF_OFF != 0 {
        return Outcome::Fail("worker IF off");
    }
    if w & CTX_NEST != 0 {
        return Outcome::Fail("worker irq_nest not 0");
    }
    if w & CTX_STACK != 0 {
        return Outcome::Fail("worker stack not guarded");
    }
    Outcome::Ok
}

fn dying_entry_s08() {}

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
pub(crate) fn spawn_stack_oom() -> Outcome {
    // A cached stack would let the spawn succeed with the buddy empty.
    thread_init::testing::drain_local_stack_cache();
    let base = quiescent_free_frames();
    let mut held: [Option<Frames>; OOM_HOLD] = [const { None }; OOM_HOLD];
    // IF off on this CPU keeps the drained window short.
    let (drained, r) = {
        let _g = x86::InterruptGuard::enter();
        let drained = drain_buddy(&mut held);
        let r = if drained {
            Some(thread_init::spawn("oom", dying_entry_s08))
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
pub(crate) fn fork_oom() -> Outcome {
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
        match thread_init::spawn_on("exit", dying_entry_s08, 0) {
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
        if ids.iter().all(|&id| thread_init::exited(id)) {
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
/// tails; no switch tail sends a shootdown and the frames come back, to
/// the buddy, a stack cache, or the kernel page tables ([`FrameCount`]).
pub(crate) fn lifetime_stack_reclaim() -> Outcome {
    if !tail_cpus_online() {
        return Outcome::Skip("needs 4 cpus");
    }
    if thread_init::current_cpu() != 0 {
        return Outcome::Fail("registry not on cpu0");
    }
    let base = FrameCount::quiescent();
    let t0 = crate::irq::ktest::shootdowns_from_tail();
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
    let tail = crate::irq::ktest::shootdowns_from_tail().wrapping_sub(t0);
    if tail != 0 {
        return crate::fail_fmt!("{tail} shootdowns sent from a switch tail");
    }
    base.unchanged(&FrameCount::quiescent())
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
pub(crate) fn exit_burst() -> Outcome {
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

/// Per tid bucket ([`tid_bucket`]): spawns that returned a tid in it, and
/// runs of a child with one. Tids are not reused before the allocator
/// wraps, so a child that ran twice or not at all shows as a mismatch.
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

/// `id`'s bucket in [`SPAWNED`] and [`RAN`].
fn tid_bucket(id: ThreadId) -> usize {
    id.raw() as usize % MAX_THREADS
}

fn child_entry() {
    if let Some(r) = RAN.get(tid_bucket(thread_init::current_id())) {
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
                if let Some(s) = SPAWNED.get(tid_bucket(h.id())) {
                    s.fetch_add(1, Ordering::AcqRel);
                }
                n += 1;
            }
            // A free slot comes with CPU 0's next exit, and KVA for a stack
            // with its worker's next free: `Kva::alloc` refuses once
            // `MAX_KVA_RANGES - 2` ranges are live, and the burst parks
            // dead stacks on CPU 0's dead list faster than it frees them.
            Err(SpawnError::NoSlot | SpawnError::NoMemory) => thread_init::yield_now(),
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
pub(crate) fn lifetime_dead_slot_on_cpu() -> Outcome {
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
    for (bucket, (s, r)) in SPAWNED.iter().zip(RAN.iter()).enumerate() {
        let (s, r) = (s.load(Ordering::Acquire), r.load(Ordering::Acquire));
        if s != r {
            return crate::fail_fmt!("tid bucket {bucket}: spawned {s}, ran {r}");
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

// P: fill XMM0-15 with the pattern, sched_yield 20,000 times, then exit 0
// only if all 16 still hold it.
user_code!(
    FP_PATTERN_YIELD,
    "
    mov rax, 0x5A5A5A5A5A5A5A5A
    movq xmm0, rax
    movq xmm1, rax
    movq xmm2, rax
    movq xmm3, rax
    movq xmm4, rax
    movq xmm5, rax
    movq xmm6, rax
    movq xmm7, rax
    movq xmm8, rax
    movq xmm9, rax
    movq xmm10, rax
    movq xmm11, rax
    movq xmm12, rax
    movq xmm13, rax
    movq xmm14, rax
    movq xmm15, rax
    mov r12d, 20000
1:
    mov eax, 24
    syscall
    dec r12d
    jnz 1b
    mov rbx, 0x5A5A5A5A5A5A5A5A
    movq rax, xmm0
    cmp rax, rbx
    jne 9f
    movq rax, xmm1
    cmp rax, rbx
    jne 9f
    movq rax, xmm2
    cmp rax, rbx
    jne 9f
    movq rax, xmm3
    cmp rax, rbx
    jne 9f
    movq rax, xmm4
    cmp rax, rbx
    jne 9f
    movq rax, xmm5
    cmp rax, rbx
    jne 9f
    movq rax, xmm6
    cmp rax, rbx
    jne 9f
    movq rax, xmm7
    cmp rax, rbx
    jne 9f
    movq rax, xmm8
    cmp rax, rbx
    jne 9f
    movq rax, xmm9
    cmp rax, rbx
    jne 9f
    movq rax, xmm10
    cmp rax, rbx
    jne 9f
    movq rax, xmm11
    cmp rax, rbx
    jne 9f
    movq rax, xmm12
    cmp rax, rbx
    jne 9f
    movq rax, xmm13
    cmp rax, rbx
    jne 9f
    movq rax, xmm14
    cmp rax, rbx
    jne 9f
    movq rax, xmm15
    cmp rax, rbx
    jne 9f
    xor edi, edi
    mov eax, 60
    syscall
9:
    mov edi, 1
    mov eax, 60
    syscall
    ud2
    "
);

// Q: exit 1 if any XMM register holds P's pattern at entry, else 0.
user_code!(
    FP_PATTERN_PROBE,
    "
    mov rbx, 0x5A5A5A5A5A5A5A5A
    movq rax, xmm0
    cmp rax, rbx
    je 9f
    movq rax, xmm1
    cmp rax, rbx
    je 9f
    movq rax, xmm2
    cmp rax, rbx
    je 9f
    movq rax, xmm3
    cmp rax, rbx
    je 9f
    movq rax, xmm4
    cmp rax, rbx
    je 9f
    movq rax, xmm5
    cmp rax, rbx
    je 9f
    movq rax, xmm6
    cmp rax, rbx
    je 9f
    movq rax, xmm7
    cmp rax, rbx
    je 9f
    movq rax, xmm8
    cmp rax, rbx
    je 9f
    movq rax, xmm9
    cmp rax, rbx
    je 9f
    movq rax, xmm10
    cmp rax, rbx
    je 9f
    movq rax, xmm11
    cmp rax, rbx
    je 9f
    movq rax, xmm12
    cmp rax, rbx
    je 9f
    movq rax, xmm13
    cmp rax, rbx
    je 9f
    movq rax, xmm14
    cmp rax, rbx
    je 9f
    movq rax, xmm15
    cmp rax, rbx
    je 9f
    xor edi, edi
    mov eax, 60
    syscall
9:
    mov edi, 1
    mov eax, 60
    syscall
    ud2
    "
);

static FP_PIDS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

static FP_SPAWNED: AtomicBool = AtomicBool::new(false);

/// Pinned to the registry's CPU: P, then 20 ms later Q, on that CPU.
fn fp_spawner() {
    let spawn = |code, name| match user::spawn(&Image::Code(code, DEFAULT), &[name]) {
        Ok(pid) => u64::from(pid),
        Err(_) => u64::MAX,
    };
    FP_PIDS[0].store(spawn(FP_PATTERN_YIELD, "fp_p"), Ordering::Relaxed);
    thread_init::sleep_ms(20);
    FP_PIDS[1].store(spawn(FP_PATTERN_PROBE, "fp_q"), Ordering::Relaxed);
    FP_SPAWNED.store(true, Ordering::Release);
}

/// A new process never sees another process's XMM state at its first
/// instruction, and a yielding process keeps its own.
pub(crate) fn test_fp_no_leak() -> Outcome {
    if x86::read_cr0() & x86::CR0_TS != 0 {
        return Outcome::Fail("CR0.TS set");
    }
    FP_SPAWNED.store(false, Ordering::Release);
    crate::ktest::spawn_thread_on("fp_spawner", fp_spawner, thread_init::current_cpu());
    if !sleep_until(|| FP_SPAWNED.load(Ordering::Acquire), 5_000) {
        return Outcome::Fail("spawner did not run");
    }
    let pids = [0, 1].map(|i| u32::try_from(FP_PIDS[i].load(Ordering::Relaxed)).ok());
    let sts = pids.map(|p| p.map(user::wait));
    match sts {
        [Some(0), Some(0)] => Outcome::Ok,
        [Some(p), Some(q)] => crate::fail_fmt!("P status {p:#x}, Q status {q:#x}, want 0 and 0"),
        _ => Outcome::Fail("spawn"),
    }
}

// Keep a counter in xmm0 and in memory, compare them on every iteration,
// exit 1 on a mismatch; getpid every 4,096 iterations so a kill lands.
user_code!(
    FP_COUNTER,
    "
    sub rsp, 16
    mov qword ptr [rsp], 0
    mov eax, 1
    movq xmm1, rax
    pxor xmm0, xmm0
    xor r12d, r12d
1:
    paddq xmm0, xmm1
    add qword ptr [rsp], 1
    movq rax, xmm0
    cmp rax, qword ptr [rsp]
    jne 9f
    inc r12d
    test r12d, 0xfff
    jnz 1b
    mov eax, 39
    syscall
    jmp 1b
9:
    mov edi, 1
    mov eax, 60
    syscall
    ud2
    "
);

/// A user thread that the requeue hook moves to the next CPU each time
/// it is preempted keeps its XMM state across every migration.
pub(crate) fn test_fp_migrate_counter() -> Outcome {
    if crate::ktest::second_cpu().is_none() {
        return Outcome::Skip("needs 2 CPUs");
    }
    let before = requeues();
    let pid = {
        set_requeue_next_cpu(true);
        let _g = RequeueGuard;
        let pid = match user::spawn(&Image::Code(FP_COUNTER, DEFAULT), &["fp_counter"]) {
            Ok(pid) => pid,
            Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
        };
        thread_init::sleep_ms(2000);
        pid
    };
    // A failed kill shows as the status check below.
    let _ = proc_init::dispatch(SYS_KILL, [u64::from(pid), u64::from(SIGKILL), 0, 0, 0, 0]);
    let st = user::wait(pid);
    let moves = requeues().wrapping_sub(before);
    if st != wait_signaled(SIGKILL) {
        if st == 1 << 8 {
            return crate::fail_fmt!("xmm0 counter mismatch after {moves} moves");
        }
        return crate::fail_fmt!("status {st:#x}, want SIGKILL");
    }
    if moves < 8 {
        return crate::fail_fmt!("{moves} requeues in 2 s, want 8");
    }
    Outcome::Ok
}

/// ROADMAP §10.6: the bootstrap thread runs on a guarded 64 KiB KVA stack
/// that its `Tcb.stack` records, with the page below it unmapped.
pub(crate) fn boot_stack_guarded() -> Outcome {
    let Some((range, rsp, running)) = thread_init::bootstrap_stack() else {
        return Outcome::Fail("bootstrap Tcb.stack not set");
    };
    let want = (thread_init::BOOT_STACK_PAGES as u64) * PAGE_SIZE_4K;
    if range.end.wrapping_sub(range.start) != want {
        return crate::fail_fmt!(
            "bootstrap stack {:#x}..{:#x}, want {want} bytes",
            range.start,
            range.end
        );
    }
    let guard = range.start.wrapping_sub(PAGE_SIZE_4K);
    if paging_init::translate(vibeos::paging::VirtAddr(guard)).is_some() {
        return crate::fail_fmt!("guard page {guard:#x} below the bootstrap stack is mapped");
    }
    if paging_init::translate(vibeos::paging::VirtAddr(range.start)).is_none() {
        return crate::fail_fmt!("bootstrap stack base {:#x} unmapped", range.start);
    }
    if !running && !range.contains(&rsp) {
        return crate::fail_fmt!("saved rsp {rsp:#x} outside the bootstrap stack");
    }
    Outcome::Ok
}

/// This subsystem's in-guest tests, in run order; `crate::ktest::GROUPS`
/// runs them (DESIGN §8.2).
pub(crate) const TESTS: &[Test] = &[
    test("spawn_sentinel", test_spawn_sentinel),
    test("switch_two_threads", test_switch_two_threads),
    test("yield_now_switches", test_yield_now_switches),
    test("sleep_ms_50", test_sleep_ms_50),
    test("preempt_two_threads", test_preempt_two_threads),
    test("idle_runs", test_idle_runs),
    test("reap_returns_frames", test_reap_returns_frames),
    test("reap_many_via_idle", test_reap_many_via_idle),
    test("sched_lock_timer_irq", test_sched_lock_timer_irq),
    test("spawn_exit_thousands", test_spawn_exit_thousands),
    test("cross_cpu_spawn", test_cross_cpu_spawn),
    test("counted_deferred_release", test_counted_deferred_release),
    test("workqueue", test_workqueue),
    test("ktest_rows", test_ktest_rows),
    test("ktest_fail_fmt", test_ktest_fail_fmt),
    test("ktest_helpers", test_ktest_helpers),
    test("ktest_context", ktest_context),
    test("spawn_stack_oom", spawn_stack_oom).deadline(10_000),
    test("fork_oom", fork_oom).deadline(10_000),
    test("lifetime_stack_reclaim", lifetime_stack_reclaim).deadline(120_000),
    test("dead_list_batched_rounds", dead_list_batched_rounds),
    test("exit_burst", exit_burst).deadline(60_000),
    test("lifetime_dead_slot_on_cpu", lifetime_dead_slot_on_cpu).deadline(180_000),
    test("fp_no_leak", test_fp_no_leak).deadline(60_000),
    test(
        "requeue_moves_each_dequeue",
        test_requeue_moves_each_dequeue,
    ),
    test("fp_migrate_counter", test_fp_migrate_counter).deadline(30_000),
    test("lock_across_switch_asserts", lock_across_switch_asserts),
    test("block_in_hard_irq_asserts", block_in_hard_irq_asserts),
    test("in_hard_irq_top_bottom", in_hard_irq_top_bottom),
    test("sleep_under_spinlock_asserts", sleep_under_spinlock_asserts),
    test("boot_stack_guarded", boot_stack_guarded),
    test("stack_depth_exit_scan", stack_depth_exit_scan),
    test("stack_depth_planted", stack_depth_planted)
        .opt_in()
        .once(),
];
