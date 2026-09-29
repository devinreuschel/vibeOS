/* A static x86_64 ELF with no crt and no libraries, for the host tests of
 * vibeos-core's ELF parser (ROADMAP §10.5). gen.sh links it twice, once
 * with ld.lld and once with GNU ld, and the checked-in outputs are
 * static-lld and static-gnuld.
 *
 * It holds one section of each kind the loader maps: code, read-only
 * data, initialised data, a bss larger than one page, and a TLS block with
 * both an init image (.tdata) and a zero tail (.tbss). It reads a word of
 * each, stores into the bss, and exits with status 0 through exit_group.
 */
    .intel_syntax noprefix

    .section .text, "ax", @progbits
    .globl _start
    .type _start, @function
_start:
    lea rsi, [rip + message]
    mov eax, dword ptr [rip + counter]
    add eax, dword ptr [rsi]
    mov dword ptr [rip + scratch + 8192], eax
    mov rax, qword ptr fs:[tls_word@tpoff]
    xor edi, edi
    mov eax, 231
    syscall
    ud2
    .size _start, . - _start

    .section .rodata, "a", @progbits
    .p2align 3
message:
    .ascii "vibeOS static fixture\0"

    .section .data, "aw", @progbits
    .p2align 3
counter:
    .long 7
    .long 0

    .section .bss, "aw", @nobits
    .p2align 12
scratch:
    .zero 3 * 4096

    .section .tdata, "awT", @progbits
    .p2align 3
tls_word:
    .quad 0x5eed

    .section .tbss, "awT", @nobits
    .p2align 3
tls_zero:
    .zero 64

    .section .note.GNU-stack, "", @progbits
