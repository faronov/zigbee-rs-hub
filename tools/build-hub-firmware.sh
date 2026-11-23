#!/usr/bin/env bash
# Builds tbr-hub for every image (CI and hub-release.yml): the 8 MB
# production images with the MQTT bridge (feature `mqtt`), the 4 MB dev
# images without it, each for quad and octal PSRAM. One cargo target dir
# per PSRAM mode (ESP-IDF is built once); the 8 MB ELF is kept as
# target-8mb[-opi]/xtensa-esp32s3-espidf/release/tbr-hub, where
# make-app-image.sh (TARGET_DIR) and release-images.py read it.
#   tools/build-hub-firmware.sh [cargo args, e.g. --locked]
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root/firmware/hub"
elf=xtensa-esp32s3-espidf/release/tbr-hub
for psram in quad octal; do
  if [ "$psram" = octal ]; then
    export ESP_IDF_SDKCONFIG_DEFAULTS="firmware/bridge/sdkconfig.defaults;firmware/hub/sdkconfig.defaults;firmware/hub/sdkconfig.octal"
    target="$root/target-opi" stash="$root/target-8mb-opi"
  else
    unset ESP_IDF_SDKCONFIG_DEFAULTS
    target="$root/target" stash="$root/target-8mb"
  fi
  CARGO_TARGET_DIR="$target" cargo build --release --features mqtt "$@"
  mkdir -p "$stash/$(dirname "$elf")"
  cp "$target/$elf" "$stash/$elf"
  CARGO_TARGET_DIR="$target" cargo build --release "$@"
done
