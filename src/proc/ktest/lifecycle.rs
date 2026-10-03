//! In-guest tests for proc (kernel_tests only): stop, kill and out-of-memory paths.
//! Rows: the parent `ktest.rs`'s `TESTS`.

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::fs::{FsError, O_RDONLY, OpenFlags};
use vibeos::kalloc::{TryBox, TryVec};
use vibeos::kerror::KError;
use vibeos::lock::RANK_DEVICE;
use vibeos::proc::{
    SIGCONT, SIGKILL, SIGQUIT, SIGSEGV, SIGSTOP, SIGTERM, wait_exited, wait_signaled,
};
use vibeos::syscall::SYS_KILL;
use vibeos::thread::{ThreadId, ThreadState};

use crate::arch::idt::testing as idt_testing;
use crate::console_init::testing as console_testing;
use crate::file_init;
use crate::heap_init::fail_after::{self, Scope, Seen};
use crate::ktest::user::{self, DEFAULT, Image, user_code};
use crate::ktest::{
    Outcome, free_frames, free_frames_owned, sleep_until_s19, spin_until, spin_until_ns,
};
use crate::kva_init;
use crate::log_init;
use crate::proc_init::{self, testing as proc_testing};
use crate::sync_init::SpinMutex;
use crate::thread_init;
use crate::time_init;

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

/// Pairs the sender finished.
static STOP_SENT: AtomicU32 = AtomicU32::new(0);

/// Pairs the sender sends, all of which must run for the test to pass
/// (ROADMAP §10.6's stop-wait box).
const STOP_PAIRS: u32 = 10_000;

/// How long before the run's deadline the sender starts no more pairs, so
/// a host too slow for [`STOP_PAIRS`] fails naming how many it sent rather
/// than with no verdict: room for the pair in flight and for the test to
/// end.
const STOP_MARGIN_MS: u32 = 2_000;

/// How long before the run's deadline a pair in flight gives up waiting on
/// the process: inside [`STOP_MARGIN_MS`], and before the registry thread's
/// own wait ends, so the failure names its pair.
const PAIR_MARGIN_MS: u32 = 1_000;

/// The process as the sender found it when a pair failed: its thread's
/// state and CPU, if it has a thread, and whether the process is stopped.
type Diag = (Option<(ThreadState, u32)>, bool);

static STOP_DIAG: SpinMutex<Option<Diag>> = SpinMutex::with_rank(None, RANK_DEVICE);

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
    let pid = STOP_PID.load(Ordering::Relaxed) as u32;
    let thread =
        proc_testing::tid_of(pid).and_then(|t| thread_init::testing::try_state_cpu(ThreadId(t)));
    *STOP_DIAG.lock() = Some((thread, proc_testing::is_stopped(pid)));
    STOP_PAIR.store(pair, Ordering::Relaxed);
    STOP_ERR.store(err as u32, Ordering::Release);
}

/// [`STOP_DIAG`] for a failure line.
struct TargetState(Option<Diag>);

impl fmt::Display for TargetState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some((thread, stopped)) = self.0 else {
            return f.write_str("no state");
        };
        match thread {
            Some((st, cpu)) => write!(f, "{} on cpu{cpu}", st.name())?,
            None => f.write_str("no thread")?,
        }
        if stopped {
            f.write_str(", stopped")?;
        }
        Ok(())
    }
}

