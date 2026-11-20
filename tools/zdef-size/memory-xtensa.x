MEMORY { IROM : ORIGIN = 0x42000000, LENGTH = 4M  DRAM : ORIGIN = 0x3FC88000, LENGTH = 320K }
ENTRY(probe_main)
SECTIONS {
  .text : { *(.literal .literal.* .text .text.*) } > IROM
  .rodata : { *(.rodata .rodata.*) } > IROM
  .data : { *(.data .data.*) } > DRAM
  .bss : { *(.bss .bss.* COMMON) } > DRAM
}
