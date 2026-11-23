#!/usr/bin/env python3
"""Release images of the S3 hub firmware (tbr-hub) for both flash layouts.

Used by .github/workflows/hub-release.yml after `cargo build --release` of
firmware/bridge and firmware/hub. Writes, for each board, the bootloader
(the ESP-IDF build's, with the board's flash size in its header), the
partition table, a blank otadata (8 MB A/B layout only), the app and a full
flash image padded to the flash size, then `release-manifest.json` and
`SHA256SUMS` for the GitHub release. The manifest format is the web
flasher's input, shared with zigbee-rs (book: Getting Started / Web
Flasher).

    tools/release-images.py --version 0.1.0 --commit SHA --tag hub-v0.1.0 --out dist
"""

from __future__ import annotations

import argparse
import datetime
import glob
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
TARGET = ROOT / "target/xtensa-esp32s3-espidf/release"
SCHEMA = 1
APP_DESC_MAGIC = 0xABCD5432

CONNECT = ("Connect the S3's USB port: USB2 on the board (USB1 is the H2's). "
           "A board running Espressif's firmware shows the S3's USB-Serial-JTAG port directly.")
CONNECT_STEPS = [
    "After the install the S3 boots tbr-hub in bridge mode (the default without a stored mode); "
    "the console commands mode hub and wifi set <ssid> <password> (second serial port) make it a hub.",
]
LAYOUTS = [
    {
        "id": "thread-br-s3-hub-8mb",
        "title": "Espressif Thread Border Router board, S3 (8 MB): zigbee-rs hub",
        "flash_size": "8MB",
        "layout": "partitions-8mb",
        "csv": "firmware/hub/partitions-8mb.csv",
        "app_partition": "ota_0",
        "max_app": 3 * 1024 * 1024,
        "target": "target-8mb",
        "features": ["mqtt", "advanced", "ota-store"],
        "note": "Production layout: two 3 MiB app slots (A/B OTA) and 1 920 KiB hubdata; "
        "with the Zigbee2MQTT-compatible MQTT bridge, advanced ZCL/Tuya console and "
        "the Zigbee device OTA image store in the inactive app slot.",
    },
    {
        "id": "thread-br-s3-hub-4mb-dev",
        "title": "Espressif Thread Border Router board, S3 (4 MB): zigbee-rs hub, dev layout",
        "flash_size": "4MB",
        "layout": "partitions-4mb-dev",
        "csv": "firmware/hub/partitions-4mb-dev.csv",
        "app_partition": "factory",
        "max_app": 2944 * 1024,
        "target": "target",
        "features": ["mqtt"],
        "note": "Development layout: one 2 944 KiB app slot, no hub A/B OTA, 1 MiB hubdata; "
        "esp-mqtt included, advanced console omitted; Zigbee device OTA with streamed images only "
        "(no second app slot for an image store).",
    },
]
# ESP-IDF fixes the PSRAM mode at build time. The web flasher picks the
# variant by the chip's embedded PSRAM (eFuse PSRAM_CAP, esptool's
# "Embedded PSRAM 8MB"); a wrong pick boots without PSRAM: the hub core keeps
# the network, the application is not started and the web page says which
# image to flash.
PSRAM_VARIANTS = [
    {"psram": "quad", "suffix": "", "title": "",
     "match": {"embedded_psram_mb": [0, 2, 4]},
     "note": "Quad PSRAM (WROOM-1 N4R2/N8R2, S3FH4R2) or none."},
    {"psram": "octal", "suffix": "-opi", "title": ", octal PSRAM",
     "match": {"embedded_psram_mb": [8, 16]},
     "note": "Octal PSRAM (WROOM-1 N4R8/N8R8/N16R8, ESP32-S3R8/R16V die)."},
]
BOARDS = [
    {**layout, "id": layout["id"] + v["suffix"], "title": layout["title"] + v["title"],
     "note": layout["note"] + " " + v["note"], "psram": v["psram"], "match": v["match"],
     # Cargo target dir of the image's build (tools/build-hub-firmware.sh).
     "target": layout["target"] + ("-opi" if v["psram"] == "octal" else "")}
    for layout in LAYOUTS
    for v in PSRAM_VARIANTS
]
PRESERVED = {
    "nvs": "Wi-Fi credentials, boot mode, API token",
    "hubdata": "device registry, settings, ZRHP host store (network backup)",
}


class ReleaseError(Exception):
    pass


def run(*cmd: str, env: dict | None = None) -> None:
    print("+", " ".join(cmd), flush=True)
    subprocess.run(cmd, check=True, cwd=ROOT, env={**os.environ, **(env or {})})


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def file_entry(path: Path, offset: int | None) -> dict:
    entry = {"name": path.name, "size": path.stat().st_size, "sha256": sha256(path)}
    if offset is not None:
        entry["offset"] = offset
    return entry


