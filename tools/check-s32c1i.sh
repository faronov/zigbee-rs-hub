#!/usr/bin/env bash
# Fail if Rust code in a firmware ELF uses the S32C1I compare-and-set (or
# reads or writes SCOMPARE1, its operand register): on PSRAM it is not
# atomic between the two cores, so Rust atomics must call ESP-IDF's
# __atomic_* functions (-C target-feature=-s32c1i,
# firmware/*/.cargo/config.toml). Only each function's own bytes are
# scanned (FUNC symbols, address and size from `readelf -sW`): literal
# pools and other data in .text lie outside them, and decoded as
# instructions they gave false hits whenever the code layout moved. Inside
# a Rust function — its line-table file is *.rs (`objdump -d -l`) or its
# name is Rust-mangled (_ZN…17h<hash>E, _R…) — any S32C1I or RSR/WSR/XSR
# SCOMPARE1 fails (exit 1); ESP-IDF's C code and blobs may use them, and
# none found in any function at all means the scan does not work (exit 2).
# The positive control (tools/s32c1i-control, CI): a Rust fetch_add and
# compare_exchange built with S32C1I must fail the check.
#   tools/check-s32c1i.sh <xtensa objdump> <elf or object>
# readelf: next to objdump (…-objdump → …-readelf), or $READELF.
set -euo pipefail
od="$1"
elf="$2"
re="${READELF:-${od%objdump}readelf}"
{
  "$re" -sW "$elf" | awk '$4 == "FUNC" && $3 != "0" { print "S", $2, $3 }'
  "$od" -d -l --no-show-raw-insn "$elf"
} | awk '
  # Hex text to a number (portable: mawk has no strtonum); stops at a
  # non-hex character (the colon of an address).
  function hex(s,   i, c, v) {
    v = 0; s = tolower(s); sub(/^0x/, "", s)
    for (i = 1; i <= length(s); i++) {
      c = index("0123456789abcdef", substr(s, i, 1))
      if (!c) break
      v = v * 16 + c - 1
    }
    return v
  }
  $1 == "S" && NF == 3 { size[hex($2)] = ($3 ~ /^0x/) ? hex($3) : $3 + 0; next }
  # A symbol: a function starts (its end from its size); a label inside the
  # current function keeps it; anything else is not code of a function.
  /^[0-9a-f]+ <.*>:$/ {
    a = hex($1)
    if (a in size) { fn = $2; src = ""; end = a + size[a] }
    else if (end < 0 || a >= end) { fn = $2; src = ""; end = -1 }
    next
  }
  /^\/.*:[0-9]+/ { src = $1; next }
  /^ *[0-9a-f]+:\t/ {
    if (!/\ts32c1i\t|\t[rwx]sr\.scompare1\t/) next
    if (end < 0 || hex($1) >= end) { data++; next }
    n++
    if (src ~ /\.rs:[0-9]+$/ || fn ~ /^<_ZN.*17h[0-9a-f]+E>:$/ || fn ~ /^<_R/) {
      r++; print "S32C1I/SCOMPARE1 in Rust: " fn " " src " " $0
    }
  }
  END {
    printf "S32C1I/SCOMPARE1 in functions: %d (Rust: %d); outside functions (data): %d\n", n, r, data
    if (n == 0) { print "error: none in any function, the scan does not work" > "/dev/stderr"; exit 2 }
    exit (r > 0)
  }'
