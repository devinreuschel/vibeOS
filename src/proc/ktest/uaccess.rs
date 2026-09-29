//! In-guest tests of the syscall user accessors (ROADMAP §10.6, §9.2).

use vibeos::proc::{wexitstatus, wifexited};

use crate::ktest::Outcome;
use crate::ktest::user::{self, Image, Layout, user_code};

// Its page, then a zeroed page, then nothing: writes the first 300 bytes of
// its page to /tmp/uaccess_syscall_copies, reopens it read-only, and reads
// 256 bytes into the last 100 of its second page. That read must return
// 100 with the offset at 100 and the bytes the file's first 100. Then a
// forked child exits, `wait4` with the status pointer on the unmapped page
// must return -EFAULT, and the next `wait4` -ECHILD. Exits with the failing
// step's number, else 0.
user_code!(
    UACCESS_SHORT,
    "
    lea rbx, [rip]
    and rbx, -4096
    lea rdi, [rip + 90f]
    mov esi, 0x241
    mov edx, 0x1a4
    mov eax, 2
    syscall
    mov r12d, 1
    test rax, rax
    js 8f
    mov r13, rax
    mov rdi, r13
    mov rsi, rbx
    mov edx, 300
    mov eax, 1
    syscall
    mov r12d, 2
    cmp rax, 300
    jne 8f
    mov rdi, r13
    mov eax, 3
    syscall
    lea rdi, [rip + 90f]
    xor esi, esi
    xor edx, edx
    mov eax, 2
    syscall
    mov r12d, 3
    test rax, rax
    js 8f
    mov r13, rax
    mov rdi, r13
    lea rsi, [rbx + 0x1f9c]
    mov edx, 256
    xor eax, eax
    syscall
    mov r12d, 4
    cmp rax, 100
    jne 8f
    mov rdi, r13
    xor esi, esi
    mov edx, 1
    mov eax, 8
    syscall
    mov r12d, 5
    cmp rax, 100
    jne 8f
    mov rsi, rbx
    lea rdi, [rbx + 0x1f9c]
    mov ecx, 100
    repe cmpsb
    mov r12d, 6
    jne 8f
    mov eax, 57
    syscall
    mov r12d, 7
    test rax, rax
    js 8f
    jnz 3f
    xor edi, edi
    mov eax, 60
    syscall
    ud2
3:
    mov rdi, -1
    lea rsi, [rbx + 0x2000]
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    mov r12d, 8
    cmp rax, -14
    jne 8f
    mov rdi, -1
    xor esi, esi
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    mov r12d, 9
    cmp rax, -10
    jne 8f
    xor r12d, r12d
8:
    mov edi, r12d
    mov eax, 60
    syscall
    ud2
90:
    .asciz \"/tmp/uaccess_syscall_copies\"
    "
);

