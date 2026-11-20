MEMORY { FLASH : ORIGIN = 0x0, LENGTH = 4M  RAM : ORIGIN = 0x20000000, LENGTH = 1M }
ENTRY(probe_main)
SECTIONS {
  .text : { *(.text .text.*) } > FLASH
  .rodata : { *(.rodata .rodata.*) } > FLASH
  .data : { *(.data .data.*) } > RAM AT > FLASH
  .bss : { *(.bss .bss.* COMMON) } > RAM
  /DISCARD/ : { *(.ARM.exidx*) *(.ARM.extab*) }
}
