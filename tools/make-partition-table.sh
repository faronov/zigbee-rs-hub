#!/usr/bin/env bash
# Partition table CSV -> binary for provisioning (ESP-IDF's gen_esp32part.py
# from the .embuild ESP-IDF; also validates offsets, alignment and sizes).
#   tools/make-partition-table.sh firmware/hub/partitions-8mb.csv target/partitions-8mb.bin 8MB
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
csv="$1"
out="$2"
size="${3:-8MB}"
gen="$(ls "$root"/.embuild/espressif/esp-idf/*/components/partition_table/gen_esp32part.py | head -1)"
python3 "$gen" --flash-size "$size" "$csv" "$out"
python3 "$gen" "$out" | grep -v '^#'
