# zigbee-rs-hub

A standalone Zigbee hub in Rust for the **Espressif Thread Border Router /
Zigbee Gateway board** (ESP32-S3 host + ESP32-H2 radio). The H2 runs the
[zigbee-rs](https://github.com/faronov/zigbee-rs) NCP; the S3 runs the hub and
talks to it with **ZRHP v1** (zigbee-rs host protocol, spec:
`docs/book/src/advanced/host-protocol.md` in zigbee-rs) over the board UART.

ZRHP stays a USB/UART protocol (no network transport). The hub's own external
interface will be a WebSocket JSON API.

## Phases

| Phase | Content | Status |
|---|---|---|
| **0** | S3 firmware `tbr-bridge`: USB ↔ H2 UART bridge with esptool auto-reset, so the H2 can be flashed and spoken to from a desktop; flash backups of both chips | bridge works (esptool `flash-id`, H2 backup read, spinel round trip); 0.1.1 programmed into ota_0 and verified, waiting for an S3 RESET press to boot (see Status) |
| **1** | ZRHP host on the S3 using `zigbee-zrhp` (`std`): session, ARQ, credits, resume; H2 NCP lifecycle: flash-size detection, provisioning and update through the H2 ROM loader (EN/BOOT driven by the S3), two NCP images kept on the S3 (known-good + candidate) with rollback | planned |
| **2** | Standalone hub: host-backed tables (paging, host store journal), WebSocket JSON API, Wi-Fi and W5500 Ethernet, PTA coexistence enabled automatically while Wi-Fi is up | planned |

## Layout

| Path | What |
|---|---|
| `crates/hub-core` | Portable `no_std` core: board wiring (`board`), esptool DTR/RTS → EN/BOOT logic (`lines`), re-export of `zigbee-zrhp` (git dependency on faronov/zigbee-rs, pinned rev) |
| `firmware/bridge` | Phase-0 S3 firmware (`tbr-bridge`, esp-idf-svc + esp_tinyusb) |
| `tools/make-app-image.sh` | ELF → ESP app image (same `elf2image` arguments as ESP-IDF) |

## Framework choice: esp-idf-svc (`std`), not esp-hal

The hub needs, on the S3: Wi-Fi and W5500 Ethernet, TLS, an HTTP server with
WebSocket upgrade, PSRAM-backed heap, NVS/OTA, a USB device stack and a UART
driver with an IRAM/DMA RX path. ESP-IDF ships all of them as maintained,
production-used components (`esp_wifi`, `esp_eth` W5500 driver, mbedTLS,
`esp_http_server` with WebSocket support, SPIRAM malloc, `app_update`,
TinyUSB, UART driver with `CONFIG_UART_ISR_IN_IRAM`), and esp-idf-svc gives
safe Rust access with `std` (threads, `std::sync`, `std::net`). `zigbee-zrhp`
already has a `std` feature for exactly this host. esp-hal + embassy is
attractive (no C, async), but the WebSocket hub would have to assemble Wi-Fi
(esp-radio), W5500 (embassy-net-wiznet), TLS and an HTTP/WS server from
younger crates, so esp-idf-svc is the practical base. The same choice is
already recorded in the ZRHP spec (12.4).

## Board facts (checked against Espressif sources)

Sources: esp-thread-br docs "Hardware Platforms" (GPIO allocation) and the
board schematic v1.3 (`esp_thread_br_zigbee_gw_schematics_v1.3.pdf`,
"Interface" block). Both agree with the ZRHP spec table:

| Signal | S3 | Link | H2 |
|---|---|---|---|
| UART S3 TX → H2 RX | GPIO18 | R17 | GPIO23 (U0RXD) |
| UART S3 RX ← H2 TX | GPIO17 | R16 | GPIO24 (U0TXD) |
| H2 reset | GPIO7 | R14 | EN (R2 10 kΩ pull-up, C5 1 µF) |
| H2 boot strap | GPIO8 | R15 | GPIO9 (button + internal pull-up; GPIO8 has R3 10 kΩ pull-up) |
| PTA request / priority / grant / tx_line | GPIO12 / 10 / 13 / 11 | R23 / R21 / R24 / R22 | GPIO0 / 2 / 1 / 3 |
| Reserved | GPIO6 / 5 / 4 | R18 / R19 / R20 | GPIO12 / 4 / 5 |
| W5500 (daughter board, SPI2) | SCLK 21, MOSI 45, MISO 38, CS 41, INT 39, RST 40 | | |

Measured on the hwlab board (early sample):

- S3 flash: **4 MB** (Zbit 5e4016, SFDP agrees), not 8 MB — Espressif's
  ordering table notes 4 MB "in some early samples"; retail boards have 8 MB.
  The stock image is laid out for 4 MB (ota_0/ota_1 1.6 MB each, `rcp_fw`
  500 KB). The hub must therefore support both sizes (see "S3 flash budget").
  PSRAM not yet checked.
- H2: ESP32-H2 **rev v0.1**, flash **4 MB** (GigaDevice c84016). This answers
  the spec's open 2 MB/4 MB question for this board; the NCP still detects
  the size at runtime (spec decision N4).
- Stock firmware: `esp_ot_br` in S3 ota_0; `esp_ot_rcp` (OpenThread RCP,
  built for 2 MB) on the H2 and a copy in the S3 `rcp_fw` partition.

### S3 flash budget

ZRHP spec 12.4 proposes, for 8 MB: hub app ×2 at 2.5 MiB, NCP images ×2 at
1 MiB, host store journal 128 KiB, hub config 64 KiB (≈ 7.3 MiB with the
128 KiB system area). That does not fit this board's 4 MB. Plan: detect the
S3 flash size like the NCP does (decision N4) and ship two partition tables.

| Partition | 8 MB | 4 MB |
|---|---|---|
| bootloader, partition table, nvs, otadata, phy | 128 KiB | 128 KiB |
| hub app ×2 (OTA) | 2.5 MiB each | 1.25 MiB each |
| NCP images (known-good + candidate) | 2 × 1 MiB, raw | one 1152 KiB partition, both images deflate-compressed |
| ZRHP host store journal | 128 KiB | 128 KiB |
| hub config | 64 KiB | 64 KiB |

On 4 MB the NCP images are stored compressed, which the H2 ROM loader
accepts directly (`FLASH_DEFL_*`, MD5-checked on the H2), so the ROM path
needs no decompression on the S3. With about 50–60 % compression that holds
two images of up to ~1 MB raw each; the hub refuses a candidate that does
not fit. The hub app has to stay within 1.25 MiB on 4 MB boards; the stock
`esp_ot_br` (Wi-Fi + OpenThread + web server) already fits in 1.6 MB.

## Phase 0: `tbr-bridge`

### Why USB OTG and not USB-Serial-JTAG

The S3's USB-Serial-JTAG (USJ) unit gives firmware no DTR/RTS state and no
line coding (the S3 register map has none; the C6/H2 added them), and its
CDC endpoint FIFO is 64 bytes. The bridge therefore runs the S3's USB OTG
controller with TinyUSB (`espressif/esp_tinyusb` 1.7.6), which reports
SET_CONTROL_LINE_STATE and SET_LINE_CODING. Both share the one internal USB
PHY on the USB2 connector, so **while the bridge runs, the USJ (and with it
hwlab's JTAG/flash access to the S3) is gone** — see Recovery.

### USB interface

VID 0x303a, PID 0x4002, serial = S3 factory MAC (e.g. `7CDFA1FE67F0`).
macOS names the ports after the serial and interface number:

| Port (this board) | Function |
|---|---|
| `/dev/cu.usbmodem7CDFA1FE67F01` | CDC 0: transparent bridge to the H2 UART |
| `/dev/cu.usbmodem7CDFA1FE67F03` | CDC 1: control console (text, any baud) |

CDC 0:

- Bytes pass through in both directions. H2 → host: UART driver with ISR in
  IRAM, 16 KiB RX ring, RX FIFO threshold 64 B and timeout 2 symbols, read by a
  priority-20 thread into the 4 KiB TinyUSB TX FIFO. Host → H2: TinyUSB RX
  FIFO (4 KiB) → 8 KiB UART TX ring; when it is full the bridge stops reading
  and USB NAKs the host (back-pressure, no loss).
- Host line coding sets the UART rate, data bits, parity and stop bits
  (applied only when it changes, see Status).
- DTR/RTS drive the H2 like esptool's classic auto-reset circuit:
  `EN low = RTS && !DTR`, `BOOT low = DTR && !RTS`; (0,0) and (1,1) leave the
  H2 running, so opening (both asserted) or closing a port never resets it.
  EN and BOOT are open-drain with pull-ups, so they never fight the H2
  buttons. Disable the mapping with console `lines off` if a host toggles
  modem lines for other reasons.
- After every reset the H2 ROM prints its boot log at 115 200; the stock RCP
  and the ZRHP NCP then use 460 800 8N1 without flow control.

Console (CDC 1): `help`, `info` (rate, framing, EN/BOOT levels, byte and
drop counters), `h2 reset`, `h2 boot` (ROM loader: BOOT low, EN pulse 20 ms,
BOOT released 100 ms after EN), `h2 hold` / `h2 release`, `baud <rate>`,
`lines on|off`, `s3 download`, `s3 usj`, `s3 reset`.

### Flashing the H2 through the bridge (for the zrhp-ncp work)

Port `/dev/cu.usbmodem7CDFA1FE67F01`, esptool's default reset (the bridge is
not PID 0x1001, so esptool uses its classic/UnixTight DTR/RTS sequence, which
the bridge maps to EN/BOOT). The ROM syncs at 115 200, then esptool switches
to `-b`:

```sh
esptool --chip esp32h2 --port /dev/cu.usbmodem7CDFA1FE67F01 flash-id
esptool --chip esp32h2 --port /dev/cu.usbmodem7CDFA1FE67F01 -b 460800 \
  write-flash --flash-size 4MB 0x0 bootloader.bin 0x8000 partition-table.bin 0x10000 app.bin
# --after hard-reset (default) boots the new image: RTS pulse with DTR released.
```

Equivalent manual sequence (console port): `h2 boot` → talk ROM protocol at
115 200 on CDC 0 → `h2 reset` for a normal boot. Afterwards open CDC 0 at
460 800 (pyserial defaults DTR=RTS=1, no reset) for the NCP; `h2 reset` or an
RTS pulse with DTR low resets the NCP.

Back up first: the stock H2 image is in `~/.hwlab/artifacts/esp-thread-br-h2-backup/`.

### Building

Xtensa Rust from espup. Intel macOS: the last release with an
`x86_64-apple-darwin` build is 1.90.0.0.

```sh
cargo install espup ldproxy --locked
espup install --toolchain-version 1.90.0.0 --targets esp32s3 --std
. ~/export-esp.sh
cd firmware/bridge && cargo build --release      # downloads ESP-IDF v5.5.1 into .embuild
cd ../.. && tools/make-app-image.sh              # -> target/tbr-bridge-app.bin
cargo test -p hub-core                           # host tests (repo root, stable Rust)
```

### Flashing the S3 (hwlab only)

Phase 0 keeps the stock partition table and bootloader and replaces only
**ota_0 at 0x20000** (otadata already selects ota_0). The S3 must be on
USB-Serial-JTAG (stock firmware, ROM download mode, or after a bridge `s3
download` / `s3 usj`):

```sh
hwlab target flash program --yes --device espusbjtag/303a:1001:7C_DF_A1_FE_67_F0 \
  --config allow_flash=true --config allow_erase=true \
  --arg path=$HOME/.hwlab/artifacts/zigbee-rs-hub/tbr-bridge-<ver>-app.bin --arg address=0x20000 \
  --arg confirm=program-flash \
  --arg backup=$HOME/.hwlab/artifacts/esp-thread-br-s3-backup/esp-flash-5e4016-4096K-13775159baf7.bin
```

hwlab leaves the S3 cores halted and refuses a SoC reset on the S3. Boot the
new image with the USJ's hardware reset, through hwlab's `uart` plugin (MCP):
`capture_start` on `uart/cu.usbmodem1474301` with config `dtr=false,
rts=false`, then `device_invoke uart SerialTransport.set_lines {dtr:false,
rts:true}`; the S3 resets and the USJ port disappears as the bridge comes up.

This works only if the S3 straps were last latched for SPI boot. A USJ chip
reset (`rst:0x15 USB_UART_CHIP_RESET`) does not re-latch the boot straps:
after the S3 was put into ROM download mode with BOOT + RESET, every RTS reset
comes back in download mode (`boot:0x23`, `GPIO_STRAP` = 0x23 while GPIO0
reads high). Press S3 RESET once without BOOT (or power cycle). hwlab refuses
the RTC-watchdog SoC reset on the S3 that would re-latch them.

### Recovery (getting hwlab back to the S3)

The PHY routing bits live in RTC_CNTL and survive software and panic resets,
so the bridge hands the PHY back to the USJ explicitly:

| Path | Result |
|---|---|
| console `s3 download`, or set the console port to 1200 baud (`stty -f /dev/cu.usbmodem7CDFA1FE67F03 1200`) | PHY → USJ, reboot into the S3 ROM download mode (hwlab's validated S3 mode) |
| console `s3 usj` | PHY → USJ, the app keeps running with the bridge off |
| USB not enumerated within 20 s of boot | PHY → USJ |
| boot after a panic or watchdog reset | safe mode: TinyUSB never starts, PHY → USJ |
| power cycle, or S3 BOOT held + RESET pressed | cold boot / ROM download on USJ |

The hand-back must make the host see a real detach: 0.1.1 drops the D+
pull-up (TinyUSB `tud_disconnect` + pull overrides) for 500 ms before giving
the PHY to the USJ.

## Status

- Verified on hardware with 0.1.0: enumeration, console, H2 boot log at
  115 200, spinel round trip with the stock RCP at 460 800, `esptool
  flash-id` through the auto-reset, a full 4 MB H2 `read-flash`.
- Found on hardware with 0.1.0, fixed in 0.1.1:
  1. pyserial re-sends SET_LINE_CODING on every timeout change; 0.1.0
     re-programmed the UART each time (6 269 times during two reads), which
     broke a second 4 MB read. 0.1.1 applies line coding only on change.
  2. 0.1.0's `s3 download` switched the PHY without a detach: the host kept
     the stale OTG device (transaction errors) and never enumerated the USJ;
     recovering needed S3 BOOT + RESET by hand. 0.1.1 detaches first.
- 0.1.1 (`bd5ac931…`) is programmed into ota_0 through hwlab and read back
  equal. It has not booted yet: after BOOT + RESET, USJ resets keep the
  download strap (see "Flashing the S3"); one S3 RESET press boots it. Then:
  verify the 0.1.1 `s3 download` path with hwlab, and repeat the H2 backup
  (two reads).

## License

MIT OR Apache-2.0, like zigbee-rs.
