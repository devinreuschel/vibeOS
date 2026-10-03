//! In-guest tests for sync (kernel_tests only). Rows: [`TESTS`].

use core::alloc::Layout;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::lock::{RANK_BUDDY, RANK_DEVICE, RANK_HEAP, RANK_PT, RANK_SCHED, RANK_SERIAL};
use vibeos::sync::OpGate;
use vibeos::thread::{ThreadId, ThreadState};
use vibeos::time::Instant;

use crate::arch;
use crate::ktest::{Outcome, Test, sleep_until, spawn_thread, test};
use crate::kva_init;
use crate::per_cpu_init;
use crate::sync::blocking_init::{
    BlockingMutex, Channel, Condvar, RwLock, RwLockReadGuard, Semaphore,
};
use crate::sync_init::{self, SpinMutex};
use crate::thread_init;
use crate::time_init;

pub(crate) fn test_irqcell_reentry_panics() -> Outcome {
    static C: crate::cell::IrqCell<u32> = crate::cell::IrqCell::new(0);
    // The longjmp skips both `IrqCell` guards; this one restores IF.
    let _g = crate::arch::current::InterruptGuard::enter();
    let nest0 = per_cpu_init::irq_nest();
    let hit = arch::catch::catch_panic(|| {
        C.with(|_| {
            C.with(|_| {});
        });
    });
    // SAFETY: the `arch::catch` longjmp skipped both `Unlock`s and neither
    // closure resumes, so the holder never touches `C` again; established
    // here.
    unsafe { C.force_unlock() };
    crate::ktest::restore_irq_nest(nest0);
    if !hit {
        return Outcome::Fail("no panic");
    }
    C.with(|v| *v = 3);
    if C.with(|v| *v) != 3 {
        return Outcome::Fail("after unlock");
    }
    if per_cpu_init::irq_nest() != nest0 {
        return Outcome::Fail("irq_nest leaked");
    }
    Outcome::Ok
}

pub(crate) fn test_bootcell_set_once() -> Outcome {
    static C: crate::cell::BootCell<u32> = crate::cell::BootCell::new();
    if C.try_get().is_some() {
        return Outcome::Fail("already set");
    }
    // SAFETY: `C` is this test's own static, set once here and never
    // before, and no other CPU reads it; established here.
    unsafe { C.set(7) };
    match C.try_get() {
        Some(&7) => {}
        _ => return Outcome::Fail("get"),
    }
    static U: crate::cell::BootCell<u32> = crate::cell::BootCell::new();
    let nest0 = per_cpu_init::irq_nest();
    let hit = arch::catch::catch_panic(|| {
        let _ = U.get();
    });
    crate::ktest::restore_irq_nest(nest0);
    if !hit {
        return Outcome::Fail("unset get");
    }
    Outcome::Ok
}

pub(crate) fn test_spin_mutex() -> Outcome {
    let m = SpinMutex::new(0u64);
    {
        let mut g = m.lock();
        if crate::arch::current::interrupts_enabled() {
            return Outcome::Fail("lock left IF on");
        }
        *g = 42;
    }
    if *m.lock() != 42 {
        return Outcome::Fail("value lost");
    }
    Outcome::Ok
}

impl<T> SpinMutex<T> {
    /// An unranked lock, which the rank checker does not count; production
    /// locks take a rank (`SpinMutex::with_rank`, AGENTS.md Cells).
    pub const fn new(v: T) -> Self {
        Self::with_rank(v, 0)
    }
}

/// Spin iterations per lock rank (index by rank; 0 unused). Phase 19 baseline.
pub(crate) fn spin_counts() -> [u64; sync_init::SPIN_RANKS] {
    core::array::from_fn(|i| sync_init::SPINS[i].load(Ordering::Relaxed))
}

/// Print the spin counters per lock rank at the end of a run, as an info
/// line: a measurement, never a result, so no test row counts it as a pass
/// (DESIGN §8.2, F142).
pub(crate) fn report_spins() {
    let c = spin_counts();
    crate::ktest_info!(
        "spins heap={} pt={} buddy={} sched={} device={} serial={}",
        c[usize::from(RANK_HEAP)],
        c[usize::from(RANK_PT)],
        c[usize::from(RANK_BUDDY)],
        c[usize::from(RANK_SCHED)],
        c[usize::from(RANK_DEVICE)],
        c[usize::from(RANK_SERIAL)]
    );
}

