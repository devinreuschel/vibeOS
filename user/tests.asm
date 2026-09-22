; Userspace syscall / EFAULT / fork+exec+wait / fault-kill runner.

BITS 64
ORG 0x40000000
%include "sys.inc"

_start:
    ; write fd1
    mov eax, SYS_WRITE
    mov edi, 1
    lea rsi, [rel banner]
    mov edx, banner_len
    syscall
    cmp rax, banner_len
    jne fail

    ; getpid != 0
    mov eax, SYS_GETPID
    syscall
    test rax, rax
    jz fail

    ; bad fd
    mov eax, SYS_WRITE
    mov edi, 3
    lea rsi, [rel banner]
    mov edx, 1
    syscall
    cmp rax, -EBADF
    jne fail

    ; EFAULT: null
    mov eax, SYS_WRITE
    mov edi, 1
    xor rsi, rsi
    mov edx, 8
    syscall
    cmp rax, -EFAULT
    jne fail

    ; EFAULT: kernel ptr
    mov eax, SYS_WRITE
    mov edi, 1
    mov rsi, 0xFFFF800000001000
    mov edx, 8
    syscall
    cmp rax, -EFAULT
    jne fail

    ; EFAULT: unmapped user
    mov eax, SYS_WRITE
    mov edi, 1
    mov rsi, 0x0000000070000000
    mov edx, 8
    syscall
    cmp rax, -EFAULT
    jne fail

    ; EFAULT: huge len
    mov eax, SYS_WRITE
    mov edi, 1
    lea rsi, [rel banner]
    mov rdx, 1
    shl rdx, 63
    syscall
    cmp rax, -EFAULT
    jne fail

    ; dup(1) then write
    mov eax, SYS_DUP
    mov edi, 1
    syscall
    cmp rax, 3
    jne fail
    mov r12, rax
    mov eax, SYS_WRITE
    mov rdi, r12
    lea rsi, [rel dupmsg]
    mov edx, dupmsg_len
    syscall
    cmp rax, dupmsg_len
    jne fail
    mov eax, SYS_CLOSE
    mov rdi, r12
    syscall

    ; fork + exit 7 + wait
    mov eax, SYS_FORK
    syscall
    test rax, rax
    js fail
    jnz .wait7
    mov edi, 7
    mov eax, SYS_EXIT
    syscall
.wait7:
    mov rdi, rax
    lea rsi, [rel st]
    xor rdx, rdx
    xor r10, r10
    mov eax, SYS_WAIT4
    syscall
    cmp dword [st], 0x0700
    jne fail

    ; fork + exec /hello + wait 42
    mov eax, SYS_FORK
    syscall
    test rax, rax
    js fail
    jnz .waith
    lea rdi, [rel hello_path]
    lea rsi, [rel hello_argv]
    xor rdx, rdx
    mov eax, SYS_EXECVE
    syscall
    mov edi, 127
    mov eax, SYS_EXIT
    syscall
.waith:
    mov rdi, rax
    lea rsi, [rel st]
    xor rdx, rdx
    xor r10, r10
    mov eax, SYS_WAIT4
    syscall
    cmp dword [st], 0x2A00
    jne fail

    ; fork + null deref → SIGSEGV
    mov eax, SYS_FORK
    syscall
    test rax, rax
    js fail
    jnz .waitf
    xor eax, eax
    mov rax, [rax]
    ud2
.waitf:
    mov rdi, rax
    lea rsi, [rel st]
    xor rdx, rdx
    xor r10, r10
    mov eax, SYS_WAIT4
    syscall
    mov eax, [st]
    and eax, 0x7f
    cmp eax, SIGSEGV
    jne fail

    ; bounded fork bomb
    xor r13, r13
.bomb:
    mov eax, SYS_FORK
    syscall
    test rax, rax
    js .bomb_done
    jnz .parentb
    xor edi, edi
    mov eax, SYS_EXIT
    syscall
.parentb:
    inc r13
    cmp r13, 32
    jb .bomb
.bomb_done:
    cmp rax, -EAGAIN
    je .reapb
    test r13, r13
    jz fail
.reapb:
    mov rdi, -1
    xor rsi, rsi
    xor rdx, rdx
    xor r10, r10
    mov eax, SYS_WAIT4
    syscall
    cmp rax, -ECHILD
    jne .reapb

    mov eax, SYS_WRITE
    mov edi, 1
    lea rsi, [rel ok]
    mov edx, ok_len
    syscall
    xor edi, edi
    mov eax, SYS_EXIT
    syscall

fail:
    mov eax, SYS_WRITE
    mov edi, 1
    lea rsi, [rel bad]
    mov edx, bad_len
    syscall
    mov edi, 1
    mov eax, SYS_EXIT
    syscall

banner:     db "user: tests begin", 10
banner_len  equ $ - banner
dupmsg:     db "user: dup ok", 10
dupmsg_len  equ $ - dupmsg
ok:         db "user: tests ok", 10
ok_len      equ $ - ok
bad:        db "user: tests fail", 10
bad_len     equ $ - bad
hello_path: db "/hello", 0
hello_arg0: db "/hello", 0
hello_argv: dq hello_arg0, 0
st:         dd 0
