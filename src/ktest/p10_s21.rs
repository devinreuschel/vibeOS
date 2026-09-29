//! In-guest tests of P10-S21, One user frame and ring-3 traps to signals (DESIGN §8.2).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use vibeos::proc::{SIGILL, SIGKILL, SIGTRAP, wait_signaled};
use vibeos::syscall::SYS_KILL;
use vibeos::vectors;

use super::Outcome;
use super::user::{self, DEFAULT, Image, Layout, user_code};
use crate::apic_init;
use crate::arch::idt::testing as idt_testing;
use crate::proc_init::{self, testing as proc_testing};
use crate::thread_init;
use crate::time_init;

/// What `arm_rcx_canary` puts in the frame's `rcx`: not the return RIP.
const RCX_CANARY: u64 = 0x0C0F_FEE0_DEAD_BEEF;

// getpid with rdi = proc_init::testing::HOOK_MAGIC; exit(0) when RCX holds
// RCX_CANARY after the call, else exit(1).
user_code!(
    RCX_CANARY_PROG,
    "
    movabs rdi, 0x5EEDCA1100005A21
    mov eax, 39
    syscall
    movabs rdx, 0x0C0FFEE0DEADBEEF
    xor edi, edi
    cmp rcx, rdx
    setne dil
    mov eax, 60
    syscall
    ud2
    "
);

const _: () = assert!(proc_testing::HOOK_MAGIC == 0x5EED_CA11_0000_5A21);

/// A hook sets a `getpid`'s saved `rcx` to a canary that is not its
/// return RIP; the exit then leaves through `iretq`, and the program finds
/// the canary in RCX.
pub(super) fn test_syscall_rcx_canary() -> Outcome {
    proc_testing::arm_rcx_canary(RCX_CANARY);
    let st = user::run(&Image::Code(RCX_CANARY_PROG, DEFAULT), &["rcx_canary"]);
    proc_testing::disarm();
    match st {
        Ok(0) => Outcome::Ok,
        Ok(st) => crate::fail_fmt!("status {st:#x}, want 0"),
        Err(e) => crate::fail_fmt!("spawn: {}", e.as_str()),
    }
}

