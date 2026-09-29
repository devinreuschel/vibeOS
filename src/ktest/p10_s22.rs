//! In-guest tests of P10-S22, Flake root cause I: the SMP boot and user-syscall hang (DESIGN §8.2).

use core::sync::atomic::{AtomicU32, Ordering};

use vibeos::thread::{MAX_THREADS, ThreadId, ThreadState};

use super::Outcome;
use super::user::{self, DEFAULT, Image, user_code};
use crate::per_cpu_init;
use crate::syscall_init::testing as entry_testing;
use crate::thread_init;
use crate::time_init;

// `user/tests.asm` from its `dup(1)` on (ROADMAP §10.2, F021), for
// FORK_WAIT_ROUNDS rounds: (1) dup(1), a zero-length write, close; (2) fork,
// the child exits 7, wait4(pid) wants 0x0700; (3) fork, the child execs
// /hello, wait4(pid) wants 0x2A00; (4) fork, the child loads from address
// 0, wait4(pid) wants SIGSEGV; (5) up to 32 forks of exit(0) children with
// two sched_yield each until fork fails, then wait4(-1, NULL) until
// -ECHILD. Exits 0; a failed check exits 10 + its step. Raise the round
// count only locally, for soak runs.
user_code!(
    FORK_WAIT,
    "
    .set FORK_WAIT_ROUNDS, 4
    sub rsp, 32
    mov r15d, FORK_WAIT_ROUNDS
1:
    mov edi, 1
    mov eax, 32
    syscall
    test rax, rax
    js 81f
    mov r12, rax
    mov rdi, r12
    lea rsi, [rip + 90f]
    xor edx, edx
    mov eax, 1
    syscall
    test rax, rax
    jnz 81f
    mov rdi, r12
    mov eax, 3
    syscall
    test rax, rax
    jnz 81f

    mov eax, 57
    syscall
    test rax, rax
    js 82f
    jnz 2f
    mov edi, 7
    mov eax, 60
    syscall
    ud2
2:
    mov r12, rax
    mov dword ptr [rsp], -1
    mov rdi, r12
    mov rsi, rsp
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    cmp rax, r12
    jne 82f
    cmp dword ptr [rsp], 0x0700
    jne 82f

    mov eax, 57
    syscall
    test rax, rax
    js 83f
    jnz 3f
    lea rdi, [rip + 91f]
    mov [rsp + 8], rdi
    mov qword ptr [rsp + 16], 0
    lea rsi, [rsp + 8]
    xor edx, edx
    mov eax, 59
    syscall
    mov edi, 127
    mov eax, 60
    syscall
    ud2
3:
    mov r12, rax
    mov dword ptr [rsp], -1
    mov rdi, r12
    mov rsi, rsp
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    cmp rax, r12
    jne 83f
    cmp dword ptr [rsp], 0x2A00
    jne 83f

    mov eax, 57
    syscall
    test rax, rax
    js 84f
    jnz 4f
    xor eax, eax
    mov rax, [rax]
    ud2
4:
    mov r12, rax
    mov dword ptr [rsp], -1
    mov rdi, r12
    mov rsi, rsp
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    cmp rax, r12
    jne 84f
    mov eax, dword ptr [rsp]
    and eax, 0x7f
    cmp eax, 11
    jne 84f

    xor r13d, r13d
5:
    mov eax, 57
    syscall
    test rax, rax
    js 6f
    jnz 7f
    xor edi, edi
    mov eax, 60
    syscall
    ud2
7:
    mov eax, 24
    syscall
    mov eax, 24
    syscall
    inc r13
    cmp r13, 32
    jb 5b
    jmp 8f
6:
    cmp rax, -11
    je 8f
    test r13, r13
    jz 85f
8:
    mov rdi, -1
    xor esi, esi
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    cmp rax, -10
    jne 8b

    dec r15d
    jnz 1b
    xor edi, edi
    mov eax, 60
    syscall
    ud2
81:
    mov edi, 11
    jmp 89f
82:
    mov edi, 12
    jmp 89f
83:
    mov edi, 13
    jmp 89f
84:
    mov edi, 14
    jmp 89f
85:
    mov edi, 15
89:
    mov eax, 60
    syscall
    ud2
90:
    .byte 0
91:
    .asciz \"/hello\"
    "
);