/// Spin until `pred` holds or the run's deadline is [`PAIR_MARGIN_MS`]
/// away. Each wait is on the process's CPU, which on a loaded host can be a
/// vCPU the host has descheduled, so no fixed bound fits (ROADMAP §10.2).
fn spin_pair(pred: impl Fn() -> bool) -> bool {
    loop {
        if pred() {
            return true;
        }
        if crate::ktest::deadline_within(PAIR_MARGIN_MS) != Some(false) {
            return pred();
        }
        core::hint::spin_loop();
    }
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
    if !spin_pair(|| proc_testing::getpid_count(pid) > 0) {
        stop_fail(StopErr::NeverRan, 0);
        return;
    }
    for i in 0..STOP_PAIRS {
        let stall = i < STOP_STALLED_PAIRS;
        if !stall && crate::ktest::deadline_within(STOP_MARGIN_MS) == Some(true) {
            return;
        }
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
            if !spin_pair(proc_testing::stop_stalled) {
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
        if !spin_pair(|| proc_testing::getpid_count(pid) > c0) {
            stop_fail(StopErr::NoRun, i);
            return;
        }
        // Relaxed: published by the Release store of `STOP_DONE`.
        STOP_SENT.store(i + 1, Ordering::Relaxed);
    }
}

/// A `SIGCONT` sent while the target sits between its stop decision and
/// its sleep on `stop_wq` wakes it: the process runs `getpid` again after
/// every `SIGSTOP`/`SIGCONT` pair (ROADMAP §10.6, F033).
pub(crate) fn test_stop_cont_no_lost_wakeup() -> Outcome {
    let Some(sender_cpu) = crate::ktest::second_cpu() else {
        return Outcome::Skip("needs 2 CPUs");
    };
    STOP_SPAWNED.store(false, Ordering::Release);
    STOP_DONE.store(false, Ordering::Release);
    STOP_ERR.store(StopErr::None as u32, Ordering::Release);
    STOP_SENT.store(0, Ordering::Relaxed);
    *STOP_DIAG.lock() = None;
    crate::ktest::spawn_thread_on("s19_stop_spawner", stop_spawner, 0);
    if !crate::ktest::sleep_for(|| STOP_SPAWNED.load(Ordering::Acquire)) {
        return Outcome::Fail("spawner did not run");
    }
    let Ok(pid) = u32::try_from(STOP_PID.load(Ordering::Relaxed)) else {
        return Outcome::Fail("spawn");
    };
    crate::ktest::spawn_thread_on("s19_stop_sender", stop_sender, sender_cpu);
    // The registry thread sleeps between checks, since the process runs on
    // CPU 0 beside it. The sender stops short of the run's deadline.
    let done = crate::ktest::sleep_for(|| STOP_DONE.load(Ordering::Acquire));
    proc_testing::disarm_stop_stall();
    let killed = kill(pid, SIGKILL);
    let st = user::wait(pid);
    if !done {
        return Outcome::Fail("sender did not finish");
    }
    let err = STOP_ERR.load(Ordering::Acquire);
    let pair = STOP_PAIR.load(Ordering::Relaxed);
    let diag = TargetState(*STOP_DIAG.lock());
    if err == StopErr::NoRun as u32 {
        return crate::fail_fmt!("pair {pair}: pid {pid} did not run again: {diag}");
    }
    if err == StopErr::NoStall as u32 {
        return crate::fail_fmt!("pair {pair}: pid {pid} never reached the stop stall: {diag}");
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
    let sent = STOP_SENT.load(Ordering::Relaxed);
    if sent < STOP_PAIRS {
        return crate::fail_fmt!("{sent} of {STOP_PAIRS} pairs before the run's deadline");
    }
    Outcome::Ok
}

// getpid, then exit(0).
user_code!(
    S19_GETPID_EXIT,
    "
    mov eax, 39
    syscall
    xor edi, edi
    mov eax, 60
    syscall
    ud2
    "
);

// A store to 0x5000_0000, which nothing maps.
user_code!(
    S19_FAULT_A,
    "
    mov eax, 0x50000000
    mov byte ptr [rax], 1
    ud2
    "
);

// 64 sched_yield calls, then a store to 0x5100_0000, which nothing maps.
user_code!(
    S19_FAULT_B,
    "
    mov r12d, 64
1:
    mov eax, 24
    syscall
    dec r12d
    jnz 1b
    mov eax, 0x51000000
    mov byte ptr [rax], 1
    ud2
    "
);

const CR2_A: u64 = 0x5000_0000;

const CR2_B: u64 = 0x5100_0000;

static BODY_STOP: AtomicBool = AtomicBool::new(false);

static BODY_BUSY_DONE: AtomicBool = AtomicBool::new(false);

static BODY_UNMAP_DONE: AtomicBool = AtomicBool::new(false);

/// `now_ns` when the unmap returned; 0 before, `u64::MAX` when it failed.
static BODY_UNMAP_NS: AtomicU64 = AtomicU64::new(0);

static BODY_PIDS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

static BODY_SPAWNED: AtomicBool = AtomicBool::new(false);

/// CPU-bound on CPU 0 until [`BODY_STOP`]: ready whenever the spinning
/// `getpid` could be preempted.
fn body_busy() {
    while !BODY_STOP.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    BODY_BUSY_DONE.store(true, Ordering::Release);
}

/// On the second CPU: once the `getpid` spin has started, map and unmap
/// one frame, whose unmap waits for every CPU's shootdown ack.
fn body_unmapper() {
    let started = spin_until(
        || proc_testing::spin_start_ns() != 0 || BODY_STOP.load(Ordering::Acquire),
        5_000_000_000,
    );
    let ns = if started && proc_testing::spin_start_ns() != 0 {
        match crate::ktest::alloc_frames_owned(0).map(kva_init::vmap) {
            Some(Ok(v)) => {
                free_frames_owned(kva_init::vunmap(v));
                time_init::now_ns()
            }
            _ => u64::MAX,
        }
    } else {
        u64::MAX
    };
    BODY_UNMAP_NS.store(ns, Ordering::Release);
    BODY_UNMAP_DONE.store(true, Ordering::Release);
}

/// Pinned to CPU 0, so the processes are too (`thread_init::spawn_user`).
fn body_spawn_getpid() {
    let pid = match user::spawn(&Image::Code(S19_GETPID_EXIT, DEFAULT), &["getpid_exit"]) {
        Ok(pid) => u64::from(pid),
        Err(_) => u64::MAX,
    };
    BODY_PIDS[0].store(pid, Ordering::Relaxed);
    BODY_SPAWNED.store(true, Ordering::Release);
}

/// A first, then B, both on CPU 0.
fn body_spawn_faults() {
    for (i, (img, name)) in [(S19_FAULT_A, "fault_a"), (S19_FAULT_B, "fault_b")]
        .into_iter()
        .enumerate()
    {
        let pid = match user::spawn(&Image::Code(img, DEFAULT), &[name]) {
            Ok(pid) => u64::from(pid),
            Err(_) => u64::MAX,
        };
        BODY_PIDS[i].store(pid, Ordering::Relaxed);
    }
    BODY_SPAWNED.store(true, Ordering::Release);
}

fn spawned_pid(i: usize) -> Option<u32> {
    u32::try_from(BODY_PIDS[i].load(Ordering::Relaxed)).ok()
}

/// Parse `digits` as lowercase hex.
fn parse_hex(digits: &[u8]) -> Option<u64> {
    if digits.is_empty() || digits.len() > 16 {
        return None;
    }
    let mut v = 0u64;
    for &d in digits {
        let n = match d {
            b'0'..=b'9' => d - b'0',
            b'a'..=b'f' => d - b'a' + 10,
            _ => return None,
        };
        v = (v << 4) | u64::from(n);
    }
    Some(v)
}

/// The `cr2=` of the last `user: pid <pid> killed SIGSEGV` record in the
/// log ring; `None` without one.
fn kill_line_cr2(pid: u32) -> Option<u64> {
    let prefix = crate::ktest::FailMsg::from_args(format_args!("user: pid {pid} killed SIGSEGV "));
    let want = prefix.as_str().as_bytes();
    let mut found = None;
    log_init::for_each_msg(|msg| {
        if !msg.starts_with(want) {
            return;
        }
        let key = b" cr2=0x";
        found = msg
            .windows(key.len())
            .rposition(|w| w == key)
            .and_then(|at| parse_hex(&msg[at + key.len()..]));
    });
    found
}

/// Syscall bodies, and fault bodies taken at CPL 3, run with IF=1
/// (ROADMAP §10.6, F011): a 50 ms `getpid` on CPU 0 acks another CPU's
/// shootdown and is preempted by a ready thread, and a `#PF` body that
/// yields to another process's fault still reports its own CR2.
pub(crate) fn test_syscall_body_if_on() -> Outcome {
    let Some(other) = crate::ktest::second_cpu() else {
        return Outcome::Skip("needs 2 CPUs");
    };
    if let Err(o) = body_unmap_and_switch(other) {
        return o;
    }
    body_cr2()
}

fn body_unmap_and_switch(other: u32) -> Result<(), Outcome> {
    BODY_STOP.store(false, Ordering::Release);
    BODY_BUSY_DONE.store(false, Ordering::Release);
    BODY_UNMAP_DONE.store(false, Ordering::Release);
    BODY_UNMAP_NS.store(0, Ordering::Release);
    BODY_SPAWNED.store(false, Ordering::Release);
    proc_testing::arm_getpid_spin();
    crate::ktest::spawn_thread_on("s19_body_busy", body_busy, 0);
    crate::ktest::spawn_thread_on("s19_body_unmap", body_unmapper, other);
    crate::ktest::spawn_thread_on("s19_body_spawn", body_spawn_getpid, 0);
    let spawned = sleep_until_s19(|| BODY_SPAWNED.load(Ordering::Acquire), 5_000);
    let st = if spawned {
        spawned_pid(0).map(user::wait)
    } else {
        None
    };
    BODY_STOP.store(true, Ordering::Release);
    proc_testing::disarm_getpid_spin();
    let settled = sleep_until_s19(
        || BODY_BUSY_DONE.load(Ordering::Acquire) && BODY_UNMAP_DONE.load(Ordering::Acquire),
        10_000,
    );
    if !spawned {
        return Err(Outcome::Fail("spawner did not run"));
    }
    let Some(st) = st else {
        return Err(Outcome::Fail("spawn"));
    };
    if !settled {
        return Err(Outcome::Fail("busy or unmap thread did not finish"));
    }
    if st != 0 {
        return Err(crate::fail_fmt!("getpid program status {st:#x}, want 0"));
    }
    let start = proc_testing::spin_start_ns();
    let done = proc_testing::spin_done_ns();
    if start == 0 || done == 0 {
        return Err(Outcome::Fail("the getpid spin never ran"));
    }
    let unmapped = BODY_UNMAP_NS.load(Ordering::Acquire);
    if unmapped == u64::MAX {
        return Err(Outcome::Fail("vmap failed"));
    }
    if unmapped >= done {
        return Err(crate::fail_fmt!(
            "unmap returned {} us after the spin start, getpid {} us: not first",
            unmapped.saturating_sub(start) / 1000,
            done.saturating_sub(start) / 1000
        ));
    }
    let (sw0, sw1) = proc_testing::spin_switches();
    if sw1 <= sw0 {
        return Err(crate::fail_fmt!(
            "CPU 0 switches {sw0} -> {sw1} during the getpid spin"
        ));
    }
    Ok(())
}

fn body_cr2() -> Outcome {
    BODY_SPAWNED.store(false, Ordering::Release);
    idt_testing::arm_pf_yield(CR2_A);
    crate::ktest::spawn_thread_on("s19_body_faults", body_spawn_faults, 0);
    let spawned = sleep_until_s19(|| BODY_SPAWNED.load(Ordering::Acquire), 5_000);
    let (a, b) = (spawned_pid(0), spawned_pid(1));
    let sts = if spawned {
        [a.map(user::wait), b.map(user::wait)]
    } else {
        [None, None]
    };
    idt_testing::disarm_pf_yield();
    if !spawned {
        return Outcome::Fail("fault spawner did not run");
    }
    let (Some(a), Some(b)) = (a, b) else {
        return Outcome::Fail("spawn A or B");
    };
    let segv = wait_signaled(SIGSEGV);
    for (who, st) in [("A", sts[0]), ("B", sts[1])] {
        if st != Some(segv) {
            return crate::fail_fmt!("{who} status {st:?}, want {segv:#x}");
        }
    }
    let during = idt_testing::pf_during_yield();
    if during != CR2_B {
        return crate::fail_fmt!("#PF body during A's yield had cr2 {during:#x}, want B's");
    }
    for (who, pid, want) in [("A", a, CR2_A), ("B", b, CR2_B)] {
        match kill_line_cr2(pid) {
            Some(c) if c == want => {}
            got => {
                return crate::fail_fmt!(
                    "{who} (pid {pid}) kill line cr2 {got:x?}, want {want:#x}"
                );
            }
        }
    }
    Outcome::Ok
}

/// A ring-3 kill line reaches the log ring whole, although the fault body
/// that writes it runs with IF=1: A's kill line is held open mid-write
/// until B, on the same CPU, has written its own, and each line still
/// names its own process's CR2.
pub(crate) fn test_kill_line_whole() -> Outcome {
    BODY_SPAWNED.store(false, Ordering::Release);
    proc_testing::arm_kill_line_yield(CR2_A);
    crate::ktest::spawn_thread_on("s19_kill_faults", body_spawn_faults, 0);
    let spawned = sleep_until_s19(|| BODY_SPAWNED.load(Ordering::Acquire), 5_000);
    let (a, b) = (spawned_pid(0), spawned_pid(1));
    let sts = if spawned {
        [a.map(user::wait), b.map(user::wait)]
    } else {
        [None, None]
    };
    proc_testing::disarm_kill_line_yield();
    if !spawned {
        return Outcome::Fail("fault spawner did not run");
    }
    let (Some(a), Some(b)) = (a, b) else {
        return Outcome::Fail("spawn A or B");
    };
    let segv = wait_signaled(SIGSEGV);
    for (who, st) in [("A", sts[0]), ("B", sts[1])] {
        if st != Some(segv) {
            return crate::fail_fmt!("{who} status {st:?}, want {segv:#x}");
        }
    }
    for (who, pid, want) in [("A", a, CR2_A), ("B", b, CR2_B)] {
        match kill_line_cr2(pid) {
            Some(c) if c == want => {}
            got => {
                return crate::fail_fmt!(
                    "{who} (pid {pid}) kill line cr2 {got:x?}, want {want:#x}"
                );
            }
        }
    }
    Outcome::Ok
}

/// Counts the lines written to it.
struct LineCount(usize);

impl fmt::Write for LineCount {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0 += s.bytes().filter(|&b| b == b'\n').count();
        Ok(())
    }
}

/// Wait up to 5 s until no process, zombies included, holds a slot. Call it
/// only while the hook is disarmed: `write_ps` allocates.
fn no_process() -> bool {
    let deadline = time_init::now_ns().saturating_add(5_000_000_000);
    loop {
        let mut w = LineCount(0);
        proc_init::write_ps(&mut w);
        if w.0 == 0 {
            return true;
        }
        if time_init::now_ns() >= deadline {
            return false;
        }
        thread_init::sleep_ms(10);
    }
}

/// Arms the hook; its `Drop` disarms, so a failing case leaves it off.
struct Armed(bool);

impl Armed {
    fn new(budget: usize, scope: Scope) -> Self {
        fail_after::arm(budget, scope);
        Self(true)
    }

    fn disarm(mut self) -> Seen {
        self.0 = false;
        fail_after::disarm()
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        if self.0 {
            // A failed case disarms only to leave the hook off.
            fail_after::disarm();
        }
    }
}

pub(crate) fn test_kalloc_fail_after_hook() -> Outcome {
    if !no_process() {
        return Outcome::Fail("a process is still running");
    }
    let me = thread_init::current_id();

    let armed = Armed::new(1, Scope::Thread(me));
    let first = TryBox::try_new(1u64);
    let second = TryBox::try_new(2u64);
    let seen = armed.disarm();
    if first.is_err() || second.is_ok() {
        return crate::fail_fmt!(
            "TryBox under budget 1: first ok {}, second ok {}",
            first.is_ok(),
            second.is_ok()
        );
    }
    let want = Seen {
        counted: 2,
        refused: 1,
    };
    if seen != want {
        return crate::fail_fmt!("TryBox counts {seen:?}, want {want:?}");
    }
    drop(first);

    let armed = Armed::new(1, Scope::Thread(me));
    let mut v = match TryVec::<u64>::try_with_capacity(8) {
        Ok(v) => v,
        Err(_) => return Outcome::Fail("TryVec::try_with_capacity(8) refused under budget 1"),
    };
    let grown = v.try_reserve(4096);
    let seen = armed.disarm();
    if grown.is_ok() {
        return Outcome::Fail("TryVec::try_reserve(4096) past the budget succeeded");
    }
    if v.capacity() < 8 || !v.is_empty() {
        return Outcome::Fail("a refused try_reserve changed the vector");
    }
    if seen != want {
        return crate::fail_fmt!("TryVec counts {seen:?}, want {want:?}");
    }
    drop(v);

    let armed = Armed::new(0, Scope::Processes { from_syscall: 1 });
    let b = TryBox::try_new(3u64);
    let seen = armed.disarm();
    if b.is_err() {
        return Outcome::Fail("a kernel thread's TryBox was refused under a process scope");
    }
    let none = Seen {
        counted: 0,
        refused: 0,
    };
    if seen != none {
        return crate::fail_fmt!("process scope counted a kernel thread: {seen:?}");
    }
    Outcome::Ok
}

// fork once. Child: exit(0). Parent: wait4(pid, 0, 0) must return pid
// (exit 4 otherwise), exit 0. On -ENOMEM: wait4(-1, 0, WNOHANG) must
// return -ECHILD (exit 3 otherwise: a child exists), exit 12. Any other
// fork error exits 1.
user_code!(
    FORK_ONCE,
    "
    mov eax, 57
    syscall
    test rax, rax
    jz 7f
    js 3f
    mov r12, rax
    mov rdi, r12
    xor esi, esi
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    mov edi, 4
    cmp rax, r12
    jne 9f
    xor edi, edi
    jmp 9f
3:
    mov edi, 1
    cmp rax, -12
    jne 9f
    mov rdi, -1
    xor esi, esi
    mov edx, 1
    xor r10d, r10d
    mov eax, 61
    syscall
    mov edi, 3
    cmp rax, -10
    jne 9f
    mov edi, 12
    jmp 9f
7:
    xor edi, edi
9:
    mov eax, 60
    syscall
    ud2
    "
);

// Push a canary, then execve("/hello", ["/hello", "s20", NULL], NULL).
// /hello exits 42. A return must be -ENOMEM (exit 1 otherwise) with the
// canary unchanged (exit 2 otherwise): exit 12.
user_code!(
    EXEC_HELLO,
    "
    mov rax, 0x5332305f43414e41
    push rax
    lea rdi, [rip + 6f]
    lea rax, [rip + 7f]
    push 0
    push rax
    push rdi
    mov rsi, rsp
    xor edx, edx
    mov eax, 59
    syscall
    add rsp, 24
    mov edi, 1
    cmp rax, -12
    jne 9f
    mov edi, 2
    mov rcx, 0x5332305f43414e41
    cmp [rsp], rcx
    jne 9f
    mov edi, 12
9:
    mov eax, 60
    syscall
    ud2
6:
    .asciz \"/hello\"
7:
    .asciz \"s20\"
    "
);

// open("/hello", O_RDONLY). On an fd: close it, exit 0. On -ENOMEM:
// close(3) must return -EBADF (exit 5 otherwise: an fd leaked), exit 12.
// Any other error exits 1.
user_code!(
    OPEN_RO,
    "
    lea rdi, [rip + 6f]
    xor esi, esi
    xor edx, edx
    mov eax, 2
    syscall
    test rax, rax
    js 3f
    mov rdi, rax
    mov eax, 3
    syscall
    xor edi, edi
    jmp 9f
3:
    mov edi, 1
    cmp rax, -12
    jne 9f
    mov edi, 3
    mov eax, 3
    syscall
    mov edi, 5
    cmp rax, -9
    jne 9f
    mov edi, 12
9:
    mov eax, 60
    syscall
    ud2
6:
    .asciz \"/hello\"
    "
);

// open("/vibe/s20", O_CREAT | O_RDWR, 0644), then as OPEN_RO.
user_code!(
    OPEN_CREAT,
    "
    lea rdi, [rip + 6f]
    mov esi, 0x42
    mov edx, 0x1a4
    mov eax, 2
    syscall
    test rax, rax
    js 3f
    mov rdi, rax
    mov eax, 3
    syscall
    xor edi, edi
    jmp 9f
3:
    mov edi, 1
    cmp rax, -12
    jne 9f
    mov edi, 3
    mov eax, 3
    syscall
    mov edi, 5
    cmp rax, -9
    jne 9f
    mov edi, 12
9:
    mov eax, 60
    syscall
    ud2
6:
    .asciz \"/vibe/s20\"
    "
);

// fork (#1). Child: getpid (#1), exit(42) (#2). Parent: wait4(child, &st,
// 0) (#2) must return the child (exit 7 otherwise) with st 0x2A00 (exit 6
// otherwise): exit 0. A failed fork exits 8.
user_code!(
    EXIT_WAIT,
    "
    mov eax, 57
    syscall
    test rax, rax
    js 8f
    jz 7f
    mov r12, rax
    sub rsp, 16
    mov dword ptr [rsp], -1
    mov rdi, r12
    mov rsi, rsp
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    mov edi, 7
    cmp rax, r12
    jne 9f
    mov edi, 6
    cmp dword ptr [rsp], 0x2A00
    jne 9f
    xor edi, edi
    jmp 9f
7:
    mov eax, 39
    syscall
    mov edi, 42
    jmp 9f
8:
    mov edi, 8
9:
    mov eax, 60
    syscall
    ud2
    "
);

// mmap 16 MiB anonymous read-write (#1; exit 8 on failure), store to each
// of its 4,096 pages, spin 2^26 iterations, munmap it (#2; exit 7 unless
// 0), then sched_yield until killed.
user_code!(
    MUNMAP_16M,
    "
    xor edi, edi
    mov esi, 0x1000000
    mov edx, 3
    mov r10d, 0x22
    mov r8, -1
    xor r9d, r9d
    mov eax, 9
    syscall
    mov edi, 8
    cmp rax, -4096
    ja 9f
    mov r12, rax
    mov rdx, rax
    mov ecx, 4096
2:
    mov byte ptr [rdx], 1
    add rdx, 4096
    dec rcx
    jnz 2b
    mov ecx, 0x4000000
3:
    dec rcx
    jnz 3b
    mov rdi, r12
    mov esi, 0x1000000
    mov eax, 11
    syscall
    mov edi, 7
    test rax, rax
    jnz 9f
4:
    mov eax, 24
    syscall
    jmp 4b
9:
    mov eax, 60
    syscall
    ud2
    "
);

const CREAT_PATH: &[u8] = b"/vibe/s20";

const NONE: Seen = Seen {
    counted: 0,
    refused: 0,
};

/// Kill `pid` and reap it: a failing case leaves no process behind.
fn kill_and_wait(pid: u32) {
    let r = proc_init::dispatch(SYS_KILL, [u64::from(pid), u64::from(SIGKILL), 0, 0, 0, 0]);
    if r == 0 {
        user::wait(pid);
    }
}

/// The exit code of a normal exit, or `None` for a signal.
fn exit_code(st: u32) -> Option<u32> {
    if st & 0x7f == 0 {
        Some((st >> 8) & 0xff)
    } else {
        None
    }
}

/// Arm at n = 0, 1, … until `prog` runs with nothing refused. Every run
/// that the hook refused must exit ENOMEM; the unrefused run exits `ok`
/// and comes at n >= `min`, the allocations the call is known to make.
fn nomem_loop(name: &str, prog: &'static [u8], ok: u32, min: usize) -> Outcome {
    let img = Image::Code(prog, user::DEFAULT);
    for n in 0..=64usize {
        if !no_process() {
            return crate::fail_fmt!("{name} n={n}: a process is still running");
        }
        let armed = Armed::new(n, Scope::Processes { from_syscall: 1 });
        let pid = match user::spawn(&img, &["s20"]) {
            Ok(p) => p,
            Err(e) => return crate::fail_fmt!("{name} n={n}: spawn: {}", e.as_str()),
        };
        let st = user::wait(pid);
        let seen = armed.disarm();
        let Some(code) = exit_code(st) else {
            return crate::fail_fmt!("{name} n={n}: status {st:#x}, not an exit");
        };
        if seen.refused > 0 {
            if code != KError::NoMem.errno() as u32 {
                return crate::fail_fmt!("{name} n={n}: exit {code} with {seen:?}, want 12");
            }
            continue;
        }
        if code != ok {
            return crate::fail_fmt!("{name} n={n}: exit {code} with nothing refused, want {ok}");
        }
        if n < min {
            return crate::fail_fmt!("{name}: {n} allocations, want at least {min}");
        }
        crate::klog!(
            vibeos::log::Level::Info,
            "ktest: kalloc_nomem: {name} makes {n} allocations"
        );
        return Outcome::Ok;
    }
    crate::fail_fmt!("{name}: still refused at n=64")
}

/// Armed at zero from each process's second syscall: the parent gets
/// the child's status and the hook counts nothing.
fn exit_wait() -> Outcome {
    if !no_process() {
        return Outcome::Fail("exit_wait: a process is still running");
    }
    let armed = Armed::new(0, Scope::Processes { from_syscall: 2 });
    let pid = match user::spawn(&Image::Code(EXIT_WAIT, user::DEFAULT), &["s20"]) {
        Ok(p) => p,
        Err(e) => return crate::fail_fmt!("exit_wait: spawn: {}", e.as_str()),
    };
    let st = user::wait(pid);
    let seen = armed.disarm();
    if st != wait_exited(0) {
        return crate::fail_fmt!("exit_wait: status {st:#x}, want exit 0");
    }
    if seen != NONE {
        return crate::fail_fmt!("exit_wait: hook saw {seen:?} after the fork");
    }
    Outcome::Ok
}

/// Armed at zero from the `munmap`: it returns 0 and the free-frame
/// count rises by at least 4,096 while the program is still alive.
fn munmap_16m() -> Outcome {
    if !no_process() {
        return Outcome::Fail("munmap_16m: a process is still running");
    }
    let f0 = free_frames();
    let armed = Armed::new(0, Scope::Processes { from_syscall: 2 });
    let pid = match user::spawn(&Image::Code(MUNMAP_16M, user::DEFAULT), &["s20"]) {
        Ok(p) => p,
        Err(e) => return crate::fail_fmt!("munmap_16m: spawn: {}", e.as_str()),
    };
    if !spin_until_ns(|| free_frames() + 4096 <= f0, 30_000_000_000) {
        kill_and_wait(pid);
        return crate::fail_fmt!("munmap_16m: no dip below {f0} - 4096");
    }
    let t0 = time_init::now_ns();
    let mut fmin = free_frames();
    loop {
        let f = free_frames();
        fmin = fmin.min(f);
        if f >= fmin + 4096 {
            break;
        }
        if time_init::now_ns().saturating_sub(t0) > 30_000_000_000 {
            kill_and_wait(pid);
            return crate::fail_fmt!("munmap_16m: no rise from {fmin}");
        }
        thread_init::yield_now();
    }
    let r = proc_init::dispatch(SYS_KILL, [u64::from(pid), u64::from(SIGKILL), 0, 0, 0, 0]);
    if r != 0 {
        let st = user::wait(pid);
        return crate::fail_fmt!("munmap_16m: kill {r}: program gone, status {st:#x}");
    }
    let st = user::wait(pid);
    let seen = armed.disarm();
    if st != wait_signaled(SIGKILL) {
        return crate::fail_fmt!("munmap_16m: status {st:#x}, want SIGKILL");
    }
    if seen.refused != 0 {
        return crate::fail_fmt!("munmap_16m: hook refused {seen:?}");
    }
    Outcome::Ok
}

pub(crate) fn test_kalloc_nomem() -> Outcome {
    // fork takes at least the address-space slot and execve that slot and
    // its file buffer; the in-memory opens below may take none.
    let loops: [(&str, &'static [u8], u32, usize); 4] = [
        ("fork_once", FORK_ONCE, 0, 1),
        ("exec_hello", EXEC_HELLO, 42, 2),
        ("open_ro", OPEN_RO, 0, 0),
        ("open_creat", OPEN_CREAT, 0, 0),
    ];
    for (name, prog, ok, min) in loops {
        let out = nomem_loop(name, prog, ok, min);
        if name == "open_creat" {
            match file_init::unlink(CREAT_PATH) {
                Ok(()) | Err(FsError::NotFound) => {}
                Err(e) => return crate::fail_fmt!("unlink /vibe/s20: {}", e.as_str()),
            }
        }
        if !matches!(out, Outcome::Ok) {
            return out;
        }
    }
    let out = exit_wait();
    if !matches!(out, Outcome::Ok) {
        return out;
    }
    munmap_16m()
}

/// `/sbin/init`'s line for a `/bin/tests` that did not pass (`user/src/bin/init.rs`).
const INIT_EXITED: &[u8] = b"init: /bin/tests exited ";

/// The copy's line, with `exited` changed so that the ktest boot does not
/// print the registered failure line.
const INIT_EXITED_COPY: &[u8] = b"init: /bin/tests EXITED ";

/// What the copy prints: `/hello` exits 42, the wait status `42 << 8`.
const INIT_REPORT: &[u8] = b"init: /bin/tests EXITED 10752\n";

/// A file's bytes.
fn read_file(path: &[u8]) -> Result<TryVec<u8>, &'static str> {
    let f = file_init::open(path, OpenFlags::from_bits(O_RDONLY), 0).map_err(|_| "open")?;
    let mut out = TryVec::new();
    let mut buf = [0u8; 512];
    let r = loop {
        match file_init::read(&f, &mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                if out
                    .try_extend_from_slice(buf.get(..n).unwrap_or(&[]))
                    .is_err()
                {
                    break Err("no memory");
                }
            }
            Err(_) => break Err("read"),
        }
    };
    let closed = file_init::close(f);
    r?;
    closed.map_err(|_| "close")?;
    Ok(out)
}

