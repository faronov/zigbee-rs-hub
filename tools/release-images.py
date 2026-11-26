#!/usr/bin/env python3
"""Release images of the hub firmware: tbr-hub (ESP32-S3 of the Thread
Border Router board) for both flash layouts and zrh-donglem (SONOFF Dongle
Max, classic ESP32).

Used by .github/workflows/hub-release.yml after `cargo build --release` of
firmware/bridge, firmware/hub and firmware/donglem. Writes, for each S3
board, the bootloader (the ESP-IDF build's, with the board's flash size in
its header), the partition table, a blank otadata (8 MB A/B layout only),
the app and a full flash image padded to the flash size. For the Dongle Max
(SONOFF's partition table and bootloader are never written) only the app
for `ota_1` (also the file for SONOFF's web console) and the `otadata`
sector 1 that selects it. Then `release-manifest.json` and `SHA256SUMS`
for the GitHub release. The manifest format is the web flasher's input,
shared with zigbee-rs (book: Getting Started / Web Flasher); a board with
`"web_install": false` is listed by the flasher, never installed with ESP
Web Tools (whose erase option would wipe the dongle's stock firmware).

    tools/release-images.py --version 0.1.0 --commit SHA --tag hub-v0.1.0 --out dist
    tools/release-images.py ... --boards tbr      # S3 images and tbr-bridge only
    tools/release-images.py ... --boards donglem  # Dongle Max only
"""

from __future__ import annotations

