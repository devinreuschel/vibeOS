//! In-guest tests of P10-S17, Syscall exit IF=0, syscall_init::first_return, USER_MAP_END and the FP binding (DESIGN §8.2).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use vibeos::elf::ElfError;
use vibeos::kbd::DecodedKey;
use vibeos::paging::{PAGE_SIZE_4K, USER_MAP_END};
use vibeos::proc::SIGKILL;
use vibeos::proc::{SIGSEGV, wait_signaled};
use vibeos::syscall::{SYS_GETPID, SYS_KILL};
use vibeos::vectors;

use super::Outcome;
use super::user::{self, DEFAULT, Image, Layout, user_code};
use crate::apic_init;
use crate::console_init;
use crate::kbd_init;
use crate::proc_init;
use crate::syscall_init::testing as sc_testing;
use crate::thread_init;
use crate::time_init;
use crate::user_init::LoadError;
use crate::x86;

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

// read(0, rsp, 1), then exit with the byte read; exit(2) if read does not
// return 1.
user_code!(
    READ_ONE_KEY,
    "
    sub rsp, 16
    xor edi, edi
    mov rsi, rsp
    mov edx, 1
    xor eax, eax
    syscall
    cmp rax, 1
    jne 1f
    movzx edi, byte ptr [rsp]
    mov eax, 60
    syscall
1:
    mov edi, 2
    mov eax, 60
    syscall
    ud2
    "
);