pub(crate) fn test_uaccess_syscall_copies() -> Outcome {
    let layout = Layout {
        vaddr: 0x4000_0000,
        memsz: Some(0x2000),
        writable: true,
    };
    let st = match user::run(&Image::Code(UACCESS_SHORT, layout), &["uaccess_short"]) {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if !wifexited(st) {
        return crate::fail_fmt!("status {st:#x}, want exited");
    }
    match wexitstatus(st) {
        0 => Outcome::Ok,
        1 => Outcome::Fail("open for write"),
        2 => Outcome::Fail("write 300"),
        3 => Outcome::Fail("open read-only"),
        4 => Outcome::Fail("short read did not return 100"),
        5 => Outcome::Fail("offset after short read is not 100"),
        6 => Outcome::Fail("short read copied the wrong bytes"),
        7 => Outcome::Fail("fork"),
        8 => Outcome::Fail("wait4 to an unmapped status did not return EFAULT"),
        9 => Outcome::Fail("second wait4 did not return ECHILD"),
        code => crate::fail_fmt!("exit {code}"),
    }
}

// Its page is read-only: writes its first 300 bytes to
// /tmp/uaccess_readonly_efault and reopens it read-only. Then `read` into
// its own text (offset left at 0), `psinfo` into it, `wait4`'s status into
// it after a forked child exits, and `read` into the kernel half must each
// return -EFAULT, and the file, read onto the stack, must still match the
// text. Exits with the failing step's number, else 0.
user_code!(
    UACCESS_RO,
    "
    lea rbx, [rip]
    and rbx, -4096
    sub rsp, 512
    lea rdi, [rip + 90f]
    mov esi, 0x241
    mov edx, 0x1a4
    mov eax, 2
    syscall
    mov r12d, 1
    test rax, rax
    js 8f
    mov r13, rax
    mov rdi, r13
    mov rsi, rbx
    mov edx, 300
    mov eax, 1
    syscall
    mov r12d, 2
    cmp rax, 300
    jne 8f
    mov rdi, r13
    mov eax, 3
    syscall
    lea rdi, [rip + 90f]
    xor esi, esi
    xor edx, edx
    mov eax, 2
    syscall
    mov r12d, 3
    test rax, rax
    js 8f
    mov r13, rax
    mov rdi, r13
    mov rsi, rbx
    mov edx, 16
    xor eax, eax
    syscall
    mov r12d, 4
    cmp rax, -14
    jne 8f
    mov rdi, r13
    xor esi, esi
    mov edx, 1
    mov eax, 8
    syscall
    mov r12d, 5
    test rax, rax
    jne 8f
    mov rdi, rbx
    mov esi, 64
    mov eax, 500
    syscall
    mov r12d, 6
    cmp rax, -14
    jne 8f
    mov eax, 57
    syscall
    mov r12d, 7
    test rax, rax
    js 8f
    jnz 3f
    xor edi, edi
    mov eax, 60
    syscall
    ud2
3:
    mov rdi, -1
    mov rsi, rbx
    xor edx, edx
    xor r10d, r10d
    mov eax, 61
    syscall
    mov r12d, 8
    cmp rax, -14
    jne 8f
    mov rdi, r13
    movabs rsi, 0xFFFF800000000000
    mov edx, 16
    xor eax, eax
    syscall
    mov r12d, 9
    cmp rax, -14
    jne 8f
    mov rdi, r13
    mov rsi, rsp
    mov edx, 256
    xor eax, eax
    syscall
    mov r12d, 10
    cmp rax, 256
    jne 8f
    mov rsi, rbx
    mov rdi, rsp
    mov ecx, 256
    repe cmpsb
    mov r12d, 11
    jne 8f
    xor r12d, r12d
8:
    mov edi, r12d
    mov eax, 60
    syscall
    ud2
90:
    .asciz \"/tmp/uaccess_readonly_efault\"
    "
);

pub(crate) fn test_uaccess_readonly_efault() -> Outcome {
    let img = Image::Code(UACCESS_RO, user::DEFAULT);
    let st = match user::run(&img, &["uaccess_ro"]) {
        Ok(st) => st,
        Err(e) => return crate::fail_fmt!("spawn: {}", e.as_str()),
    };
    if !wifexited(st) {
        return crate::fail_fmt!("status {st:#x}, want exited");
    }
    match wexitstatus(st) {
        0 => Outcome::Ok,
        1 => Outcome::Fail("open for write"),
        2 => Outcome::Fail("write 300"),
        3 => Outcome::Fail("open read-only"),
        4 => Outcome::Fail("read into own text did not return EFAULT"),
        5 => Outcome::Fail("failed read moved the offset"),
        6 => Outcome::Fail("psinfo into own text did not return EFAULT"),
        7 => Outcome::Fail("fork"),
        8 => Outcome::Fail("wait4 status into own text did not return EFAULT"),
        9 => Outcome::Fail("read into the kernel half did not return EFAULT"),
        10 => Outcome::Fail("read back to the stack"),
        11 => Outcome::Fail("own text changed"),
        code => crate::fail_fmt!("exit {code}"),
    }
}
