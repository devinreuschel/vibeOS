; Interactive user shell. Prints the existing boot marker, then vibeos>.
; echo is enough for the e2e COM1 / PS/2 contract. ps uses SYS_PSINFO.

BITS 64
ORG 0x40000000
%include "sys.inc"

_start:
    mov eax, SYS_WRITE
    mov edi, 1
    lea rsi, [rel ready]
    mov edx, ready_len
    syscall

.loop:
    mov eax, SYS_WRITE
    mov edi, 1
    lea rsi, [rel prompt]
    mov edx, prompt_len
    syscall

    lea rbx, [rel line]
    xor r12, r12
.read:
    mov eax, SYS_READ
    xor edi, edi
    lea rsi, [rel keyb]
    mov edx, 1
    syscall
    cmp rax, 1
    jl .loop
    mov al, [keyb]
    cmp al, 13
    je .submit
    cmp al, 10
    je .submit
    cmp al, 8
    je .bs
    cmp al, 127
    je .bs
    cmp r12, 127
    jae .read
    mov [rbx + r12], al
    inc r12
    mov eax, SYS_WRITE
    mov edi, 1
    lea rsi, [rel keyb]
    mov edx, 1
    syscall
    jmp .read

.bs:
    test r12, r12
    jz .read
    dec r12
    mov eax, SYS_WRITE
    mov edi, 1
    lea rsi, [rel bseq]
    mov edx, 3
    syscall
    jmp .read

.submit:
    mov byte [rbx + r12], 0
    mov eax, SYS_WRITE
    mov edi, 1
    lea rsi, [rel nl]
    mov edx, 1
    syscall
    test r12, r12
    jz .loop
    call skip_sp
    lea rdi, [rel echo_s]
    mov ecx, 4
    call starts
    test rax, rax
    jnz .do_echo
    lea rdi, [rel ps_s]
    mov ecx, 2
    call starts
    test rax, rax
    jnz .do_ps
    jmp .loop

.do_echo:
.echo_sp:
    cmp byte [rsi], 32
    jne .echo_out
    inc rsi
    jmp .echo_sp
.echo_out:
    mov rdx, rsi
    lea rcx, [rel line]
    add rcx, r12
    sub rcx, rdx
    mov rdx, rcx
    mov eax, SYS_WRITE
    mov edi, 1
    syscall
    mov eax, SYS_WRITE
    mov edi, 1
    lea rsi, [rel nl]
    mov edx, 1
    syscall
    jmp .loop

.do_ps:
    mov eax, SYS_PSINFO
    lea rdi, [rel pbuf]
    mov esi, 512
    syscall
    test rax, rax
    jle .loop
    mov rdx, rax
    mov eax, SYS_WRITE
    mov edi, 1
    lea rsi, [rel pbuf]
    syscall
    jmp .loop

; rsi = line start after skip_sp. rdi = prefix, ecx = prefix len.
; returns rax=1 match, rsi unchanged? we use rsi as line ptr.
skip_sp:
    lea rsi, [rel line]
.ss:
    cmp byte [rsi], 32
    jne .ss_done
    inc rsi
    jmp .ss
.ss_done:
    ret

starts:
    push rsi
    push rcx
.cmp:
    test ecx, ecx
    jz .ok
    mov al, [rsi]
    cmp al, [rdi]
    jne .no
    inc rsi
    inc rdi
    dec ecx
    jmp .cmp
.ok:
    pop rcx
    pop rdx
    mov rax, 1
    ret
.no:
    pop rcx
    pop rsi
    xor eax, eax
    ret

ready:      db "vibeOS: shell ready", 10
ready_len   equ $ - ready
prompt:     db "vibeos> "
prompt_len  equ $ - prompt
echo_s:     db "echo"
ps_s:       db "ps"
nl:         db 10
bseq:       db 8, 32, 8
keyb:       db 0
line:       times 128 db 0
pbuf:       times 512 db 0
