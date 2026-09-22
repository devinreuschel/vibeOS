; Freestanding ring-3 hello. nasm -f bin. ORG matches PT_LOAD vaddr.
; ROADMAP §9.8. Syscalls: write(1), getpid, exit(42).

BITS 64
ORG 0x40000000

_start:
    mov eax, 1              ; write
    mov edi, 1              ; fd 1 (early console sink)
    lea rsi, [rel msg]
    mov edx, msg_len
    syscall

    mov eax, 39             ; getpid
    syscall

    mov eax, 60             ; exit
    mov edi, 42
    syscall
    ud2

msg:
    db "hello from ring3", 10
msg_len equ $ - msg
