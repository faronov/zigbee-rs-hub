#!/usr/bin/env bash
# Builds tbr-hub for every image (CI and hub-release.yml): the 8 MB
# production images with MQTT, the advanced console and the Zigbee device
# OTA server with its image store (in the inactive app slot), the 4 MB
# images with MQTT but without the console and without Zigbee OTA, and,
# once firmware/hub has the feature `dev`, the 8 MB developer edition
# ("devtools": the 8 MB image with `dev`), each for quad and octal PSRAM.
# One cargo target dir per PSRAM mode (ESP-IDF is built once); the 8 MB
# ELFs are kept as target-8mb[-devtools][-opi]/xtensa-esp32s3-espidf/release/tbr-hub,
# where make-app-image.sh (TARGET_DIR) and release-images.py read them; the
# latter checks the features and PSRAM mode marked in every image.
#   tools/build-hub-firmware.sh [--layout all|4mb|8mb|devtools] [--psram all|quad|octal] [cargo args]
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
      echo "usage: $0 [--layout all|4mb|8mb|devtools] [--psram all|quad|octal] [cargo args]"
      exit 0
      ;;
    --) shift; cargo_args+=("$@"); break ;;
    *) cargo_args+=("$1"); shift ;;
  esac
done
case "$layout" in all|4mb|8mb|devtools) ;; *) echo "error: unknown layout: $layout" >&2; exit 2 ;; esac
case "$psram_mode" in all|quad|octal) ;; *) echo "error: unknown PSRAM mode: $psram_mode" >&2; exit 2 ;; esac
cd "$root/firmware/hub"
elf=xtensa-esp32s3-espidf/release/tbr-hub
# The developer edition exists once tbr-hub has the feature (release-images.py asks the same).
devtools=false
if grep -Eq '^dev[[:space:]]*=' Cargo.toml 2>/dev/null; then devtools=true; fi
if [ "$layout" = devtools ] && [ "$devtools" = false ]; then
  echo "error: firmware/hub has no feature dev (the developer edition)" >&2
  exit 2
fi
stash() {  # target dir to copy the ELF into
  mkdir -p "$1/$(dirname "$elf")"
  cp "$target/$elf" "$1/$elf"
}
for psram in quad octal; do
  if [ "$psram_mode" != all ] && [ "$psram_mode" != "$psram" ]; then continue; fi
  if [ "$psram" = octal ]; then
    export ESP_IDF_SDKCONFIG_DEFAULTS="firmware/bridge/sdkconfig.defaults;firmware/hub/sdkconfig.defaults;firmware/hub/sdkconfig.octal"
    target="$root/target-opi" stash="$root/target-8mb-opi" devstash="$root/target-8mb-devtools-opi"
  else
    unset ESP_IDF_SDKCONFIG_DEFAULTS
    target="$root/target" stash="$root/target-8mb" devstash="$root/target-8mb-devtools"
  fi
  if [ "$layout" = all ] || [ "$layout" = 8mb ]; then
    CARGO_TARGET_DIR="$target" cargo build --release --features mqtt,advanced,ota-store ${cargo_args[@]+"${cargo_args[@]}"}
    stash "$stash"
  fi
  if [ "$devtools" = true ] && { [ "$layout" = all ] || [ "$layout" = devtools ]; }; then
    CARGO_TARGET_DIR="$target" cargo build --release --features mqtt,dev,ota-store ${cargo_args[@]+"${cargo_args[@]}"}
    stash "$devstash"
  fi
  # Last, so that target[-opi] holds the 4 MB ELF.
  if [ "$layout" = all ] || [ "$layout" = 4mb ]; then
    CARGO_TARGET_DIR="$target" cargo build --release --features mqtt ${cargo_args[@]+"${cargo_args[@]}"}
  fi
done
