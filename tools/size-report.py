#!/usr/bin/env python3
"""Firmware size breakdown of a tbr-hub / tbr-bridge ELF.

    tools/size-report.py [--top N] [--sym] <elf> [<esp-idf build dir>]

Attributes every sized symbol of the image (flash code, flash rodata,
IRAM, DRAM data/bss) to an owner:
- C/asm symbols: the ESP-IDF component (or toolchain / prebuilt library)
  whose static archive defines them (`nm` of every archive the ESP-IDF
  build and the IDF prebuilt `lib/esp32s3` folders contain);
- Rust symbols: the crate of the demangled path (`<T as Trait>::f`
  counts for T's crate).
Bytes no symbol covers (alignment, literal pools without a symbol,
string tables) are shown as "(unattributed)". The build dir defaults to
the newest `esp-idf-sys-*/out/build` next to the ELF. Needs the Xtensa
binutils (`xtensa-esp32s3-elf-nm`) on PATH, e.g. from `.embuild`.
"""

import argparse
import glob
import os
import re
import subprocess
import sys
from collections import defaultdict

NM = os.environ.get("NM", "xtensa-esp32s3-elf-nm")
CPPFILT = os.environ.get("CPPFILT", "xtensa-esp32s3-elf-c++filt")

# Section name -> region shown in the table.
REGIONS = [
    (".flash.text", "flash code"),
    (".flash.rodata", "flash rodata"),
    (".iram0", "IRAM"),
    (".dram0.data", "DRAM data"),
    (".dram0.bss", "DRAM bss"),
    (".rtc", "RTC"),
]


def run(*args):
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout


def sections(elf):
    """[(start, end, region)] of the loaded sections."""
    out = []
    for line in run(NM.replace("-nm", "-objdump"), "-h", elf).splitlines():
        f = line.split()
        if len(f) >= 7 and f[0].isdigit():
            name, size, vma = f[1], int(f[2], 16), int(f[3], 16)
            for prefix, region in REGIONS:
                if name.startswith(prefix) and size:
                    out.append((vma, vma + size, region, name))
    return out


def archive_owner(path):
    base = os.path.basename(path)
    m = re.search(r"/build/esp-idf/([^/]+)/", path)
    if m:
        return m.group(1)
    m = re.search(r"/components/([^/]+)/lib/", path)
    if m:
        return f"{m.group(1)} ({base})"
    m = re.search(r"/(?:managed_)?components/([^/]+)/", path)
    if m:
        return m.group(1)
    return "toolchain " + base


def archives(build_dir, idf_dir):
    paths = [p for p in glob.glob(os.path.join(build_dir, "**", "*.a"), recursive=True) if "/bootloader/" not in p]
    if idf_dir:
        paths += glob.glob(os.path.join(idf_dir, "components", "**", "esp32s3", "*.a"), recursive=True)
    tc = os.path.dirname(os.path.dirname(subprocess.run(["which", NM], capture_output=True, text=True).stdout.strip()))
    for lib in ("libc.a", "libm.a", "libgcc.a", "libstdc++.a", "libnosys.a"):
        paths += [p for p in glob.glob(os.path.join(tc, "**", "esp32s3", "**", lib), recursive=True) if "no-rtti" not in p and "picolibc" not in p][:1]
    owner = {}
    for p in paths:
        try:
            text = run(NM, "--defined-only", p)
        except subprocess.CalledProcessError:
            continue
        who = archive_owner(p)
        for line in text.splitlines():
            f = line.split()
            if len(f) == 3:
                owner.setdefault(f[2], who)
    return owner


RUST_PREFIXES = ("_ZN", "_R")