def partitions(csv: Path) -> dict[str, tuple[int, int]]:
    out = {}
    for line in csv.read_text().splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            f = [x.strip() for x in line.split(",")]
            out[f[0]] = (int(f[3], 0), int(f[4], 0))
    return out


def app_version(image: bytes) -> str:
    desc = 24 + 8
    if image[0] != 0xE9 or int.from_bytes(image[desc:desc + 4], "little") != APP_DESC_MAGIC:
        raise ReleaseError("no esp_app_desc_t in the app image")
    return image[desc + 16:desc + 48].split(b"\0", 1)[0].decode()


BUILD_MARKER = re.compile(rb"tbr-hub-build:features=([a-z0-9,-]*);psram=(quad|octal);")


def build_marker(image: bytes) -> tuple[list[str], str]:
    """Features and PSRAM mode tbr-hub marks in its image (src/resources.rs)."""
    found = set(BUILD_MARKER.findall(image))
    if len(found) != 1:
        raise ReleaseError(f"expected one tbr-hub build marker in the app image, found {len(found)}")
    features, psram = found.pop()
    return [f for f in features.decode().split(",") if f], psram.decode()


def idf_bootloader() -> Path:
    """The bootloader of the tbr-hub ESP-IDF build (the esp-idf-sys output
    whose sdkconfig names the hub)."""
    found = []
    for out in glob.glob(str(TARGET / "build/esp-idf-sys-*/out")):
        sdkconfig = Path(out, "sdkconfig")
        boot = Path(out, "build/bootloader/bootloader.bin")
        if boot.is_file() and sdkconfig.is_file() and 'CONFIG_APP_PROJECT_VER="tbr-hub-' in sdkconfig.read_text():
            found.append(boot)
    if len(found) != 1:
        raise ReleaseError(f"expected one tbr-hub ESP-IDF bootloader, found {found}")
    return found[0]


def cargo_version(manifest: str) -> str:
    return re.search(r'^version = "([^"]+)"', (ROOT / manifest).read_text(), re.M).group(1)


def build_board(spec: dict, version: str, out: Path, boot_src: Path) -> dict:
    stem = f"tbr-hub-{version}-{spec['id'].removeprefix('thread-br-s3-hub-')}"
    size = spec["flash_size"]
    parts = partitions(ROOT / spec["csv"])
    app_off, slot = parts[spec["app_partition"]]
    if slot < spec["max_app"]:
        raise ReleaseError(f"{spec['id']}: slot {slot} smaller than the checked limit")
    app = out / f"{stem}-app.bin"
    table = out / f"{stem}-partition-table.bin"
    boot = out / f"{stem}-bootloader.bin"
    merged = out / f"{stem}-merged.bin"
    env = {"FLASH_SIZE": size, "MAX_APP_BYTES": str(slot), "TARGET_DIR": str(ROOT / spec["target"])}
    if "elf" in spec:
        # In the variant's target directory (built first, see hub-release.yml).
        elf = ROOT / spec["target"] / spec["elf"]
        if not elf.is_file():
            raise ReleaseError(f"{spec['id']}: {elf.relative_to(ROOT)} missing (build it first, see hub-release.yml)")
        env["ELF"] = str(elf)
    run("tools/make-app-image.sh", "release", str(app), "tbr-hub", env=env)
    run("tools/make-partition-table.sh", spec["csv"], str(table), size)
    # The bootloader configures the flash from its own header: give it the board's size.
    run("esptool", "--chip", "esp32s3", "merge-bin", "--flash-size", size, "-o", str(boot), "0x0", str(boot_src))
    files = {
        "bootloader": file_entry(boot, 0),
        "partition_table": file_entry(table, 0x8000),
    }
    flash = ["bootloader", "partition_table"]
    images = ["0x0", str(boot), "0x8000", str(table)]
    if "otadata" in parts:
        otadata = out / f"{stem}-otadata-blank.bin"
        otadata.write_bytes(b"\xff" * parts["otadata"][1])
        files["otadata"] = file_entry(otadata, parts["otadata"][0])
        flash.append("otadata")
    files["app"] = file_entry(app, app_off)
    flash.append("app")
    images += [hex(app_off), str(app)]
    run("esptool", "--chip", "esp32s3", "merge-bin", "--flash-size", size, "--pad-to-size", size,
        "-o", str(merged), *images)
    files = {"merged": file_entry(merged, 0), **files}

    data = merged.read_bytes()
    for role in flash:
        item = files[role]
        piece = (out / item["name"]).read_bytes()
        if data[item["offset"]:item["offset"] + len(piece)] != piece:
            raise ReleaseError(f"{spec['id']}: {role} differs inside the merged image")
    reported = app_version(app.read_bytes())
    if reported != f"tbr-hub-{version}":
        raise ReleaseError(f"app reports {reported!r}, the tag says tbr-hub-{version}")
    features, psram = build_marker(app.read_bytes())
    if sorted(features) != sorted(spec["features"]) or psram != spec["psram"]:
        raise ReleaseError(f"{spec['id']}: image built with features {features} for {psram} PSRAM, "
                           f"the board entry says {spec['features']} for {spec['psram']} "
                           "(rebuild with tools/build-hub-firmware.sh)")
    return {
        "id": spec["id"],
        "title": spec["title"],
        "product": "zigbee-rs-hub",
        "firmware": "tbr-hub",
        "chip": "esp32s3",
        "chip_family": "ESP32-S3",
        "flash_size": size,
        "layout": spec["layout"],
        "psram": spec["psram"],
        "features": spec["features"],
        "match": spec["match"],
        "note": spec["note"],
        "connect": CONNECT,
        "connect_steps": CONNECT_STEPS,
        "app_version": reported,
        "files": files,
        "flash": flash,
        "preserved": [{"name": name, "offset": parts[name][0], "size": parts[name][1], "what": what}
                      for name, what in PRESERVED.items()],
        "fw_update": None,
    }


