//! In-guest tests of P10-S19, Preemptible syscall bodies, lossless stop/continue, bounded console writes (DESIGN §8.2).

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::proc::{SIGCONT, SIGKILL, SIGSTOP, wait_signaled};
use vibeos::syscall::SYS_KILL;

use super::user::{self, DEFAULT, Image, user_code};
use super::{Outcome, Test, test};
use crate::proc_init::{self, testing as proc_testing};
use crate::thread_init;
use crate::time_init;

pub(super) const TESTS: &[Test] =
    &[test("stop_cont_no_lost_wakeup", test_stop_cont_no_lost_wakeup).deadline(30_000)];

/// Sleep until `pred` holds, for at most `ms`.
fn sleep_until(pred: impl Fn() -> bool, ms: u64) -> bool {
    let deadline = time_init::now_ns().saturating_add(ms.saturating_mul(1_000_000));
    while !pred() {
        if time_init::now_ns() >= deadline {
            return false;
        }
        thread_init::sleep_ms(1);
    }
    true
}

/// Spin on TSC time until `pred` holds, for at most `ns`.
fn spin_until(pred: impl Fn() -> bool, ns: u64) -> bool {
    let t0 = time_init::now_ns();
    while !pred() {
        if time_init::now_ns().saturating_sub(t0) > ns {
            return false;
        }
        core::hint::spin_loop();
    }
    true
}

fn kill(pid: u32, sig: u32) -> i64 {
    proc_init::dispatch(SYS_KILL, [u64::from(pid), u64::from(sig), 0, 0, 0, 0])
}

// getpid forever.
user_code!(
    S19_GETPID_LOOP,
    "
1:
    mov eax, 39
    syscall
    jmp 1b
    "
);

/// The pid a spawner on CPU 0 started; `u64::MAX` when the spawn failed.
static STOP_PID: AtomicU64 = AtomicU64::new(0);
static STOP_SPAWNED: AtomicBool = AtomicBool::new(false);
static STOP_DONE: AtomicBool = AtomicBool::new(false);
/// What the sender found: see [`StopErr`].
static STOP_ERR: AtomicU32 = AtomicU32::new(0);
static STOP_PAIR: AtomicU32 = AtomicU32::new(0);
static STOP_RC: AtomicU64 = AtomicU64::new(0);

const STOP_PAIRS: u32 = 10_000;
/// Pairs whose SIGCONT lands while the process sits in the stop stall.
const STOP_STALLED_PAIRS: u32 = 16;

#[repr(u32)]
enum StopErr {
    None = 0,
    KillFailed = 1,
    NoStall = 2,
    NoRun = 3,
    NeverRan = 4,
}

/// Pinned to CPU 0, so the process is too (`thread_init::spawn_user`).
fn stop_spawner() {
    let pid = match user::spawn(&Image::Code(S19_GETPID_LOOP, DEFAULT), &["getpid_loop"]) {
        Ok(pid) => u64::from(pid),
        Err(_) => u64::MAX,
    };
    STOP_PID.store(pid, Ordering::Relaxed);
    STOP_SPAWNED.store(true, Ordering::Release);
}

fn stop_fail(err: StopErr, pair: u32) {
    STOP_PAIR.store(pair, Ordering::Relaxed);
    STOP_ERR.store(err as u32, Ordering::Release);
}

/// Pinned to the second CPU: `SIGSTOP` then `SIGCONT`, [`STOP_PAIRS`]
/// times, each pair followed by a wait for the process to call `getpid`
/// again. The first [`STOP_STALLED_PAIRS`] send `SIGCONT` while the
/// process sits between its stop decision and its sleep.
fn stop_sender() {
    stop_pairs();
    STOP_DONE.store(true, Ordering::Release);
}

fn stop_pairs() {
    let pid = STOP_PID.load(Ordering::Relaxed) as u32;
    if !spin_until(|| proc_testing::getpid_count(pid) > 0, 1_000_000_000) {
        stop_fail(StopErr::NeverRan, 0);
        return;
    }
    for i in 0..STOP_PAIRS {
        let stall = i < STOP_STALLED_PAIRS;
        if stall {
            proc_testing::arm_stop_stall(pid);
        }
        let mut c0 = proc_testing::getpid_count(pid);
        let rc = kill(pid, SIGSTOP);
        if rc != 0 {
            STOP_RC.store(rc as u64, Ordering::Relaxed);
            stop_fail(StopErr::KillFailed, i);
            return;
        }
        if stall {
            if !spin_until(proc_testing::stop_stalled, 1_000_000_000) {
                stop_fail(StopErr::NoStall, i);
                return;
            }
            // Inside the stall, before its `getpid` counts.
            c0 = proc_testing::getpid_count(pid);
        }
        let rc = kill(pid, SIGCONT);
        if stall {
            proc_testing::release_stop_stall();
        }
        if rc != 0 {
            STOP_RC.store(rc as u64, Ordering::Relaxed);
            stop_fail(StopErr::KillFailed, i);
            return;
        }
        if !spin_until(|| proc_testing::getpid_count(pid) > c0, 1_000_000_000) {
            stop_fail(StopErr::NoRun, i);
            return;
        }
    }
}

/// A `SIGCONT` sent while the target sits between its stop decision and
/// its sleep on `stop_wq` wakes it: the process runs `getpid` again after
/// every `SIGSTOP`/`SIGCONT` pair (ROADMAP §10.6, F033).
fn test_stop_cont_no_lost_wakeup() -> Outcome {
    let Some(sender_cpu) = super::second_cpu() else {
        return Outcome::Skip("needs 2 CPUs");
    };
    STOP_SPAWNED.store(false, Ordering::Release);
    STOP_DONE.store(false, Ordering::Release);
    STOP_ERR.store(StopErr::None as u32, Ordering::Release);
    super::spawn_thread_on("s19_stop_spawner", stop_spawner, 0);
    if !sleep_until(|| STOP_SPAWNED.load(Ordering::Acquire), 5_000) {
        return Outcome::Fail("spawner did not run");
    }
    let Ok(pid) = u32::try_from(STOP_PID.load(Ordering::Relaxed)) else {
        return Outcome::Fail("spawn");
    };
    super::spawn_thread_on("s19_stop_sender", stop_sender, sender_cpu);
    let done = sleep_until(|| STOP_DONE.load(Ordering::Acquire), 25_000);
    proc_testing::disarm_stop_stall();
    let killed = kill(pid, SIGKILL);
    let st = user::wait(pid);
    if !done {
        return Outcome::Fail("sender did not finish");
    }
    let pair = STOP_PAIR.load(Ordering::Relaxed);
    let err = STOP_ERR.load(Ordering::Acquire);
    if err == StopErr::NoRun as u32 {
        return crate::fail_fmt!("pair {pair}: pid {pid} did not run again");
    }
    if err == StopErr::NoStall as u32 {
        return crate::fail_fmt!("pair {pair}: pid {pid} never reached the stop stall");
    }
    if err == StopErr::KillFailed as u32 {
        let rc = STOP_RC.load(Ordering::Relaxed) as i64;
        return crate::fail_fmt!("pair {pair}: kill returned {rc}");
    }
    if err == StopErr::NeverRan as u32 {
        return crate::fail_fmt!("pid {pid} never called getpid");
    }
    if killed != 0 {
        return crate::fail_fmt!("SIGKILL returned {killed}");
    }
    if st != wait_signaled(SIGKILL) {
        return crate::fail_fmt!("status {st:#x}, want {:#x}", wait_signaled(SIGKILL));
    }
    Outcome::Ok
}
