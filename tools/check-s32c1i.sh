#!/usr/bin/env bash
# Fail if Rust code in a firmware ELF has an S32C1I compare-and-set: on
# PSRAM it is not atomic between the two cores, so Rust atomics must call
# ESP-IDF's __atomic_* functions (-C target-feature=-s32c1i,
# firmware/*/.cargo/config.toml).
#
# Only a real compare-and-set counts, as every atomic sequence is: SCOMPARE1
# written (WSR/XSR SCOMPARE1) and then an S32C1I at most 4 instructions on,
# in one straight run — nothing undecodable (`.byte`), no jump or return in
# between, no branch target inside it. Data in .text decoded as
# instructions (string and jump tables, literal pools) gave lone S32C1I and
# RSR/WSR SCOMPARE1 lines that moved with every layout change, also within
# what objdump shows as a function; they are listed (in Rust functions) and
# not counted. Besides, only each function's own bytes are scanned (FUNC
# symbols, address and size from `readelf -sW`), and a sequence on a
# literal word (the target of an L32R; a real instruction never is one) is
# data.
#
# objdump decodes linearly: padding or data can shift it off the real
# instruction boundaries and hide a sequence (it does so in ESP-IDF's
# esp_cpu_compare_and_set). Every branch target inside a Rust function that
# falls within a decoded instruction is decoded again from there and
# checked the same way; none so far (Rust code has no padding or data
# inside its functions).
#
# A sequence in a Rust function — its line-table file is *.rs (`objdump -d
# -l`) or its name is Rust-mangled (_ZN…17h<hash>E, _R…) — fails (exit 1);
# ESP-IDF's C code and blobs have them, and none in any function at all
# means the scan does not work (exit 2). The positive control
# (tools/s32c1i-control, CI): six Rust atomics built with S32C1I, each must
# be found. The CI also checks that the targets' `s32c1i` feature is off
# for the firmware (rustc --print cfg); a check of the calls to __atomic_*
# in the linked image would add little: with the feature off the compiler
# has no other lowering.
#   tools/check-s32c1i.sh <xtensa objdump> <elf or object>
# readelf: next to objdump (…-objdump → …-readelf), or $READELF.
set -euo pipefail
od="$1"
elf="$2"
re="${READELF:-${od%objdump}readelf}"

# Shared awk: hex text to a number (portable: mawk has no strtonum; stops
# at a non-hex character), number to an array key.
lib='
  function hex(s,   i, c, v) {
    v = 0; s = tolower(s); sub(/^0x/, "", s)
    for (i = 1; i <= length(s); i++) {
      c = index("0123456789abcdef", substr(s, i, 1))
      if (!c) break
      v = v * 16 + c - 1
    }
    return v
  }
  function key(a) { return sprintf("%.0f", a) }
  # Instruction lines: f[2] the mnemonic, f[3] the operands. A straight
  # run ends at undecodable bytes, a jump or a return.
  function breaks(op) { return op == ".byte" || op ~ /^(j|jx|ret|ret\.n|retw|retw\.n|ill|ill\.n|rfe|rfi)$/ }
  function wsr(op) { return op ~ /^[wx]sr\.scompare1$/ }
'

