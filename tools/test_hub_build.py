import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parent.parent


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
            ["mqtt,advanced", "mqtt", "mqtt,advanced", "mqtt"],
        )
        for folder in ["target-8mb", "target-8mb-opi"]:
            elf = self.root / folder / "xtensa-esp32s3-espidf/release/tbr-hub"
            self.assertIn("mqtt,advanced", elf.read_text())

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