/// One byte allocated and freed under PT must fail the rank check: the heap
/// ranks first (DESIGN §2.1).
pub(crate) fn test_rank_alloc_under_pt_asserts() -> Outcome {
    let layout = Layout::new::<u8>();
    let if0 = crate::arch::current::interrupts_enabled();
    let nest0 = per_cpu_init::irq_nest();
    let held0 = sync_init::testing::held();
    let fails0 = sync_init::testing::rank_failures();
    let hit = crate::paging_init::with_pt(|_pt| {
        let held_pt = sync_init::testing::held();
        let nest_pt = per_cpu_init::irq_nest();
        let hit = arch::catch::catch_panic(|| {
            // `black_box` keeps LLVM from eliding the unused pair.
            // SAFETY: `layout` has a nonzero size; established here.
            let p = core::hint::black_box(unsafe { alloc::alloc::alloc(layout) });
            if !p.is_null() {
                // SAFETY: `p` came from `alloc` with `layout` just above;
                // established here.
                unsafe { alloc::alloc::dealloc(p, layout) };
            }
        });
        // The longjmp skipped the `InterruptGuard` that `HEAP.lock()`
        // entered; PT's guard drop restores IF.
        crate::ktest::restore_irq_nest(nest_pt);
        // SAFETY: invariant: a rank refusal panics in
        // `sync_init::lock_enter` before the spin, so `HEAP` was never taken
        // and `held_pt`, the word with PT counted, is what this CPU holds;
        // established by `sync_init::SpinMutex::lock`.
        unsafe { sync_init::testing::restore_held(held_pt) };
        hit
    });
    if !hit {
        return Outcome::Fail("allocation under PT passed the rank check");
    }
    let fails = sync_init::testing::rank_failures() - fails0;
    if fails != 1 {
        return crate::fail_fmt!("{} rank failures, want 1", fails);
    }
    if crate::arch::current::interrupts_enabled() != if0 || per_cpu_init::irq_nest() != nest0 {
        return Outcome::Fail("IF or irq_nest changed");
    }
    if sync_init::testing::held() != held0 || sync_init::held_mask() != held0.mask() {
        return Outcome::Fail("held word changed");
    }
    // SAFETY: `layout` has a nonzero size; established here.
    let p = core::hint::black_box(unsafe { alloc::alloc::alloc(layout) });
    if p.is_null() {
        return Outcome::Fail("heap unusable after the catch");
    }
    // SAFETY: `p` came from `alloc` with `layout` just above; established
    // here.
    unsafe { alloc::alloc::dealloc(p, layout) };
    Outcome::Ok
}

/// Two `RANK_DEVICE` locks the rank tests nest.
static RANK_A: SpinMutex<u32> = SpinMutex::with_rank(0, RANK_DEVICE);
static RANK_B: SpinMutex<u32> = SpinMutex::with_rank(0, RANK_DEVICE);

/// A second `RANK_DEVICE` lock taken with `lock` or `try_lock` while one is
/// held fails the rank check (DESIGN §2.3's nesting rule).
pub(crate) fn test_rank_same_rank_lock_asserts() -> Outcome {
    let if0 = crate::arch::current::interrupts_enabled();
    let nest0 = per_cpu_init::irq_nest();
    let held0 = sync_init::testing::held();
    let fails0 = sync_init::testing::rank_failures();
    let hits;
    {
        let _a = RANK_A.lock();
        let held_a = sync_init::testing::held();
        let nest_a = per_cpu_init::irq_nest();
        let restore = || {
            // The longjmp skipped the `InterruptGuard` `RANK_B`'s acquire
            // entered; `RANK_A`'s guard drop restores IF.
            crate::ktest::restore_irq_nest(nest_a);
            // SAFETY: invariant: a rank refusal panics in
            // `sync_init::lock_enter` before the spin, so `RANK_B` was never
            // taken and `held_a`, the word with `RANK_A` counted, is what
            // this CPU holds; established by `sync_init::SpinMutex::lock`
            // and `try_lock`.
            unsafe { sync_init::testing::restore_held(held_a) };
        };
        let hit_lock = arch::catch::catch_panic(|| {
            let _b = RANK_B.lock();
        });
        restore();
        let hit_try = arch::catch::catch_panic(|| {
            let _b = RANK_B.try_lock();
        });
        restore();
        hits = [hit_lock, hit_try];
    }
    if hits != [true, true] {
        return crate::fail_fmt!("hits [lock, try_lock] = {:?}, want both", hits);
    }
    let fails = sync_init::testing::rank_failures() - fails0;
    if fails != 2 {
        return crate::fail_fmt!("{} rank failures, want 2", fails);
    }
    if RANK_B.is_locked() {
        return Outcome::Fail("RANK_B left held");
    }
    if crate::arch::current::interrupts_enabled() != if0 || per_cpu_init::irq_nest() != nest0 {
        return Outcome::Fail("IF or irq_nest changed");
    }
    if sync_init::testing::held() != held0 || sync_init::held_mask() != held0.mask() {
        return Outcome::Fail("held word changed");
    }
    Outcome::Ok
}

pub(crate) fn test_rank_lock_nested_keeps_outer() -> Outcome {
    let if0 = crate::arch::current::interrupts_enabled();
    let nest0 = per_cpu_init::irq_nest();
    let held0 = sync_init::testing::held();
    let fails0 = sync_init::testing::rank_failures();
    let mut counts = [u8::MAX; 2];
    let hit;
    {
        let _a = RANK_A.lock();
        let held_a = sync_init::testing::held();
        let nest_a = per_cpu_init::irq_nest();
        hit = arch::catch::catch_panic(|| {
            // pair order: RANK_A, then RANK_B
            let _b = RANK_B.lock_nested(1);
            counts[0] = sync_init::testing::held().count(RANK_DEVICE);
        });
        if hit {
            // SAFETY: invariant: a rank refusal panics in
            // `sync_init::lock_enter` before the spin, so `RANK_B` was never
            // taken and `held_a`, this CPU's word with `RANK_A` alone
            // counted, is what it holds; established by
            // `sync_init::SpinMutex::lock_nested`.
            unsafe { sync_init::testing::restore_held(held_a) };
            crate::ktest::restore_irq_nest(nest_a);
        }
        counts[1] = sync_init::testing::held().count(RANK_DEVICE);
    }
    let after = sync_init::testing::held();
    if hit {
        return Outcome::Fail("lock_nested hit the rank check");
    }
    if counts != [2, 1] {
        return crate::fail_fmt!("device counts {:?}, want [2, 1]", counts);
    }
    if after.count(RANK_DEVICE) != 0 || after != held0 {
        return crate::fail_fmt!("held {:#x} after, {:#x} before", after.raw(), held0.raw());
    }
    if sync_init::held_mask() != held0.mask() {
        return Outcome::Fail("held_mask changed");
    }
    if sync_init::testing::rank_failures() != fails0 {
        return Outcome::Fail("rank failure counted");
    }
    if crate::arch::current::interrupts_enabled() != if0 || per_cpu_init::irq_nest() != nest0 {
        return Outcome::Fail("IF or irq_nest changed");
    }
    if RANK_B.is_locked() {
        return Outcome::Fail("RANK_B left held");
    }
    Outcome::Ok
}

