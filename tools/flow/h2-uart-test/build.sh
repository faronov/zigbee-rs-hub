#!/usr/bin/env bash
# Build the RAM-only H2 test program (riscv32-esp-elf GCC from ESP-IDF tools).
set -euo pipefail
cd "$(dirname "$0")"
CC=${CC:-$(ls -d "$HOME"/.espressif/tools/riscv32-esp-elf/*/riscv32-esp-elf/bin/riscv32-esp-elf-gcc | tail -1)}
"$CC" -march=rv32imac_zicsr -mabi=ilp32 -Os -ffreestanding -nostdlib -Wall -Wextra \
  -T link.ld -o h2-uart-test.elf start.S main.c -lgcc
esptool --chip esp32h2 elf2image --ram-only-header -o h2-uart-test.bin h2-uart-test.elf >/dev/null
echo "h2-uart-test.bin $(wc -c < h2-uart-test.bin) bytes"
