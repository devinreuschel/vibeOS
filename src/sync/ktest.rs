//! In-guest tests for sync (kernel_tests only). Rows: the list in crate::ktest.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::time::Instant;

use crate::arch;
use crate::ktest::Outcome;
use crate::per_cpu_init;
use crate::sync_init::{BlockingMutex, Channel, Condvar, RwLock, Semaphore, SpinMutex};
use crate::thread_init;
use crate::time_init;
use crate::x86;

pub(crate) fn test_irqcell_reentry_panics() -> Outcome {
    static C: crate::cell::IrqCell<u32> = crate::cell::IrqCell::new(0);
    // The longjmp skips both `IrqCell` guards; this one restores IF.
    let _g = x86::InterruptGuard::enter();
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
    per_cpu_init::current()
        .irq_nest
        .store(nest0, Ordering::Relaxed);
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
    per_cpu_init::current()
        .irq_nest
        .store(nest0, Ordering::Relaxed);
    if !hit {
        return Outcome::Fail("unset get");
    }
    Outcome::Ok
}

pub(crate) fn test_spin_mutex() -> Outcome {
    let m = SpinMutex::new(0u64);
    {
        let mut g = m.lock();
        if x86::interrupts_enabled() {
            return Outcome::Fail("lock left IF on");
        }
        *g = 42;
    }
    if *m.lock() != 42 {
        return Outcome::Fail("value lost");
    }
    Outcome::Ok
}

pub(crate) fn test_lock_spins() -> Outcome {
    let c = crate::sync_init::spin_counts();
    crate::marker!(
        "vibeOS: ktest:   spins pt={} buddy={} heap={} sched={} device={} serial={}",
        c[1],
        c[2],
        c[3],
        c[4],
        c[5],
        c[6]
    );
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

fn rw_timeout_writer() {
    let ns = time_init::now_ns().saturating_add(15_000_000);
    match RW_TO.write_until(Some(Instant { ns })) {
        None => RW_WR_OUT.store(1, Ordering::SeqCst),
        Some(_g) => RW_WR_OUT.store(2, Ordering::SeqCst),
    }
}

fn rw_pref_reader() {
    let _g = RW_TO.read();
    RW_RD_GOT.store(1, Ordering::SeqCst);
}

pub(crate) fn test_rwlock_writer_timeout() -> Outcome {
    RW_WR_OUT.store(0, Ordering::SeqCst);
    RW_RD_GOT.store(0, Ordering::SeqCst);
    *RW_TO.write() = 0;
    let r = RW_TO.read();
    let Ok(_w) = thread_init::spawn("rw-to-w", rw_timeout_writer) else {
        return Outcome::Fail("spawn");
    };
    thread_init::yield_now();
    thread_init::sleep_ms(5);
    let Ok(_rd) = thread_init::spawn("rw-to-r", rw_pref_reader) else {
        return Outcome::Fail("spawn");
    };
    thread_init::yield_now();
    thread_init::sleep_ms(40);
    if RW_WR_OUT.load(Ordering::SeqCst) != 1 {
        drop(r);
        return Outcome::Fail("writer did not timeout");
    }
    let t0 = time_init::uptime_ms();
    loop {
        if RW_RD_GOT.load(Ordering::SeqCst) == 1 {
            drop(r);
            return Outcome::Ok;
        }
        if time_init::uptime_ms().saturating_sub(t0) > 2_000 {
            drop(r);
            return Outcome::Fail("reader stranded after writer timeout");
        }
        thread_init::yield_now();
    }
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
