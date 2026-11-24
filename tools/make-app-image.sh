#!/usr/bin/env bash
# Convert a firmware ELF into an ESP app image. Same elf2image arguments as
# the ESP-IDF build.
#   tools/make-app-image.sh [profile] [out.bin] [app]   (app: tbr-bridge | tbr-hub | zrh-donglem)
# CHIP (default esp32s3; esp32 for zrh-donglem) selects the target and the
# ELF directory. FLASH_SIZE (default 4MB) and FLASH_FREQ (default 80m; 40m
# for the Dongle-M, as its stock bootloader) go into the image header, with
# the chip revision range: MIN_REV_FULL (default 0; 300 for the Dongle-M,
# CONFIG_ESP32_REV_MIN_3) up to ESP-IDF's default maximum (S3 v0.99, ESP32
# v3.99). With MAX_APP_BYTES set, fail if the image is larger (app slots:
# 3 MiB = 3145728 on the 8 MB production layout, 2 944 KiB = 3014656 on the
# 4 MB dev layout, 6 MiB = 6291456 on the Dongle-M's stock table).
# TARGET_DIR (default target/) selects the cargo target directory, e.g.
# target-opi for the octal PSRAM build.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
profile="${1:-release}"
app="${3:-tbr-bridge}"
chip="${CHIP:-esp32s3}"
case "$chip" in
  esp32s3) max_rev=99 ;;
  esp32) max_rev=399 ;;
  *) echo "error: CHIP must be esp32s3 or esp32" >&2; exit 1 ;;
esac
elf="${TARGET_DIR:-$root/target}/xtensa-$chip-espidf/$profile/$app"
out="${2:-$root/target/$app-app.bin}"
esptool --chip "$chip" elf2image --flash-mode dio --flash-freq "${FLASH_FREQ:-80m}" --flash-size "${FLASH_SIZE:-4MB}" \
  --elf-sha256-offset 0xb0 --min-rev-full "${MIN_REV_FULL:-0}" --max-rev-full "$max_rev" -o "$out" "$elf"
esptool --chip "$chip" image-info "$out" | sed -n '1,40p'
shasum -a 256 "$out"
size=$(wc -c < "$out" | tr -d ' ')
echo "$app image: $size bytes"
if [ -n "${MAX_APP_BYTES:-}" ] && [ "$size" -gt "$MAX_APP_BYTES" ]; then
  echo "error: $app image $size B exceeds the $MAX_APP_BYTES B app slot" >&2
  exit 1
fi
