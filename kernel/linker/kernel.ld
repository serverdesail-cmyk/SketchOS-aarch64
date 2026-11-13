/*
 * SketchOS kernel image layout.
 *
 * The kernel is a position-independent ET_DYN image linked at virtual address
 * zero. The UEFI loader chooses a free physical range, copies the image there
 * and applies the R_AARCH64_RELATIVE relocations, so no fixed load address is
 * reserved and firmware cannot make the boot fail by occupying one.
 *
 * Since milestone 0.0.4 the loader runs the image through a higher-half map:
 * the runtime virtual address of `physical` is `KERNEL_VA_BASE + physical`.
 * The section symbols below let the kernel rebuild its final W^X map without
 * relying on the ELF program headers at runtime.
 */

ENTRY(_start)

SECTIONS
{
    . = 0;
    __kernel_start = .;

    .text : ALIGN(4096) {
        __text_start = .;
        KEEP(*(.text._start))
        *(.text .text.*)
        __text_end = .;
    }

    .rodata : ALIGN(4096) {
        __rodata_start = .;
        *(.rodata .rodata.*)
        __rodata_end = .;
    }

    .data : ALIGN(4096) {
        __data_start = .;
        *(.data .data.*)
        *(.data.rel.ro .data.rel.ro.*)
    }

    /* A static PIE has a GOT, a dynamic section and relative relocations even
     * though it has no shared libraries. The loader reads DT_RELA/DT_RELASZ
     * from .dynamic. These sections are read-only after relocation but are
     * mapped with the writable data span; the W^X invariant only forbids
     * writable and executable pages, so that is safe.
     *
     * The read-only relocation sections must start on a page boundary of their
     * own: the loader maps whole pages with one permission set per segment, so
     * a segment that starts in the middle of a page would conflict with the
     * data page that precedes it. */
    .got : ALIGN(8) {
        *(.got .got.*)
        *(.got.plt)
    }

    .dynamic : ALIGN(8) {
        *(.dynamic)
    }

    .rela.dyn : ALIGN(4096) {
        *(.rela.dyn)
        *(.rela.plt)
    }

    .dynsym : {
        *(.dynsym)
    }

    .dynstr : {
        *(.dynstr)
    }

    .gnu.hash : {
        *(.gnu.hash)
    }

    .hash : {
        *(.hash)
    }

    .bss : ALIGN(4096) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)

        /* 64 KiB early kernel stack. boot.S installs __stack_top after it has
         * cleared this span. */
        . = ALIGN(16);
        __stack_bottom = .;
        . += 64K;
        __stack_top = .;

        . = ALIGN(8);
        __bss_end = .;
    }

    . = ALIGN(4096);
    __kernel_end = .;

    /DISCARD/ : {
        *(.comment)
        *(.note .note.*)
        *(.eh_frame)
        *(.eh_frame_hdr)
    }
}
