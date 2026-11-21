#!/usr/bin/env python3
"""Where an S3 app image goes, from the device's own partition table.

    s3_partition.py MANIFEST IMAGE [--slot NAME] [--layout CSV]

MANIFEST is the JSON hwlab writes next to a flash backup; its `partitions`
are parsed from the table on the device (0x8000). Prints the app partition's
offset (hex) for IMAGE and exits non-zero, with the reason, when:

- the device has no such app partition (`--slot`, default: `factory`, else
  `ota_0`),
- IMAGE is not an ESP app image (magic 0xE9) or does not fit the partition,
- `--layout` is given and the device's table differs from that CSV
  (`firmware/hub/partitions-*.csv`): flashing an app built for another
  layout would boot-loop.
"""

import argparse
import csv
import json
import sys

APP = 0
SUBTYPES = {
    ("app", "factory"): 0x00,
    ("data", "ota"): 0x00,
    ("data", "phy"): 0x01,
    ("data", "nvs"): 0x02,
    ("data", "coredump"): 0x03,
}
TYPES = {"app": 0, "data": 1}


class Refused(Exception):
    pass


def _int(text):
    return int(text.strip(), 0)


def parse_csv(path):
    """Partitions of an ESP-IDF partition CSV (explicit offsets only)."""
    parts = []
    with open(path, newline="") as f:
        for row in csv.reader(f):
            if not row or row[0].strip().startswith("#"):
                continue
            name, kind, sub, offset, size = (c.strip() for c in row[:5])
            if kind not in TYPES:
                raise Refused(f"{path}: {name}: type {kind!r}")
            if sub.startswith("ota_"):
                subtype = 0x10 + int(sub[4:])
            elif (kind, sub) in SUBTYPES:
                subtype = SUBTYPES[(kind, sub)]
            else:
                subtype = _int(sub)
            parts.append(
                {"label": name, "type": TYPES[kind], "subtype": subtype, "offset": _int(offset), "size": _int(size)}
            )
    return parts


def from_manifest(path):
    with open(path) as f:
        parts = json.load(f).get("partitions")
    if not parts:
        raise Refused(f"{path}: no partition table in the backup manifest")
    return parts


def choose_app(parts, slot=None):
    apps = {p["label"]: p for p in parts if p["type"] == APP}
    names = [slot] if slot else ["factory", "ota_0"]
    for name in names:
        if name in apps:
            return apps[name]
    raise Refused(f"no app partition {' / '.join(names)} on the device (has: {', '.join(apps) or 'none'})")


def same_layout(device, layout):
    key = lambda p: (p["label"], p["type"], p["subtype"], p["offset"], p["size"])  # noqa: E731
    return sorted(map(key, device)) == sorted(map(key, layout))


def check_image(image, part):
    with open(image, "rb") as f:
        data = f.read()
    if not data or data[0] != 0xE9:
        raise Refused(f"{image}: not an ESP app image (no 0xE9 magic)")
    if len(data) > part["size"]:
        raise Refused(f"{image}: {len(data)} B does not fit {part['label']} ({part['size']} B)")


def target(manifest, image, slot=None, layout=None):
    device = from_manifest(manifest)
    if layout is not None and not same_layout(device, parse_csv(layout)):
        table = ", ".join(f"{p['label']}@{p['offset']:#x}" for p in device)
        raise Refused(f"the device's partition table ({table}) is not {layout}")
    part = choose_app(device, slot)
    check_image(image, part)
    return part


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("manifest")
    ap.add_argument("image")
    ap.add_argument("--slot")
    ap.add_argument("--layout")
    a = ap.parse_args(argv)
    try:
        part = target(a.manifest, a.image, a.slot, a.layout)
    except (Refused, OSError, ValueError) as e:
        print(f"s3_partition: refused: {e}", file=sys.stderr)
        return 1
    print(f"{part['offset']:#x}")
    print(f"s3_partition: {a.image} -> {part['label']} at {part['offset']:#x} ({part['size']} B)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
