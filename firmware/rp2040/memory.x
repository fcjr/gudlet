MEMORY {
    BOOT2 : ORIGIN = 0x10000000, LENGTH = 0x100
    FLASH : ORIGIN = 0x10000100, LENGTH = 2048K - 0x100
    RAM   : ORIGIN = 0x20000000, LENGTH = 256K
}

EXTERN(BOOT2_FIRMWARE)

SECTIONS {
    /* Second-stage bootloader; must sit at the start of flash. */
    .boot2 ORIGIN(BOOT2) :
    {
        KEEP(*(.boot2));
    } > BOOT2
} INSERT BEFORE .text;

/* Band buffers and core 1 stack are static. Reserve room for core 0 as the
   pipeline grows, instead of allowing a successful link with no stack space. */
ASSERT(_stack_start - _stack_end >= 16K, "RP2040 needs at least 16 KiB for core 0 stack");