def rust_crate(demangled):
    s = demangled
    while s.startswith("<"):
        s = s[1:]
        s = s.lstrip("&*").replace("mut ", "", 1).replace("dyn ", "", 1).replace("const ", "", 1)
    s = s.lstrip("[(")
    m = re.match(r"([A-Za-z_][A-Za-z0-9_]*)", s)
    return m.group(1) if m else "?"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("elf")
    ap.add_argument("build_dir", nargs="?")
    ap.add_argument("--top", type=int, default=40)
    ap.add_argument("--sym", type=int, default=0, help="also list the N largest symbols")
    ap.add_argument("--prefix", default="", help="only symbols of owners starting with this")
    a = ap.parse_args()
    build_dir = a.build_dir
    if not build_dir:
        cands = glob.glob(os.path.join(os.path.dirname(a.elf), "build", "esp-idf-sys-*", "out", "build"))
        build_dir = max(cands, key=os.path.getmtime) if cands else None
    idf_dir = None
    if build_dir:
        cache = os.path.join(build_dir, "CMakeCache.txt")
        if os.path.exists(cache):
            m = re.search(r"^esp-idf_SOURCE_DIR:\w+=(.*)$", open(cache).read(), re.M)
            idf_dir = m.group(1) if m else None
    secs = sections(a.elf)
    owner = archives(build_dir, idf_dir) if build_dir else {}

    syms = []
    for line in run(NM, "-S", "--size-sort", a.elf).splitlines():
        f = line.split()
        if len(f) != 4:
            continue
        addr, size, name = int(f[0], 16), int(f[1], 16), f[3]
        region = next((r for s, e, r, _ in secs if s <= addr < e), None)
        if region:
            syms.append((addr, size, name, region))
    rust = [s[2] for s in syms if s[2].startswith(RUST_PREFIXES)]
    dem = {}
    if rust:
        p = subprocess.run([CPPFILT], input="\n".join(rust), capture_output=True, text=True, check=True)
        dem = dict(zip(rust, p.stdout.splitlines()))

    # Overlapping symbols (aliases) are counted once.
    syms.sort()
    table = defaultdict(lambda: defaultdict(int))
    per_sym = []
    end = 0
    for addr, size, name, region in syms:
        size = max(0, min(size, addr + size - end)) if addr < end else size
        end = max(end, addr + size)
        if name in owner:
            who = owner[name]
        elif name in dem:
            who = "rust " + rust_crate(dem[name])
        else:
            who = "(no archive: " + ("rust?" if name.startswith("_") else "C?") + ")"
        table[who][region] += size
        per_sym.append((size, who, region, dem.get(name, name)))

    totals = defaultdict(int)
    for s, e, r, _ in secs:
        totals[r] += e - s
    regions = [r for _, r in REGIONS if totals[r]]
    attributed = defaultdict(int)
    for who in table:
        for r in regions:
            attributed[r] += table[who][r]
    for r in regions:
        table["(unattributed)"][r] = totals[r] - attributed[r]

    image = sum(totals[r] for r in regions if r not in ("DRAM bss", "RTC"))
    rows = sorted(table.items(), key=lambda kv: -sum(v for r, v in kv[1].items() if r != "DRAM bss"))
    hdr = f"{'owner':<44}" + "".join(f"{r:>13}" for r in regions) + f"{'image %':>9}"
    print(hdr)
    print("-" * len(hdr))
    for who, v in rows[: a.top]:
        img = sum(v[r] for r in regions if r not in ("DRAM bss", "RTC"))
        print(f"{who[:44]:<44}" + "".join(f"{v[r]:>13}" for r in regions) + f"{100 * img / image:>8.1f}%")
    rest = rows[a.top :]
    if rest:
        v = defaultdict(int)
        for _, x in rest:
            for r in regions:
                v[r] += x[r]
        img = sum(v[r] for r in regions if r not in ("DRAM bss", "RTC"))
        print(f"{f'({len(rest)} more)':<44}" + "".join(f"{v[r]:>13}" for r in regions) + f"{100 * img / image:>8.1f}%")
    print(f"{'total':<44}" + "".join(f"{totals[r]:>13}" for r in regions))
    if a.sym:
        print()
        for size, who, region, name in sorted((p for p in per_sym if p[1].startswith(a.prefix)), reverse=True)[: a.sym]:
            print(f"{size:>8} {region:<12} {who[:28]:<28} {name[:160]}")


if __name__ == "__main__":
    sys.exit(main())
