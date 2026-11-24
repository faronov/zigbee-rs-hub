import importlib.util
import pathlib
import struct
import unittest

spec = importlib.util.spec_from_file_location("make_otadata", pathlib.Path(__file__).with_name("make-otadata.py"))
otadata = importlib.util.module_from_spec(spec)
spec.loader.exec_module(otadata)


class OtadataTest(unittest.TestCase):
    def test_stock_dongle_max_entry_is_reproduced(self):
        # Sector 0 of the stock Dongle Max otadata: seq 1, VALID, CRC 0x4743989A.
        want = struct.pack("<I", 1) + b"\xff" * 20 + struct.pack("<II", 2, 0x4743989A)
        self.assertEqual(otadata.entry(1), want)
        self.assertEqual(otadata.slot(1), 0)

    def test_next_sequence_selects_the_other_slot(self):
        s = otadata.sector(2)
        self.assertEqual(len(s), 0x1000)
        self.assertEqual(set(s[32:]), {0xFF})
        seq, state, crc = struct.unpack("<I20xII", s[:32])
        self.assertEqual((seq, state), (2, 2))
        self.assertNotEqual(crc, 0x4743989A)
        self.assertEqual(otadata.slot(seq), 1)

    def test_out_of_range_sequence_is_refused(self):
        for seq in (0, 0xFFFFFFFF):
            with self.assertRaises(ValueError):
                otadata.entry(seq)


if __name__ == "__main__":
    unittest.main()
