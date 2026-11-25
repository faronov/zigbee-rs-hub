import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib
import unittest


ROOT = Path(__file__).resolve().parent.parent
_spec = importlib.util.spec_from_file_location("release_images", ROOT / "tools/release-images.py")
release_images = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(release_images)


def marked_features(selected: str, manifest: Path = ROOT / "firmware/hub/Cargo.toml") -> list[str]:
    """The features firmware/hub/build.rs marks in an image built with
    `--features selected`: those and the tbr-hub features they imply,
    without `default` and `app`."""
    table = tomllib.loads(manifest.read_text())["features"]
    out, todo = set(), selected.split(",")
    while todo:
        f = todo.pop()
        # `dep:x` and `crate/feature` entries are not features of tbr-hub.
        if f in table and f not in out:
            out.add(f)
            todo.extend(table[f])
    return sorted(out - {"default", "app"})


class HubBuildTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / "tools").mkdir()
        (self.root / "firmware/hub").mkdir(parents=True)
        self.script = self.root / "tools/build-hub-firmware.sh"
        shutil.copyfile(ROOT / "tools/build-hub-firmware.sh", self.script)
        bindir = self.root / "bin"
        bindir.mkdir()
        cargo = bindir / "cargo"
        cargo.write_text("""#!/usr/bin/env python3
import json, os, pathlib, sys
with open(os.environ["CALL_LOG"], "a") as out:
    out.write(json.dumps({"args": sys.argv[1:], "target": os.environ["CARGO_TARGET_DIR"],
                          "sdkconfig": os.environ.get("ESP_IDF_SDKCONFIG_DEFAULTS")}) + "\\n")
elf = pathlib.Path(os.environ["CARGO_TARGET_DIR"]) / "xtensa-esp32s3-espidf/release/tbr-hub"
elf.parent.mkdir(parents=True, exist_ok=True)
elf.write_text(" ".join(sys.argv[1:]))
""")
        cargo.chmod(0o755)
        self.log = self.root / "calls.jsonl"
        self.env = {
            **os.environ,
            "PATH": str(bindir) + os.pathsep + os.environ["PATH"],
            "CALL_LOG": str(self.log),
        }

    def run_build(self, *args):
        result = subprocess.run(
            ["bash", str(self.script), *args], env=self.env, capture_output=True, text=True
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    @staticmethod
    def features(call):
        args = call["args"]
        return args[args.index("--features") + 1]

    def test_four_mb_builds_only_basic_quad_and_octal(self):
        calls = self.run_build("--layout", "4mb", "--locked", "--offline")
        self.assertEqual([self.features(c) for c in calls], ["mqtt", "mqtt"])
        self.assertEqual([Path(c["target"]).name for c in calls], ["target", "target-opi"])
        self.assertIsNone(calls[0]["sdkconfig"])
        self.assertTrue(calls[1]["sdkconfig"].endswith("firmware/hub/sdkconfig.octal"))
        self.assertTrue(all(c["args"][-2:] == ["--locked", "--offline"] for c in calls))
        self.assertFalse((self.root / "target-8mb").exists())

    def test_default_build_keeps_advanced_eight_mb_artifacts_separate(self):
        calls = self.run_build("--locked")
        self.assertEqual(
            [self.features(c) for c in calls],
            ["mqtt,advanced,ota-store", "mqtt", "mqtt,advanced,ota-store", "mqtt"],
        )
        for folder in ["target-8mb", "target-8mb-opi"]:
            elf = self.root / folder / "xtensa-esp32s3-espidf/release/tbr-hub"
            self.assertIn("mqtt,advanced,ota-store", elf.read_text())

    def test_release_board_features_are_what_the_build_selects(self):
        # release-images.py checks each image's marker against its board
        # entry: the entries must name the features this script builds
        # (with this tree's tbr-hub features: the developer edition once it
        # has `dev`).
        shutil.copyfile(ROOT / "firmware/hub/Cargo.toml", self.root / "firmware/hub/Cargo.toml")
        calls = self.run_build("--psram", "quad")
        stashes = ["target-8mb"] + (["target-8mb-devtools"] if len(calls) == 3 else []) + ["target"]
        self.assertEqual(len(calls), len(stashes))
        built = {d: marked_features(self.features(c)) for d, c in zip(stashes, calls)}
        boards = release_images.s3_boards()
        self.assertEqual({b["target"].removesuffix("-opi") for b in boards}, set(stashes))
        for spec in boards:
            with self.subTest(board=spec["id"]):
                release_images.check_build(spec, built[spec["target"].removesuffix("-opi")], spec["psram"])
        # Zigbee device OTA in the 8 MB images only.
        self.assertEqual(built["target-8mb"], ["advanced", "mqtt", "ota", "ota-store"])
        self.assertEqual(built["target"], ["mqtt"])

    def test_developer_edition_once_tbr_hub_has_dev(self):
        manifest = self.root / "firmware/hub/Cargo.toml"
        manifest.write_text('[features]\nadvanced = []\nmqtt = []\nota = []\nota-store = ["ota"]\n'
                            'dev = ["advanced", "hub-web/dev"]\n')
        calls = self.run_build("--locked")
        self.assertEqual([self.features(c) for c in calls], ["mqtt,advanced,ota-store", "mqtt,dev,ota-store", "mqtt"] * 2)
        for folder in ["target-8mb-devtools", "target-8mb-devtools-opi"]:
            elf = self.root / folder / "xtensa-esp32s3-espidf/release/tbr-hub"
            self.assertIn("mqtt,dev,ota-store", elf.read_text())
        # The 4 MB build stays last: target[-opi] holds the 4 MB ELF.
        self.assertEqual((self.root / "target/xtensa-esp32s3-espidf/release/tbr-hub").read_text().split()[-2:],
                         ["mqtt", "--locked"])
        spec = next(b for b in release_images.BOARDS if b["id"] == "thread-br-s3-hub-8mb-devtools")
        release_images.check_build(spec, marked_features("mqtt,dev,ota-store", manifest), "quad")
        self.log.unlink()
        calls = self.run_build("--layout", "devtools", "--psram", "octal")
        self.assertEqual([(self.features(c), Path(c["target"]).name) for c in calls], [("mqtt,dev,ota-store", "target-opi")])

    def test_devtools_layout_needs_the_feature(self):
        result = subprocess.run(["bash", str(self.script), "--layout", "devtools"], env=self.env,
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertIn("no feature dev", result.stderr)
        self.assertFalse(self.log.exists())

    def test_one_psram_mode_and_extra_features_reach_cargo(self):
        calls = self.run_build("--layout", "4mb", "--psram", "octal", "--", "--features", "advanced")
        self.assertEqual(len(calls), 1)
        self.assertEqual(self.features(calls[0]), "mqtt")
        self.assertEqual(calls[0]["args"][-2:], ["--features", "advanced"])
        self.assertTrue(calls[0]["sdkconfig"].endswith("sdkconfig.octal"))

    def test_selection_after_cargo_args_is_still_a_selection(self):
        calls = self.run_build("--locked", "--layout", "4mb", "--psram", "quad")
        self.assertEqual([self.features(c) for c in calls], ["mqtt"])
        self.assertEqual(calls[0]["args"][-1], "--locked")
        self.assertNotIn("--layout", calls[0]["args"])

    def test_invalid_selection_fails_before_building(self):
        for args in [("--layout", "16mb"), ("--psram", "4"), ("--layout",)]:
            with self.subTest(args=args):
                result = subprocess.run(
                    ["bash", str(self.script), *args], env=self.env, capture_output=True, text=True
                )
                self.assertEqual(result.returncode, 2)
                self.assertIn("error:", result.stderr)
                self.assertFalse(self.log.exists())


if __name__ == "__main__":
    unittest.main()
