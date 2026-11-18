/* Celebron OS Microkernel Linker Script (x86_64 ELF64) */
ENTRY(kernel_main)

SECTIONS
{
    /* Load kernel at 16 MiB (0x01000000) to avoid colliding with early UEFI firmware buffers */
    . = 0x01000000;

    .text ALIGN(4K) :
    {
        *(.text.kernel_main)
        *(.text .text.*)
    }

    .rodata ALIGN(4K) :
    {
        *(.rodata .rodata.*)
    }

    .data ALIGN(4K) :
    {
        *(.data .data.*)
    }

    .bss ALIGN(4K) :
    {
        *(COMMON)
        *(.bss .bss.*)
    }

    /DISCARD/ :
    {
        *(.eh_frame*)
        *(.note*)
        *(.comment*)
    }
}
