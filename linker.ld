/* vibeOS kernel link script.
 *
 * Higher-half at 0xFFFF_FFFF_8000_0000 to match the "kernel" code model.
 * Sections page-aligned so W^X per section is possible in phase 1.
 * .got sits inside the mapped range and before .bss, DESIGN §3.4.
 */

OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

KERNEL_VMA = 0xFFFFFFFF80000000;

PHDRS
{
    limine_requests PT_LOAD;
    text            PT_LOAD;
    rodata          PT_LOAD;
    data            PT_LOAD;
}

SECTIONS
{
    . = KERNEL_VMA;
    __kernel_vma_start = .;

    /* Limine scans this range for request statics. Keep it first so the
     * loader finds it without walking the whole image. */
    .limine_requests : ALIGN(4K) {
        __limine_requests_start = .;
        KEEP(*(.limine_requests_start))
        KEEP(*(.limine_requests))
        KEEP(*(.limine_requests_end))
        __limine_requests_end = .;
    } :limine_requests

    . = ALIGN(4K);
    __text_start = .;
    .text : ALIGN(4K) {
        *(.text .text.*)
    } :text
    __text_end = .;

    . = ALIGN(4K);
    __rodata_start = .;
    .rodata : ALIGN(4K) {
        *(.rodata .rodata.*)
    } :rodata

    /* The user accessors' exception-table records (INTERRUPTS §5.1): read
     * only, and kept whole because nothing references them by symbol. */
    __ex_table : ALIGN(4) {
        __ex_table_start = .;
        KEEP(*(__ex_table))
        __ex_table_end = .;
    } :rodata

    /* Inside __rodata_* so paging maps it (copied to 0x8000, not executed here). */
    .trampoline : ALIGN(16) {
        __trampoline_start = .;
        KEEP(*(.trampoline))
        __trampoline_end = .;
    } :rodata

    /* The ksyms table (scripts/gen_ksyms.py). Inside __rodata_* so paging
     * maps it; the kernel reads it only through these bounds, so the second
     * ksyms link leaves .text where the first put it (DESIGN §2.5). */
    .ksyms : ALIGN(8) {
        __ksyms_start = .;
        KEEP(*(.ksyms))
        __ksyms_end = .;
    } :rodata
    __rodata_end = .;

    . = ALIGN(4K);
    __data_start = .;
    .data : ALIGN(4K) {
        *(.data .data.*)
    } :data

    /* .got before .bss and inside the mapped image, DESIGN §9.1. */
    .got : {
        *(.got .got.*)
    } :data

    __bss_start = .;
    .bss : {
        *(COMMON)
        *(.bss .bss.*)
    } :data
    __bss_end = .;
    __data_end = .;

    . = ALIGN(4K);
    __kernel_vma_end = .;

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
        *(.comment)
    }
}
