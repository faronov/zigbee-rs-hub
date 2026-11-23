import importlib.util
from pathlib import Path
import unittest


spec = importlib.util.spec_from_file_location("size_report", Path(__file__).with_name("size-report.py"))
report = importlib.util.module_from_spec(spec)
spec.loader.exec_module(report)


class SizeReportTests(unittest.TestCase):
    def test_noload_flash_is_not_an_image_but_bss_is_still_ram(self):
        text = """
  0 .flash.appdesc 00000100 3c000000 3c000000 00001000 2**4
                  CONTENTS, ALLOC, LOAD, READONLY, DATA
  1 .flash.rodata 00000008 3c000100 3c000100 00001100 2**4
                  CONTENTS, ALLOC, LOAD, DATA
  2 .flash.rodata_noload 00000020 3c000108 3c000108 00001108 2**0
                  ALLOC, READONLY
  3 .dram0.bss 00000010 3fc00000 3fc00000 00001108 2**3
                  ALLOC
  4 .iram0.text_end 00000004 40300000 40300000 00001108 2**0
                  ALLOC
  5 .debug_info 00001000 00000000 00000000 00001108 2**0
                  CONTENTS, READONLY, DEBUGGING, OCTETS
"""
        sections = report.parse_sections(text)
        self.assertEqual(sum(e - s for s, e, _, _, load in sections if load), 264)
        self.assertEqual({name for _, _, _, name, _ in sections},
                         {".flash.appdesc", ".flash.rodata", ".dram0.bss", ".iram0.text_end"})
        self.assertIn((0x3FC00000, 0x3FC00010, "DRAM bss", ".dram0.bss", False), sections)

    def test_partial_overlaps_keep_the_original_covered_end(self):
        symbols = [
            (10, 10, "a", "flash code", True, ".flash.text"),
            (15, 10, "b", "flash code", True, ".flash.text"),
            (22, 5, "c", "flash code", True, ".flash.text"),
            (10, 10, "alias", "flash code", True, ".flash.text"),
        ]
        counted = list(report.accounted_symbols(symbols))
        self.assertEqual(sum(row[0] for row in counted), 17)
        self.assertEqual(next(row[0] for row in counted if row[1] == "c"), 2)


if __name__ == "__main__":
    unittest.main()
