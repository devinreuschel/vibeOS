; /sbin/init: run tests, then the interactive shell, then reap. ROADMAP §9.8.

BITS 64
ORG 0x40000000
%include "sys.inc"

_start:
    mov eax, SYS_FORK
    syscall
    test rax, rax
    js .shell
    jnz .wait_tests
    lea rdi, [rel tests_path]
    lea rsi, [rel tests_argv]
    xor rdx, rdx
    mov eax, SYS_EXECVE
    syscall
    mov edi, 127
    mov eax, SYS_EXIT
    syscall

.wait_tests:
    mov rdi, rax
    xor rsi, rsi
    xor rdx, rdx
    xor r10, r10
    mov eax, SYS_WAIT4
    syscall

.shell:
    mov eax, SYS_FORK
    syscall
    test rax, rax
    js .reap
    jnz .reap
    lea rdi, [rel sh_path]
    lea rsi, [rel sh_argv]
    xor rdx, rdx
    mov eax, SYS_EXECVE
    syscall
    mov edi, 127
    mov eax, SYS_EXIT
    syscall

.reap:
    mov rdi, -1
    xor rsi, rsi
    xor rdx, rdx
    xor r10, r10
    mov eax, SYS_WAIT4
    syscall
    jmp .reap

tests_path: db "/bin/tests", 0
tests_arg0: db "/bin/tests", 0
tests_argv: dq tests_arg0, 0
sh_path:    db "/bin/sh", 0
sh_arg0:    db "/bin/sh", 0
sh_argv:    dq sh_arg0, 0
