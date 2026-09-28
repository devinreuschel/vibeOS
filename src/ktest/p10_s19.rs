//! In-guest tests of P10-S19, Preemptible syscall bodies, lossless stop/continue, bounded console writes (DESIGN §8.2).

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use vibeos::proc::{SIGCONT, SIGKILL, SIGSEGV, SIGSTOP, wait_signaled};
use vibeos::syscall::SYS_KILL;

use super::user::{self, DEFAULT, Image, user_code};
use super::{Outcome, Test, free_frames_owned, test};
use crate::arch::idt::testing as idt_testing;
use crate::kva_init;
use crate::log_init;
use crate::proc_init::{self, testing as proc_testing};
use crate::thread_init;
use crate::time_init;

pub(super) const TESTS: &[Test] = &[
    test("stop_cont_no_lost_wakeup", test_stop_cont_no_lost_wakeup).deadline(30_000),
    test("syscall_body_if_on", test_syscall_body_if_on).deadline(30_000),
];

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
        match super::alloc_frames_owned(0).map(kva_init::vmap) {
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
    let prefix = super::FailMsg::from_args(format_args!("user: pid {pid} killed SIGSEGV "));
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
fn test_syscall_body_if_on() -> Outcome {
    let Some(other) = super::second_cpu() else {
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
    super::spawn_thread_on("s19_body_busy", body_busy, 0);
    super::spawn_thread_on("s19_body_unmap", body_unmapper, other);
    super::spawn_thread_on("s19_body_spawn", body_spawn_getpid, 0);
    let spawned = sleep_until(|| BODY_SPAWNED.load(Ordering::Acquire), 5_000);
    let st = if spawned {
        spawned_pid(0).map(user::wait)
    } else {
        None
    };
    BODY_STOP.store(true, Ordering::Release);
    proc_testing::disarm_getpid_spin();
    let settled = sleep_until(
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
    super::spawn_thread_on("s19_body_faults", body_spawn_faults, 0);
    let spawned = sleep_until(|| BODY_SPAWNED.load(Ordering::Acquire), 5_000);
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