fn noop_work(_: usize) {}

/// Each cross-CPU cell box 1304 converted, the call that takes it as
/// production does, and what the acquisition trace must hold for it.
struct CellCase {
    name: &'static str,
    take: fn(),
    file: &'static str,
    rank: u8,
    count: u8,
}

const CELL_CASES: &[CellCase] = &[
    CellCase {
        name: "kva_init::KVA",
        take: || {
            let _ = kva_init::stats();
        },
        file: "src/mm/kva_init.rs",
        rank: RANK_PT,
        count: 2,
    },
    CellCase {
        name: "proc_init::TABLE",
        take: || {
            let _ = crate::proc_init::dispatch(vibeos::syscall::SYS_GETPPID, [0; 6]);
        },
        file: "src/proc/proc_init/mod.rs",
        rank: RANK_SCHED,
        count: 2,
    },
    CellCase {
        name: "work_init::ST",
        take: || {
            if !crate::work_init::enqueue(noop_work, 0) {
                crate::klog!(vibeos::log::Level::Warn, "ktest: work ring full");
            }
        },
        file: "src/sched/work_init.rs",
        rank: RANK_SCHED,
        count: 2,
    },
    CellCase {
        name: "irq_init::IRQ",
        take: || {
            let _ = crate::irq_init::cpu_of(0x40);
        },
        file: "src/irq/irq_init.rs",
        rank: RANK_DEVICE,
        count: 1,
    },
    CellCase {
        name: "apic_init::STATE",
        take: || {
            let _ = crate::apic_init::timer_mode();
        },
        file: "src/arch/x86_64/apic_init.rs",
        rank: RANK_DEVICE,
        count: 1,
    },
    CellCase {
        name: "kbd_init::KBD",
        take: || {
            if let Some(k) = crate::kbd_init::pop() {
                crate::console::ktest::push_for_test(k);
            }
        },
        file: "src/console/kbd_init.rs",
        rank: RANK_DEVICE,
        count: 1,
    },
    CellCase {
        name: "pci_init::ECAM",
        take: || {
            let bdf = vibeos::dev::pci::Bdf {
                segment: 0,
                bus: 0,
                device: 0,
                function: 0,
            };
            let _ = crate::dev::ktest::cfg_read32(bdf, 0);
        },
        file: "src/dev/pci_init.rs",
        rank: RANK_DEVICE,
        count: 1,
    },
    CellCase {
        name: "shell::cmds::fs::CWD_REF",
        take: || {
            let _ = crate::shell::cmds::fs::shell_cwd();
        },
        file: "src/shell/cmds/fs.rs",
        rank: RANK_DEVICE,
        count: 1,
    },
    CellCase {
        name: "fat_init::INITRD",
        take: crate::fs::ktest::probe_initrd,
        file: "src/fs/fat_init.rs",
        rank: RANK_DEVICE,
        count: 1,
    },
    CellCase {
        name: "vibefs_init::Media::Mem image",
        take: crate::fs::ktest::probe_image,
        file: "src/fs/vibefs_init.rs",
        rank: RANK_DEVICE,
        count: 1,
    },
];

/// Every cross-CPU cell box 1304 converted is a ranked `SpinMutex`: taking
/// it through its production path records an acquisition in its file, at
/// its rank, nested where it pairs with the lock its callers hold.
pub(crate) fn test_cross_cpu_cells_ranked() -> Outcome {
    for c in CELL_CASES {
        let trace = {
            let _irq = crate::arch::current::InterruptGuard::enter();
            sync_init::testing::trace_arm();
            (c.take)();
            sync_init::testing::trace_take()
        };
        let found = trace
            .iter()
            .flatten()
            .any(|e| e.at.file() == c.file && e.rank == c.rank && e.count == c.count);
        if !found {
            let mut seen = 0usize;
            for e in trace.iter().flatten() {
                if e.at.file() == c.file {
                    return crate::fail_fmt!(
                        "{}: rank {} count {} at line {}, want rank {} count {}",
                        c.name,
                        e.rank,
                        e.count,
                        e.at.line(),
                        c.rank,
                        c.count
                    );
                }
                seen += 1;
            }
            return crate::fail_fmt!("{}: no ranked acquisition ({} others)", c.name, seen);
        }
    }
    Outcome::Ok
}

const MUTEX_ITERS: u64 = 1000;

static COUNTER: BlockingMutex<u64> = BlockingMutex::new(0);

static MUTEX_DONE: AtomicU32 = AtomicU32::new(0);

