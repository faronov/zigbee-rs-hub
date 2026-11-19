ENTRY(_start)
MEMORY { RAM (rwx) : ORIGIN = 0x40810000, LENGTH = 0x18000 }
SECTIONS {
  .text : { *(.text.entry) *(.text*) *(.rodata*) *(.srodata*) } > RAM
  .data : { *(.data*) *(.sdata*) *(.sbss*) *(.bss*) *(COMMON) } > RAM
  _stack_top = ORIGIN(RAM) + LENGTH(RAM);
}
