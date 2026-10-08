/* vibeOS aarch64 kernel link script.
 *
 * Same higher-half VMA as x86_64 so the kernel code model still fits.
 * Sections page-aligned so W^X per section is possible in phase 1.
 * .got sits inside the mapped range and before .bss, DESIGN §3.4.
 */

OUTPUT_FORMAT(elf64-littleaarch64)
OUTPUT_ARCH(aarch64)
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

    __ex_table : ALIGN(4) {
        __ex_table_start = .;
        KEEP(*(__ex_table))
        __ex_table_end = .;
    } :rodata

    .note.gnu.build-id : ALIGN(4) {
        __build_id_start = .;
        KEEP(*(.note.gnu.build-id))
        __build_id_end = .;
    } :rodata

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

    .stack_sizes 0 (INFO) : {
        KEEP(*(.stack_sizes))
    }

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
        *(.comment)
    }
}
