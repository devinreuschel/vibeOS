//! In-guest tests of P10-S17, Syscall exit IF=0, syscall_init::first_return, USER_MAP_END and the FP binding (DESIGN §8.2).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use vibeos::proc::SIGKILL;
use vibeos::proc::wait_signaled;
use vibeos::syscall::SYS_KILL;

use super::Outcome;
use super::sleep_until;
use super::user::{self, DEFAULT, Image, user_code};
use crate::proc_init;
use crate::thread_init;
use crate::x86;

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
pub(super) fn test_fp_no_leak() -> Outcome {
    if x86::read_cr0() & x86::CR0_TS != 0 {
        return Outcome::Fail("CR0.TS set");
    }
    FP_SPAWNED.store(false, Ordering::Release);
    super::spawn_thread_on("fp_spawner", fp_spawner, thread_init::current_cpu());
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

/// Turns the requeue hook off when dropped.
struct RequeueGuard;

impl Drop for RequeueGuard {
    fn drop(&mut self) {
        thread_init::testing::set_requeue_next_cpu(false);
    }
}

/// A user thread that the requeue hook moves to the next CPU each time
/// it is preempted keeps its XMM state across every migration.
pub(super) fn test_fp_migrate_counter() -> Outcome {
    if super::second_cpu().is_none() {
        return Outcome::Skip("needs 2 CPUs");
    }
    let before = thread_init::testing::requeues();
    let pid = {
        thread_init::testing::set_requeue_next_cpu(true);
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
    let moves = thread_init::testing::requeues().wrapping_sub(before);
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