// fork with 12 syscall-preserved GPRs holding canaries. The child exits 0
// iff all 12 survived its first return, else 1; the parent exits 2 if its
// own 12 changed, else waits for the child and exits with its code.
user_code!(
    FORK_GPRS,
    "
    movabs rbx, 0xC0DE0100F00DF001
    movabs rbp, 0xC0DE0200F00DF002
    movabs rdx, 0xC0DE0300F00DF003
    movabs rsi, 0xC0DE0400F00DF004
    movabs rdi, 0xC0DE0500F00DF005
    movabs r8, 0xC0DE0600F00DF006
    movabs r9, 0xC0DE0700F00DF007
    movabs r10, 0xC0DE0800F00DF008
    movabs r12, 0xC0DE0900F00DF009
    movabs r13, 0xC0DE0A00F00DF00A
    movabs r14, 0xC0DE0B00F00DF00B
    movabs r15, 0xC0DE0C00F00DF00C
    mov eax, 57
    syscall
    test rax, rax
    jz 5f
    js 8f
    movabs rcx, 0xC0DE0100F00DF001
    cmp rbx, rcx
    jne 8f
    movabs rcx, 0xC0DE0200F00DF002
    cmp rbp, rcx
    jne 8f
    movabs rcx, 0xC0DE0300F00DF003
    cmp rdx, rcx
    jne 8f
    movabs rcx, 0xC0DE0400F00DF004
    cmp rsi, rcx
    jne 8f
    movabs rcx, 0xC0DE0500F00DF005
    cmp rdi, rcx
    jne 8f
    movabs rcx, 0xC0DE0600F00DF006
    cmp r8, rcx
    jne 8f
    movabs rcx, 0xC0DE0700F00DF007
    cmp r9, rcx
    jne 8f
    movabs rcx, 0xC0DE0800F00DF008
    cmp r10, rcx
    jne 8f
    movabs rcx, 0xC0DE0900F00DF009
    cmp r12, rcx
    jne 8f
    movabs rcx, 0xC0DE0A00F00DF00A
    cmp r13, rcx
    jne 8f
    movabs rcx, 0xC0DE0B00F00DF00B
    cmp r14, rcx
    jne 8f
    movabs rcx, 0xC0DE0C00F00DF00C
    cmp r15, rcx
    jne 8f
    mov rdi, rax
    lea rsi, [rip + 7f]
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    mov edi, dword ptr [rip + 7f]
    shr edi, 8
    and edi, 0xff
    mov eax, 60
    syscall
    ud2
8:
    mov edi, 2
    mov eax, 60
    syscall
    ud2
5:
    movabs rcx, 0xC0DE0100F00DF001
    cmp rbx, rcx
    jne 9f
    movabs rcx, 0xC0DE0200F00DF002
    cmp rbp, rcx
    jne 9f
    movabs rcx, 0xC0DE0300F00DF003
    cmp rdx, rcx
    jne 9f
    movabs rcx, 0xC0DE0400F00DF004
    cmp rsi, rcx
    jne 9f
    movabs rcx, 0xC0DE0500F00DF005
    cmp rdi, rcx
    jne 9f
    movabs rcx, 0xC0DE0600F00DF006
    cmp r8, rcx
    jne 9f
    movabs rcx, 0xC0DE0700F00DF007
    cmp r9, rcx
    jne 9f
    movabs rcx, 0xC0DE0800F00DF008
    cmp r10, rcx
    jne 9f
    movabs rcx, 0xC0DE0900F00DF009
    cmp r12, rcx
    jne 9f
    movabs rcx, 0xC0DE0A00F00DF00A
    cmp r13, rcx
    jne 9f
    movabs rcx, 0xC0DE0B00F00DF00B
    cmp r14, rcx
    jne 9f
    movabs rcx, 0xC0DE0C00F00DF00C
    cmp r15, rcx
    jne 9f
    xor edi, edi
    mov eax, 60
    syscall
    ud2
9:
    mov edi, 1
    mov eax, 60
    syscall
    ud2
    .balign 8
7:
    .quad 0
    "
);

/// A forked child's first return to ring 3 is the syscall exit over the
/// parent's frame with `rax` = 0: the 12 GPRs a syscall preserves reach
/// the child intact, and the parent keeps its own.
pub(super) fn test_fork_child_gprs() -> Outcome {
    let img = Image::Code(
        FORK_GPRS,
        Layout {
            writable: true,
            ..DEFAULT
        },
    );
    match user::run(&img, &["fork_gprs"]) {
        Ok(0) => Outcome::Ok,
        Ok(st) => crate::fail_fmt!("status {st:#x}, want 0 (exit 1: child GPRs, 2: parent GPRs)"),
        Err(e) => crate::fail_fmt!("spawn: {}", e.as_str()),
    }
}

