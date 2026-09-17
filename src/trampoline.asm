; AP trampoline. DESIGN §7.3 / ROADMAP §4.4.
;
; SIPI vector 0x08 → physical 0x8000. Real → protected → long mode.
; EFER.LME and EFER.NXE: kernel PTEs are NX; without NXE those bits are
; reserved and the first kernel stack/data touch #PFs.
;
; Param block (BSP write_volatile, not in this blob):
;   0xD0 CR3, 0xD8 stack top, 0xE0 entry, 0xE8 IDTR (10 bytes).
; Do not lidt here: GS is still 0. ap_entry loads GDT, then GS, then IDT.
; `times` below is the static "blob fits below param block" check.

bits 16
org 0x8000

CR3_OFF  equ 0x8000 + 0xD0
RSP_OFF  equ 0x8000 + 0xD8
RIP_OFF  equ 0x8000 + 0xE0
IDT_OFF  equ 0x8000 + 0xE8

start:
    cli
    cld
    xor ax, ax
    mov ds, ax
    mov es, ax
    mov ss, ax
    lgdt [gdt_ptr]
    mov eax, cr0
    or eax, 1
    mov cr0, eax
    jmp 0x18:pm32

bits 32
pm32:
    mov ax, 0x20
    mov ds, ax
    mov es, ax
    mov ss, ax
    ; PAE + PGE. GLOBAL kernel mappings match the BSP tables.
    mov eax, cr4
    or eax, (1 << 5) | (1 << 7)
    mov cr4, eax
    mov eax, [CR3_OFF]
    mov cr3, eax
    mov ecx, 0xC0000080
    rdmsr
    or eax, (1 << 8) | (1 << 11)    ; LME | NXE
    wrmsr
    mov eax, cr0
    or eax, (1 << 31) | (1 << 16)   ; PG | WP
    mov cr0, eax
    jmp 0x08:lm64

bits 64
lm64:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov fs, ax
    mov gs, ax
    ; Numeric addrs are RIP-relative under some nasm defaults. These are
    ; identity-mapped physical slots, so force abs.
    mov rsp, [abs RSP_OFF]
    xor rbp, rbp
    mov rax, [abs RIP_OFF]
    call rax
.hang:
    cli
    hlt
    jmp .hang

align 16
gdt:
    dq 0
    dq 0x00AF9A000000FFFF           ; 0x08 64-bit code (KERNEL_CS)
    dq 0x00CF92000000FFFF           ; 0x10 data (KERNEL_DS)
    dq 0x00CF9A000000FFFF           ; 0x18 32-bit code
    dq 0x00CF92000000FFFF           ; 0x20 32-bit data
gdt_end:

gdt_ptr:
    dw gdt_end - gdt - 1
    dd gdt

times 0xD0 - ($ - $$) db 0
