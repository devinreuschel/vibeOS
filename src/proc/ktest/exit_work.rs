//! In-guest tests for the exit work on every return to ring 3 (DESIGN §5.10
//! rule 11, ROADMAP §10.6, F033). Rows: the parent `ktest.rs`'s `TESTS`.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use vibeos::proc::{SIGCONT, SIGKILL, SIGSTOP, wait_signaled};
use vibeos::syscall::{SYS_GETPID, SYS_KILL};
use vibeos::vectors;

use super::hooks as exit_testing;
use crate::ktest::user::{self, DEFAULT, Image, user_code};
use crate::ktest::{Outcome, sleep_until};
use crate::proc_init;
use crate::proc_init::testing as proc_testing;
use crate::thread_init;

// A loop in ring 3 with no syscall.
user_code!(
    RING3_SPIN,
    "
2:
    jmp 2b
    "
);

// A loop in ring 3 with no syscall that counts in R12.
user_code!(
    RING3_COUNT,
    "
    xor r12d, r12d
2:
    inc r12
    jmp 2b
    "
);

fn kill(pid: u32, sig: u32) -> i64 {
    proc_init::dispatch(SYS_KILL, [u64::from(pid), u64::from(sig), 0, 0, 0, 0])
}

static SPAWNED: AtomicBool = AtomicBool::new(false);
static PID_A: AtomicU64 = AtomicU64::new(u64::MAX);
static PID_B: AtomicU64 = AtomicU64::new(u64::MAX);

/// Starts both children on the spawner's CPU (`spawn_user` pins each to
/// its creator's).
fn spawner() {
    let a = user::spawn(&Image::Code(RING3_SPIN, DEFAULT), &["spin"]);
    let b = user::spawn(&Image::Code(RING3_COUNT, DEFAULT), &["count"]);
    PID_A.store(a.map_or(u64::MAX, u64::from), Ordering::Relaxed);
    PID_B.store(b.map_or(u64::MAX, u64::from), Ordering::Relaxed);
    SPAWNED.store(true, Ordering::Release);
}

/// The watched child's counter has moved past `from` within `ms`.
fn counter_moves(from: u64, ms: u64) -> bool {
    sleep_until(|| exit_testing::watched_r12() > from, ms)
}

/// A kill ends, and a stop stops, a process that makes no syscall: one
/// child looping in ring 3 is killed; another, counting in R12, is stopped
/// (state `Stopped`, its counter still for 50 ms), continued (the counter
/// moves) and killed; both are reaped with `SIGKILL`. The children run on
/// a second CPU when there is one, so the kill and stop reach them through
/// `sys_kill`'s reschedule IPI.
pub(crate) fn test_signal_on_return() -> Outcome {
    SPAWNED.store(false, Ordering::Release);
    match crate::ktest::second_cpu() {
        Some(cpu) => {
            crate::ktest::spawn_thread_on("s80_spawner", spawner, cpu);
        }
        None => spawner(),
    }
    if !sleep_until(|| SPAWNED.load(Ordering::Acquire), 5_000) {
        return Outcome::Fail("spawner did not run");
    }
    let (Ok(a), Ok(b)) = (
        u32::try_from(PID_A.load(Ordering::Relaxed)),
        u32::try_from(PID_B.load(Ordering::Relaxed)),
    ) else {
        return Outcome::Fail("spawn");
    };
    exit_testing::watch_r12(b);
    let r = check_children(a, b);
    // Whatever failed, neither child outlives the test. `a` is a zombie
    // already when the checks passed, so its kill returns 0 and leaves its
    // status as its first `SIGKILL` set it, as Linux's `kill` does.
    let ka = kill(a, SIGKILL);
    let kb = kill(b, SIGKILL);
    let (sa, sb) = (user::wait(a), user::wait(b));
    exit_testing::watch_r12(0);
    if let Err(e) = r {
        return e;
    }
    if ka != 0 || kb != 0 {
        return crate::fail_fmt!("SIGKILL returned {ka}, {kb}, want 0, 0");
    }
    let want = wait_signaled(SIGKILL);
    if sa != want || sb != want {
        return crate::fail_fmt!("status {sa:#x}, {sb:#x}, want {want:#x}");
    }
    Outcome::Ok
}

fn check_children(a: u32, b: u32) -> Result<(), Outcome> {
    if !counter_moves(0, 5_000) {
        return Err(Outcome::Fail("counting child never ran"));
    }
    let rc = kill(a, SIGKILL);
    if rc != 0 {
        return Err(crate::fail_fmt!("kill(a, SIGKILL) returned {rc}"));
    }
    if !sleep_until(|| proc_testing::is_zombie(a), 2_000) {
        return Err(Outcome::Fail("SIGKILL did not end the spinning child"));
    }
    let rc = kill(b, SIGSTOP);
    if rc != 0 {
        return Err(crate::fail_fmt!("kill(b, SIGSTOP) returned {rc}"));
    }
    if !proc_testing::is_stopped(b) {
        return Err(Outcome::Fail("SIGSTOP: state not Stopped"));
    }
    // Long enough for its CPU's next exit to ring 3 to act on the stop.
    thread_init::sleep_ms(20);
    let c0 = exit_testing::watched_r12();
    thread_init::sleep_ms(50);
    let c1 = exit_testing::watched_r12();
    if c1 != c0 {
        return Err(crate::fail_fmt!("stopped child ran: counter {c0} -> {c1}"));
    }
    let rc = kill(b, SIGCONT);
    if rc != 0 {
        return Err(crate::fail_fmt!("kill(b, SIGCONT) returned {rc}"));
    }
    if !counter_moves(c1, 2_000) {
        return Err(Outcome::Fail("continued child did not run"));
    }
    Ok(())
}

// getpid, then a loop in ring 3 with no syscall.
user_code!(
    GETPID_SPIN,
    "
    mov eax, 39
    syscall
2:
    jmp 2b
    "
);

/// A kill posted after a syscall exit's last check, with this CPU's
/// reschedule IPI sent, is acted on by that IPI's exit from ring 3: the
/// syscall exit's check ran with IF=0, so the IPI waited for ring 3. A
/// check made with IF=1 would take the IPI before its `cli` and leave the
/// kill to the next tick's exit.
pub(crate) fn test_exit_work_ipi() -> Outcome {
    exit_testing::arm_exit_kill(SYS_GETPID);
    let st = user::run(&Image::Code(GETPID_SPIN, DEFAULT), &["getpid_spin"]);
    let kind = exit_testing::exit_kill_kind();
    exit_testing::disarm_exit_kill();
    let st = match st {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if st != wait_signaled(SIGKILL) {
        return crate::fail_fmt!("status {st:#x}, want {:#x}", wait_signaled(SIGKILL));
    }
    match kind {
        Some(k) if k == u64::from(vectors::IPI_RESCHEDULE) => Outcome::Ok,
        Some(k) => crate::fail_fmt!(
            "kill acted on in exit kind {k:#x}, want {:#x}",
            vectors::IPI_RESCHEDULE
        ),
        None => Outcome::Fail("no exit recorded acting on the kill"),
    }
}
