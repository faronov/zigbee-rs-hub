#!/usr/bin/env python3
"""Catalog-scale size estimate from measure.cjs's proxy.json.

The proxy is the generator *input* per zhc definition (identity strings,
extend calls with their arguments, named converter references, exposes, Tuya
DP tables). It is encoded the way docs/device-support.md section 8 encodes
definitions: CBOR with small integer map keys and a shared string table for
strings that occur at least 3 times catalog-wide; definitions are then
packed into 4 KiB raw-deflate blocks as in a bank (section 9.1).

It approximates the size of the translated definitions (the generator
output carries about the same information: generic extends become implicit
tier-a entities, m.numeric & co. become entity records of similar size).

usage: python3 proxy_size.py <out dir of measure.cjs>
needs: cbor2 (pip install cbor2)
"""
import collections
import json
import statistics
import sys
import zlib

import cbor2

BLOCK = 4096          # uncompressed bytes per bank block (9.1)
INDEX_ENTRY = 12      # bytes per index key (9.1)
SHARED_MIN = 3        # occurrences for a string to go into the shared table


def main(out_dir):
    proxy = json.load(open(f"{out_dir}/proxy.json"))
    bodies = [{k: v for k, v in d.items() if k not in ("tier", "file")} for d in proxy]

    key_count, str_count = collections.Counter(), collections.Counter()

    def walk(v):
        if isinstance(v, dict):
            for k, x in v.items():
                key_count[k] += 1
                walk(x)
        elif isinstance(v, list):
            for x in v:
                walk(x)
        elif isinstance(v, str):
            str_count[v] += 1

    for b in bodies:
        walk(b)
    keys = {k: i for i, (k, _) in enumerate(key_count.most_common())}
    table = [s for s, n in str_count.most_common() if n >= SHARED_MIN]
    sid = {s: i for i, s in enumerate(table)}

    def enc(v):
        if isinstance(v, dict):
            return {keys[k]: enc(x) for k, x in v.items() if x is not None}
        if isinstance(v, list):
            return [enc(x) for x in v]
        if isinstance(v, str) and v in sid:
            return cbor2.CBORTag(6, sid[v])   # shared-string reference (8.4)
        return v

    blobs = collections.defaultdict(list)
    keys_per_tier = collections.Counter()
    for d, b in zip(proxy, bodies):
        blobs[d["tier"]].append(cbor2.dumps(enc(b), canonical=True))
        keys_per_tier[d["tier"]] += len(d.get("zm") or []) + len(d.get("fp") or [])

    def block_deflate(items):
        total, cur = 0, b""
        for b in items:
            if cur and len(cur) + len(b) > BLOCK:
                total += len(zlib.compress(cur, 9)) - 6   # raw deflate: no zlib header/adler
                cur = b""
            cur += b
        if cur:
            total += len(zlib.compress(cur, 9)) - 6
        return total

    table_raw = cbor2.dumps(table)
    table_z = len(zlib.compress(table_raw, 9)) - 6
    print(f"shared string table: {len(table)} strings, {len(table_raw) // 1024} KiB raw, {table_z // 1024} KiB deflated\n")
    print("| Tiers | Definitions | Mean CBOR B | Median | Raw KiB | Block-deflated KiB | Index KiB | Bank total KiB |")
    print("|---|---:|---:|---:|---:|---:|---:|---:|")
    for tiers in ("A", "B", "C", "D", "AB", "ABC", "ABCD"):
        items = [b for t in tiers for b in blobs[t]]
        sizes = [len(b) for b in items]
        nkeys = sum(keys_per_tier[t] for t in tiers)
        bd = block_deflate(items)
        idx = nkeys * INDEX_ENTRY
        print(f"| {tiers} | {len(items)} | {statistics.mean(sizes):.0f} | {statistics.median(sizes):.0f} | "
              f"{sum(sizes) // 1024} | {bd // 1024} | {idx // 1024} | {(bd + table_z + idx) // 1024} |")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