/// Replace each `from` in `image` with `to`, of the same length; how many.
fn patch_all(image: &mut [u8], from: &[u8], to: &[u8]) -> usize {
    let mut n = 0usize;
    let mut i = 0usize;
    while let Some(w) = image.get_mut(i..i.saturating_add(from.len())) {
        if w == from {
            w.copy_from_slice(to);
            n += 1;
            i += from.len();
        } else {
            i += 1;
        }
    }
    n
}

/// `/sbin/init` reports a `/bin/tests` that did not pass: a copy whose
/// `/bin/tests` and `/bin/sh` are `/hello`, which exits 42, prints its
/// line with that wait status, whole, on the console (ROADMAP §10.2, F073).
pub(crate) fn test_init_reports_failed_tests() -> Outcome {
    let mut image = match read_file(b"/sbin/init") {
        Ok(v) => v,
        Err(why) => return crate::fail_fmt!("/sbin/init: {why}"),
    };
    let lines = image
        .windows(INIT_EXITED.len())
        .filter(|w| *w == INIT_EXITED)
        .count();
    if lines != 1 {
        return crate::fail_fmt!("{lines} exited lines in /sbin/init, want 1");
    }
    for (from, to, what) in [
        (&b"/bin/tests\0"[..], &b"/hello\0\0\0\0\0"[..], "/bin/tests"),
        (&b"/bin/sh\0"[..], &b"/hello\0\0"[..], "/bin/sh"),
        (INIT_EXITED, INIT_EXITED_COPY, "exited line"),
    ] {
        if patch_all(&mut image, from, to) == 0 {
            return crate::fail_fmt!("no {what} in /sbin/init");
        }
    }
    console_testing::start_capture();
    let pid = match proc_init::spawn_image(&image, &[&b"/sbin/init"[..]], 0) {
        Ok(pid) => pid,
        Err(e) => {
            console_testing::stop_capture();
            return crate::fail_fmt!("spawn: {}", e.as_str());
        }
    };
    let seen = pid != 1
        && sleep_until_s19(
            || {
                let mut got = [0u8; console_testing::CAPTURE_CAP];
                let n = console_testing::captured(&mut got);
                got.get(..n)
                    .unwrap_or(&[])
                    .windows(INIT_REPORT.len())
                    .any(|w| w == INIT_REPORT)
            },
            5_000,
        );
    let mut got = [0u8; console_testing::CAPTURE_CAP];
    let n = console_testing::captured(&mut got);
    console_testing::stop_capture();
    // After both children exit the copy spins in `wait4` on ECHILD, so the
    // kill lands at its next syscall exit.
    let killed = kill(pid, SIGKILL);
    let st = proc_init::wait_kernel(pid);
    if pid == 1 {
        return Outcome::Fail("the copy of /sbin/init got pid 1");
    }
    if !seen {
        let text = core::str::from_utf8(got.get(..n).unwrap_or(&[])).unwrap_or("<not utf-8>");
        return crate::fail_fmt!("no EXITED 10752 line; captured {text:?}");
    }
    if killed != 0 {
        return crate::fail_fmt!("SIGKILL returned {killed}");
    }
    if st != wait_signaled(SIGKILL) {
        return crate::fail_fmt!("status {st:#x}, want {:#x}", wait_signaled(SIGKILL));
    }
    Outcome::Ok
}

