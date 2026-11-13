/*
 * SketchOS 0.0.5 static user image.
 *
 * Images are non-PIE ET_EXEC files linked at 0x400000. The kernel maps each
 * PT_LOAD segment page-by-page, so every section starts on its own page and
 * file offsets stay congruent with virtual addresses at the 4 KiB granule.
 */

ENTRY(_start)

SECTIONS
{
    . = 0x400000;

    .text : ALIGN(4096) {
        KEEP(*(.text._start))
        *(.text .text.*)
    }

    .rodata : ALIGN(4096) {
        *(.rodata .rodata.*)
    }

    .data : ALIGN(4096) {
        *(.data .data.*)
        *(.data.rel.ro .data.rel.ro.*)
    }

    .bss : ALIGN(4096) {
        *(.bss .bss.*)
        *(COMMON)
    }

    /DISCARD/ : {
        *(.comment)
        *(.note .note.*)
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.dynamic)
        *(.dynsym)
        *(.dynstr)
        *(.hash)
        *(.gnu.hash)
        *(.got .got.*)
        *(.rela .rela.*)
    }
}
