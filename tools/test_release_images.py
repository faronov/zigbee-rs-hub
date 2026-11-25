import importlib.util
from pathlib import Path
import unittest
from unittest import mock

spec = importlib.util.spec_from_file_location(
    "release_images", Path(__file__).resolve().parent / "release-images.py"
)
release_images = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release_images)


class BuildMarkerTests(unittest.TestCase):
    def test_marker_gives_features_and_psram_mode(self):
        image = b"\xe9junk tbr-hub-build:features=advanced,mqtt;psram=octal;\0more"
        self.assertEqual(release_images.build_marker(image), (["advanced", "mqtt"], "octal"))
        image = b"tbr-hub-build:features=;psram=quad;"
        self.assertEqual(release_images.build_marker(image), ([], "quad"))
        image = b"tbr-hub-build:features=advanced,mqtt,ota,ota-store;psram=quad;"
        self.assertEqual(release_images.build_marker(image), (["advanced", "mqtt", "ota", "ota-store"], "quad"))

    def test_missing_or_conflicting_markers_are_refused(self):
        for image in [
            b"no marker",
            b"tbr-hub-build:features=mqtt;psram=quad; tbr-hub-build:features=mqtt;psram=octal;",
        ]:
            with self.subTest(image=image), self.assertRaises(release_images.ReleaseError):
                release_images.build_marker(image)


class BuildCheckTests(unittest.TestCase):
    @staticmethod
    def board(board_id):
        return next(b for b in release_images.BOARDS if b["id"] == board_id)

    def test_board_entries_pass_their_own_check(self):
        for spec in release_images.BOARDS:
            with self.subTest(board=spec["id"]):
                release_images.check_build(spec, list(spec["features"]), spec["psram"])

    def test_zigbee_ota_only_in_eight_mb_images(self):
        for board_id in ["thread-br-s3-hub-4mb-dev", "thread-br-s3-hub-4mb-dev-opi"]:
            spec = self.board(board_id)
            self.assertNotIn("ota", spec["features"])
            for features in [["mqtt", "ota"], ["mqtt", "ota", "ota-store"], ["mqtt", "ota-store"]]:
                with self.subTest(board=board_id, features=features), \
                        self.assertRaisesRegex(release_images.ReleaseError, "with Zigbee device OTA"):
                    release_images.check_build(spec, features, spec["psram"])
        for board_id in ["thread-br-s3-hub-8mb", "thread-br-s3-hub-8mb-opi"]:
            spec = self.board(board_id)
            self.assertIn("ota", spec["features"])
            self.assertIn("ota-store", spec["features"])
            with self.subTest(board=board_id), \
                    self.assertRaisesRegex(release_images.ReleaseError, "without Zigbee device OTA"):
                release_images.check_build(spec, ["advanced", "mqtt"], spec["psram"])
            # Streamed images only (no store) is not the 8 MB image either.
            with self.subTest(board=board_id), self.assertRaises(release_images.ReleaseError):
                release_images.check_build(spec, ["advanced", "mqtt", "ota"], spec["psram"])

    def test_developer_edition_is_its_own_group_and_marker(self):
        dev = [b for b in release_images.BOARDS if b.get("edition") == "devtools"]
        self.assertEqual([b["id"] for b in dev], ["thread-br-s3-hub-8mb-devtools", "thread-br-s3-hub-8mb-devtools-opi"])
        std = self.board("thread-br-s3-hub-8mb")
        for spec in dev:
            # Its own web flasher entry (never offered instead of the standard image), the 8 MB layout.
            self.assertEqual(spec["size_group"], "thread-br-s3-hub-devtools")
            self.assertIn("Developer edition", spec["size_group_title"])
            self.assertEqual((spec["layout"], spec["csv"]), (std["layout"], std["csv"]))
            with self.subTest(board=spec["id"]), self.assertRaises(release_images.ReleaseError):
                # A standard 8 MB image left in the devtools target directory.
                release_images.check_build(spec, list(std["features"]), spec["psram"])
        with self.assertRaises(release_images.ReleaseError):
            release_images.check_build(std, list(dev[0]["features"]), std["psram"])
        self.assertNotIn("size_group", std)

    def test_developer_edition_only_with_the_feature(self):
        with mock.patch.object(release_images, "has_feature", return_value=False):
            self.assertFalse(any(b.get("edition") for b in release_images.s3_boards()))
            self.assertEqual(len(release_images.s3_boards()), 4)
        with mock.patch.object(release_images, "has_feature", return_value=True):
            self.assertEqual(len(release_images.s3_boards()), 6)

    def test_other_features_and_psram_mode_must_match(self):
        spec = self.board("thread-br-s3-hub-4mb-dev")
        for features, psram in [(["mqtt", "advanced"], "quad"), ([], "quad"), (["mqtt"], "octal")]:
            with self.subTest(features=features, psram=psram), self.assertRaises(release_images.ReleaseError):
                release_images.check_build(spec, features, psram)


