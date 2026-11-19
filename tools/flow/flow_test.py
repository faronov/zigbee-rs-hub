#!/usr/bin/env python3
"""Throughput and byte loss on the S3<->H2 UART, with and without RTS/CTS.

Runs the RAM-only test program (h2-uart-test/) on the H2 through the bridge:
nothing is written to H2 flash; `h2 reset` at the end boots its firmware.
Usage: flow_test.py [baud ...]
"""
import struct, sys, time

import serial
from esptool.cmds import load_ram

import h2link

IMAGE = __file__.rsplit('/', 1)[0] + '/h2-uart-test/h2-uart-test.bin'
N_BULK = 256 * 1024
N_STALL = 128 * 1024
STALL_EVERY, STALL_MS = 4096, 30
PAUSE_CHUNK, PAUSE_S = 32 * 1024, 1.0


def pattern(n):
    return bytes(((i + (i >> 8) + (i >> 16)) & 0xFF) for i in range(n))


def line(data, prefix, timeout=10):
    buf = b''
    t0 = time.time()
    while time.time() - t0 < timeout:
        buf += data.read(256)
        for l in buf.split(b'\n'):
            if l.startswith(prefix) and buf.endswith(b'\n'):
                return l.decode()
    raise TimeoutError(f'no {prefix!r}: {buf[-200:]!r}')


def fields(l):
    return {k: int(v) for k, v in (kv.split('=') for kv in l.split()[1:])}


def h2_receive(data, n, every=0, ms=0):
    data.reset_input_buffer()
    data.write(b'R' + struct.pack('<III', n, every, ms))
    t0 = time.time()
    while data.read(1) != b'+':
        if time.time() - t0 > 3:
            raise TimeoutError('no ready marker')
    payload = pattern(n)
    t0 = time.time()
    data.write(payload)
    data.flush()
    r = fields(line(data, b'R ', timeout=60))
    r['secs'] = time.time() - t0
    r['lost'] = n - r['got']
    sync(data)
    return r


def sync(data):
    """Wait until both directions are idle and the H2 answers again."""
    time.sleep(0.3)
    data.reset_input_buffer()
    data.write(b'P')
    line(data, b'PONG', timeout=3)


def h2_transmit(data, n, pause_every=0, pause_s=0.0):
    data.reset_input_buffer()
    data.write(b'T' + struct.pack('<I', n))
    t0 = time.time()
    got = bytearray()
    idle = time.time()
    while len(got) < n and time.time() - idle < 2.0:
        want = n - len(got)
        if pause_every:
            want = min(want, pause_every - len(got) % pause_every)
        chunk = data.read(want)
        if chunk:
            got += chunk
            idle = time.time()
            if pause_every and len(got) % pause_every == 0 and len(got) < n:
                time.sleep(pause_s)
                idle = time.time()
    secs = time.time() - t0
    exp = pattern(n)
    first_bad = next((i for i in range(min(len(got), n)) if got[i] != exp[i]), None)
    tail = line(data, b'T ', timeout=5) if len(got) >= n else 'T sent=?'
    sync(data)
    return {'got': len(got), 'lost': n - len(got), 'first_bad': first_bad, 'secs': secs, 'tail': tail}


def main(bauds):
    con = h2link.Console()
    data = serial.Serial(h2link.DATA, 115200, timeout=0.2)
    h2 = h2link.rom_loader(con, data)
    print('H2:', h2.get_chip_description())
    con('flow off')
    load_ram(h2, IMAGE)
    time.sleep(0.2)
    data.write(b'P')
    print(line(data, b'PONG'))
    rows = []
    try:
        for baud in bauds:
            for flow in (0, 1):
                con('flow on' if flow else 'flow off')
                data.write(b'F' + bytes([flow]))
                line(data, b'OK flow')
                data.write(b'B' + struct.pack('<I', baud))
                line(data, b'OK')
                time.sleep(0.05)
                data.baudrate = baud
                time.sleep(0.05)
                data.write(b'P')
                line(data, b'PONG')
                con('stats reset')
                tests = [
                    ('S3->H2 bulk', lambda: h2_receive(data, N_BULK)),
                    (f'S3->H2, H2 stalls {STALL_MS} ms / {STALL_EVERY} B',
                     lambda: h2_receive(data, N_STALL, STALL_EVERY, STALL_MS)),
                    ('H2->S3 bulk', lambda: h2_transmit(data, N_BULK)),
                    (f'H2->S3, host pauses {PAUSE_S}s / {PAUSE_CHUNK // 1024} KiB',
                     lambda: h2_transmit(data, N_BULK, PAUSE_CHUNK, PAUSE_S)),
                ]
                for name, fn in tests:
                    r = fn()
                    kbs = r['got'] / r['secs'] / 1024
                    rows.append((baud, flow, name, r, kbs))
                    print(f'{baud:>8} flow={flow} {name:<40} {kbs:7.1f} KiB/s  {r}')
                info = con('info', wait=0.4)
                print('   bridge:', ' | '.join(l for l in info.splitlines() if 'uart' in l or 'flow' in l))
                data.write(b'B' + struct.pack('<I', 115200))
                line(data, b'OK')
                time.sleep(0.05)
                data.baudrate = 115200
                time.sleep(0.05)
    finally:
        data.close()
        print(con('h2 reset').strip().splitlines()[-1:])
        con('lines on')
        con('flow on')
    return rows


if __name__ == '__main__':
    main([int(b) for b in sys.argv[1:]] or [460800, 921600, 2000000])
