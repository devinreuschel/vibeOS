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

; Wait for /bin/tests with a status pointer: a zeroed slot on the stack,
; since the image is mapped R+X. A nonzero status word (an exit code other
; than 0, or a signal) prints `init: /bin/tests exited <status>` on fd 2,
; the status in decimal, built on the stack and written in one call so the
; line stays whole. /bin/sh starts either way.
.wait_tests:
    mov rdi, rax
    sub rsp, 16
    mov qword [rsp], 0
    mov rsi, rsp
    xor rdx, rdx
    xor r10, r10
    mov eax, SYS_WAIT4
    syscall
    mov eax, dword [rsp]
    test eax, eax
    jz .tests_done
    sub rsp, 64
    mov rdi, rsp
    lea rsi, [rel exited_msg]
    mov ecx, exited_len
    rep movsb
    lea r8, [rsp + 64]          ; digits go backwards from here
    mov r9d, 10
.digit:
    xor edx, edx
    div r9d
    add dl, '0'
    dec r8
    mov [r8], dl
    test eax, eax
    jnz .digit
    mov rsi, r8
    lea rcx, [rsp + 64]
    sub rcx, r8
    rep movsb                   ; the digits after the message
    mov byte [rdi], 10
    inc rdi
    mov rdx, rdi
    sub rdx, rsp
    mov rsi, rsp
    mov edi, 2
    mov eax, SYS_WRITE
    syscall
    add rsp, 64
.tests_done:
    add rsp, 16

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
exited_msg: db "init: /bin/tests exited "
exited_len  equ $ - exited_msg