/// How long the registry waits for one run of [`FORK_WAIT`].
const RUN_MS: u64 = 4_000;

/// First ring-3 entries each run holds in the entry window: the
/// program's own and its first forked children's.
const HELD_ENTRIES: u32 = 4;

/// [`RUN_ST`] before the run's thread has published.
const PENDING: u32 = u32::MAX;
/// [`RUN_ST`] when the spawn failed.
const SPAWN_FAILED: u32 = u32::MAX - 1;

/// The status word of the current run, published by its thread.
static RUN_ST: AtomicU32 = AtomicU32::new(PENDING);

/// The body of the thread pinned to the CPU under test: spawn the
/// program there (`spawn_user` pins a process to its spawner's CPU),
/// wait for it, and publish the status as its last access.
fn fork_wait_runner() {
    let st = match user::spawn(&Image::Code(FORK_WAIT, DEFAULT), &["fork_wait"]) {
        Ok(pid) => user::wait(pid),
        Err(_) => SPAWN_FAILED,
    };
    RUN_ST.store(st, Ordering::Release);
}

/// The processes of a [`FORK_WAIT`] run that are not Dead, with their
/// state and CPU.
struct Stuck;

impl core::fmt::Display for Stuck {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut buf = [thread_init::ThreadInfo {
            id: ThreadId::NONE,
            name: "",
            state: ThreadState::Dead,
            cpu: 0,
        }; MAX_THREADS];
        let n = thread_init::snapshot(&mut buf);
        for t in &buf[..n] {
            if !matches!(t.name, "fork_wait" | "user" | "/hello") {
                continue;
            }
            let st = match t.state {
                ThreadState::Ready => "R",
                ThreadState::Running => "run",
                ThreadState::Sleeping { .. } => "S",
                ThreadState::Blocked { .. } => "B",
                ThreadState::Dead => continue,
            };
            write!(f, " {}:{}{}@{}", t.id.raw(), t.name, st, t.cpu)?;
        }
        Ok(())
    }
}

/// Run [`FORK_WAIT`] from a thread pinned to `cpu`, with its first
/// [`HELD_ENTRIES`] ring-3 entries held in the window, waiting at most
/// [`RUN_MS`].
fn run_on(cpu: u32) -> Outcome {
    RUN_ST.store(PENDING, Ordering::Relaxed);
    entry_testing::arm_fork_wait_stall(HELD_ENTRIES);
    let out = wait_run(cpu);
    entry_testing::arm_fork_wait_stall(0);
    out
}

fn wait_run(cpu: u32) -> Outcome {
    super::spawn_thread_on("fork_wait_run", fork_wait_runner, cpu);
    let t0 = time_init::uptime_ms();
    loop {
        let st = RUN_ST.load(Ordering::Acquire);
        match st {
            PENDING => {}
            0 => return Outcome::Ok,
            SPAWN_FAILED => return crate::fail_fmt!("spawn failed cpu{cpu}"),
            st => return crate::fail_fmt!("status {st:#x} cpu{cpu}"),
        }
        if time_init::uptime_ms().saturating_sub(t0) >= RUN_MS {
            return crate::fail_fmt!("stalled {RUN_MS} ms cpu{cpu}:{}", Stuck);
        }
        thread_init::sleep_ms(10);
    }
}

/// `user/tests.asm`'s fork, wait4, execve, fault and fork-bomb sequence
/// after `user: dup ok` finishes on every spawning CPU, the registry's and
/// a second one, while `syscall_init::testing::fork_wait_stall_point`
/// holds each run's first ring-3 entries in the window after GS is
/// loaded for ring 3 (ROADMAP §10.2, F021).
pub(super) fn user_fork_wait_stall() -> Outcome {
    let here = per_cpu_init::current().cpu_id;
    let out = run_on(here);
    if !matches!(out, Outcome::Ok) {
        return out;
    }
    match super::second_cpu() {
        Some(cpu) if cpu != here => run_on(cpu),
        _ => Outcome::Ok,
    }
}
