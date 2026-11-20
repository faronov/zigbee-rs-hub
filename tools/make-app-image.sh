#!/usr/bin/env bash
# Convert a firmware ELF into an ESP app image. Same elf2image arguments as
# the ESP-IDF build.
#   tools/make-app-image.sh [profile] [out.bin] [app]
#   (app: tbr-bridge | tbr-hub | zrh-s3-mg24 for CHIP=esp32s3, zrh-donglem for CHIP=esp32)
# CHIP (default esp32s3) selects the target; FLASH_SIZE (default 4MB) goes
# into the image header; MIN_REV_FULL (default 0) is the minimum chip
# revision in the header (300 for the Dongle-M build, CONFIG_ESP32_REV_MIN_3).
# With MAX_APP_BYTES set, fail if the image is larger (app slots: 4 MiB =
# 4194304 on the 16 MB layout, 3 MiB = 3145728 on the 8 MB production
# layout, 2 944 KiB = 3014656 on the 4 MB dev layout).
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
profile="${1:-release}"
app="${3:-tbr-bridge}"
chip="${CHIP:-esp32s3}"
# ESP-IDF's defaults: ESP32-S3 up to v0.99, ESP32 up to v3.99.
if [ "$chip" = esp32 ]; then max_rev=399; else max_rev=99; fi
elf="$root/target/xtensa-$chip-espidf/$profile/$app"
out="${2:-$root/target/$app-app.bin}"
esptool --chip "$chip" elf2image --flash-mode dio --flash-freq 80m --flash-size "${FLASH_SIZE:-4MB}" \
  --elf-sha256-offset 0xb0 --min-rev-full "${MIN_REV_FULL:-0}" --max-rev-full "$max_rev" -o "$out" "$elf"
esptool --chip "$chip" image-info "$out" | sed -n '1,40p'
shasum -a 256 "$out"
size=$(wc -c < "$out" | tr -d ' ')
echo "$app image: $size bytes"
if [ -n "${MAX_APP_BYTES:-}" ] && [ "$size" -gt "$MAX_APP_BYTES" ]; then
  echo "error: $app image $size B exceeds the $MAX_APP_BYTES B app slot" >&2
  exit 1
fi