// 15 GPR canaries (arch::idt::testing::GPR_CANARIES), then a spin at
// +0x100; the hook sends the frame's RIP to +0x200, which exits 0 iff
// every GPR still holds its canary, else 1.
user_code!(
    GPR_CANARY_SPIN,
    "
    movabs r15, 0xC0DE000000000101
    movabs r14, 0xC0DE000000000202
    movabs r13, 0xC0DE000000000303
    movabs r12, 0xC0DE000000000404
    movabs rbp, 0xC0DE000000000505
    movabs rbx, 0xC0DE000000000606
    movabs r11, 0xC0DE000000000707
    movabs r10, 0xC0DE000000000808
    movabs r9, 0xC0DE000000000909
    movabs r8, 0xC0DE000000000A0A
    movabs rax, 0xC0DE000000000B0B
    movabs rcx, 0xC0DE000000000C0C
    movabs rdx, 0xC0DE000000000D0D
    movabs rsi, 0xC0DE000000000E0E
    movabs rdi, 0xC0DE000000000F0F
    jmp 2f
    .org vibeos_user_code_GPR_CANARY_SPIN + 0x100, 0xcc
2:
    jmp 2b
    .org vibeos_user_code_GPR_CANARY_SPIN + 0x200, 0xcc
    cmp r15, qword ptr [rip + 3f + 0]
    jne 9f
    cmp r14, qword ptr [rip + 3f + 8]
    jne 9f
    cmp r13, qword ptr [rip + 3f + 16]
    jne 9f
    cmp r12, qword ptr [rip + 3f + 24]
    jne 9f
    cmp rbp, qword ptr [rip + 3f + 32]
    jne 9f
    cmp rbx, qword ptr [rip + 3f + 40]
    jne 9f
    cmp r11, qword ptr [rip + 3f + 48]
    jne 9f
    cmp r10, qword ptr [rip + 3f + 56]
    jne 9f
    cmp r9, qword ptr [rip + 3f + 64]
    jne 9f
    cmp r8, qword ptr [rip + 3f + 72]
    jne 9f
    cmp rax, qword ptr [rip + 3f + 80]
    jne 9f
    cmp rcx, qword ptr [rip + 3f + 88]
    jne 9f
    cmp rdx, qword ptr [rip + 3f + 96]
    jne 9f
    cmp rsi, qword ptr [rip + 3f + 104]
    jne 9f
    cmp rdi, qword ptr [rip + 3f + 112]
    jne 9f
    xor edi, edi
    mov eax, 60
    syscall
    ud2
9:
    mov edi, 1
    mov eax, 60
    syscall
    ud2
    .balign 8
3:
    .quad 0xC0DE000000000101
    .quad 0xC0DE000000000202
    .quad 0xC0DE000000000303
    .quad 0xC0DE000000000404
    .quad 0xC0DE000000000505
    .quad 0xC0DE000000000606
    .quad 0xC0DE000000000707
    .quad 0xC0DE000000000808
    .quad 0xC0DE000000000909
    .quad 0xC0DE000000000A0A
    .quad 0xC0DE000000000B0B
    .quad 0xC0DE000000000C0C
    .quad 0xC0DE000000000D0D
    .quad 0xC0DE000000000E0E
    .quad 0xC0DE000000000F0F
    "
);

/// Reschedule IPIs the canary hook must see the child take in its loop.
const CANARY_IPIS: u64 = 1_000;

static CANARY_CHILD: AtomicU64 = AtomicU64::new(0);
static CANARY_SPAWNED: AtomicBool = AtomicBool::new(false);
static CANARY_STOP: AtomicBool = AtomicBool::new(false);
static CANARY_SENT: AtomicBool = AtomicBool::new(false);

/// Pinned to CPU 0, so the child is too (`thread_init::spawn_user`).
fn canary_spawner() {
    let pid = match user::spawn(&Image::Code(GPR_CANARY_SPIN, DEFAULT), &["gpr_canary"]) {
        Ok(pid) => u64::from(pid),
        Err(_) => u64::MAX,
    };
    CANARY_CHILD.store(pid, Ordering::Relaxed);
    CANARY_SPAWNED.store(true, Ordering::Release);
}

/// Pinned to CPU 0: a ready thread, so each reschedule IPI switches the
/// child out.
fn canary_yielder() {
    while !CANARY_STOP.load(Ordering::Acquire) {
        thread_init::yield_now();
    }
}

/// Sends CPU 0 a reschedule IPI, waits up to 10 ms for the hook's hit
/// count to move, and sends again, until [`CANARY_IPIS`] hits or 30 s.
fn canary_sender() {
    let deadline = time_init::now_ns().saturating_add(30_000_000_000);
    while idt_testing::hits() < CANARY_IPIS && time_init::now_ns() < deadline {
        let h = idt_testing::hits();
        // A failed send shows as a shortfall in the hit count.
        let _ = apic_init::send_ipi_cpu(0, vectors::IPI_RESCHEDULE);
        let t = time_init::now_ns().saturating_add(10_000_000);
        while idt_testing::hits() == h && time_init::now_ns() < t {
            core::hint::spin_loop();
        }
    }
    CANARY_SENT.store(true, Ordering::Release);
}

