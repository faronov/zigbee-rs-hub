#!/usr/bin/env bash
# Button-free S3 reflash (README "Flashing the S3"): hand the USB PHY back to
# USB-Serial-JTAG, take hwlab's guard backup, program the app partition of
# the table on the device (factory on the 4 MB dev layout, else ota_0).
# Booting the new image needs the USJ RTS reset (hwlab uart plugin, MCP)
# afterwards.
#
#   s3_reflash.sh IMAGE TAG [LAYOUT [SLOT]]
#
# LAYOUT (4mb-dev, 8mb or a CSV path) makes it refuse when the device's
# partition table is a different one; nothing is written on a refusal.
set -euo pipefail
img=$1; tag=$2; layout=${3:-}; slot=${4:-}
here=$(cd "$(dirname "$0")" && pwd)
case $layout in
  "") ;;
  4mb-dev|8mb) layout=$here/../../firmware/hub/partitions-$layout.csv ;;
esac
dev=espusbjtag/303a:1001:7C_DF_A1_FE_67_F0
python3 - <<'PY'
import serial, time
c = serial.Serial('/dev/cu.usbmodem7CDFA1FE67F03', 115200, timeout=0.3)
c.write(b's3 usj\r'); time.sleep(0.3)
try: c.close()
except Exception: pass
PY
for i in $(seq 1 15); do sleep 1; ioreg -p IOUSB -w0 | grep -q "JTAG/serial debug unit@14743" && break; done
dir=$HOME/.hwlab/artifacts/zigbee-rs-hub/s3-backup-$tag; mkdir -p "$dir"
~/hwlab/bin/hwlab target flash backup --yes --device $dev --arg dir="$dir" | grep -E '"(reads_identical|sha256)"' | head -2
backup=$(ls -t "$dir"/*.bin | head -1)
# The offset comes from the partition table read with the backup.
addr=$(python3 "$here/s3_partition.py" "$backup.json" "$img" ${layout:+--layout "$layout"} ${slot:+--slot "$slot"})
~/hwlab/bin/hwlab target flash program --yes --device $dev --config allow_flash=true --config allow_erase=true \
  --arg path="$img" --arg address="$addr" --arg confirm=program-flash --arg backup="$backup" \
  | grep -E '"(image_sha256|verified|target_verified)"'
