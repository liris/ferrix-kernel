/* User programs live in the second half of the lower address range, far
 * from anything the kernel maps (see USER_BASE in the kernel's mmu.rs).
 * Code, read-only data and writable data get their own pages so each can
 * be mapped with its own permissions. */
ENTRY(_start)

SECTIONS
{
    . = 0x400000000000;
    .text : { *(.text.start) *(.text .text.*) }
    . = ALIGN(4096);
    .rodata : { *(.rodata .rodata.*) }
    . = ALIGN(4096);
    .data : { *(.data .data.*) }
    .bss : { *(.bss .bss.*) *(COMMON) }
    /DISCARD/ : { *(.comment) *(.note .note.*) *(.eh_frame*) }
}
