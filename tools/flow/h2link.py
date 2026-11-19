"""Shared helpers: bridge console, H2 ROM loader access through the bridge."""
import time

import serial
from esptool.targets import ESP32H2ROM

CON = '/dev/cu.usbmodem7CDFA1FE67F03'
DATA = '/dev/cu.usbmodem7CDFA1FE67F01'


class Console:
    def __init__(self, port=CON):
        self.s = serial.Serial(port, 115200, timeout=0.3)

    def __call__(self, line, wait=0.2):
        self.s.reset_input_buffer()
        self.s.write(line.encode() + b'\r')
        time.sleep(wait)
        return self.s.read(4000).decode(errors='replace')


def drain(port, quiet=0.3, limit=3.0):
    """Read and discard until the port is quiet (stale bytes from earlier sessions)."""
    t0 = time.time()
    old = port.timeout
    port.timeout = quiet
    while time.time() - t0 < limit and port.read(65536):
        pass
    port.timeout = old


def rom_loader(con, data):
    """Reset the H2 into its ROM loader through the bridge and connect esptool."""
    con('lines off')
    drain(data)
    con('h2 boot', wait=0.4)
    log = b''
    t0 = time.time()
    while b'waiting for download' not in log and time.time() - t0 < 2:
        log += data.read(4096)
    drain(data, quiet=0.1, limit=0.5)
    h2 = ESP32H2ROM(data)
    h2.connect('no_reset')
    return h2