/// Stop `pid` and wait until it has decided to stop, `n` decisions in.
fn stopped(pid: u32, n: u32) -> bool {
    kill(pid, SIGSTOP) == 0 && crate::ktest::sleep_for(|| proc_testing::stops() >= n)
}

/// Wait until `pid` has made `n` stop decisions or exited.
fn stops_or_exit(pid: u32, n: u32) -> bool {
    crate::ktest::sleep_for(|| proc_testing::stops() >= n || proc_testing::is_zombie(pid))
}

/// Kill `pid` unless it already exited, and reap it: its wait status.
fn end_and_reap(pid: u32) -> u32 {
    if !proc_testing::is_zombie(pid) {
        kill(pid, SIGKILL);
    }
    user::wait(pid)
}

/// The cases of [`stop_holds_signals_until_cont`].
#[derive(Clone, Copy)]
enum StopCase {
    /// This fatal signal reaches a stopped process.
    Held(u32),
    /// `SIGKILL` reaches a stopped process.
    Kill,
    /// `SIGQUIT`, then `SIGSTOP`, both pending when the process acts.
    QuitThenStop,
}

/// One case of [`stop_holds_signals_until_cont`] on a fresh `getpid`
/// loop; `None` when it held.
fn stop_case(case: StopCase) -> Option<Outcome> {
    let sig = match case {
        StopCase::Held(sig) => sig,
        StopCase::Kill => SIGKILL,
        StopCase::QuitThenStop => SIGQUIT,
    };
    let pid = match user::spawn(&Image::Code(S19_GETPID_LOOP, DEFAULT), &["stop_hold"]) {
        Ok(pid) => pid,
        Err(e) => return Some(crate::fail_fmt!("spawn: {}", e.as_str())),
    };
    proc_testing::watch_stops(pid);
    let r = if matches!(case, StopCase::QuitThenStop) {
        // SIGCONT, SIGQUIT and SIGSTOP all land while the process sits
        // between a stop decision and its sleep, so it acts on SIGQUIT and
        // a stop that has not taken effect together: lowest number first.
        proc_testing::arm_stop_stall(pid);
        let r = if kill(pid, SIGSTOP) != 0 || !crate::ktest::sleep_for(proc_testing::stop_stalled) {
            Some(crate::fail_fmt!("pid {pid} did not reach the stop stall"))
        } else if kill(pid, SIGCONT) != 0 || kill(pid, SIGQUIT) != 0 || kill(pid, SIGSTOP) != 0 {
            Some(Outcome::Fail("kill failed"))
        } else {
            proc_testing::release_stop_stall();
            if !stops_or_exit(pid, 2) {
                Some(crate::fail_fmt!("pid {pid} neither stopped nor exited"))
            } else if !proc_testing::is_zombie(pid) {
                Some(Outcome::Fail(
                    "a pending SIGQUIT lost to a SIGSTOP sent after it",
                ))
            } else {
                None
            }
        };
        proc_testing::disarm_stop_stall();
        r
    } else if !stopped(pid, 1) {
        Some(crate::fail_fmt!("pid {pid} did not stop"))
    } else if kill(pid, sig) != 0 {
        Some(crate::fail_fmt!("kill {sig} failed"))
    } else if matches!(case, StopCase::Kill) {
        if crate::ktest::sleep_for(|| proc_testing::is_zombie(pid)) {
            None
        } else {
            Some(Outcome::Fail("SIGKILL did not end a stopped process"))
        }
    } else if !stops_or_exit(pid, 2) || proc_testing::is_zombie(pid) {
        // Woken by the signal, it re-checks and stays stopped.
        Some(crate::fail_fmt!("signal {sig} ended a stopped process"))
    } else if kill(pid, SIGSTOP) != 0 || !stops_or_exit(pid, 3) || proc_testing::is_zombie(pid) {
        Some(crate::fail_fmt!(
            "a second SIGSTOP let signal {sig} end a stopped process"
        ))
    } else if kill(pid, SIGCONT) != 0 || !crate::ktest::sleep_for(|| proc_testing::is_zombie(pid)) {
        Some(crate::fail_fmt!(
            "signal {sig} did not end it after SIGCONT"
        ))
    } else {
        None
    };
    proc_testing::watch_stops(0);
    let st = end_and_reap(pid);
    let want = wait_signaled(sig);
    if r.is_some() {
        return r;
    }
    (st != want).then(|| crate::fail_fmt!("signal {sig}: status {st:#x}, want {want:#x}"))
}

/// A process that has stopped stays stopped, as on Linux: only `SIGKILL`
/// ends it at once, and any other fatal signal, with a core (`SIGQUIT`) or
/// without (`SIGTERM`), stays pending through a second `SIGSTOP` and ends
/// it once `SIGCONT` lets it run. A stop sent but not yet acted on is
/// dequeued in number order: `SIGQUIT` pending beside it ends the process.
pub(crate) fn stop_holds_signals_until_cont() -> Outcome {
    for case in [
        StopCase::Held(SIGTERM),
        StopCase::Held(SIGQUIT),
        StopCase::Kill,
        StopCase::QuitThenStop,
    ] {
        if let Some(o) = stop_case(case) {
            return o;
        }
    }
    Outcome::Ok
}
