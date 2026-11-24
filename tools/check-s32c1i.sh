#!/usr/bin/env bash
# Fail if Rust code in a firmware ELF uses the S32C1I compare-and-set (or
# writes SCOMPARE1, its operand register): on PSRAM it is not atomic between
# the two cores, so Rust atomics must call ESP-IDF's __atomic_* functions
# (-C target-feature=-s32c1i, firmware/*/.cargo/config.toml). Each such
# instruction is attributed to its source file through the ELF's line table
# (`objdump -d -l`); ESP-IDF's C code and blobs may use it, Rust files
# (*.rs, std included) must not. A literal pool decoded as S32C1I right
# after Rust code would also fail the check (fail-safe; none today).
# Checked against a control: a Rust fetch_add for xtensa-esp32 is reported
# without -s32c1i and passes with it.
#   tools/check-s32c1i.sh <xtensa objdump> <elf>
set -euo pipefail
od="$1"
elf="$2"
"$od" -d -l --no-show-raw-insn "$elf" | awk '
  /^[0-9a-f]+ <.*>:$/ { fn = $2; src = "" }
  /^\/.*:[0-9]+/ { src = $1 }
  /\ts32c1i\t|scompare1/ {
    n++
    if (src ~ /\.rs:[0-9]+$/) { r++; print "S32C1I/SCOMPARE1 in Rust: " fn " " src }
  }
  END {
    printf "S32C1I/SCOMPARE1 instructions: %d (ESP-IDF C code and blobs), in Rust code: %d\n", n, r
    if (n == 0) { print "error: no S32C1I found at all, the scan does not work" > "/dev/stderr"; exit 2 }
    exit (r > 0)
  }'
