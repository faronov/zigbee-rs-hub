import importlib.util
from pathlib import Path
import unittest

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

    def test_other_features_and_psram_mode_must_match(self):
        spec = self.board("thread-br-s3-hub-4mb-dev")
        for features, psram in [(["mqtt", "advanced"], "quad"), ([], "quad"), (["mqtt"], "octal")]:
            with self.subTest(features=features, psram=psram), self.assertRaises(release_images.ReleaseError):
                release_images.check_build(spec, features, psram)


if __name__ == "__main__":
    unittest.main()
