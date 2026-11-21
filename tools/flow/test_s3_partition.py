"""tools/flow/s3_partition.py against the repository's partition CSVs."""

import json
import os
import tempfile
import unittest

import s3_partition as sp

ROOT = os.path.join(os.path.dirname(__file__), "..", "..")
DEV = os.path.join(ROOT, "firmware/hub/partitions-4mb-dev.csv")
PROD = os.path.join(ROOT, "firmware/hub/partitions-8mb.csv")


class S3PartitionTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)

    def file(self, name, data):
        path = os.path.join(self.dir.name, name)
        with open(path, "wb" if isinstance(data, bytes) else "w") as f:
            f.write(data)
        return path

    def manifest(self, csv_path):
        return self.file("backup.bin.json", json.dumps({"partitions": sp.parse_csv(csv_path)}))

    def test_dev_layout_flashes_factory_at_0x10000(self):
        image = self.file("app.bin", b"\xe9" + bytes(1000))
        part = sp.target(self.manifest(DEV), image, layout=DEV)
        self.assertEqual((part["label"], part["offset"]), ("factory", 0x10000))

    def test_production_layout_flashes_ota_0_at_0x20000(self):
        image = self.file("app.bin", b"\xe9" + bytes(1000))
        part = sp.target(self.manifest(PROD), image, layout=PROD)
        self.assertEqual((part["label"], part["offset"]), ("ota_0", 0x20000))

    def test_refuses_a_table_that_is_not_the_given_layout(self):
        image = self.file("app.bin", b"\xe9" + bytes(1000))
        with self.assertRaisesRegex(sp.Refused, "is not"):
            sp.target(self.manifest(DEV), image, layout=PROD)

    def test_refuses_missing_slot_oversized_and_non_app_images(self):
        manifest = self.manifest(DEV)
        small = self.file("app.bin", b"\xe9" + bytes(1000))
        with self.assertRaisesRegex(sp.Refused, "no app partition ota_1"):
            sp.target(manifest, small, slot="ota_1")
        with self.assertRaisesRegex(sp.Refused, "does not fit"):
            sp.target(manifest, self.file("big.bin", b"\xe9" + bytes(0x2E0000)))
        with self.assertRaisesRegex(sp.Refused, "0xE9"):
            sp.target(manifest, self.file("raw.bin", b"\x00" + bytes(10)))

    def test_cli_prints_the_offset(self):
        image = self.file("app.bin", b"\xe9" + bytes(10))
        self.assertEqual(sp.main([self.manifest(DEV), image, "--layout", DEV]), 0)
        self.assertEqual(sp.main([self.manifest(DEV), image, "--layout", PROD]), 1)


if __name__ == "__main__":
    unittest.main()
