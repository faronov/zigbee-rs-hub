#!/usr/bin/env python3
"""One ESP-IDF `otadata` sector that selects an OTA app slot.

The bootloader reads the two 4 KiB sectors of `otadata`, takes the valid
entry (CRC correct, state not INVALID/ABORTED) with the highest sequence
number and boots slot `(seq - 1) % <number of OTA slots>` (ESP-IDF
`bootloader_common_get_active_otadata`, `esp_ota_select_entry_t`). Writing
the sector that does not hold the current entry, with the next sequence
number, switches the slot without touching the other sector; erasing it
again switches back.

SONOFF Dongle Max (docs/dongle-max.md): the stock entry is sector 0
(0x5C000), seq 1 -> ota_0; `--seq 2` written to sector 1 (0x5D000) boots
ota_1.

    tools/make-otadata.py --seq 2 -o target/otadata-donglem-ota1.bin
"""

import argparse
import binascii
import struct
import sys

SECTOR = 0x1000
STATES = {"new": 0x0, "pending-verify": 0x1, "valid": 0x2, "undefined": 0xFFFFFFFF}


def entry(seq: int, state: int = STATES["valid"]) -> bytes:
    """`esp_ota_select_entry_t`: ota_seq, seq_label[20] (unused, 0xFF),
    ota_state, crc = ROM crc32_le(UINT32_MAX, &ota_seq, 4)."""
    if not 1 <= seq < 0xFFFFFFFF:
        raise ValueError("seq must be 1..0xFFFFFFFE")
    crc = binascii.crc32(struct.pack("<I", seq), 0xFFFFFFFF) & 0xFFFFFFFF
    return struct.pack("<I20sII", seq, b"\xff" * 20, state, crc)


def sector(seq: int, state: int = STATES["valid"]) -> bytes:
    e = entry(seq, state)
    return e + b"\xff" * (SECTOR - len(e))


def slot(seq: int, ota_slots: int = 2) -> int:
    return (seq - 1) % ota_slots


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--seq", type=int, required=True, help="sequence number (higher than the current entry's)")
    p.add_argument("--state", choices=sorted(STATES), default="valid")
    p.add_argument("--ota-slots", type=int, default=2)
    p.add_argument("-o", "--out", required=True)
    a = p.parse_args()
    data = sector(a.seq, STATES[a.state])
    with open(a.out, "wb") as f:
        f.write(data)
    print(f"{a.out}: seq {a.seq} ({a.state}) selects ota_{slot(a.seq, a.ota_slots)}; entry {data[:32].hex()}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
