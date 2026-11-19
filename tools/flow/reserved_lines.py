#!/usr/bin/env python3
"""Check which reserved S3<->H2 links (R18-R20) are fitted.

Puts the H2 into its ROM loader through the bridge console, drives S3 GPIO4/5/6
from the bridge and reads the H2 GPIO_IN register with esptool `read_reg`
through the bridge data port; then the reverse direction (H2 drives with
ROM WRITE_REG, the S3 senses). Nothing is written to any flash.
"""
import serial

import h2link

H2_GPIO_IN = 0x6009103C
H2_IO_MUX = {4: 0x60090014, 5: 0x60090018, 12: 0x60090034}
H2_GPIO_ENABLE_W1TS, H2_GPIO_ENABLE_W1TC = 0x60091024, 0x60091028
H2_GPIO_OUT_W1TS, H2_GPIO_OUT_W1TC = 0x60091008, 0x6009100C
H2_FUNC_OUT_SEL = lambda n: 0x60091554 + 4 * n
PAIRS = [(6, 12, 'R18'), (5, 4, 'R19'), (4, 5, 'R20')]  # (S3 gpio, H2 gpio, link)

con = h2link.Console()
data = serial.Serial(h2link.DATA, 115200, timeout=0.3)
cmd = con
rom = h2link.rom_loader(con, data)
print("connected:", rom.get_chip_description())


def read_mem(addr):
    return rom.read_reg(addr)


def write_mem(addr, val, mask=0xFFFFFFFF):
    rom.write_reg(addr, val, mask)


for g, a in H2_IO_MUX.items():
    v = read_mem(a)
    print(f'H2 IO_MUX GPIO{g} = 0x{v:08x} (FUN_IE={(v >> 9) & 1}, MCU_SEL={(v >> 12) & 7}, '
          f'PU={(v >> 8) & 1}, PD={(v >> 7) & 1})')
    if not (v >> 9) & 1:
        write_mem(a, v | (1 << 9))  # input enable only, so GPIO_IN reflects the pad
        print(f'  set FUN_IE on H2 GPIO{g}')
print('H2 GPIO_ENABLE =', hex(read_mem(0x60091020)))

print('\n# S3 drives, H2 reads')
results = {}
for s3, h2, link in PAIRS:
    seen = []
    for level in ('low', 'high', 'low'):
        cmd(f'gpio {s3} {level}')
        v = read_mem(H2_GPIO_IN)
        seen.append((v >> h2) & 1)
    others = [p for p in PAIRS if p[0] != s3]
    ok = seen == [0, 1, 0]
    results[link] = ok
    print(f'{link}: S3 GPIO{s3} low/high/low -> H2 GPIO{h2} = {seen} -> {"CONNECTED" if ok else "no"}')
    cmd(f'gpio {s3} pd')

print('\n# H2 drives (ROM WRITE_REG), S3 senses with its pull opposite to the driven level')
for s3, h2, link in PAIRS:
    seen = []
    write_mem(H2_FUNC_OUT_SEL(h2), 0x80 | (1 << 9))  # simple GPIO out, OE from GPIO_ENABLE
    write_mem(H2_IO_MUX[h2], (read_mem(H2_IO_MUX[h2]) & ~(7 << 12)) | (1 << 12))  # MCU_SEL = GPIO
    write_mem(H2_GPIO_ENABLE_W1TS, 1 << h2)
    for level, pull in (('high', 'pd'), ('low', 'pu')):
        write_mem(H2_GPIO_OUT_W1TS if level == 'high' else H2_GPIO_OUT_W1TC, 1 << h2)
        cmd(f'gpio {s3} {pull}')
        r = cmd('gpio')
        seen.append(int(r.split(f'GPIO{s3}=')[1][0]))
    write_mem(H2_GPIO_ENABLE_W1TC, 1 << h2)
    ok = seen == [1, 0]
    print(f'{link}: H2 GPIO{h2} high/low -> S3 GPIO{s3} = {seen} -> {"CONNECTED" if ok else "no"}')
    results[link] = results[link] and ok
    cmd(f'gpio {s3} pd')

print('\nresult:', results)
data.close()
print(cmd('h2 reset').strip())
print(cmd('lines on').strip())
print(cmd('flow on').strip())
