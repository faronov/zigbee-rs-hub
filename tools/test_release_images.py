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

    def test_missing_or_conflicting_markers_are_refused(self):
        for image in [
            b"no marker",
            b"tbr-hub-build:features=mqtt;psram=quad; tbr-hub-build:features=mqtt;psram=octal;",
        ]:
            with self.subTest(image=image), self.assertRaises(release_images.ReleaseError):
                release_images.build_marker(image)


if __name__ == "__main__":
    unittest.main()