def esp_image(chip_id=0, mode=2, size_freq=0x40, min_rev=300, hash_appended=1, version=b"zrh-donglem-0.3.0"):
    """A synthetic app image: header, extended header, first segment header, esp_app_desc_t."""
    header = bytes([0xE9, 1, mode, size_freq]) + b"\0" * 4 + b"\xee" * 4 + chip_id.to_bytes(2, "little") + b"\0" \
        + min_rev.to_bytes(2, "little") + (399).to_bytes(2, "little") + b"\0" * 4 + bytes([hash_appended])
    desc = release_images.APP_DESC_MAGIC.to_bytes(4, "little") + b"\0" * 12 + version.ljust(32, b"\0")
    return header + b"\0" * 8 + desc + b"\0" * 64


class DongleImageTests(unittest.TestCase):
    def test_sonoff_bootloader_settings_pass(self):
        release_images.check_dongle_app(esp_image(), "0.3.0")
        self.assertEqual(release_images.image_header(esp_image())["flash_size"], "16MB")

    def test_wrong_version_chip_or_flash_settings_are_refused(self):
        for image, why in [
            (esp_image(version=b"zrh-donglem-0.2.0"), "reports"),
            (esp_image(version=b"tbr-hub-0.3.0"), "reports"),
            (esp_image(chip_id=9), "header"),            # an S3 image
            (esp_image(size_freq=0x4F), "header"),       # 80 MHz: SONOFF's bootloader runs 40 MHz
            (esp_image(size_freq=0x30), "header"),       # 8 MB
            (esp_image(mode=0), "header"),               # QIO
            (esp_image(min_rev=0), "header"),            # without CONFIG_ESP32_REV_MIN_3
            (esp_image(hash_appended=0), "header"),      # SONOFF's OTA checks the appended SHA-256
            (esp_image() + b"tbr-hub-build:features=mqtt;psram=quad;", "marker"),
        ]:
            with self.subTest(why=why, image=image[:24].hex()), self.assertRaisesRegex(release_images.ReleaseError, why):
                release_images.check_dongle_app(image, "0.3.0")

    def test_otadata_sector_selects_ota_1_over_sonoff_entry(self):
        sector = release_images.dongle_otadata()
        self.assertEqual(len(sector), release_images.OTADATA_SECTOR)
        self.assertEqual(int.from_bytes(sector[:4], "little"), 2)

    def test_board_entry_writes_only_ota_1_and_otadata_sector_1(self):
        parts = release_images.partitions(release_images.ROOT / release_images.DONGLE["csv"])
        app = {"name": "zrh-donglem-0.3.0-app.bin", "size": 2_960_000, "sha256": "0" * 64, "offset": parts["ota_1"][0]}
        otadata = {"name": "zrh-donglem-0.3.0-otadata-ota1.bin", "size": 0x1000, "sha256": "1" * 64,
                   "offset": parts["otadata"][0] + release_images.OTADATA_SECTOR}
        board = release_images.dongle_entry("0.3.0", parts, app, otadata)
        dev = release_images.dongle_entry("0.3.0", parts, {**app, "name": "zrh-donglem-0.3.0-devtools-app.bin"}, otadata, True)
        self.assertEqual((dev["id"], dev["edition"]), ("sonoff-dongle-max-devtools", "devtools"))
        self.assertIn("dev", dev["features"])
        self.assertIn("Developer edition", dev["title"])
        self.assertNotIn("edition", board)
        self.assertIs(board["web_install"], False)
        self.assertTrue(board["experimental"])
        self.assertEqual(board["flash"], ["app", "otadata_ota1"])
        self.assertEqual((board["files"]["app"]["offset"], board["files"]["otadata_ota1"]["offset"]), (0x660000, 0x5D000))
        self.assertEqual([p["name"] for p in board["preserved"]],
                         ["bootloader", "partition_table", "nvs", "otadata sector 0", "phy_init", "ota_0", "files"])
        for role in board["flash"]:
            f = board["files"][role]
            for kept in board["preserved"]:
                with self.subTest(role=role, kept=kept["name"]):
                    self.assertTrue(f["offset"] + f["size"] <= kept["offset"] or kept["offset"] + kept["size"] <= f["offset"])
        self.assertLessEqual(app["size"], parts["ota_1"][1])

if __name__ == "__main__":
    unittest.main()
