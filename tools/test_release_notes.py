import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("release_notes", Path(__file__).resolve().parent / "release-notes.py")
release_notes = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release_notes)

CHANGELOG = """# Changelog

Intro text.

## 0.4.0-rc.1

Release candidate notes.

## [0.3.0] - 2026-11-25

### Highlights

- Crash fix.

## 0.2.0

Old.
"""


def entry(name, offset):
    return {"name": name, "size": 4096, "sha256": "0" * 64, "offset": offset}


MANIFEST = {
    "version": "0.3.0", "tag": "hub-v0.3.0", "repository": "faronov/zigbee-rs-hub", "commit": "a" * 40,
    "zrhp": {"protocol": "1.0"}, "components": {"tbr-bridge (bridge mode)": "0.2.4", "ESP-IDF": "v5.5.1"},
    "extra_files": [{"name": "tbr-bridge-0.2.4-app.bin", "role": "tbr-bridge app"}],
    "boards": [
        {"id": "thread-br-s3-hub-8mb", "title": "TBR S3 (8 MB)", "layout": "partitions-8mb",
         "files": {"merged": entry("tbr-hub-0.3.0-8mb-merged.bin", 0), "app": entry("tbr-hub-0.3.0-8mb-app.bin", 0x20000)}},
        {"id": "sonoff-dongle-max", "title": "SONOFF Dongle Max", "layout": "sonoff-dongle-m-stock",
         "web_install": False, "hardware_status": "bring-up tested; full hub HIL pending",
         "precheck": ["esptool --chip esp32 --port <PORT> read-flash 0x5C000 0x2000 otadata.bin",
                      "esptool --chip esp32 --port <PORT> read-flash 0x660000 0x100 ota1.bin", "python3 -c CHECK"],
         "files": {"app": entry("zrh-donglem-0.3.0-app.bin", 0x660000),
                   "otadata_ota1": entry("zrh-donglem-0.3.0-otadata-ota1.bin", 0x5D000)}},
    ],
}


class ChangelogTests(unittest.TestCase):
    def test_section_of_the_version_and_of_the_base_version_for_a_prerelease(self):
        self.assertEqual(release_notes.changelog_section(CHANGELOG, "0.3.0"), "### Highlights\n\n- Crash fix.")
        self.assertEqual(release_notes.changelog_section(CHANGELOG, "0.3.0-rc.2"), "### Highlights\n\n- Crash fix.")
        self.assertEqual(release_notes.changelog_section(CHANGELOG, "0.4.0-rc.1"), "Release candidate notes.")
        self.assertIsNone(release_notes.changelog_section(CHANGELOG, "0.4.0"))
        self.assertIsNone(release_notes.changelog_section(CHANGELOG, "0.3"))

    def test_a_tagged_release_needs_its_section(self):
        with self.assertRaisesRegex(release_notes.NotesError, "no section for 0.3.0"):
            release_notes.notes(MANIFEST, "## 0.2.0\n\nOld.\n", require=True)
        self.assertIn("## Install", release_notes.notes(MANIFEST, None, require=False))


class NotesTests(unittest.TestCase):
    def test_changelog_first_then_the_asset_table_and_the_dongle_usb_parts(self):
        text = release_notes.notes(MANIFEST, CHANGELOG, require=True)
        self.assertLess(text.index("- Crash fix."), text.index("## Install"))
        self.assertIn("| TBR S3 (8 MB) | `partitions-8mb` | `tbr-hub-0.3.0-8mb-merged.bin` | "
                      "`tbr-hub-0.3.0-8mb-app.bin` at 0x20000 |", text)
        # The dongle is never a row of the full-flash-image table, and its USB parts are written app first.
        self.assertNotIn("| SONOFF Dongle Max |", text)
        app = text.index("write-flash 0x660000 zrh-donglem-0.3.0-app.bin")
        self.assertLess(app, text.index("write-flash 0x5D000 zrh-donglem-0.3.0-otadata-ota1.bin"))
        # The otadata check comes before both routes (the web console's and USB's writes).
        check = text.index("read-flash 0x5C000 0x2000 otadata.bin\n"
                           "esptool --chip esp32 --port <PORT> read-flash 0x660000 0x100 ota1.bin\npython3 -c CHECK")
        self.assertLess(check, text.index("**SONOFF's web console**"))
        self.assertLess(check, app)
        self.assertIn("experimental: bring-up tested; full hub HIL pending", text)
        # The Home Assistant integration: its own repository and the release to use.
        ha, v = "https://github.com/faronov/ha-zigbee-rs-hub", release_notes.HA_INTEGRATION_VERSION
        self.assertIn(f"**Home Assistant integration:** install via HACS from [faronov/ha-zigbee-rs-hub]({ha}) "
                      f"(HACS > Custom repositories, type Integration), version [{v}]({ha}/releases/tag/v{v})",
                      text)
        self.assertNotIn("erase-flash", text)
        self.assertIn("`tbr-bridge-0.2.4-app.bin`: tbr-bridge app.", text)

    def test_developer_edition_rows_and_note(self):
        dev = {"id": "thread-br-s3-hub-8mb-devtools", "title": "TBR S3 (8 MB), Developer edition (devtools)",
               "layout": "partitions-8mb", "edition": "devtools",
               "files": {"merged": entry("tbr-hub-0.3.0-8mb-devtools-merged.bin", 0),
                         "app": entry("tbr-hub-0.3.0-8mb-devtools-app.bin", 0x20000)}}
        text = release_notes.notes({**MANIFEST, "boards": MANIFEST["boards"] + [dev]}, CHANGELOG, require=True)
        self.assertIn("| TBR S3 (8 MB), Developer edition (devtools) | `partitions-8mb` | "
                      "`tbr-hub-0.3.0-8mb-devtools-merged.bin` |", text)
        self.assertIn("**Developer edition** (`-8mb-devtools` files", text)
        self.assertNotIn("Developer edition", release_notes.notes(MANIFEST, CHANGELOG, require=True))

    def test_release_without_the_dongle(self):
        m = {**MANIFEST, "boards": MANIFEST["boards"][:1]}
        self.assertNotIn("Dongle Max", release_notes.notes(m, CHANGELOG, require=True))


if __name__ == "__main__":
    unittest.main()