out=$({
  "$re" -sW "$elf" | awk '$4 == "FUNC" && $3 != "0" { print "S", $2, $3 }'
  "$od" -d -l --no-show-raw-insn "$elf"
} | awk "$lib"'
  # The 4-byte word holding address a, as an array key.
  function word(a) { return sprintf("%.0f", a - a % 4) }
  # Whether the 3-byte instruction at a overlaps a literal word.
  function inlit(a) { return (word(a) in lit) || (word(a + 2) in lit) }
  function rustfn() { return (src ~ /\.rs:[0-9]+$/) || (fn ~ /^<_ZN.*17h[0-9a-f]+E>:$/) || (fn ~ /^<_R/) }
  $1 == "S" && NF == 3 { size[hex($2)] = ($3 ~ /^0x/) ? hex($3) : $3 + 0; next }
  # A symbol: a function starts (its end from its size); a label inside the
  # current function keeps it; anything else is not code of a function.
  /^[0-9a-f]+ <.*>:$/ {
    a = hex($1); w = -99
    if (a in size) { fn = $2; src = ""; beg = a; end = a + size[a] }
    else if (end < 0 || a >= end) { fn = $2; src = ""; end = -1 }
    next
  }
  /^\/.*:[0-9]+/ { src = $1; next }
  /^ *[0-9a-f]+:\t/ {
    a = hex($1); k++
    split($0, f, "\t"); op = f[2]
    # Literal words (L32R targets) and branch targets may come after what
    # they concern: they are applied at the end.
    if (op == "l32r") { t = f[3]; sub(/^[a-z0-9]+, /, "", t); lit[word(hex(t))] = 1 }
    if (end >= 0 && a < end && rustfn()) {
      start[key(a)] = 1
      if (op ~ /^(b|j$|j\.|loop)/ && match($0, /[0-9a-f]+ <[^>]*>$/)) {
        t = hex(substr($0, RSTART, RLENGTH))
        if (t >= beg && t < end) { bt[key(t)] = 1; btw[key(t)] = fn " " src }
      }
    }
    if (breaks(op)) { w = -99; next }
    if (op !~ /^s32c1i$|^[rwx]sr\.scompare1$/) next
    if (end < 0 || a >= end) { data++; next }
    if (wsr(op)) { w = k; wa = a; wwhat = $0; next }
    if (op == "s32c1i" && k - w <= 4) {
      m++; s0[m] = wa; s1[m] = a; rust[m] = rustfn(); what[m] = fn " " src "\n    " wwhat "\n    " $0; w = -99; next
    }
    # Not part of a compare-and-set: data decoded as an instruction.
    lone++; if (rustfn()) print "not a compare-and-set (data), not counted: " fn " " src " " $0
  }
  END {
    for (i = 1; i <= m; i++) {
      cut = inlit(s0[i]) || inlit(s1[i])
      for (o = 1; !cut && s0[i] + o <= s1[i]; o++) if (key(s0[i] + o) in bt) cut = 1
      if (cut) { l++; if (rust[i]) print "literal word or branch target inside, data, not counted: " what[i]; continue }
      n++; if (rust[i]) { r++; print "S32C1I compare-and-set in Rust: " what[i] }
    }
    # Branch targets inside a decoded instruction of a Rust function: the
    # decode is off there; the script decodes them again.
    for (t in bt) if (!(t in start)) { d++; printf "RESYNC %x %s\n", t + 0, btw[t] }
    printf "compare-and-set sequences in functions: %d (Rust: %d); not counted: %d lone S32C1I/SCOMPARE1, %d sequences on literal words or branch targets, %d outside functions; Rust branch targets off the decode: %d\n", n, r, lone, l, data, d
    if (n == 0) { print "error: no compare-and-set in any function, the scan does not work" > "/dev/stderr"; exit 2 }
    exit (r > 0)
  }') && rc=0 || rc=$?
grep -v '^RESYNC ' <<<"$out" || true
[ "$rc" -eq 2 ] && exit 2
hidden=0
while read -r tag addr what; do
  [ "$tag" = RESYNC ] || continue
  # 8 instructions from the branch target, decoded from there.
  if ! "$od" -d --no-show-raw-insn --start-address="0x$addr" --stop-address="$(printf '0x%x' $((0x$addr + 24)))" "$elf" | awk "$lib"'
    /^ *[0-9a-f]+:\t/ {
      k++; split($0, f, "\t"); op = f[2]
      if (breaks(op)) { w = -99; next }
      if (wsr(op)) w = k
      else if (op == "s32c1i" && k - w <= 4) { print "    " $0; exit 1 }
    }'; then
    echo "S32C1I compare-and-set in Rust (decoded again from branch target 0x$addr): $what"
    hidden=1
  fi
done <<<"$out"
[ "$rc" -eq 1 ] || [ "$hidden" -eq 1 ] && exit 1
exit 0
