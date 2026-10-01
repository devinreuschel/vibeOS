//! The per-process syscall count (ROADMAP §10.7, F150), read back from
//! ring 3 through psinfo (syscall 500).

use vibeos::proc::wait_exited;

use crate::ktest::Outcome;
use crate::ktest::user::{self, DEFAULT, Image, user_code};

// getpid; psinfo into a 512-byte stack buffer and parse this process's
// line's last field as c1, which is 2 (getpid and psinfo: a new thread
// starts at 0, and an entry counts before its body runs); 20 getpids;
// psinfo again for c2; exit with c2 - c1, or 255 if c1 is not 2 or the
// line is missing.
user_code!(
    SYSCALL_COUNT,
    "
    mov eax, 39
    syscall
    mov r12, rax
    sub rsp, 512
    mov r15, rsp
    call 8f
    mov r13, rax
    mov ebx, 20
1:
    mov eax, 39
    syscall
    dec ebx
    jnz 1b
    call 8f
    mov r14, rax
    mov edi, 255
    cmp r13, 2
    jne 2f
    cmp r14, -1
    je 2f
    mov rdi, r14
    sub rdi, r13
2:
    mov eax, 60
    syscall
    ud2

    // This process's count from psinfo (syscall 500) into [r15], 512 bytes:
    // the last field of the line whose pid is r12, or -1.
8:
    mov rdi, r15
    mov esi, 512
    mov eax, 500
    syscall
    test rax, rax
    jle 90f
    mov rcx, r15
    lea r9, [r15 + rax]
10:
    cmp rcx, r9
    jae 90f
    xor r8d, r8d
11:
    movzx edx, byte ptr [rcx]
    sub edx, 48
    cmp edx, 9
    ja 12f
    imul r8, r8, 10
    add r8, rdx
    inc rcx
    jmp 11b
12:
    mov r10, rcx
13:
    cmp r10, r9
    jae 90f
    cmp byte ptr [r10], 10
    je 14f
    inc r10
    jmp 13b
14:
    cmp r8, r12
    je 15f
    lea rcx, [r10 + 1]
    jmp 10b
15:
    mov r11, r10
16:
    dec r11
    cmp byte ptr [r11], 32
    jne 16b
    inc r11
    xor eax, eax
17:
    cmp r11, r10
    jae 18f
    movzx edx, byte ptr [r11]
    sub edx, 48
    cmp edx, 9
    ja 90f
    imul rax, rax, 10
    add rax, rdx
    inc r11
    jmp 17b
18:
    ret
90:
    mov rax, -1
    ret
    "
);

/// A process's psinfo count grows by the syscalls it made: 20 `getpid`s
/// and the second psinfo, 21 in all.
pub(crate) fn proc_syscall_count() -> Outcome {
    let st = match user::run(&Image::Code(SYSCALL_COUNT, DEFAULT), &["syscount"]) {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if st != wait_exited(21) {
        return crate::fail_fmt!("status {st:#x}, want exited 21 (255: first count not 2)");
    }
    Outcome::Ok
}
