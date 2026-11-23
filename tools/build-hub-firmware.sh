#!/usr/bin/env bash
# Builds tbr-hub for every image (CI and hub-release.yml): the 8 MB
# production images with MQTT, the advanced console and the Zigbee OTA
# image store (in the inactive app slot), the 4 MB images with MQTT but
# without the console and the store (no second app slot; device OTA from
# streamed images only), each for quad and octal PSRAM. One cargo target dir
# per PSRAM mode (ESP-IDF is built once); the 8 MB ELF is kept as
# target-8mb[-opi]/xtensa-esp32s3-espidf/release/tbr-hub, where
# make-app-image.sh (TARGET_DIR) and release-images.py read it; the
# latter checks the features and PSRAM mode marked in every image.
#   tools/build-hub-firmware.sh [--layout all|4mb|8mb] [--psram all|quad|octal] [cargo args]
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
layout=all
psram_mode=all
cargo_args=()
while [ "$#" -gt 0 ]; do
  case "$1" in
    --layout|--psram)
      if [ "$#" -lt 2 ]; then
        echo "error: $1 requires a value" >&2
        exit 2
      fi
      if [ "$1" = --layout ]; then layout="$2"; else psram_mode="$2"; fi
      shift 2
      ;;
    --help)
      echo "usage: $0 [--layout all|4mb|8mb] [--psram all|quad|octal] [cargo args]"
      exit 0
      ;;
    --) shift; cargo_args+=("$@"); break ;;
    *) cargo_args+=("$1"); shift ;;
  esac
done
case "$layout" in all|4mb|8mb) ;; *) echo "error: unknown layout: $layout" >&2; exit 2 ;; esac
case "$psram_mode" in all|quad|octal) ;; *) echo "error: unknown PSRAM mode: $psram_mode" >&2; exit 2 ;; esac
cd "$root/firmware/hub"
elf=xtensa-esp32s3-espidf/release/tbr-hub
for psram in quad octal; do
  if [ "$psram_mode" != all ] && [ "$psram_mode" != "$psram" ]; then continue; fi
  if [ "$psram" = octal ]; then
    export ESP_IDF_SDKCONFIG_DEFAULTS="firmware/bridge/sdkconfig.defaults;firmware/hub/sdkconfig.defaults;firmware/hub/sdkconfig.octal"
    target="$root/target-opi" stash="$root/target-8mb-opi"
  else
    unset ESP_IDF_SDKCONFIG_DEFAULTS
    target="$root/target" stash="$root/target-8mb"
  fi
  if [ "$layout" != 4mb ]; then
    CARGO_TARGET_DIR="$target" cargo build --release --features mqtt,advanced,ota-store ${cargo_args[@]+"${cargo_args[@]}"}
    mkdir -p "$stash/$(dirname "$elf")"
    cp "$target/$elf" "$stash/$elf"
  fi
  if [ "$layout" != 8mb ]; then
    CARGO_TARGET_DIR="$target" cargo build --release --features mqtt ${cargo_args[@]+"${cargo_args[@]}"}
  fi
done