/// A ring-3 loop holding canaries in all 15 GPRs takes 1,000 reschedule
/// IPIs on CPU 0, and at each one the hook finds the canaries in the user
/// frame at the top of the preempted thread's kernel stack.
pub(super) fn test_preempt_gpr_canaries() -> Outcome {
    let Some(sender_cpu) = super::second_cpu() else {
        return Outcome::Skip("needs 2 CPUs");
    };
    let base = DEFAULT.vaddr;
    CANARY_SPAWNED.store(false, Ordering::Release);
    CANARY_STOP.store(false, Ordering::Release);
    CANARY_SENT.store(false, Ordering::Release);
    idt_testing::arm_canaries(base + 0x100, base + 0x102, base + 0x200, CANARY_IPIS);
    super::spawn_thread_on("canary_yielder", canary_yielder, 0);
    super::spawn_thread_on("canary_spawner", canary_spawner, 0);
    let spawned = sleep_until(|| CANARY_SPAWNED.load(Ordering::Acquire), 5_000);
    let pid = u32::try_from(CANARY_CHILD.load(Ordering::Relaxed)).ok();
    let st = match (spawned, pid) {
        (true, Some(pid)) => {
            super::spawn_thread_on("canary_sender", canary_sender, sender_cpu);
            let sent = sleep_until(|| CANARY_SENT.load(Ordering::Acquire), 35_000);
            if !sent || idt_testing::hits() < CANARY_IPIS {
                let _ =
                    proc_init::dispatch(SYS_KILL, [u64::from(pid), u64::from(SIGKILL), 0, 0, 0, 0]);
            }
            Some(user::wait(pid))
        }
        _ => None,
    };
    CANARY_STOP.store(true, Ordering::Release);
    idt_testing::disarm_canaries();
    let Some(st) = st else {
        return Outcome::Fail("spawn");
    };
    let (hits, bad, misplaced) = (
        idt_testing::hits(),
        idt_testing::bad(),
        idt_testing::misplaced(),
    );
    if hits != CANARY_IPIS || bad != 0 || misplaced != 0 || st != 0 {
        return crate::fail_fmt!(
            "hits {hits} bad {bad} misplaced {misplaced} status {st:#x}, want {CANARY_IPIS} 0 0 0"
        );
    }
    Outcome::Ok
}

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

const _: () = {
    // The program's literals are the hook's canaries.
    assert!(idt_testing::GPR_CANARIES[0] == 0xC0DE_0000_0000_0101);
    assert!(idt_testing::GPR_CANARIES[14] == 0xC0DE_0000_0000_0F0F);
};

// Set RFLAGS.TF with popf: the single-step trap follows the nop.
user_code!(
    SINGLE_STEP,
    "
    pushfq
    or qword ptr [rsp], 0x100
    popfq
    nop
    nop
    xor edi, edi
    mov eax, 60
    syscall
    ud2
    "
);

// int1 (ICEBP).
user_code!(
    INT1,
    "
    .byte 0xf1
    xor edi, edi
    mov eax, 60
    syscall
    ud2
    "
);

/// Run `code` and expect `SIGTRAP` in its `wait4` status, the kernel up.
fn expect_sigtrap(code: &'static [u8], name: &str) -> Outcome {
    match user::run(&Image::Code(code, DEFAULT), &[name]) {
        Ok(st) if st == wait_signaled(SIGTRAP) => Outcome::Ok,
        Ok(st) => crate::fail_fmt!("status {st:#x}, want SIGTRAP"),
        Err(e) => crate::fail_fmt!("spawn: {}", e.as_str()),
    }
}

/// A user `popf` that sets TF ends in `SIGTRAP`, not a halt.
pub(super) fn test_user_single_step() -> Outcome {
    expect_sigtrap(SINGLE_STEP, "single_step")
}

