#!/usr/bin/env bash
# Fail if Rust code in a firmware ELF uses the S32C1I compare-and-set (or
# writes SCOMPARE1, its operand register): on PSRAM it is not atomic between
# the two cores, so Rust atomics must call ESP-IDF's __atomic_* functions
# (-C target-feature=-s32c1i, firmware/*/.cargo/config.toml). Each such
# instruction is attributed to its source file through the ELF's line table
# (`objdump -d -l`); ESP-IDF's C code and blobs may use it, Rust files
# (*.rs, std included) must not. Only a compare-and-set counts: an S32C1I
# with SCOMPARE1 written (WSR) in the 8 instructions before it, in the same
# function, as every atomic sequence is; data in .text (f64 constants,
# literal pools) decoded as instructions yields lone S32C1I or RSR/WSR
# SCOMPARE1 lines, which moved with the code layout and failed the check.
# Checked against a control: a Rust fetch_add for xtensa-esp32 is reported
# without -s32c1i and passes with it.
#   tools/check-s32c1i.sh <xtensa objdump> <elf>
set -euo pipefail
od="$1"
elf="$2"
"$od" -d -l --no-show-raw-insn "$elf" | awk '
  /^[0-9a-f]+ <.*>:$/ { fn = $2; src = ""; k = 0; w = -99; next }
  /^\/.*:[0-9]+/ { src = $1; next }
  /^ *[0-9a-f]+:\t/ {
    k++
    if (/\twsr\.scompare1\t/) w = k
    if (/\ts32c1i\t|scompare1/) raw++
    if (/\ts32c1i\t/ && k - w <= 8) {
      n++
      if (src ~ /\.rs:[0-9]+$/) { r++; print "S32C1I compare-and-set in Rust: " fn " " src }
    }
  }
  END {
    printf "S32C1I/SCOMPARE1 lines: %d; compare-and-set sequences: %d (ESP-IDF C code and blobs), in Rust code: %d\n", raw, n, r
    if (n == 0) { print "error: no compare-and-set found at all, the scan does not work" > "/dev/stderr"; exit 2 }
    exit (r > 0)
  }'