/// A user `read` on fd 0 blocks in `console_init::wait_key`'s halt branch
/// until a key is queued, then returns through the syscall exit, whose
/// debug-build check faults if `wait_key` left IF set.
pub(super) fn test_console_read_exit() -> Outcome {
    console_init::testing::reset_halts();
    let pid = match user::spawn(&Image::Code(READ_ONE_KEY, DEFAULT), &["read_one_key"]) {
        Ok(pid) => pid,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    let halted = sleep_until(|| console_init::testing::halts() != 0, 5_000);
    kbd_init::push_for_test(DecodedKey::Char(b'k'));
    let st = user::wait(pid);
    if !halted {
        return Outcome::Fail("reader never reached wait_key's halt");
    }
    if st != 0x6B << 8 {
        return crate::fail_fmt!("status {st:#x}, want {:#x}", 0x6B << 8);
    }
    Outcome::Ok
}

// 1,000 times: fork; the child exits 0 and the parent waits for it. Exit
// 0, or 2 on a negative return.
user_code!(
    FORK_1000,
    "
    mov r12d, 1000
2:
    mov eax, 57
    syscall
    test rax, rax
    js 9f
    jz 8f
    mov rdi, rax
    xor esi, esi
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    test rax, rax
    js 9f
    dec r12d
    jnz 2b
    xor edi, edi
    mov eax, 60
    syscall
8:
    xor edi, edi
    mov eax, 60
    syscall
9:
    mov edi, 2
    mov eax, 60
    syscall
    ud2
    "
);

static ENTRY_CHILD: AtomicU64 = AtomicU64::new(0);
static ENTRY_SPAWNED: AtomicBool = AtomicBool::new(false);
static ENTRY_STOP: AtomicBool = AtomicBool::new(false);
static ENTRY_SENDER_DONE: AtomicBool = AtomicBool::new(false);

/// Pinned to CPU 0, so the process and its fork children are too
/// (`thread_init::spawn_user`).
fn entry_spawner() {
    let pid = match user::spawn(&Image::Code(FORK_1000, DEFAULT), &["fork_1000"]) {
        Ok(pid) => u64::from(pid),
        Err(_) => u64::MAX,
    };
    ENTRY_CHILD.store(pid, Ordering::Relaxed);
    ENTRY_SPAWNED.store(true, Ordering::Release);
}

/// Reschedule IPIs to CPU 0 about every 20 us until `ENTRY_STOP`.
fn entry_ipi_sender() {
    while !ENTRY_STOP.load(Ordering::Acquire) {
        // A failed send only thins the IPI stream.
        let _ = apic_init::send_ipi_cpu(0, vectors::IPI_RESCHEDULE);
        let t = time_init::now_ns().saturating_add(20_000);
        while time_init::now_ns() < t {
            core::hint::spin_loop();
        }
    }
    ENTRY_SENDER_DONE.store(true, Ordering::Release);
}

/// Forked processes enter ring 3 through `syscall_init::first_return` on CPU 0 while
/// another CPU keeps sending it reschedule IPIs; an interrupt taken there
/// with the user GS loaded halts the kernel, and the debug-build check
/// before the selector loads faults if IF is set.
pub(super) fn test_user_entry_irq() -> Outcome {
    let Some(sender_cpu) = super::second_cpu() else {
        return Outcome::Skip("needs 2 CPUs");
    };
    ENTRY_SPAWNED.store(false, Ordering::Release);
    ENTRY_STOP.store(false, Ordering::Release);
    ENTRY_SENDER_DONE.store(false, Ordering::Release);
    super::spawn_thread_on("entry_ipi_sender", entry_ipi_sender, sender_cpu);
    super::spawn_thread_on("entry_spawner", entry_spawner, 0);
    let spawned = sleep_until(|| ENTRY_SPAWNED.load(Ordering::Acquire), 5_000);
    let pid = u32::try_from(ENTRY_CHILD.load(Ordering::Relaxed)).ok();
    let st = if spawned { pid.map(user::wait) } else { None };
    ENTRY_STOP.store(true, Ordering::Release);
    let stopped = sleep_until(|| ENTRY_SENDER_DONE.load(Ordering::Acquire), 5_000);
    if !spawned {
        return Outcome::Fail("spawner did not run");
    }
    let Some(st) = st else {
        return Outcome::Fail("spawn");
    };
    if !stopped {
        return Outcome::Fail("IPI sender did not stop");
    }
    if st != 0 {
        return crate::fail_fmt!("status {st:#x}, want 0");
    }
    Outcome::Ok
}

// getpid, then a jmp to a `syscall` in the page's last two bytes, whose
// return RIP is the first byte past the page.
user_code!(
    SYSCALL_AT_PAGE_END,
    "
    mov eax, 39
    jmp 1f
    .org vibeos_user_code_SYSCALL_AT_PAGE_END + 4094, 0xcc
1:
    syscall
    "
);

/// An image whose last page is the one below `USER_END` does not load:
/// its `syscall` would return to a non-canonical RIP. The same code one
/// page lower loads, and its `syscall` returns to the unmapped top page.
pub(super) fn test_exec_top_page_enoexec() -> Outcome {
    let at = |vaddr| Image::Code(SYSCALL_AT_PAGE_END, Layout { vaddr, ..DEFAULT });
    match user::spawn(&at(USER_MAP_END), &["top_page"]) {
        Err(LoadError::Elf(ElfError::KernelVa)) => {}
        Err(e) => return crate::fail_fmt!("top page: {}, want kernel va", e.as_str()),
        Ok(pid) => {
            let st = user::wait(pid);
            return crate::fail_fmt!("top page loaded, status {st:#x}");
        }
    }
    let st = match user::run(&at(USER_MAP_END - PAGE_SIZE_4K), &["below_top"]) {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("page below: spawn: {}", e.as_str()),
    };
    if st != wait_signaled(SIGSEGV) {
        return crate::fail_fmt!("page below: status {st:#x}, want SIGSEGV");
    }
    Outcome::Ok
}

// getpid, then exit(0).
user_code!(
    GETPID_EXIT0,
    "
    mov eax, 39
    syscall
    xor edi, edi
    mov eax, 60
    syscall
    ud2
    "
);

// exit(0).
user_code!(
    EXIT0,
    "
    xor edi, edi
    mov eax, 60
    syscall
    ud2
    "
);

/// A syscall whose saved RIP a hook makes non-canonical kills the process
/// with `SIGSEGV` on the exit's own path, before `swapgs`; a process whose
/// first entry `iretq`s to a non-canonical RIP gets `SIGSEGV` too (on KVM
/// from the labeled `iretq`'s `#GP`, on TCG from the fetch).
pub(super) fn test_noncanonical_rip_sigsegv() -> Outcome {
    let kills = sc_testing::bad_rip_kills();
    sc_testing::arm_noncanonical_rip(SYS_GETPID);
    let st = user::run(&Image::Code(GETPID_EXIT0, DEFAULT), &["bad_rip_exit"]);
    sc_testing::disarm_noncanonical();
    let st = match st {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("exit case: spawn: {}", e.as_str()),
    };
    if st != wait_signaled(SIGSEGV) {
        return crate::fail_fmt!("exit case: status {st:#x}, want SIGSEGV");
    }
    let got = sc_testing::bad_rip_kills().wrapping_sub(kills);
    if got != 1 {
        return crate::fail_fmt!("exit case: {got} bad-RIP kills, want 1");
    }
    sc_testing::arm_noncanonical_entry();
    let st = user::run(&Image::Code(EXIT0, DEFAULT), &["bad_rip_entry"]);
    sc_testing::disarm_noncanonical();
    let st = match st {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("entry case: spawn: {}", e.as_str()),
    };
    if st != wait_signaled(SIGSEGV) {
        return crate::fail_fmt!("entry case: status {st:#x}, want SIGSEGV");
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
