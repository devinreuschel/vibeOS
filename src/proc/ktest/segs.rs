//! In-guest tests for ring 3's segment selectors (DESIGN §5.1, §7.5): Linux's
//! CS and SS, the null DS, ES, FS and GS, and the per-thread four across
//! `fork` and the context switch. Rows: the parent `ktest.rs`'s `TESTS`.

use vibeos::proc::wait_exited;

use crate::ktest::Outcome;
use crate::ktest::user::{self, DEFAULT, Image, x86_user_code};

// CS 0x33, SS 0x2b, and DS, ES, FS and GS 0, at the first instruction and
// again after a `getpid` (the `sysretq` path): exit 0, else the number of
// the first check that failed (1 to 6), plus 10 after the `getpid`.
x86_user_code!(
    USER_SELECTORS,
    "
    xor r12d, r12d
2:
    mov edi, 1
    mov ax, cs
    cmp ax, 0x33
    jne 9f
    mov edi, 2
    mov ax, ss
    cmp ax, 0x2b
    jne 9f
    mov edi, 3
    mov ax, ds
    test ax, ax
    jnz 9f
    mov edi, 4
    mov ax, es
    test ax, ax
    jnz 9f
    mov edi, 5
    mov ax, fs
    test ax, ax
    jnz 9f
    mov edi, 6
    mov ax, gs
    test ax, ax
    jnz 9f
    cmp r12d, 10
    je 3f
    mov r12d, 10
    mov eax, 39
    syscall
    jmp 2b
3:
    xor edi, edi
    jmp 8f
9:
    add edi, r12d
8:
    mov eax, 60
    syscall
    ud2
    "
);

/// A process the kernel starts runs with CS `0x33`, SS `0x2b` and null
/// DS, ES, FS and GS, as a Linux process does.
pub(crate) fn test_user_selectors() -> Outcome {
    match user::run(&Image::Code(USER_SELECTORS, DEFAULT), &["user_selectors"]) {
        Ok(st) if st == wait_exited(0) => Outcome::Ok,
        Ok(st) => crate::fail_fmt!("status {st:#x}: exit 1-6 CS SS DS ES FS GS, +10 after sysret"),
        Err(e) => crate::fail_fmt!("spawn: {}", e.as_str()),
    }
}

// Load 0x2b into DS and fork. Child: exit 0 if its DS is 0x2b, else 1.
// Parent: exit 3 if its own DS changed, else wait4 the child and exit with
// its exit code (9 if it did not exit).
x86_user_code!(
    USER_DS_FORK,
    "
    mov eax, 0x2b
    mov ds, ax
    mov eax, 57
    syscall
    test rax, rax
    jnz 2f
    mov ax, ds
    xor edi, edi
    cmp ax, 0x2b
    setne dil
    mov eax, 60
    syscall
    ud2
2:
    mov ebx, eax
    mov cx, ds
    mov edi, 3
    cmp cx, 0x2b
    jne 8f
    sub rsp, 16
    mov dword ptr [rsp], 0xffff
    mov edi, ebx
    mov rsi, rsp
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    mov eax, dword ptr [rsp]
    mov edi, 9
    test al, 0x7f
    jnz 8f
    movzx edi, ah
8:
    mov eax, 60
    syscall
    ud2
    "
);

/// The child of a parent that loaded `0x2b` into DS reads `0x2b`
/// (`fork` copies the parent's DS, ES, FS and GS).
pub(crate) fn test_user_ds_fork() -> Outcome {
    match user::run(&Image::Code(USER_DS_FORK, DEFAULT), &["user_ds_fork"]) {
        Ok(st) if st == wait_exited(0) => Outcome::Ok,
        Ok(st) => {
            crate::fail_fmt!("status {st:#x}: exit 1 child DS, 3 parent DS, 9 child not exited")
        }
        Err(e) => crate::fail_fmt!("spawn: {}", e.as_str()),
    }
}

// 1,000 rounds of sched_yield, each followed by a check that DS still holds
// the value this process loaded (0x2b in `USER_DS_SWITCH_2B`, 0 in
// `USER_DS_SWITCH_0`): exit 0, or 1 on the first mismatch. RBX counts and
// R12 holds the value: `syscall` preserves both.
x86_user_code!(
    USER_DS_SWITCH_2B,
    "
    mov r12d, 0x2b
    mov ds, r12w
    mov ebx, 1000
2:
    mov eax, 24
    syscall
    mov ax, ds
    cmp ax, r12w
    jne 9f
    dec ebx
    jnz 2b
    xor edi, edi
    mov eax, 60
    syscall
    ud2
9:
    mov edi, 1
    mov eax, 60
    syscall
    ud2
    "
);

x86_user_code!(
    USER_DS_SWITCH_0,
    "
    xor r12d, r12d
    mov ds, r12w
    mov ebx, 1000
2:
    mov eax, 24
    syscall
    mov ax, ds
    cmp ax, r12w
    jne 9f
    dec ebx
    jnz 2b
    xor edi, edi
    mov eax, 60
    syscall
    ud2
9:
    mov edi, 1
    mov eax, 60
    syscall
    ud2
    "
);

/// Two processes on this CPU (`spawn_user` pins each to its creator's),
/// one with `0x2b` in DS and one with 0, yield to each other 1,000 times,
/// and each reads back its own DS every time.
pub(crate) fn test_user_ds_switch() -> Outcome {
    let a = user::spawn(&Image::Code(USER_DS_SWITCH_2B, DEFAULT), &["ds_2b"]);
    let b = user::spawn(&Image::Code(USER_DS_SWITCH_0, DEFAULT), &["ds_0"]);
    let sa = a.as_ref().map(|&pid| user::wait(pid));
    let sb = b.as_ref().map(|&pid| user::wait(pid));
    match (sa, sb) {
        (Ok(sa), Ok(sb)) if sa == wait_exited(0) && sb == wait_exited(0) => Outcome::Ok,
        (Ok(sa), Ok(sb)) => crate::fail_fmt!("status 0x2b: {sa:#x}, 0: {sb:#x}, want 0 and 0"),
        (Err(e), _) | (_, Err(e)) => crate::fail_fmt!("spawn: {}", e.as_str()),
    }
}
