//! In-guest tests of P10-S21, One user frame and ring-3 traps to signals (DESIGN §8.2).

use super::user::{self, DEFAULT, Image, Layout, user_code};
use super::{Outcome, Test, test};
use crate::proc_init::testing as proc_testing;

pub(super) const TESTS: &[Test] = &[
    test("syscall_rcx_canary", test_syscall_rcx_canary).deadline(30_000),
    test("fork_child_gprs", test_fork_child_gprs).deadline(30_000),
];

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
fn test_syscall_rcx_canary() -> Outcome {
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
fn test_fork_child_gprs() -> Outcome {
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
