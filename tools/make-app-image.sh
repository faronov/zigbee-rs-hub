#!/usr/bin/env bash
# Convert a firmware ELF into an ESP app image. Same elf2image arguments as
# the ESP-IDF build.
#   tools/make-app-image.sh [profile] [out.bin] [app]   (app: tbr-bridge | tbr-hub)
# FLASH_SIZE (default 4MB) goes into the image header. With MAX_APP_BYTES set,
# fail if the image is larger (app slots: 3 MiB = 3145728 on the 8 MB
# production layout, 2 944 KiB = 3014656 on the 4 MB dev layout).
# TARGET_DIR (default target/) selects the cargo target directory, e.g.
# target-opi for the octal PSRAM build.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
profile="${1:-release}"
app="${3:-tbr-bridge}"
elf="${TARGET_DIR:-$root/target}/xtensa-esp32s3-espidf/$profile/$app"
out="${2:-$root/target/$app-app.bin}"
esptool --chip esp32s3 elf2image --flash-mode dio --flash-freq 80m --flash-size "${FLASH_SIZE:-4MB}" \
  --elf-sha256-offset 0xb0 --min-rev-full 0 --max-rev-full 99 -o "$out" "$elf"
esptool --chip esp32s3 image-info "$out" | sed -n '1,40p'
shasum -a 256 "$out"
size=$(wc -c < "$out" | tr -d ' ')
echo "$app image: $size bytes"
if [ -n "${MAX_APP_BYTES:-}" ] && [ "$size" -gt "$MAX_APP_BYTES" ]; then
  echo "error: $app image $size B exceeds the $MAX_APP_BYTES B app slot" >&2
  exit 1
fi
