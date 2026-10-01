/* AP trampoline. DESIGN §7.3 / ROADMAP §4.4, §10.6.
 *
 * SIPI vector N starts the AP in real mode at CS:IP = N<<8 : 0, the page
 * `boot::capture` chose (`BootInfo.trampoline_page`). Real → protected →
 * long mode. EFER.LME and EFER.NXE: kernel PTEs are NX; without NXE those
 * bits are reserved and the first kernel stack/data touch #PFs.
 *
 * Linked into `.trampoline` and copied to the chosen page, which may be
 * any page from 0x1000 to 0x9F000, so the blob is position-independent:
 * real mode sets DS = ES = SS = CS and uses offsets from the blob's start,
 * long mode loads the param block RIP-relative, and every other absolute
 * operand is assembled as if the blob sat at 0 and patched by `smp_init`
 * (`vibeos::smp::patch_blob`) from the page's base. `.org` pins each
 * patched operand to its `vibeos::smp::PATCH_SITES` offset, and the
 * `vibeos_tramp_patch_*` labels mark them for `smp_init`'s boot check.
 * INIT leaves SP at 0, so nothing pushes before long mode.
 *
 * The GDT's code and data descriptors carry their accessed bit preset
 * (0x9B, 0x93): the page is mapped read-only, so the CPU must never write
 * it (ROADMAP §10.6).
 *
 * Param block (BSP write_volatile through the physmap, not in this blob):
 *   0xD0 CR3, 0xD8 stack top, 0xE0 entry.
 * Do not lidt here: GS is still 0. ap_entry loads GDT, then GS, then IDT.
 */

.section .trampoline, "a"

.set PM32, 0x30
.set LM64, 0x78
.set GDT, 0xA0
.set GDT_PTR, 0xC8
.set CR3_OFF, 0xD0
.set RSP_OFF, 0xD8
.set RIP_OFF, 0xE0

vibeos_tramp_start:
.code16
    cli
    cld
    mov ax, cs
    mov ds, ax
    mov es, ax
    mov ss, ax
    lgdt [GDT_PTR]
    /* INIT leaves CR0 = 0x60000010 (CD|NW|ET). OR PE only keeps caches
       off. SDM: clear CD|NW, then WBINVD, so APs match the BSP. */
    mov eax, cr0
    or eax, 1
    and eax, 0x9FFFFFFF
    mov cr0, eax
    wbinvd

    /* ljmp 0x18:PM32 with a 32-bit offset (66 EA imm32 imm16): a 16-bit
       offset cannot reach a page above 64 KiB. */
.org 0x28, 0x90
    .byte 0x66, 0xEA
.global vibeos_tramp_patch_pm32
vibeos_tramp_patch_pm32:
    .long PM32
    .word 0x18

.org PM32, 0x90
.code32
    mov ax, 0x20
    /* sreg from eax, not ax: a 66 prefix here would miss the nasm blob. */
    mov ds, eax
    mov es, eax
    mov ss, eax
    /* mov eax, [CR3_OFF] (A1 moffs32). */
.org 0x3C, 0x90
    .byte 0xA1
.global vibeos_tramp_patch_cr3
vibeos_tramp_patch_cr3:
    .long CR3_OFF
    mov cr3, eax
    /* PAE + PGE for long mode; arch::cpu::init_control_regs rewrites CR4 whole once the AP runs Rust. */
    mov eax, cr4
    or eax, 0xA0
    mov cr4, eax
    mov ecx, 0xC0000080
    rdmsr
    or eax, 0x900
    wrmsr
    mov eax, cr0
    or eax, 0x80010000
    mov cr0, eax
    /* ljmp 0x08:LM64 (EA imm32 imm16). */
.org 0x6C, 0x90
    .byte 0xEA
.global vibeos_tramp_patch_lm64
vibeos_tramp_patch_lm64:
    .long LM64
    .word 0x08

.org LM64, 0x90
.code64
    mov ax, 0x10
    mov ds, eax
    mov es, eax
    mov ss, eax
    mov fs, eax
    mov gs, eax
    /* The param block, RIP-relative: upper GPR halves are undefined here. */
    mov rsp, qword ptr [rip + vibeos_tramp_start + RSP_OFF]
    xor ebp, ebp
    mov rax, qword ptr [rip + vibeos_tramp_start + RIP_OFF]
    call rax
.hang:
    cli
    hlt
    jmp .hang

.org GDT, 0x90
    .quad 0
    .quad 0x00AF9B000000FFFF           /* 0x08 64-bit code (KERNEL_CS) */
    .quad 0x00CF93000000FFFF           /* 0x10 data (KERNEL_DS) */
    .quad 0x00CF9B000000FFFF           /* 0x18 32-bit code */
    .quad 0x00CF93000000FFFF           /* 0x20 32-bit data */

.org GDT_PTR
    .word 0x27
.global vibeos_tramp_patch_gdt
vibeos_tramp_patch_gdt:
    .long GDT