def zigbee_rs_rev() -> str:
    lock = (ROOT / "Cargo.lock").read_text()
    m = re.search(r'name = "zigbee-zrhp"\nversion = "[^"]+"\nsource = "git\+[^#]+#([0-9a-f]{40})"', lock)
    if m is None:
        raise ReleaseError("zigbee-zrhp source revision not found in Cargo.lock")
    return m.group(1)


def zrhp_protocol(rev: str) -> str | None:
    for checkout in glob.glob(os.path.expanduser(f"~/.cargo/git/checkouts/zigbee-rs-*/{rev[:7]}/zigbee-zrhp/src/lib.rs")):
        text = Path(checkout).read_text()
        major = re.search(r"PROTOCOL_MAJOR: u8 = (\d+);", text)
        minor = re.search(r"PROTOCOL_MINOR: u8 = (\d+);", text)
        if major and minor:
            return f"{major.group(1)}.{minor.group(1)}"
    return None


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--version", required=True)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--date")
    parser.add_argument("--repository", default=os.environ.get("GITHUB_REPOSITORY", "faronov/zigbee-rs-hub"))
    parser.add_argument("--toolchain", default="{}")
    args = parser.parse_args()
    try:
        if cargo_version("firmware/hub/Cargo.toml") != args.version:
            raise ReleaseError("firmware/hub/Cargo.toml version differs from the tag")
        out = Path(args.out)
        out.mkdir(parents=True, exist_ok=True)
        boot_src = idf_bootloader()
        boards = [build_board(spec, args.version, out, boot_src) for spec in BOARDS]
        bridge_version = cargo_version("firmware/bridge/Cargo.toml")
        bridge = out / f"tbr-bridge-{bridge_version}-app.bin"
        run("tools/make-app-image.sh", "release", str(bridge), "tbr-bridge", env={"FLASH_SIZE": "4MB"})
        if app_version(bridge.read_bytes()) != f"tbr-bridge-{bridge_version}":
            raise ReleaseError(f"tbr-bridge reports {app_version(bridge.read_bytes())!r}, Cargo.toml {bridge_version}")
        rev = zigbee_rs_rev()
        protocol = zrhp_protocol(rev)
        manifest = {
            "schema": SCHEMA,
            "product": "zigbee-rs-hub",
            "name": "zigbee-rs hub",
            "repository": args.repository,
            "tag": args.tag,
            "version": args.version,
            "prerelease": "-" in args.version,
            "commit": args.commit,
            "build_id": args.commit[:16],
            "date": args.date or datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            "zrhp": {"protocol": protocol} if protocol else None,
            "components": {"tbr-bridge (bridge mode)": bridge_version, "zigbee-rs": rev[:12], "ESP-IDF": "v5.5.1"},
            "toolchain": json.loads(args.toolchain),
            "extra_files": [{"role": "tbr-bridge app (stock Espressif layout, ota_0 at 0x20000)",
                             **file_entry(bridge, None)}],
            "boards": boards,
        }
        (out / "release-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        names = sorted(p.name for p in out.iterdir() if p.is_file() and p.name != "SHA256SUMS")
        (out / "SHA256SUMS").write_text("".join(f"{sha256(out / n)}  {n}\n" for n in names))
        print(json.dumps({"tag": args.tag, "boards": [b["id"] for b in boards], "zrhp": protocol}))
    except (ReleaseError, OSError, subprocess.CalledProcessError) as error:
        sys.exit(f"release-images: {error}")


if __name__ == "__main__":
    main()
