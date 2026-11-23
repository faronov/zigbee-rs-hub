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


def parse_sections(text):
    """Allocated sections: (start, end, region, name, stored in the image)."""
    out = []
    lines = text.splitlines()
    for i, line in enumerate(lines):
        f = line.split()
        if len(f) < 7 or not f[0].isdigit() or i + 1 >= len(lines):
            continue
        flags = {s.strip() for s in lines[i + 1].split(",")}
        if "ALLOC" not in flags:
            continue
        name, size, vma = f[1], int(f[2], 16), int(f[3], 16)
        if not size:
            continue
        loaded = "LOAD" in flags and "CONTENTS" in flags
        region = {".flash.appdesc": "flash rodata", ".noinit": "DRAM bss"}.get(name)
        if region is None:
            region = next((r for prefix, r in REGIONS if name.startswith(prefix)), None)
        # NOLOAD flash sections reserve address space but add no image bytes.
        if region is None or (region.startswith("flash ") and not loaded):
            continue
        out.append((vma, vma + size, region, name, loaded))
    return out


def sections(elf):
    return parse_sections(run(NM.replace("-nm", "-objdump"), "-h", elf))


def accounted_symbols(symbols):
    """Count aliases and partial overlaps once, independently in each section."""
    ends = defaultdict(int)
    for addr, size, name, region, loaded, section in sorted(symbols):
        end = addr + size
        count = max(0, end - max(addr, ends[section]))
        ends[section] = max(ends[section], end)
        yield count, name, region, loaded


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
        section = next((s for s in secs if s[0] <= addr < s[1]), None)
        if section:
            _, end, region, section_name, loaded = section
            syms.append((addr, min(size, end - addr), name, region, loaded, section_name))
    rust = [s[2] for s in syms if s[2].startswith(RUST_PREFIXES)]
    dem = {}
    if rust:
        p = subprocess.run([CPPFILT], input="\n".join(rust), capture_output=True, text=True, check=True)
        dem = dict(zip(rust, p.stdout.splitlines()))

    # Overlapping symbols (aliases) are counted once.
    table = defaultdict(lambda: defaultdict(int))
    owner_image = defaultdict(int)
    per_sym = []
    for size, name, region, loaded in accounted_symbols(syms):
        if name in owner:
            who = owner[name]
        elif name in dem:
            who = "rust " + rust_crate(dem[name])
        else:
            who = "(no archive: " + ("rust?" if name.startswith("_") else "C?") + ")"
        table[who][region] += size
        if loaded:
            owner_image[who] += size
        per_sym.append((size, who, region, dem.get(name, name)))

    totals = defaultdict(int)
    image = 0
    for s, e, r, _, loaded in secs:
        totals[r] += e - s
        if loaded:
            image += e - s
    regions = [r for _, r in REGIONS if totals[r]]
    attributed = defaultdict(int)
    for who in table:
        for r in regions:
            attributed[r] += table[who][r]
    for r in regions:
        table["(unattributed)"][r] = totals[r] - attributed[r]

    owner_image["(unattributed)"] = image - sum(owner_image.values())
    rows = sorted(table.items(), key=lambda kv: -owner_image[kv[0]])
    hdr = f"{'owner':<44}" + "".join(f"{r:>13}" for r in regions) + f"{'image %':>9}"
    print(hdr)
    print("-" * len(hdr))
    for who, v in rows[: a.top]:
        img = owner_image[who]
        print(f"{who[:44]:<44}" + "".join(f"{v[r]:>13}" for r in regions) + f"{100 * img / image:>8.1f}%")
    rest = rows[a.top :]
    if rest:
        v = defaultdict(int)
        for _, x in rest:
            for r in regions:
                v[r] += x[r]
        img = sum(owner_image[who] for who, _ in rest)
        print(f"{f'({len(rest)} more)':<44}" + "".join(f"{v[r]:>13}" for r in regions) + f"{100 * img / image:>8.1f}%")
    print(f"{'total':<44}" + "".join(f"{totals[r]:>13}" for r in regions))
    print(f"ELF loadable payload: {image} B (ESP image headers/alignment are additional)")
    if a.sym:
        print()
        for size, who, region, name in sorted((p for p in per_sym if p[1].startswith(a.prefix)), reverse=True)[: a.sym]:
            print(f"{size:>8} {region:<12} {who[:28]:<28} {name[:160]}")


if __name__ == "__main__":
    sys.exit(main())