/// A user `int1` ends in `SIGTRAP`, not a halt. TCG (QEMU 8.2) does not
/// deliver `int1` as `#DB`, and the child dies of `#UD`'s `SIGILL`
/// instead, so off KVM either signal passes, with the kernel up.
pub(super) fn test_user_int1() -> Outcome {
    if super::p10_s16::on_kvm() {
        return expect_sigtrap(INT1, "int1");
    }
    match user::run(&Image::Code(INT1, DEFAULT), &["int1"]) {
        Ok(st) if st == wait_signaled(SIGTRAP) => Outcome::Ok,
        Ok(st) if st == wait_signaled(SIGILL) => {
            crate::marker!(
                "vibeOS: ktest:   user_int1: SIGILL off KVM (TCG takes int1 as an invalid opcode)"
            );
            Outcome::Ok
        }
        Ok(st) => crate::fail_fmt!("status {st:#x}, want SIGTRAP (or SIGILL off KVM)"),
        Err(e) => crate::fail_fmt!("spawn: {}", e.as_str()),
    }
}

// Set TF; the #DB after the nop re-pins the thread and clears TF. Then 100
// getpid calls with rdi = HOOK_MAGIC, and exit(0). The instruction after
// the popf is a nop, never a syscall.
user_code!(
    TF_REPIN,
    "
    pushfq
    or qword ptr [rsp], 0x100
    popfq
    nop
    nop
    mov r12d, 100
1:
    movabs rdi, 0x5EEDCA1100005A21
    mov eax, 39
    syscall
    dec r12d
    jnz 1b
    xor edi, edi
    mov eax, 60
    syscall
    ud2
    "
);

static REPIN_CHILD: AtomicU64 = AtomicU64::new(0);
static REPIN_SPAWNED: AtomicBool = AtomicBool::new(false);
static REPIN_STOP: AtomicBool = AtomicBool::new(false);

/// Pinned to CPU 0, so the child is too (`thread_init::spawn_user`).
fn repin_spawner() {
    let pid = match user::spawn(&Image::Code(TF_REPIN, DEFAULT), &["tf_repin"]) {
        Ok(pid) => u64::from(pid),
        Err(_) => u64::MAX,
    };
    REPIN_CHILD.store(pid, Ordering::Relaxed);
    REPIN_SPAWNED.store(true, Ordering::Release);
}

/// Pinned to CPU 0: the thread the `#DB` body's yield switches to.
fn repin_yielder() {
    while !REPIN_STOP.load(Ordering::Acquire) {
        thread_init::yield_now();
    }
}

/// A CPL-3 `#DB` body that yields moves to another CPU; the program then
/// runs there with `PerCpu` matching the CPU at each of 100 `getpid`s, and
/// exits 0: the moved frame returned through the common exit.
pub(super) fn test_user_tf_repin() -> Outcome {
    if super::second_cpu().is_none() {
        return Outcome::Skip("needs 2 CPUs");
    }
    let base = DEFAULT.vaddr;
    REPIN_SPAWNED.store(false, Ordering::Release);
    REPIN_STOP.store(false, Ordering::Release);
    idt_testing::arm_db_repin(base, base + 0x1000);
    proc_testing::arm_getpid_apic();
    super::spawn_thread_on("repin_yielder", repin_yielder, 0);
    super::spawn_thread_on("repin_spawner", repin_spawner, 0);
    let spawned = sleep_until(|| REPIN_SPAWNED.load(Ordering::Acquire), 5_000);
    let pid = u32::try_from(REPIN_CHILD.load(Ordering::Relaxed)).ok();
    let st = match (spawned, pid) {
        (true, Some(pid)) => Some(user::wait(pid)),
        _ => None,
    };
    REPIN_STOP.store(true, Ordering::Release);
    idt_testing::disarm_db_repin();
    proc_testing::disarm();
    let Some(st) = st else {
        return Outcome::Fail("spawn");
    };
    let (from, to) = idt_testing::repin_cpus();
    let (checks, bad) = (proc_testing::apic_checks(), proc_testing::apic_mismatches());
    if st != 0 || from == u32::MAX || from == to || checks != 100 || bad != 0 {
        return crate::fail_fmt!(
            "status {st:#x}, cpus {from} -> {to}, {checks} apic checks, {bad} mismatches; \
             want 0, two CPUs, 100, 0"
        );
    }
    Outcome::Ok
}
