#!/usr/bin/env bash
# Convert a firmware ELF into an ESP app image for ota_0 at 0x20000. Same
# elf2image arguments as the ESP-IDF build.
#   tools/make-app-image.sh [profile] [out.bin] [app]   (app: tbr-bridge | tbr-hub)
# With MAX_APP_BYTES set, fail if the image is larger (hub app slot on 4 MB
# boards: 1.5 MiB = 1572864).
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
profile="${1:-release}"
app="${3:-tbr-bridge}"
elf="$root/target/xtensa-esp32s3-espidf/$profile/$app"
out="${2:-$root/target/$app-app.bin}"
esptool --chip esp32s3 elf2image --flash-mode dio --flash-freq 80m --flash-size 4MB \
  --elf-sha256-offset 0xb0 --min-rev-full 0 --max-rev-full 99 -o "$out" "$elf"
esptool --chip esp32s3 image-info "$out" | sed -n '1,40p'
shasum -a 256 "$out"
size=$(wc -c < "$out" | tr -d ' ')
echo "$app image: $size bytes"
if [ -n "${MAX_APP_BYTES:-}" ] && [ "$size" -gt "$MAX_APP_BYTES" ]; then
  echo "error: $app image $size B exceeds the $MAX_APP_BYTES B app slot" >&2
  exit 1
fi
