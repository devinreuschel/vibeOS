//! Test-only hooks over proc's kernel half (kernel_tests only, Q2): CR3
//! loads and SYSCALL MSR reads that no production path needs.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use vibeos::proc::SIGKILL;
use vibeos::syscall::UserFrame;

use vibeos::desc::star_value;

use crate::addr_space_init;
use crate::addr_space_init::Space;
use crate::pmm_init;
#[cfg(target_arch = "x86_64")]
use crate::x86::{self, EFER_SCE, IA32_EFER, IA32_STAR};

/// Load `space`'s root into CR3 unless this CPU already has it, and record
/// it in this CPU's `PerCpuRemote.as_cr3`. No TCB names it, so the next
/// switch back to the calling thread loads that thread's own root again.
pub(crate) fn load_cr3(space: &Space) {
    // SAFETY: invariant I44: the root is a PML4 `addr_space_init::create`
    // or `addr_space_init::clone_full` built, whose kernel half is the
    // kernel's, and only the core's free frees it, which refuses a root this
    // CPU has loaded or recorded, and `space` holds a reference meanwhile;
    // established by `addr_space_init::SpaceCore`'s drop.
    unsafe { addr_space_init::load_cr3_u64(space.root().as_u64()) };
}

/// This CPU's recorded root is `space`'s: a [`load_cr3`] of it skipped the
/// write.
pub(crate) fn cr3_was_skipped(space: &Space) -> bool {
    crate::per_cpu_init::with_current(|c| c.remote.as_cr3.load(Ordering::Relaxed))
        == space.root().as_u64()
}

/// STAR holds `desc::star_value()` (the kernel and Linux's SYSRET
/// selectors), and EFER.SCE is set.
#[cfg(target_arch = "x86_64")]
pub(crate) fn star_configured() -> bool {
    let star = x86::rdmsr(IA32_STAR);
    let efer = x86::rdmsr(IA32_EFER);
    star == star_value() && (efer & EFER_SCE) != 0
}

// Frame counts around `user_init::load_path`, recorded by its inline hook
// points.

/// Free-frame counts around one [`crate::user_init::load_path`] call.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ExecFrames {
    /// Buddy free frames at entry.
    pub before: usize,
    /// Buddy free frames at return, after a failed load's teardown.
    pub after: usize,
    pub ok: bool,
}

const SLOTS: usize = 4;
static NEXT: AtomicUsize = AtomicUsize::new(0);
static BEFORE: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
static AFTER: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
static OK: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];

pub(crate) fn free_now() -> usize {
    pmm_init::with_buddy(|b| b.stats().free_frames)
}

pub(crate) fn record(before: usize, ok: bool) {
    let after = free_now();
    let i = NEXT.load(Ordering::Relaxed);
    BEFORE[i % SLOTS].store(before as u64, Ordering::Relaxed);
    AFTER[i % SLOTS].store(after as u64, Ordering::Relaxed);
    OK[i % SLOTS].store(u64::from(ok), Ordering::Relaxed);
    NEXT.store(i.wrapping_add(1), Ordering::Release);
}

/// Forget the recorded loads.
pub(crate) fn clear_exec_frames() {
    NEXT.store(0, Ordering::Release);
}

/// The last four `load_path` calls since [`clear_exec_frames`], oldest
/// first.
pub(crate) fn exec_frames() -> Vec<ExecFrames> {
    let n = NEXT.load(Ordering::Acquire);
    let first = n.saturating_sub(SLOTS);
    (first..n)
        .map(|i| ExecFrames {
            before: BEFORE[i % SLOTS].load(Ordering::Relaxed) as usize,
            after: AFTER[i % SLOTS].load(Ordering::Relaxed) as usize,
            ok: OK[i % SLOTS].load(Ordering::Relaxed) != 0,
        })
        .collect()
}

// The exit-work hooks (ROADMAP §10.6, F033): `syscall_init::exit_work`
// calls them in kernel_tests builds.

/// Syscall number plus one whose next exit, right after its last exit-work
/// check, posts `SIGKILL` to the returning process and sends this CPU a
/// reschedule IPI; 0 for none.
static KILL_AFTER_NR: AtomicU64 = AtomicU64::new(0);
/// Set by the hook once it has posted the kill; taken by the next exit
/// that finds work.
static KILL_POSTED: AtomicBool = AtomicBool::new(false);
/// The `kind` of the exit that acted on the posted kill
/// (`syscall_init::EXIT_SYSCALL` or a vector); `u64::MAX` for none yet.
static KILL_ACTED_KIND: AtomicU64 = AtomicU64::new(u64::MAX);
/// The pid whose user `r12` every exit to ring 3 records; 0 for none.
static WATCH_PID: AtomicU32 = AtomicU32::new(0);
static WATCH_R12: AtomicU64 = AtomicU64::new(0);

/// Arm the exit hook for syscall `nr` and clear the record.
pub(crate) fn arm_exit_kill(nr: u64) {
    KILL_POSTED.store(false, Ordering::Release);
    KILL_ACTED_KIND.store(u64::MAX, Ordering::Release);
    KILL_AFTER_NR.store(nr.wrapping_add(1), Ordering::Release);
}

/// Disarm [`arm_exit_kill`].
pub(crate) fn disarm_exit_kill() {
    KILL_AFTER_NR.store(0, Ordering::Release);
    KILL_POSTED.store(false, Ordering::Release);
}

/// The exit kind that acted on the hook's kill, once one has.
pub(crate) fn exit_kill_kind() -> Option<u64> {
    let k = KILL_ACTED_KIND.load(Ordering::Acquire);
    (k != u64::MAX).then_some(k)
}

/// Record `pid`'s user `r12` at each of its exits to ring 3; 0 stops.
pub(crate) fn watch_r12(pid: u32) {
    WATCH_R12.store(0, Ordering::Release);
    WATCH_PID.store(pid, Ordering::Release);
}

/// The watched pid's `r12` at its last exit to ring 3.
pub(crate) fn watched_r12() -> u64 {
    WATCH_R12.load(Ordering::Acquire)
}

/// `syscall_init::exit_work`, at its start: records the watched process's
/// `r12`.
pub(crate) fn exit_seen(frame: &UserFrame) {
    let w = WATCH_PID.load(Ordering::Acquire);
    if w != 0 && crate::thread_init::current_pid() == w {
        WATCH_R12.store(frame.r12, Ordering::Release);
    }
}

/// `syscall_init::exit_work`, each time a check finds work: the first one
/// after the hook posted its kill records its `kind`.
pub(crate) fn exit_work_found(kind: u64) {
    if KILL_POSTED.swap(false, Ordering::AcqRel) {
        KILL_ACTED_KIND.store(kind, Ordering::Release);
    }
}

/// `syscall_init::exit_work` on a syscall exit, after its last check: the
/// armed syscall's exit posts `SIGKILL` to the returning process and sends
/// this CPU a reschedule IPI, which IF=0 holds pending until ring 3.
pub(crate) fn exit_check_hook(frame: &UserFrame) {
    let pid = crate::thread_init::current_pid();
    let armed = frame.orig_rax.wrapping_add(1);
    // 0 is "unarmed", and also what an `orig_rax` of -1 (no syscall)
    // gives.
    if pid == 0
        || armed == 0
        || KILL_AFTER_NR
            .compare_exchange(armed, 0, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    crate::proc_init::testing::post_pending(pid, SIGKILL);
    KILL_POSTED.store(true, Ordering::Release);
    crate::ipi_init::kick(crate::thread_init::current_cpu());
}
