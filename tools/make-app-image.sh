#!/usr/bin/env bash
# Convert the bridge ELF into an ESP app image for the stock Thread BR layout
# (ota_0 at 0x20000). Same elf2image arguments as the ESP-IDF build.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
profile="${1:-release}"
elf="$root/target/xtensa-esp32s3-espidf/$profile/tbr-bridge"
out="${2:-$root/target/tbr-bridge-app.bin}"
esptool --chip esp32s3 elf2image --flash-mode dio --flash-freq 80m --flash-size 4MB \
  --elf-sha256-offset 0xb0 --min-rev-full 0 --max-rev-full 99 -o "$out" "$elf"
esptool --chip esp32s3 image-info "$out" | sed -n '1,40p'
shasum -a 256 "$out"
