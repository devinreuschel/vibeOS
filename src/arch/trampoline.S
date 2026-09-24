/* AP trampoline. DESIGN §7.3 / ROADMAP §4.4.
 *
 * SIPI vector 0x08 → physical 0x8000. Real → protected → long mode.
 * EFER.LME and EFER.NXE: kernel PTEs are NX; without NXE those bits are
 * reserved and the first kernel stack/data touch #PFs.
 *
 * Linked into `.trampoline` and copied to 0x8000. Addresses in the blob
 * are physical (BASE), not the kernel VMA. `.org` pins the layout, which
 * `global_asm!` assembles (arch/trampoline.rs), so far jumps and the
 * GDT pointer stay byte-stable.
 *
 * Param block (BSP write_volatile, not in this blob):
 *   0xD0 CR3, 0xD8 stack top, 0xE0 entry, 0xE8 IDTR (10 bytes).
 * Do not lidt here: GS is still 0. ap_entry loads GDT, then GS, then IDT.
 */

.section .trampoline, "a"

.set BASE, 0x8000
.set PM32, BASE + 0x26
.set LM64, BASE + 0x63
.set GDT, BASE + 0x90
.set GDT_PTR, BASE + 0xB8
.set CR3_OFF, BASE + 0xD0
.set RSP_OFF, BASE + 0xD8
.set RIP_OFF, BASE + 0xE0

.code16
    cli
    cld
    xor ax, ax
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
    ljmp 0x18, PM32

.org 0x26
.code32
pm32:
    mov ax, 0x20
    /* sreg from eax, not ax: a 66 prefix here would miss the nasm blob. */
    mov ds, eax
    mov es, eax
    mov ss, eax
    /* PAE + PGE. GLOBAL kernel mappings match the BSP tables. */
    mov eax, cr4
    or eax, 0xA0
    mov cr4, eax
    mov eax, [CR3_OFF]
    mov cr3, eax
    mov ecx, 0xC0000080
    rdmsr
    or eax, 0x900
    wrmsr
    mov eax, cr0
    or eax, 0x80010000
    mov cr0, eax
    ljmp 0x08, LM64

.org 0x63
.code64
lm64:
    mov ax, 0x10
    mov ds, eax
    mov es, eax
    mov ss, eax
    mov fs, eax
    mov gs, eax
    /* Identity-mapped physical slots. LLVM emits SIB abs, not RIP-rel. */
    mov rsp, qword ptr [RSP_OFF]
    xor rbp, rbp
    mov rax, qword ptr [RIP_OFF]
    call rax
.hang:
    cli
    hlt
    jmp .hang

.org 0x90, 0x90
gdt:
    .quad 0
    .quad 0x00AF9A000000FFFF           /* 0x08 64-bit code (KERNEL_CS) */
    .quad 0x00CF92000000FFFF           /* 0x10 data (KERNEL_DS) */
    .quad 0x00CF9A000000FFFF           /* 0x18 32-bit code */
    .quad 0x00CF92000000FFFF           /* 0x20 32-bit data */

.org 0xB8
gdt_ptr:
    .word 0x27
    .long GDT

.org 0xD0