fn mutex_worker() {
    let mut i = 0u64;
    while i < MUTEX_ITERS {
        let mut g = COUNTER.lock();
        *g = (*g).wrapping_add(1);
        i += 1;
    }
    MUTEX_DONE.fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn test_blocking_mutex_counter() -> Outcome {
    MUTEX_DONE.store(0, Ordering::SeqCst);
    *COUNTER.lock() = 0;
    let Ok(_a) = thread_init::spawn("mu-a", mutex_worker) else {
        return Outcome::Fail("spawn");
    };
    let Ok(_b) = thread_init::spawn("mu-b", mutex_worker) else {
        return Outcome::Fail("spawn");
    };
    let t0 = time_init::uptime_ms();
    loop {
        if MUTEX_DONE.load(Ordering::SeqCst) == 2 {
            break;
        }
        if time_init::uptime_ms().saturating_sub(t0) > 8_000 {
            let n = MUTEX_DONE.load(Ordering::SeqCst);
            let c = *COUNTER.lock();
            crate::marker!("vibeOS: ktest:   mutex done={n} count={c}");
            return Outcome::Fail("mutex stall");
        }
        thread_init::yield_now();
    }
    let c = *COUNTER.lock();
    if c != MUTEX_ITERS * 2 {
        crate::marker!("vibeOS: ktest:   mutex count={c}");
        return Outcome::Fail("mutex count");
    }
    Outcome::Ok
}

static LATE_M: BlockingMutex<u64> = BlockingMutex::new(0);
/// Runs of [`late_wake_waiter`]'s entry.
static LATE_RUNS: AtomicU32 = AtomicU32::new(0);
static LATE_PROBE: AtomicBool = AtomicBool::new(false);
/// Set by [`late_wake_call`] on the CPU the waiter is held on.
static LATE_CALLED: AtomicBool = AtomicBool::new(false);
/// Bound on each wait in [`test_late_wake_after_exit`].
const LATE_WAIT_MS: u64 = 2_000;

/// Block on [`LATE_M`] with the wait window held, then take it and exit.
fn late_wake_waiter() {
    LATE_RUNS.fetch_add(1, Ordering::AcqRel);
    thread_init::testing::arm_wait_window(thread_init::current_id());
    let mut g = LATE_M.lock();
    *g = (*g).wrapping_add(1);
}

fn late_wake_probe() {
    LATE_PROBE.store(true, Ordering::Release);
}

fn late_wake_call(_: *mut ()) {
    LATE_CALLED.store(true, Ordering::Release);
}

/// Wait, bounded, until `pred` holds, yielding between looks.
fn late_wait(pred: impl Fn() -> bool) -> bool {
    let t0 = time_init::uptime_ms();
    while !pred() {
        if time_init::uptime_ms().saturating_sub(t0) > LATE_WAIT_MS {
            return false;
        }
        thread_init::yield_now();
    }
    true
}

/// A wake whose push to the woken thread's CPU lands after that thread has
/// already resumed, through its own `schedule` finding it `Ready`, and has
/// exited, does not run the dead thread again (ROADMAP §10.2): a waiter on
/// CPU 1 is held between queueing on a mutex and its `schedule`; the unlock
/// makes it `Ready`, and the hook holds the waker between dropping SCHED
/// and pushing the waiter to CPU 1 until the waiter has taken the mutex and
/// exited. CPU 1 then takes that push; it stays up, and the waiter's entry
/// ran once. `blocking_mutex_counter` panicked `dead thread resumed` this
/// way at random under `-smp 4`.
pub(crate) fn test_late_wake_after_exit() -> Outcome {
    if !per_cpu_init::is_online(1) {
        return Outcome::Skip("needs 2 cpus");
    }
    LATE_RUNS.store(0, Ordering::Release);
    LATE_PROBE.store(false, Ordering::Release);
    let g = LATE_M.lock();
    let waiter = match thread_init::spawn_on("late-wake", late_wake_waiter, 1) {
        Ok(h) => h.id(),
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if !late_wait(thread_init::testing::wait_window_held) {
        thread_init::testing::disarm_late_wake();
        drop(g);
        return Outcome::Fail("waiter did not block");
    }
    // The hold keeps serving other CPUs' IPIs, as every IF-off spin must:
    // a call-function IPI to CPU 1 completes while the waiter is still
    // held. A hold that starved it would stall its sender, such as a
    // shootdown from this CPU, until the hold gave up.
    LATE_CALLED.store(false, Ordering::Release);
    crate::ipi_init::call_cpu(1, late_wake_call, core::ptr::null_mut(), true);
    if !LATE_CALLED.load(Ordering::Acquire) || !thread_init::testing::wait_window_held() {
        thread_init::testing::disarm_late_wake();
        drop(g);
        return Outcome::Fail("a call to the held CPU waited out the hold");
    }
    thread_init::testing::arm_late_wake(waiter);
    // The unlock's wake: its push to CPU 1 goes out after the waiter died.
    drop(g);
    thread_init::testing::disarm_late_wake();
    if !thread_init::testing::late_wake_after_death() {
        return Outcome::Fail("the wake went out before the waiter exited");
    }
    // Wait for CPU 1 to take the push off its inbox and its run queue
    // before any spawn can reuse the waiter's thread slot, then check that
    // CPU 1 still schedules.
    let Some(cpu1) = crate::ktest::cpu_remote(1) else {
        return Outcome::Fail("no cpu1");
    };
    if !late_wait(|| cpu1.wake_inbox.is_empty() && cpu1.runq_len.load(Ordering::Relaxed) == 0) {
        return Outcome::Fail("cpu1 did not take the wake");
    }
    if let Err(e) = thread_init::spawn_on("late-probe", late_wake_probe, 1) {
        return crate::fail_fmt!("probe spawn: {}", e.as_str());
    }
    if !late_wait(|| LATE_PROBE.load(Ordering::Acquire)) {
        return Outcome::Fail("cpu1 did not run the probe");
    }
    let runs = LATE_RUNS.load(Ordering::Acquire);
    if runs != 1 {
        return crate::fail_fmt!("waiter entry ran {runs} times");
    }
    Outcome::Ok
}

static RW: RwLock<u64> = RwLock::new(0);

static RW_DONE: AtomicU32 = AtomicU32::new(0);

fn rw_writer() {
    let mut g = RW.write();
    *g = (*g).wrapping_add(1);
    RW_DONE.fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn test_rwlock_exclusion() -> Outcome {
    RW_DONE.store(0, Ordering::SeqCst);
    *RW.write() = 0;
    {
        let r = RW.read();
        let Ok(_a) = thread_init::spawn("rw-a", rw_writer) else {
            return Outcome::Fail("spawn");
        };
        let Ok(_b) = thread_init::spawn("rw-b", rw_writer) else {
            return Outcome::Fail("spawn");
        };
        thread_init::yield_now();
        thread_init::sleep_ms(5);
        if RW_DONE.load(Ordering::SeqCst) != 0 {
            return Outcome::Fail("writer ran under read");
        }
        if *r != 0 {
            return Outcome::Fail("reader saw writer");
        }
        drop(r);
    }
    let t0 = time_init::uptime_ms();
    loop {
        if RW_DONE.load(Ordering::SeqCst) == 2 {
            break;
        }
        if time_init::uptime_ms().saturating_sub(t0) > 2_000 {
            return Outcome::Fail("rwlock stall");
        }
        thread_init::yield_now();
    }
    if *RW.read() != 2 {
        return Outcome::Fail("rwlock count");
    }
    Outcome::Ok
}

static RW_TO: RwLock<u64> = RwLock::new(0);

static RW_WR_OUT: AtomicU32 = AtomicU32::new(0);

static RW_RD_GOT: AtomicU32 = AtomicU32::new(0);

/// Queue for write with no deadline of its own: the test gives it one once
/// the reader has queued behind it (`testing::set_wait_deadline`).
fn rw_timeout_writer() {
    match RW_TO.write_until(None) {
        None => RW_WR_OUT.store(1, Ordering::SeqCst),
        Some(_g) => RW_WR_OUT.store(2, Ordering::SeqCst),
    }
}

fn rw_pref_reader() {
    let _g = RW_TO.read();
    RW_RD_GOT.store(1, Ordering::SeqCst);
}

/// Wait for thread `id` to block; false when it ran to its end instead or
/// the run's deadline drew near (`ktest::wait_for`).
fn blocks(id: ThreadId) -> bool {
    crate::ktest::wait_for(|| {
        !matches!(
            thread_init::try_state(id),
            Some(ThreadState::Ready | ThreadState::Running | ThreadState::Sleeping { .. })
        )
    }) && matches!(
        thread_init::try_state(id),
        Some(ThreadState::Blocked { .. })
    )
}

/// A queued writer that times out wakes the readers queued behind it
/// (writer preference would leave them parked). Under a read hold, the
/// writer queues with no deadline, a reader queues behind it, and only
/// then does the writer's deadline arrive, so no host delay reorders the
/// three steps. The writer's timeout path wakes the readers before it
/// returns, so once the writer is Dead the reader must not be `Blocked`.
pub(crate) fn test_rwlock_writer_timeout() -> Outcome {
    RW_WR_OUT.store(0, Ordering::SeqCst);
    RW_RD_GOT.store(0, Ordering::SeqCst);
    *RW_TO.write() = 0;
    let r = RW_TO.read();
    let out = writer_timeout_under(&r);
    drop(r);
    out
}

fn writer_timeout_under(_read: &RwLockReadGuard<'_, u64>) -> Outcome {
    let w = match thread_init::spawn("rw-to-w", rw_timeout_writer) {
        Ok(h) => h.id(),
        Err(_) => return Outcome::Fail("spawn"),
    };
    if !blocks(w) {
        return crate::fail_fmt!(
            "writer did not queue under a read hold (out {})",
            RW_WR_OUT.load(Ordering::SeqCst)
        );
    }
    let rd = match thread_init::spawn("rw-to-r", rw_pref_reader) {
        Ok(h) => h.id(),
        Err(_) => return Outcome::Fail("spawn"),
    };
    if !blocks(rd) {
        return crate::fail_fmt!(
            "reader did not queue behind the writer (got {})",
            RW_RD_GOT.load(Ordering::SeqCst)
        );
    }
    let now = Instant {
        ns: time_init::now_ns(),
    };
    if !thread_init::testing::set_wait_deadline(w, now) {
        return Outcome::Fail("writer left its wait before its deadline");
    }
    if !crate::ktest::wait_for(|| thread_init::exited(w)) {
        return Outcome::Fail("writer did not time out");
    }
    if RW_WR_OUT.load(Ordering::SeqCst) != 1 {
        return Outcome::Fail("writer got the lock under a read hold");
    }
    if matches!(
        thread_init::try_state(rd),
        Some(ThreadState::Blocked { .. })
    ) {
        return Outcome::Fail("reader stranded after writer timeout");
    }
    if !crate::ktest::wait_for(|| thread_init::exited(rd)) {
        return Outcome::Fail("woken reader did not finish");
    }
    if RW_RD_GOT.load(Ordering::SeqCst) != 1 {
        return Outcome::Fail("reader exited without the read lock");
    }
    Outcome::Ok
}

static SEM: Semaphore = Semaphore::new(0);

static SEM_N: AtomicU32 = AtomicU32::new(0);

fn sem_waiter() {
    SEM.acquire();
    SEM_N.fetch_add(1, Ordering::SeqCst);
}

pub(crate) fn test_semaphore_wake() -> Outcome {
    SEM_N.store(0, Ordering::SeqCst);
    let Ok(_a) = thread_init::spawn("sem-a", sem_waiter) else {
        return Outcome::Fail("spawn");
    };
    let Ok(_b) = thread_init::spawn("sem-b", sem_waiter) else {
        return Outcome::Fail("spawn");
    };
    thread_init::yield_now();
    thread_init::sleep_ms(5);
    if SEM_N.load(Ordering::SeqCst) != 0 {
        return Outcome::Fail("sema acquired empty");
    }
    SEM.release();
    SEM.release();
    let t0 = time_init::uptime_ms();
    loop {
        if SEM_N.load(Ordering::SeqCst) == 2 {
            return Outcome::Ok;
        }
        if time_init::uptime_ms().saturating_sub(t0) > 2_000 {
            return Outcome::Fail("sema stall");
        }
        thread_init::yield_now();
    }
}

static CM: BlockingMutex<bool> = BlockingMutex::new(false);

static CV: Condvar = Condvar::new();

static CV_DONE: AtomicBool = AtomicBool::new(false);

fn cv_waiter() {
    let mut g = CM.lock();
    while !*g {
        g = CV.wait(g);
    }
    CV_DONE.store(true, Ordering::SeqCst);
}

pub(crate) fn test_condvar_signal() -> Outcome {
    CV_DONE.store(false, Ordering::SeqCst);
    *CM.lock() = false;
    let Ok(_h) = thread_init::spawn("cv", cv_waiter) else {
        return Outcome::Fail("spawn");
    };
    thread_init::sleep_ms(10);
    {
        let mut g = CM.lock();
        *g = true;
        CV.notify_one();
    }
    let t0 = time_init::uptime_ms();
    loop {
        if CV_DONE.load(Ordering::SeqCst) {
            return Outcome::Ok;
        }
        if time_init::uptime_ms().saturating_sub(t0) > 2_000 {
            return Outcome::Fail("condvar stall");
        }
        thread_init::yield_now();
    }
}

static CVREL_M: BlockingMutex<u32> = BlockingMutex::new(0);

static CVREL_CV: Condvar = Condvar::new();

static CVREL_WAITING: AtomicBool = AtomicBool::new(false);

static CVREL_DONE: AtomicBool = AtomicBool::new(false);

fn cvrel_waiter() {
    let g = CVREL_M.lock();
    CVREL_WAITING.store(true, Ordering::SeqCst);
    let _g = CVREL_CV.wait(g);
    CVREL_DONE.store(true, Ordering::SeqCst);
}

pub(crate) fn test_condvar_wait_releases() -> Outcome {
    CVREL_WAITING.store(false, Ordering::SeqCst);
    CVREL_DONE.store(false, Ordering::SeqCst);
    *CVREL_M.lock() = 0;
    let Ok(_h) = thread_init::spawn("cvrel", cvrel_waiter) else {
        return Outcome::Fail("spawn");
    };
    let t0 = time_init::uptime_ms();
    loop {
        if CVREL_WAITING.load(Ordering::SeqCst) {
            break;
        }
        if time_init::uptime_ms().saturating_sub(t0) > 2_000 {
            return Outcome::Fail("waiter never locked");
        }
        thread_init::yield_now();
    }
    {
        let mut g = CVREL_M.lock();
        *g = 1;
        CVREL_CV.notify_one();
    }
    let t1 = time_init::uptime_ms();
    loop {
        if CVREL_DONE.load(Ordering::SeqCst) {
            return Outcome::Ok;
        }
        if time_init::uptime_ms().saturating_sub(t1) > 2_000 {
            return Outcome::Fail("condvar wait held mutex");
        }
        thread_init::yield_now();
    }
}

static CH: Channel<u64, 4> = Channel::new();

static CH_SUM: AtomicU64 = AtomicU64::new(0);

fn ch_consumer() {
    let mut i = 0u64;
    let mut sum = 0u64;
    while i < 32 {
        sum = sum.wrapping_add(CH.recv());
        i += 1;
    }
    CH_SUM.store(sum, Ordering::SeqCst);
}

pub(crate) fn test_channel_mpsc() -> Outcome {
    CH_SUM.store(0, Ordering::SeqCst);
    let Ok(_c) = thread_init::spawn("ch-rx", ch_consumer) else {
        return Outcome::Fail("spawn");
    };
    let mut i = 1u64;
    while i <= 32 {
        CH.send(i);
        i += 1;
    }
    let t0 = time_init::uptime_ms();
    loop {
        let s = CH_SUM.load(Ordering::SeqCst);
        if s != 0 {
            if s != 32 * 33 / 2 {
                crate::marker!("vibeOS: ktest:   chan sum={s}");
                return Outcome::Fail("channel sum");
            }
            return Outcome::Ok;
        }
        if time_init::uptime_ms().saturating_sub(t0) > 2_000 {
            return Outcome::Fail("channel stall");
        }
        thread_init::yield_now();
    }
}

static TM: BlockingMutex<u64> = BlockingMutex::new(0);

static TM_OUT: AtomicU32 = AtomicU32::new(0);

fn timeout_waiter() {
    let ns = time_init::now_ns().saturating_add(15_000_000);
    match TM.lock_until(Some(Instant { ns })) {
        None => TM_OUT.store(1, Ordering::SeqCst),
        Some(_g) => TM_OUT.store(2, Ordering::SeqCst),
    }
}

pub(crate) fn test_mutex_deadline() -> Outcome {
    TM_OUT.store(0, Ordering::SeqCst);
    let g = TM.lock();
    let Ok(_h) = thread_init::spawn("tm", timeout_waiter) else {
        return Outcome::Fail("spawn");
    };
    thread_init::sleep_ms(40);
    if TM_OUT.load(Ordering::SeqCst) != 1 {
        drop(g);
        return Outcome::Fail("deadline did not fire");
    }
    drop(g);
    Outcome::Ok
}

pub(crate) fn test_sync_try_paths() -> Outcome {
    let m = BlockingMutex::new(1u64);
    {
        let _g = m.lock();
        if m.try_lock().is_some() {
            return Outcome::Fail("try_lock while held");
        }
    }
    if m.try_lock().is_none() {
        return Outcome::Fail("try_lock free");
    }
    let ch = Channel::<u64, 2>::new();
    if ch.try_send(3).is_err() {
        return Outcome::Fail("try_send");
    }
    if ch.try_recv() != Some(3) {
        return Outcome::Fail("try_recv");
    }
    if ch.try_recv().is_some() {
        return Outcome::Fail("try_recv empty");
    }
    CV.notify_all();
    Outcome::Ok
}

/// `ipi_init::init` installed `SpinMutex::lock`'s spin poll
/// (`sync_init::set_spin_poll`), so a spinning CPU still services IPIs.
pub(crate) fn test_spin_poll_hook_installed() -> Outcome {
    if !crate::sync_init::spin_poll_installed() {
        return Outcome::Fail("spin poll hook unset");
    }
    Outcome::Ok
}

// ----- op_gate_kill_sleeps (ROADMAP §10.4, DESIGN §2.11 rule 3) -----

/// The gate the test kills. Once killed it stays dead, so the test runs
/// once per boot.
static OGK_GATE: OpGate = OpGate::new();
/// Thread A is inside the gate.
static OGK_IN: AtomicBool = AtomicBool::new(false);
/// Thread A's enter failed.
static OGK_REFUSED: AtomicBool = AtomicBool::new(false);
/// Tells A to leave.
static OGK_RELEASE: AtomicBool = AtomicBool::new(false);
/// A is about to leave: set just before its exit.
static OGK_EXITED: AtomicBool = AtomicBool::new(false);
/// Killer K's `kill` returned, and whether it saw `OGK_EXITED` then.
static OGK_RETURNED: AtomicBool = AtomicBool::new(false);
static OGK_SAW_EXITED: AtomicBool = AtomicBool::new(false);

fn ogk_holder() {
    let Ok(g) = OGK_GATE.enter() else {
        OGK_REFUSED.store(true, Ordering::SeqCst);
        return;
    };
    OGK_IN.store(true, Ordering::SeqCst);
    // Bounded, so a broken test still lets the killer return.
    let _released = sleep_until(|| OGK_RELEASE.load(Ordering::SeqCst), 5_000);
    OGK_EXITED.store(true, Ordering::SeqCst);
    g.exit();
}

fn ogk_killer() {
    OGK_GATE.kill();
    OGK_SAW_EXITED.store(OGK_EXITED.load(Ordering::SeqCst), Ordering::SeqCst);
    OGK_RETURNED.store(true, Ordering::SeqCst);
}

pub(crate) fn test_op_gate_kill_sleeps() -> Outcome {
    if OGK_GATE.is_dead() {
        return Outcome::Fail("op_gate_kill_sleeps: gate already killed (ran twice)");
    }
    let _a = spawn_thread("ogk_a", ogk_holder);
    if !sleep_until(
        || OGK_IN.load(Ordering::SeqCst) || OGK_REFUSED.load(Ordering::SeqCst),
        2_000,
    ) || !OGK_IN.load(Ordering::SeqCst)
    {
        return Outcome::Fail("holder did not enter the gate");
    }
    let _k = spawn_thread("ogk_k", ogk_killer);
    let dead = sleep_until(|| OGK_GATE.is_dead(), 2_000);
    let refused = OGK_GATE.enter().is_err();
    // Let the killer reach its sleep.
    thread_init::sleep_ms(30);
    let early = OGK_RETURNED.load(Ordering::SeqCst);
    OGK_RELEASE.store(true, Ordering::SeqCst);
    let returned = sleep_until(|| OGK_RETURNED.load(Ordering::SeqCst), 2_000);
    if !dead {
        return Outcome::Fail("kill did not mark the gate dead");
    }
    if !refused {
        return Outcome::Fail("enter succeeded on a killed gate");
    }
    if early {
        return Outcome::Fail("kill returned with an operation inside");
    }
    if !returned {
        return Outcome::Fail("kill did not return within 2 s of the last exit");
    }
    if !OGK_SAW_EXITED.load(Ordering::SeqCst) {
        return Outcome::Fail("kill returned before the holder left");
    }
    if OGK_GATE.inside() != 0 {
        return Outcome::Fail("an operation still inside after kill");
    }
    Outcome::Ok
}

/// This subsystem's in-guest tests, in run order; `crate::ktest::GROUPS`
/// runs them (DESIGN §8.2).
/// `condvar_notifier_queued`'s mutex, condvar and state.
static CNQ_M: BlockingMutex<u32> = BlockingMutex::new(0);
static CNQ_CV: Condvar = Condvar::new();
/// The notifier's tid, `u32::MAX` until it starts.
static CNQ_NOTIFIER: AtomicU32 = AtomicU32::new(u32::MAX);
/// Set by the notifier once it holds the mutex, before it notifies.
static CNQ_NOTIFIED: AtomicBool = AtomicBool::new(false);
/// Set by each thread as it finishes.
static CNQ_WAITER_DONE: AtomicBool = AtomicBool::new(false);
static CNQ_NOTIFIER_DONE: AtomicBool = AtomicBool::new(false);
/// Set by the waiter once it holds the mutex.
static CNQ_LOCKED: AtomicBool = AtomicBool::new(false);
/// Set by the waiter when the notifier never blocked on the mutex.
static CNQ_NO_QUEUE: AtomicBool = AtomicBool::new(false);

/// Holds the mutex until the notifier is queued on it, then waits on the
/// condvar with the preempt hook armed for itself: `wait`'s one SCHED
/// section blocks it and records the notifier's wake.
fn cnq_waiter() {
    let mut g = CNQ_M.lock();
    CNQ_LOCKED.store(true, Ordering::Release);
    let queued = sleep_until(
        || {
            let n = CNQ_NOTIFIER.load(Ordering::Acquire);
            n != u32::MAX
                && matches!(
                    thread_init::try_state(vibeos::thread::ThreadId(n)),
                    Some(vibeos::thread::ThreadState::Blocked { .. })
                )
        },
        2_000,
    );
    if !queued {
        CNQ_NO_QUEUE.store(true, Ordering::Release);
        drop(g);
        CNQ_WAITER_DONE.store(true, Ordering::Release);
        return;
    }
    thread_init::ktest_preempt_before_places(thread_init::current_id());
    while !CNQ_NOTIFIED.load(Ordering::Acquire) {
        g = CNQ_CV.wait(g);
    }
    *g += 1;
    drop(g);
    CNQ_WAITER_DONE.store(true, Ordering::Release);
}

/// Queues on the mutex the waiter holds; once it has it, notifies.
fn cnq_notifier() {
    CNQ_NOTIFIER.store(thread_init::current_id().raw(), Ordering::Release);
    let mut g = CNQ_M.lock();
    *g += 1;
    CNQ_NOTIFIED.store(true, Ordering::Release);
    CNQ_CV.notify_one();
    drop(g);
    CNQ_NOTIFIER_DONE.store(true, Ordering::Release);
}

/// F034: the notifier is queued on the mutex when the waiter calls
/// `Condvar::wait`, whose SCHED section blocks the waiter and records the
/// notifier's wake; a reschedule IPI then lands before `with_sched` places
/// that wake. Both threads must finish.
pub(crate) fn test_condvar_notifier_queued() -> Outcome {
    CNQ_NOTIFIER.store(u32::MAX, Ordering::Relaxed);
    CNQ_NOTIFIED.store(false, Ordering::Relaxed);
    CNQ_WAITER_DONE.store(false, Ordering::Relaxed);
    CNQ_NOTIFIER_DONE.store(false, Ordering::Relaxed);
    CNQ_NO_QUEUE.store(false, Ordering::Relaxed);
    CNQ_LOCKED.store(false, Ordering::Relaxed);
    *CNQ_M.lock() = 0;
    let cpu = Some(thread_init::current_cpu());
    let opts = thread_init::SpawnOpts {
        stack_pages: vibeos::kva::DEFAULT_STACK_PAGES,
        cpu,
    };
    if thread_init::spawn_opts("cnq-wait", cnq_waiter, opts).is_err() {
        return Outcome::Fail("spawn waiter");
    }
    // The waiter holds the mutex before the notifier asks for it.
    if !sleep_until(|| CNQ_LOCKED.load(Ordering::Acquire), 2_000) {
        return Outcome::Fail("waiter never locked");
    }
    if thread_init::spawn_opts("cnq-notify", cnq_notifier, opts).is_err() {
        return Outcome::Fail("spawn notifier");
    }
    let done = sleep_until(
        || CNQ_WAITER_DONE.load(Ordering::Acquire) && CNQ_NOTIFIER_DONE.load(Ordering::Acquire),
        2_000,
    );
    if CNQ_NO_QUEUE.load(Ordering::Acquire) {
        return Outcome::Fail("notifier never queued on the mutex");
    }
    if !done {
        return crate::fail_fmt!(
            "notifier stranded: waiter done {}, notifier done {}",
            CNQ_WAITER_DONE.load(Ordering::Acquire),
            CNQ_NOTIFIER_DONE.load(Ordering::Acquire)
        );
    }
    if *CNQ_M.lock() != 2 {
        return Outcome::Fail("count");
    }
    Outcome::Ok
}

pub(crate) const TESTS: &[Test] = &[
    test("irqcell_reentry_panics", test_irqcell_reentry_panics),
    test("bootcell_set_once", test_bootcell_set_once).once(),
    test("spin_mutex", test_spin_mutex),
    test("blocking_mutex_counter", test_blocking_mutex_counter),
    test("late_wake_after_exit", test_late_wake_after_exit),
    test("rwlock_exclusion", test_rwlock_exclusion),
    test("rwlock_writer_timeout", test_rwlock_writer_timeout),
    test("semaphore_wake", test_semaphore_wake),
    test("condvar_signal", test_condvar_signal),
    test("condvar_wait_releases", test_condvar_wait_releases),
    test("channel_mpsc", test_channel_mpsc),
    test("mutex_deadline", test_mutex_deadline),
    test("sync_try_paths", test_sync_try_paths),
    test("spin_poll_hook_installed", test_spin_poll_hook_installed),
    test(
        "rank_alloc_under_pt_asserts",
        test_rank_alloc_under_pt_asserts,
    ),
    test(
        "rank_same_rank_lock_asserts",
        test_rank_same_rank_lock_asserts,
    ),
    test(
        "rank_lock_nested_keeps_outer",
        test_rank_lock_nested_keeps_outer,
    ),
    test("cross_cpu_cells_ranked", test_cross_cpu_cells_ranked),
    test("op_gate_kill_sleeps", test_op_gate_kill_sleeps).once(),
    test("condvar_notifier_queued", test_condvar_notifier_queued),
];
