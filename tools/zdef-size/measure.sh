#!/bin/sh
# Code size of the zdef entry points on the hub target (xtensa-esp32s3, the
# `esp` toolchain + ESP-IDF's xtensa GCC as linker) and on thumbv7em as a
# proxy. Prints `feature target text rodata` lines; `+soft` rows use the
# software SHA-256 and miniz_oxide, the others the ROM/accelerator stand-ins.
#   XTENSA_GCC=/path/to/xtensa-esp32s3-elf-gcc tools/zdef-size/measure.sh
set -e
cd "$(dirname "$0")"
THUMB_RF='target.thumbv7em-none-eabihf.rustflags=["-C","link-arg=-Tmemory.x","-C","link-arg=--gc-sections"]'
XT_RF='target.xtensa-esp32s3-none-elf.rustflags=["-C","link-arg=-nostdlib","-C","link-arg=-nostartfiles","-C","link-arg=-Wl,--gc-sections","-C","link-arg=-Tmemory-xtensa.x"]'
SIZE=$(find "$(rustc --print sysroot)" -name llvm-size | head -1)
for f in load vm matching resolve bank all load,soft bank,soft all,soft; do
  cargo build -q --release --no-default-features --features "$f" --target thumbv7em-none-eabihf --config "$THUMB_RF"
  "$SIZE" -A target/thumbv7em-none-eabihf/release/zdef-size | awk -v f="$f" '$1==".text"{t=$2} $1==".rodata"{r=$2} END{print f, "thumbv7em", t, r}'
  if [ -n "$XTENSA_GCC" ]; then
    cargo +esp build -q --release --no-default-features --features "$f" --target xtensa-esp32s3-none-elf -Zbuild-std=core,alloc \
      --config "target.xtensa-esp32s3-none-elf.linker=\"$XTENSA_GCC\"" --config "$XT_RF"
    "$(dirname "$XTENSA_GCC")/xtensa-esp32s3-elf-size" -A target/xtensa-esp32s3-none-elf/release/zdef-size | awk -v f="$f" '$1==".text"{t=$2} $1==".rodata"{r=$2} END{print f, "xtensa-esp32s3", t, r}'
  fi
done
