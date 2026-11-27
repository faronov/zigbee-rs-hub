#!/usr/bin/env python3
"""Release notes of a hub release (.github/workflows/hub-release.yml).

The notes are the release's section of CHANGELOG.md (`## X.Y.Z`; a
pre-release X.Y.Z-rc.N uses its own section if there is one, else X.Y.Z's),
then the install part generated from release-manifest.json: the asset table,
the esptool commands, when the release has it, the SONOFF Dongle Max
part, and the Home Assistant integration to use with it. GitHub's
generated list of merged pull requests follows them
(`gh release create --generate-notes`).

    tools/release-notes.py --manifest dist/release-manifest.json [--changelog CHANGELOG.md] [--require-changelog]
    tools/release-notes.py --changelog CHANGELOG.md --check 0.3.0
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parent.parent
HEADING = re.compile(r"^## \[?v?([0-9][0-9A-Za-z.+-]*)\]?(?:\s.*)?$")
# The Home Assistant integration has its own repository and releases (HACS);
# the notes name the version to use with this hub release. Raise it when a
# hub release needs a newer integration.
HA_INTEGRATION_REPO = "faronov/ha-zigbee-rs-hub"
HA_INTEGRATION_VERSION = "0.6.1"


class NotesError(Exception):
    pass


def changelog_section(text: str, version: str) -> str | None:
    """Body of `## <version>` (or of the base version for a pre-release), without its heading."""
    sections: dict[str, list[str]] = {}
    current = None
    for line in text.splitlines():
        m = HEADING.match(line)
        if m:
            current = m.group(1)
            sections.setdefault(current, [])
        elif line.startswith("## "):
            current = None
        elif current is not None:
            sections[current].append(line)
    for wanted in (version, version.split("-", 1)[0]):
        if wanted in sections:
            body = "\n".join(sections[wanted]).strip()
            return body or None
    return None


def install_notes(m: dict) -> list[str]:
    out = []
    s3 = [b for b in m["boards"] if b.get("web_install", True) and "merged" in b["files"]]
    dongle = [b for b in m["boards"] if b["id"] == "sonoff-dongle-max"]
    dongle_dev = [b for b in m["boards"] if b["id"] == "sonoff-dongle-max-devtools"]
    if s3:
        out.append("## Install\n")
        out.append("**Web flasher:** https://faronov.github.io/zigbee-rs/flasher/ (Chrome or Edge; updates keep "
                   "Wi-Fi settings and `hubdata` unless you choose to erase).\n")
        out.append("| Board | Layout | Full flash image (offset 0x0, erases everything) | App |")
        out.append("|---|---|---|---|")
        for b in s3:
            app = b["files"]["app"]
            out.append(f"| {b['title']} | `{b['layout']}` | `{b['files']['merged']['name']}` | "
                       f"`{app['name']}` at 0x{app['offset']:X} |")
        out.append("\nBoth flash sizes include the Zigbee2MQTT-compatible MQTT bridge. The 8 MB images also include "
                   "the advanced ZCL/Tuya console and the Zigbee device OTA server; the 4 MB images have neither.")
        if any(b.get("edition") == "devtools" for b in s3):
            out.append("\n**Developer edition** (`-8mb-devtools` files; its own entry in the web flasher): the 8 MB "
                       "image with the web UI's developer tools always on, without the developer mode switch. Same "
                       "layout as the 8 MB image: switching between the two keeps `nvs` and `hubdata`. For "
                       "developers; everyone else takes the standard image.")
        out.append("\nWith esptool, a first install (erases the S3):")
        out.append("```\nesptool --chip esp32s3 --port <PORT> write-flash 0x0 tbr-hub-<v>-<8mb|4mb-dev>[-opi]-merged.bin\n```")
        out.append("`-opi` images: modules with octal PSRAM (`esptool chip-id` shows \"Embedded PSRAM 8MB\" or 16MB: "
                   "WROOM-1 N4R8/N8R8/N16R8); the others: quad PSRAM (\"Embedded PSRAM 2MB\"/4MB: N4R2/N8R2, "
                   "S3FH4R2) or none. A wrong choice boots without PSRAM.")
        out.append("Update keeping `nvs` and `hubdata` (8 MB layout; the 4 MB dev layout has no otadata and its app "
                   "is at 0x10000):")
        out.append("```\nesptool --chip esp32s3 --port <PORT> write-flash 0x0 <...-bootloader.bin> 0x8000 "
                   "<...-partition-table.bin> 0xF000 <...-otadata-blank.bin> 0x20000 <...-app.bin>\n```")
    for b in dongle:
        app, otadata = b["files"]["app"], b["files"]["otadata_ota1"]
        out.append(f"\n## SONOFF Dongle Max (experimental: {b['hardware_status']})\n")
        out.append("The hub application on SONOFF's stock EmberZNet NCP, in `ota_1` of SONOFF's unchanged "
                   "partition table; SONOFF's bootloader, `nvs` and application (`ota_0`) stay. **Not offered "
                   "by the web flasher** (its erase option would wipe SONOFF's firmware and factory data).\n")
        doc = f"https://github.com/{m['repository']}/blob/{m['tag']}/docs/dongle-max.md"
        precheck = "\n".join(b.get("precheck", []))
        out.append("- **First, for either route** (over USB): back up the whole flash and check `otadata` and the "
                   "start of `ota_1`. Continue only if the check prints `OK`: sector 0 holds SONOFF's seq 1 (its "
                   "application in `ota_0`) and sector 1 is blank, or holds this release's otadata sector with a "
                   "`zrh-donglem` image in `ota_1` after a USB install. "
                   "Anything else (SONOFF's own updater moved its application to `ota_1`, `stock confirm`, a hub "
                   f"self-update): stop, see [docs/dongle-max.md]({doc}).")
        out.append("```\n"
                   "esptool --chip esp32 --port <PORT> read-flash 0 ALL dongle-max-backup.bin   # keep it private\n"
                   f"{precheck}\n"
                   "```")
        out.append(f"- **SONOFF's web console** (recommended): Firmware > ESP32-D0WDR2-V3 > Upload & Flash "
                   f"Firmware with `{app['name']}` (a plain ESP32 app image, like ESPHome's `.ota.bin`; SONOFF's "
                   "firmware writes it into the other slot, `ota_1`; not yet verified on hardware).")
        out.append("- **Or USB with esptool**: the app first, then the otadata sector; never erase the chip or "
                   "write a merged image:")
        out.append("```\n"
                   f"esptool --chip esp32 --port <PORT> write-flash 0x{app['offset']:X} {app['name']}\n"
                   f"esptool --chip esp32 --port <PORT> write-flash 0x{otadata['offset']:X} {otadata['name']}\n"
                   "```")
        out.append(f"  `{app['name']}` goes into `ota_1` (0x{app['offset']:X}); `{otadata['name']}` is `otadata` "
                   f"sector 1 (0x{otadata['offset']:X}, seq 2), which makes SONOFF's bootloader boot `ota_1`. "
                   "Back to SONOFF's firmware: console `stock confirm`, or the backup's 4 KiB at "
                   f"0x{otadata['offset']:X} written back. A hub already running is updated through Settings > "
                   "Hub firmware.")
        for d in dongle_dev:
            out.append(f"- **Developer edition:** `{d['files']['app']['name']}` instead of `{app['name']}`, the same "
                       "way (web console, or USB with the same otadata sector).")
        out.append("- First boot: the hub claims SONOFF's `files` partition for its data after copying SONOFF's "
                   "files aside (`stock confirm` writes them back); console at 115200 8N1 on the dongle's USB port "
                   f"(`token` prints the API token). Details: {doc}")
    for f in m.get("extra_files", []):
        out.append(f"\n`{f['name']}`: {f['role']}.")
    ha = f"https://github.com/{HA_INTEGRATION_REPO}"
    out.append(f"\n**Home Assistant integration:** install via HACS from [{HA_INTEGRATION_REPO}]({ha}) "
               f"(HACS > Custom repositories, type Integration), version "
               f"[{HA_INTEGRATION_VERSION}]({ha}/releases/tag/v{HA_INTEGRATION_VERSION}) or newer.")
    out.append("\nVerify downloads with `SHA256SUMS`; `release-manifest.json` describes every image.")
    return out


def notes(m: dict, changelog: str | None, require: bool) -> str:
    zrhp = f", ZRHP {m['zrhp']['protocol']}" if m.get("zrhp") else ""
    out = [f"zigbee-rs hub {m['version']}{zrhp}, commit `{m['commit'][:12]}`; "
           + ", ".join(f"{k} {v}" for k, v in m["components"].items()) + ".\n"]
    body = changelog_section(changelog, m["version"]) if changelog is not None else None
    if body is None and require:
        raise NotesError(f"CHANGELOG.md has no section for {m['version']}")
    if body:
        out += [body, ""]
    out += install_notes(m)
    return "\n".join(out) + "\n"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--manifest")
    parser.add_argument("--changelog", default=str(ROOT / "CHANGELOG.md"))
    parser.add_argument("--require-changelog", action="store_true")
    parser.add_argument("--check", metavar="VERSION", help="only check that CHANGELOG.md has the version's section")
    args = parser.parse_args()
    try:
        path = Path(args.changelog)
        changelog = path.read_text() if path.is_file() else None
        if args.check:
            if changelog is None or changelog_section(changelog, args.check) is None:
                raise NotesError(f"{args.changelog} has no section for {args.check}")
            print(f"{args.changelog}: section for {args.check} found")
            return
        if not args.manifest:
            parser.error("--manifest is required")
        sys.stdout.write(notes(json.loads(Path(args.manifest).read_text()), changelog, args.require_changelog))
    except (NotesError, OSError, KeyError, json.JSONDecodeError) as error:
        sys.exit(f"release-notes: {error}")


if __name__ == "__main__":
    main()