import argparse
import datetime
import glob
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib

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
        # As marked in the image: `ota-store` implies `ota`.
        "features": ["mqtt", "advanced", "ota", "ota-store"],
        "note": "Production layout: two 3 MiB app slots (A/B OTA) and 1 920 KiB hubdata; "
        "with the Zigbee2MQTT-compatible MQTT bridge, advanced ZCL/Tuya console and "
        "the Zigbee device OTA server with its image store in the inactive app slot.",
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
        "esp-mqtt included, advanced console and Zigbee device OTA omitted.",
    },
    {
        # The developer edition ("devtools"): the 8 MB image with tbr-hub feature `dev`, built
        # once firmware/hub has it (tools/build-hub-firmware.sh). Its own entry in the web
        # flasher (`size_group`), never chosen instead of the standard image.
        "id": "thread-br-s3-hub-8mb-devtools",
        "title": "Espressif Thread Border Router board, S3 (8 MB): zigbee-rs hub, Developer edition (devtools)",
        "flash_size": "8MB",
        "layout": "partitions-8mb",
        "csv": "firmware/hub/partitions-8mb.csv",
        "app_partition": "ota_0",
        "max_app": 3 * 1024 * 1024,
        "target": "target-8mb-devtools",
        "features": ["mqtt", "advanced", "dev", "ota", "ota-store"],
        "edition": "devtools",
        "requires_feature": "dev",
        "size_group": "thread-br-s3-hub-devtools",
        "size_group_title": "Espressif Thread Border Router board, S3 (8 MB, octal/quad PSRAM, detected): "
        "zigbee-rs hub, Developer edition (devtools)",
        "note": "Developer edition of the 8 MB production image, for developers: the web UI's developer "
        "tools (the ZCL/Tuya console) always on, without the developer mode switch; otherwise the 8 MB "
        "image (MQTT, Zigbee device OTA with its store). Same layout: switching between the two keeps "
        "nvs and hubdata.",
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


def has_feature(manifest: str, feature: str) -> bool:
    """Whether a firmware's Cargo.toml declares a feature (the developer edition: `dev`)."""
    return feature in tomllib.loads((ROOT / manifest).read_text()).get("features", {})


def s3_boards() -> list[dict]:
    """The S3 board entries this tree builds: the developer edition once tbr-hub has `dev`."""
    return [b for b in BOARDS if "requires_feature" not in b or has_feature("firmware/hub/Cargo.toml", b["requires_feature"])]


# Zigbee device OTA (tbr-hub feature `ota`; `ota-store` implies it) is in
# the 8 MB images only.
ZIGBEE_OTA_LAYOUTS = {"partitions-8mb"}
PRESERVED = {
    "nvs": "Wi-Fi credentials, boot mode, API token",
    "hubdata": "device registry, settings, ZRHP host store (network backup)",
}


# SONOFF Dongle Max (docs/dongle-max.md): the hub in `ota_1` of SONOFF's
# unchanged partition table, booted by SONOFF's bootloader.
DONGLE_STOCK_TABLE_SHA256 = "639d27a0418b260e0242fa6d4c93514bf5273c2ffd22f6229066ebac21f3ca66"
DONGLE_TABLE_SIZE = 0xC00
DONGLE = {
    "id": "sonoff-dongle-max",
    "title": "SONOFF Dongle Max (Dongle-M, MG24): zigbee-rs hub on SONOFF's EmberZNet NCP (experimental)",
    "csv": "firmware/donglem/partitions.csv",
    "app_partition": "ota_1",
    "flash_size": "16MB",
    "flash_freq": "40m",
    # CONFIG_ESP32_REV_MIN_3: chip v3.0 up to ESP-IDF's maximum (v3.99).
    "min_rev_full": 300,
    "layout": "sonoff-dongle-m-stock",
    "psram": "quad",
    # firmware/gateway/Cargo.toml (no marker in the image): `ota-store` implies `ota`.
    "features": ["ezsp", "mqtt", "advanced", "ota", "ota-store"],
    "hardware_status": "bring-up tested; the hub runs on one lab unit; full hub HIL pending",
    "note": "Experimental: the hub application on SONOFF's stock EmberZNet NCP (EZSP backend), with "
    "SONOFF's partition table, bootloader and application (ota_0) kept. The hub runs on one lab unit "
    "(network formed, a sleepy device joined, interviewed and reporting); the rest of its hardware "
    "checklist is pending (a network backup and same-network restore ran on it; a migration import "
    "has not). No Green Power, no MG24 firmware update, at most 8 groups on the hub's endpoint. "
    "Not installable through the web flasher: "
    "ESP Web Tools could erase the whole chip, which destroys SONOFF's firmware and factory data.",
    "connect": "Plug the dongle into USB (CP2102N, \"SONOFF Dongle Max MG24\"); its DTR/RTS reset the ESP32 "
    "into the ROM loader, as esptool expects.",
}


# esp_app_desc_t.version in an app image: after the 24-byte image header and the first segment's
# 8-byte header (the app description is the first segment's start), at offset 16 of the description.
APP_DESC_VERSION = 0x30


def dongle_precheck(otadata_off: int, otadata_size: int, ota1_off: int, ota1_file: str) -> list[str]:
    """Commands that check the dongle before either install route. Sector 0 of otadata must hold
    SONOFF's seq 1 (its application in ota_0), and sector 1 be blank, or hold this release's
    sector with a zrh-donglem image in ota_1 (a hub installed with the USB parts). SONOFF's own
    updater puts its application into ota_1 with seq 2: once that application confirmed itself
    the entry is byte-identical to ours, so the app version at the start of ota_1 tells them
    apart. Stock confirm or a hub self-update (seq 3 and up) leave other entries. In those states
    the parts would overwrite the running application or not be booted."""
    return [
        f"esptool --chip esp32 --port <PORT> read-flash 0x{otadata_off:X} 0x{otadata_size:X} otadata.bin",
        f"esptool --chip esp32 --port <PORT> read-flash 0x{ota1_off:X} 0x100 ota1.bin",
        "python3 -c \"d=open('otadata.bin','rb').read(); s=open('" + ota1_file + "','rb').read(); "
        "a=open('ota1.bin','rb').read(); "
        "print('OK' if d[:4]==bytes([1,0,0,0]) and (d[4096:]==bytes([255])*4096 or "
        f"(d[4096:]==s and a[{APP_DESC_VERSION:#x}:{APP_DESC_VERSION + 12:#x}]==b'zrh-donglem-')) "
        "else 'STOP: not the stock otadata or no hub in ota_1, see docs/dongle-max.md')\"",
    ]


def dongle_install_steps(precheck: list[str]) -> list[str]:
    """Shown by the web flasher instead of an install button (it renders the esptool writes from `flash`)."""
    return [
        "First, for either route, over USB: back up the whole flash (esptool read-flash 0 ALL; it holds "
        "the dongle's credentials, keep it private) and check otadata and the start of ota_1: "
        + "; then ".join(precheck) + ". "
        "Continue only on OK (sector 0: SONOFF's seq 1, its application in ota_0; sector 1 blank, or this "
        "release's otadata sector with a zrh-donglem image in ota_1 after a USB install). Anything else "
        "(SONOFF's own updater moved its application to ota_1, stock confirm, a hub self-update): stop, "
        "see docs/dongle-max.md.",
        "Recommended: SONOFF's web console of the dongle (http://<dongle address>/), Firmware > "
        "ESP32-D0WDR2-V3 > Upload & Flash Firmware, with the app file. SONOFF's firmware writes it into "
        "the other slot, ota_1, and selects it (not yet verified on hardware).",
        "Or over USB with esptool (below): write the app first, then the otadata sector. Never erase the "
        "chip and never write a merged image: SONOFF's bootloader, partition table, nvs and files must stay.",
        "SONOFF's firmware comes back with the console command stock confirm, or by writing the backup's "
        "otadata sector 1 (0x5D000, 4 KiB) back. A hub already running is updated through Settings > Hub "
        "firmware in its web UI.",
        "First boot: console on the same port at 115200 8N1 (token prints the API token); the web UI "
        "is at http://<address>/ once Ethernet (DHCP) or Wi-Fi (wifi set <ssid> <password>) is up.",
    ]


# What the Dongle Max install leaves alone: (name, partition or (offset, size), what).
PRESERVED_DONGLE = [
    ("bootloader", (0x1000, 0x7000), "SONOFF's ESP-IDF v5.2.1 second-stage bootloader"),
    ("partition_table", (0x8000, 0x1000), "SONOFF's stock table, unchanged"),
    ("nvs", "nvs", "SONOFF's settings and credentials; the hub adds its own namespace zrh"),
    ("otadata sector 0", "otadata", "SONOFF's entry selecting ota_0"),
    ("phy_init", "phy_init", "RF calibration data"),
    ("ota_0", "ota_0", "SONOFF's application, kept until the hub's first self-update"),
    ("files", "files", "SONOFF's LittleFS (config, device info, MG24 factory data); at its first boot "
     "the hub copies SONOFF's blocks into a reserved area of it and keeps its own data there "
     "(stock confirm writes them back)"),
]
OTADATA_SECTOR = 0x1000


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


def app_build_time(image: bytes) -> str:
    """esp_app_desc_t time and date (empty with CONFIG_APP_COMPILE_TIME_DATE=n)."""
    desc = 24 + 8
    return " ".join(image[desc + o:desc + o + 16].split(b"\0", 1)[0].decode() for o in (80, 96)).strip()


BUILD_MARKER = re.compile(rb"tbr-hub-build:features=([a-z0-9,-]*);psram=(quad|octal);")


def build_marker(image: bytes) -> tuple[list[str], str]:
    """Features and PSRAM mode tbr-hub marks in its image (src/resources.rs)."""
    found = set(BUILD_MARKER.findall(image))
    if len(found) != 1:
        raise ReleaseError(f"expected one tbr-hub build marker in the app image, found {len(found)}")
    features, psram = found.pop()
    return [f for f in features.decode().split(",") if f], psram.decode()


def check_build(spec: dict, features: list[str], psram: str) -> None:
    """The features and PSRAM mode marked in an image against its board entry."""
    ota = "ota" in features or "ota-store" in features
    if ota != (spec["layout"] in ZIGBEE_OTA_LAYOUTS):
        raise ReleaseError(f"{spec['id']}: image built {'with' if ota else 'without'} Zigbee device OTA "
                           f"(features {features}); it belongs to the 8 MB images only")
    if sorted(features) != sorted(spec["features"]) or psram != spec["psram"]:
        raise ReleaseError(f"{spec['id']}: image built with features {features} for {psram} PSRAM, "
                           f"the board entry says {spec['features']} for {spec['psram']} "
                           "(rebuild with tools/build-hub-firmware.sh)")


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
    check_build(spec, *build_marker(app.read_bytes()))
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
        **{k: spec[k] for k in ("edition", "size_group", "size_group_title") if k in spec},
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


# esptool's image header fields (esp_image_header_t).
FLASH_MODES = {0: "qio", 1: "qout", 2: "dio", 3: "dout"}
FLASH_SIZES = {0: "1MB", 1: "2MB", 2: "4MB", 3: "8MB", 4: "16MB", 5: "32MB", 6: "64MB", 7: "128MB"}
FLASH_FREQS = {0: "40m", 1: "26m", 2: "20m", 0xF: "80m"}
CHIP_IDS = {0: "esp32", 9: "esp32s3"}


def image_header(image: bytes) -> dict:
    """Chip, flash parameters and revision range in an ESP app image header."""
    if len(image) < 24 or image[0] != 0xE9:
        raise ReleaseError("not an ESP app image")
    return {
        "chip": CHIP_IDS.get(int.from_bytes(image[12:14], "little"), f"id {int.from_bytes(image[12:14], 'little')}"),
        "flash_mode": FLASH_MODES.get(image[2], f"mode {image[2]}"),
        "flash_size": FLASH_SIZES.get(image[3] >> 4, f"size {image[3] >> 4}"),
        "flash_freq": FLASH_FREQS.get(image[3] & 0xF, f"freq {image[3] & 0xF}"),
        "min_rev_full": int.from_bytes(image[15:17], "little"),
        "hash_appended": image[23] == 1,
    }


def check_dongle_app(image: bytes, version: str) -> None:
    """What SONOFF's bootloader and its web console upload need (docs/dongle-max.md)."""
    reported = app_version(image)
    if reported != f"zrh-donglem-{version}":
        raise ReleaseError(f"zrh-donglem reports {reported!r}, the release is {version}")
    if BUILD_MARKER.search(image):
        raise ReleaseError("the Dongle Max image carries a tbr-hub build marker")
    header = image_header(image)
    want = {"chip": "esp32", "flash_mode": "dio", "flash_size": DONGLE["flash_size"],
            "flash_freq": DONGLE["flash_freq"], "min_rev_full": DONGLE["min_rev_full"], "hash_appended": True}
    if header != want:
        raise ReleaseError(f"zrh-donglem image header {header}, SONOFF's bootloader needs {want}")


def dongle_otadata() -> bytes:
    """The otadata sector 1 entry that selects ota_1 over SONOFF's seq 1 in sector 0 (tools/make-otadata.py)."""
    spec = importlib.util.spec_from_file_location("make_otadata", ROOT / "tools/make-otadata.py")
    otadata = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(otadata)
    if otadata.slot(2) != 1 or otadata.slot(1) != 0:
        raise ReleaseError("otadata seq 2 must select ota_1")
    return otadata.sector(2)


def build_dongle(version: str, out: Path, devtools: bool = False) -> dict:
    """The Dongle Max entry; `devtools`: its developer edition (zrh-donglem feature `dev`, built
    into target-devtools/ by hub-release.yml), sharing the otadata sector."""
    spec = DONGLE
    if cargo_version("firmware/donglem/Cargo.toml") != version:
        raise ReleaseError(f"firmware/donglem/Cargo.toml is at {cargo_version('firmware/donglem/Cargo.toml')}, "
                           f"the release is {version} (the hub images of one release share its version)")
    parts = partitions(ROOT / spec["csv"])
    app_off, slot = parts[spec["app_partition"]]
    otadata_off, otadata_size = parts["otadata"]
    if otadata_size != 2 * OTADATA_SECTOR:
        raise ReleaseError(f"Dongle Max otadata is {otadata_size} B, expected two sectors")
    table = ROOT / "target/partitions-donglem.bin"
    table.parent.mkdir(parents=True, exist_ok=True)
    run("tools/make-partition-table.sh", spec["csv"], str(table), spec["flash_size"])
    if sha256(table) != DONGLE_STOCK_TABLE_SHA256 or table.stat().st_size != DONGLE_TABLE_SIZE:
        raise ReleaseError(f"{spec['csv']} no longer generates SONOFF's stock partition table")
    stem = f"zrh-donglem-{version}"
    app = out / f"{stem}{'-devtools' if devtools else ''}-app.bin"
    env = {"CHIP": "esp32", "FLASH_SIZE": spec["flash_size"], "FLASH_FREQ": spec["flash_freq"],
           "MIN_REV_FULL": str(spec["min_rev_full"]), "MAX_APP_BYTES": str(slot)}
    if devtools:
        elf = ROOT / "target-devtools/xtensa-esp32-espidf/release/zrh-donglem"
        if not elf.is_file():
            raise ReleaseError(f"{elf.relative_to(ROOT)} missing (build zrh-donglem --features dev first, see hub-release.yml)")
        env["TARGET_DIR"] = str(ROOT / "target-devtools")
    run("tools/make-app-image.sh", "release", str(app), "zrh-donglem", env=env)
    check_dongle_app(app.read_bytes(), version)
    otadata = out / f"{stem}-otadata-ota1.bin"
    otadata.write_bytes(dongle_otadata())
    return dongle_entry(version, parts, file_entry(app, app_off), file_entry(otadata, otadata_off + OTADATA_SECTOR),
                        devtools)


def dongle_entry(version: str, parts: dict[str, tuple[int, int]], app: dict, otadata: dict,
                 devtools: bool = False) -> dict:
    spec = DONGLE
    preserved = []
    for name, where, what in PRESERVED_DONGLE:
        offset, size = where if isinstance(where, tuple) else parts[where]
        if name.startswith("otadata"):
            size = OTADATA_SECTOR
        preserved.append({"name": name, "offset": offset, "size": size, "what": what})
    edition = {"edition": "devtools"} if devtools else {}
    otadata_off, otadata_size = parts["otadata"]
    precheck = dongle_precheck(otadata_off, otadata_size, parts[spec["app_partition"]][0], otadata["name"])
    return {
        "id": spec["id"] + ("-devtools" if devtools else ""),
        "title": spec["title"].replace(" (experimental)", ", Developer edition (devtools) (experimental)")
        if devtools else spec["title"],
        "product": "zigbee-rs-hub",
        "firmware": "zrh-donglem",
        "chip": "esp32",
        "chip_family": "ESP32",
        "flash_size": spec["flash_size"],
        "layout": spec["layout"],
        "psram": spec["psram"],
        "features": sorted(spec["features"] + ["dev"]) if devtools else spec["features"],
        **edition,
        "experimental": True,
        "hardware_status": spec["hardware_status"],
        # Never through ESP Web Tools: its "Erase device" (and the erase manifest) wipes the
        # whole chip, SONOFF's bootloader, table, nvs and factory data with it.
        "web_install": False,
        "note": spec["note"],
        "connect": spec["connect"],
        "install_steps": dongle_install_steps(precheck),
        # Before either route: read otadata and go on only on OK (dongle_precheck).
        "precheck": precheck,
        "app_version": f"zrh-donglem-{version}",
        "files": {
            "app": {**app, "partition": spec["app_partition"],
                    "use": "SONOFF's web console (Upload & Flash Firmware) or USB"},
            "otadata_ota1": {**otadata, "partition": "otadata",
                             "use": "USB only: otadata sector 1, seq 2, selects ota_1"},
        },
        # USB write order: the app first, so a failed write leaves SONOFF's application selected.
        "flash": ["app", "otadata_ota1"],
        "stock_partition_table": {"offset": 0x8000, "size": DONGLE_TABLE_SIZE, "sha256": DONGLE_STOCK_TABLE_SHA256},
        "preserved": preserved,
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


def hub_elfs(tbr: bool, donglem: bool) -> list[tuple[str, Path]]:
    """(chip, ELF) of every hub image the selected boards release: the tbr-hub and zrh-donglem
    builds, whose Rust heap is in PSRAM, so their Rust code must not use S32C1I
    (tools/check-s32c1i.sh). tbr-bridge keeps no Rust data in PSRAM and is not listed."""
    out = []
    if tbr:
        for target in dict.fromkeys(spec["target"] for spec in s3_boards()):
            out.append(("esp32s3", ROOT / target / "xtensa-esp32s3-espidf/release/tbr-hub"))
    if donglem:
        out.append(("esp32", ROOT / "target/xtensa-esp32-espidf/release/zrh-donglem"))
        if has_feature("firmware/donglem/Cargo.toml", "dev"):
            out.append(("esp32", ROOT / "target-devtools/xtensa-esp32-espidf/release/zrh-donglem"))
    return out


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--list-elfs", action="store_true",
                        help="print `chip path` of every hub ELF the selected boards release, then exit")
    parser.add_argument("--version")
    parser.add_argument("--tag")
    parser.add_argument("--commit")
    parser.add_argument("--out")
    parser.add_argument("--date")
    parser.add_argument("--repository", default=os.environ.get("GITHUB_REPOSITORY", "faronov/zigbee-rs-hub"))
    parser.add_argument("--toolchain", default="{}")
    parser.add_argument("--boards", choices=["all", "tbr", "donglem"], default="all",
                        help="tbr: the S3 images and tbr-bridge; donglem: the SONOFF Dongle Max (default: all)")
    args = parser.parse_args()
    tbr = args.boards in ("all", "tbr")
    donglem = args.boards in ("all", "donglem")
    if args.list_elfs:
        for chip, elf in hub_elfs(tbr, donglem):
            print(chip, elf.relative_to(ROOT))
        return
    missing = [f"--{k}" for k in ("version", "tag", "commit", "out") if getattr(args, k) is None]
    if missing:
        parser.error("the following arguments are required: " + ", ".join(missing))
    try:
        if cargo_version("firmware/hub/Cargo.toml") != args.version:
            raise ReleaseError("firmware/hub/Cargo.toml version differs from the tag")
        out = Path(args.out)
        out.mkdir(parents=True, exist_ok=True)
        boards, components, extra_files = [], {}, []
        if tbr:
            release_boards = s3_boards()
            boot_src = idf_bootloader()
            boards += [build_board(spec, args.version, out, boot_src) for spec in release_boards]
            bridge_version = cargo_version("firmware/bridge/Cargo.toml")
            bridge = out / f"tbr-bridge-{bridge_version}-app.bin"
            run("tools/make-app-image.sh", "release", str(bridge), "tbr-bridge", env={"FLASH_SIZE": "4MB"})
            if app_version(bridge.read_bytes()) != f"tbr-bridge-{bridge_version}":
                raise ReleaseError(f"tbr-bridge reports {app_version(bridge.read_bytes())!r}, Cargo.toml {bridge_version}")
            # Reproducible: one name, one image across releases (firmware/bridge/sdkconfig.defaults).
            if app_build_time(bridge.read_bytes()):
                raise ReleaseError(f"tbr-bridge carries a build time ({app_build_time(bridge.read_bytes())}): "
                                   "CONFIG_APP_COMPILE_TIME_DATE must stay off")
            components["tbr-bridge (bridge mode)"] = bridge_version
            extra_files.append({"role": "tbr-bridge app (stock Espressif layout, ota_0 at 0x20000)",
                                **file_entry(bridge, None)})
        if donglem:
            boards.append(build_dongle(args.version, out))
            if has_feature("firmware/donglem/Cargo.toml", "dev"):
                boards.append(build_dongle(args.version, out, devtools=True))
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
            "components": {**components, "zigbee-rs": rev[:12], "ESP-IDF": "v5.5.1"},
            "toolchain": json.loads(args.toolchain),
            "extra_files": extra_files,
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
