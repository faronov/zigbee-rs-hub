# zigbee-rs-hub

[Русская версия](README.ru.md)

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
| **0** | S3 firmware `tbr-bridge`: USB ↔ H2 UART bridge with esptool auto-reset, so the H2 can be flashed and spoken to from a desktop; flash backups of both chips | **done**: bridge 0.2.2 on the board; esptool through the auto-reset, verified backups of both chips, spinel round trip; S3 reflash cycle through hwlab without buttons; RTS/CTS on the reserved lines, measured up to 2 Mbaud (see Status) |
| **1** | ZRHP host on the S3 using `zigbee-zrhp` (`std`): session, ARQ, credits, resume; H2 NCP lifecycle: flash-size detection, `FW_*` updates streamed into the H2's inactive slot with trial boot, confirm and **H2-side rollback** (A/B slots; the S3 stores no NCP images); H2 ROM-loader recovery only as a manual path with an operator-supplied image | **in progress** (branch `phase1`): host core `hub-ncp` tested against a simulated NCP, `hubctl` on macOS, `tbr-hub` firmware (hub mode + the bridge as default mode) builds at 721 KiB (47 % of the 1.5 MiB slot); hardware runs pending (see "Phase 1") |
| **2** | Standalone hub: host-backed tables (paging, host store journal), WebSocket JSON API, Wi-Fi and W5500 Ethernet, PTA coexistence enabled automatically while Wi-Fi is up | planned |

## Device support

Specification (draft, design only): [`docs/device-support.md`](docs/device-support.md),
example definitions: [`definitions/examples/`](definitions/examples/),
measurement tools: [`tools/herdsman-coverage/`](tools/herdsman-coverage/),
compiler prototype: [`tools/zdefc-proto/`](tools/zdefc-proto/).

- **Three tiers, none of them needs a hub reflash.** (a) Generic ZCL: after the
  interview the hub creates entities from the standard clusters (On/Off, Level,
  Color, sensors, IAS, metering, battery, thermostat, covers, locks…) and sets up
  binding/reporting, like ZHA. (b) Declarative definitions (data, not code):
  match on manufacturer/model/fingerprint (masks included), manufacturer
  attributes, Tuya EF00 datapoints, commands/events, and a small expression
  language compiled to verifiable loop-free bytecode. (c) YAML → deterministic,
  hashed CBOR; YAML is compiled on the client only (CLI and WASM in the UI), the
  hub accepts CBOR; a generator from zigbee-herdsman-converters reports its
  coverage.
- **Layer precedence:** user definition > device self-description > catalog >
  generic; the catalog may demote the self-description of a known-broken
  firmware.
- **Catalog:** separate repository `faronov/zigbee-rs-definitions`, GitHub
  releases (manifest, 256 shards, banks for 4 MB and 8 MB), Ed25519 signature
  with a key kept in GitHub Actions secrets. The 4 MB bank holds only fully
  translated definitions, chosen by popularity (model mentions in Z2M/ZHA
  issues, `tools/herdsman-coverage/popularity.py`); 4 MB boards also get
  64 KiB for user and downloaded definitions, 8 MB boards get 2 × 576 KiB banks
  and 128 KiB of local definitions.
- **zigbee-rs uses manufacturer code 0x6D6B** (no CSA registration planned; the
  collision risk is documented): self-description cluster Device Schema 0xFC6B,
  settings 0xFC6C. The schema is served in 48-byte chunks through plain Read
  Attributes and generated at build time from `#[derive(ZclSettings)]`.
- **Green Power:** the sink runs on the NCP (`NCP_SINK`, security level 0x00
  with a UI warning for unsecured GPDs); the hub parses `EVT_GP_*`:
  identification by SrcID/DeviceID/Manufacturer+Model ID, a generic GP tier,
  GP definitions (FlowerX GP, BGT1 GP, the zigbee-rs GP sensor and switch), a
  Model ID allocation table under 0x6D6B; every report refreshes last_seen and
  state. Translation on the NCP is only a possible future local fallback.
- **Entity model maps 1:1 onto Home Assistant** (platforms, device/state class,
  units, config/diagnostic), so a future HA integration is a thin bridge.
- **Measured** on zigbee-herdsman-converters 26.130.0 (4,664 definitions):
  55.1 % fully declarative (generic 41.7 % + Tuya 13.4 %), another 26.1 % need
  vendor templates (order: Hue light — one template covers 583 —, then Lumi,
  then Tuya specifics), 18.8 % are arbitrary JS. Tuya DP tables are 79 %
  declarative.
- **Sizes:** simple definitions are 120–320 B of CBOR; a 256 KiB bank holds
  ~2,000 compressed definitions (~83 B per definition in a deflate block,
  ~226 KiB total); on 8 MB a 576 KiB bank holds everything translatable
  (~365 KiB).

## Layout

| Path | What |
|---|---|
| `crates/hub-core` | Portable `no_std` core: board wiring (`board`), esptool DTR/RTS → EN/BOOT logic (`lines`), re-export of `zigbee-zrhp` (git dependency on faronov/zigbee-rs, pinned rev) |
| `crates/hub-ncp` | Phase 1: portable ZRHP host core (`std`): link driver, sessions/resume, client API, NCP lifecycle, `FW_*` streaming, host store journal, simulated NCP (`sim`) |
| `tools/hubctl` | Phase 1: `hub-ncp` on a desktop: the real NCP through the bridge, or `--sim` |
| `firmware/bridge` | Phase-0 S3 firmware (`tbr-bridge`, esp-idf-svc + esp_tinyusb); also a library, the default mode of `tbr-hub` |
| `firmware/hub` | Phase-1 S3 firmware (`tbr-hub`): bridge mode (default) and hub mode; 4 MB / 8 MB partition tables for provisioning |
| `tools/make-app-image.sh` | ELF → ESP app image (same `elf2image` arguments as ESP-IDF), optional size limit |
| `tools/flow/` | Reserved-line check, RAM-only H2 UART test program and the flow-control measurements (`reserved_lines.py`, `h2-uart-test/`, `flow_test.py`, `s3_reflash.sh`) |
| `docs/device-support.md` | Device support design: tiers, definition format, expression language, self-description, Green Power, entity model, budgets |
| `definitions/examples/` | Example device definitions (YAML source form) |
| `tools/herdsman-coverage/` | Coverage and size measurement of zigbee-herdsman-converters definitions (Node + Python) |
| `tools/zdefc-proto/` | Python prototype of the definition compiler (YAML → CBOR, expression bytecode) |

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
| Reserved (fitted, verified both ways; used for RTS/CTS) | GPIO6 / 5 / 4 | R18 / R19 / R20 | GPIO12 / 4 / 5 |
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
- R18, R19 and R20 are fitted (2026-11-19, `tools/flow/reserved_lines.py`):
  each S3 pin driven low/high was read on the H2 (ROM loader, `GPIO_IN`),
  and each H2 pin driven high/low was read on the S3.
- Stock firmware: `esp_ot_br` in S3 ota_0; `esp_ot_rcp` (OpenThread RCP,
  built for 2 MB) on the H2 and a copy in the S3 `rcp_fw` partition.

### S3 flash layout

The S3 stores **no NCP images**: NCP rollback lives on the H2 (A/B app
slots, `FW_*` into the inactive slot, trial boot, `FW_CONFIRM`, H2 bootloader
rollback; ZRHP decision N6 as amended 2026-11-18). The flash holds the hub app
twice and one data partition. The hub detects 4 MB / 8 MB at boot and uses
the matching table (ZRHP spec 3.4, decision N9; branch `zrhp-s3-4mb` of
zigbee-rs):

| Partition | 4 MB | 8 MB |
|---|---|---|
| bootloader | 0x000000, 32 KiB | same |
| partition table | 0x008000, 4 KiB | same |
| `nvs` (Wi-Fi, PHY calibration, small settings) | 0x009000, 24 KiB | same |
| `otadata` | 0x00F000, 8 KiB | same |
| `phy_init` | 0x011000, 4 KiB | same |
| reserved (optional core dump) | 0x012000, 56 KiB | same |
| hub app `ota_0` | 0x020000, **1.5 MiB** | 0x020000, 3 MiB |
| hub app `ota_1` | 0x1A0000, **1.5 MiB** | 0x320000, 3 MiB |
| `hubdata` (LittleFS or log-structured store) | 0x320000, **896 KiB** | 0x620000, 1 920 KiB |

`hubdata` on 4 MB: ZRHP host store journal 128 KiB, configuration 32 KiB,
converter database 2 × 256 KiB (active + staged, swapped atomically) and
224 KiB (25 %) left free for copy-on-write and wear levelling. The hub app
must stay within 1.5 MiB (to be enforced in CI); the stock `esp_ot_br` is
1.6 MB *with* OpenThread, which the hub does not need.

**Converter database.** Device definitions are declarative data, updated
without an app OTA: compact CBOR records (model/manufacturer ids, exposes,
cluster/attribute mappings, value scaling and enum maps, reporting, and
references to generic handlers compiled into the app). No code in the
database. Capacity estimate per 256 KiB bank, with dictionaries for keys,
vendors and handler ids and an index of about 18 B per definition:

| Definition kind | CBOR size | Per bank, raw | Per bank, 4 KiB-block deflate (≈ 2–2.5×) |
|---|---:|---:|---:|
| Simple (lights, plugs, sensors on standard clusters) | 150–250 B | ~1 000–1 500 | ~2 500–3 500 |
| Tuya-style datapoint mappings | 600–1 200 B | ~200–400 | ~500–1 000 |
| Realistic mix (≈ 300 B average) | | **~800** | **~1 600–2 000** |

The full zigbee-herdsman-converters catalogue (≈ 4 500+ definitions,
roughly 1.3–1.5 MB raw) does not fit a 4 MB board, even compressed with
two banks. On 4 MB: a curated subset plus definitions fetched on demand for
paired devices (a home rarely has more than ~100 distinct models). On 8 MB
(`hubdata` 1 920 KiB) the whole catalogue fits compressed, with two banks.

**Option (not decided): H2 spare flash as host storage over ZRHP BULK.**
With the spec's 4 MB H2 layout (two 1.875 MiB slots) almost nothing is
spare; ~1.8–2 MB becomes spare only if the 4 MB H2 keeps 896 KiB–1 MiB slots.

- Pros: about 2 MB more without hardware changes; survives hub reflashes
  and S3 partition changes; could hold the converter catalogue, backups or
  logs; S3 flash stays free for the app.
- Cons: hub data then depends on the NCP (a ROM-path reflash, chip swap or
  layout change can lose it); 460 800 baud is about 40 KB/s, shared with
  Zigbee traffic and credits, so random access needs an S3-side cache;
  flash erases on the single-core H2 stall flash fetches and can hurt MAC
  timing; new BULK object type, capacity negotiation and power-loss rules in
  the spec; only on 4 MB H2 modules; not portable to other NCPs.

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
  and the ZRHP NCP then use 460 800 8N1.
- Hardware flow control (RTS/CTS) is **on by default** since 0.2.0; see
  "UART flow control". With it on and the host port open, H2 → host data is
  never dropped: the bridge waits for the host and RTS pauses the H2.

Console (CDC 1): `help`, `info` (rate, framing, EN/BOOT levels, byte and
drop counters), `h2 reset`, `h2 boot` (ROM loader: BOOT low, EN pulse 20 ms,
BOOT released 100 ms after EN), `h2 hold` / `h2 release`, `baud <rate>`,
`lines on|off`, `flow on|off`, `stats reset`, `gpio` / `gpio <4|5|6>
high|low|in|pu|pd` (test access to the reserved lines; turns flow off),
`diag` (thread breadcrumbs and the last supervisor restart), `s3 usj` (alias
`s3 download`: hand the USB PHY back to USB-Serial-JTAG, app keeps running
with the bridge off), `s3 reset`. `info` also shows the flow-control pins and
the UART RX FIFO overflow, ring-full, frame and parity error counters.

A supervisor thread restarts the S3 if the console loop stops for 15 s or a
TinyUSB callback is stuck in bridge code for 5 s; it first saves the
breadcrumbs and FreeRTOS task states to RTC memory for `diag`.

### UART flow control (RTS/CTS)

The "reserved" 0 Ω links carry hardware flow control (all fitted, see Board
facts):

| Signal | S3 | Link | H2 |
|---|---|---|---|
| S3 RTS → H2 CTS | GPIO4 (UART1 RTS) | R20 | GPIO5 (U0CTS via GPIO matrix, pull-down) |
| H2 RTS → S3 CTS | GPIO5 (UART1 CTS, pull-down) | R19 | GPIO4 (U0RTS via GPIO matrix) |
| spare | GPIO6 | R18 | GPIO12 |

- S3: ESP-IDF `uart_set_hw_flow_ctrl(CTS_RTS)`, RTS deasserted at 100 of the
  128 RX FIFO bytes. CTS has a pull-down, so an H2 image without flow control
  (pin undriven) never blocks the S3. `flow off` holds S3 RTS low ("ready")
  and ignores CTS.
- H2 (what the NCP's BR variant configures): U0RTS on GPIO4, U0CTS on GPIO5
  with a pull-down, `rx_flow_en` with a threshold of about 96 bytes,
  `tx_flow_en`. Both GPIOs are inputs in the H2 ROM loader and neither is a
  strapping pin (ESP32-H2 datasheet v1.3: GPIO8, GPIO9, GPIO25), so esptool
  through the bridge is unaffected. GPIO4/5 are the MTCK/MTDI pads; pad JTAG
  is not selected by default (USB-JTAG is), so using them as GPIO costs
  nothing.

**Measurements** (2026-11-19, bridge 0.2.2, `tools/flow/flow_test.py`; the H2
runs a RAM-only test program loaded through the ROM loader, nothing written to
its flash; 256 KiB bulk transfers, 128 KiB for the stall case):

| Baud | Flow | S3→H2 bulk | S3→H2, H2 stalls 30 ms every 4 KiB (interrupts off) | H2→S3 bulk | H2→S3, host pauses 1 s every 32 KiB |
|---:|---|---:|---|---:|---|
| 460 800 | off | 43.8 KiB/s, 0 lost | **30 018 B lost** (27 FIFO overflows) | 45.1 KiB/s, 0 lost | 0 lost |
| 460 800 | on | 43.9 KiB/s, 0 lost | **0 lost**, 33.9 KiB/s | 45.1 KiB/s, 0 lost | 0 lost |
| 921 600 | off | 87.8 KiB/s, 0 lost | **50 063 B lost** (20 overflows) | 90.2 KiB/s, 0 lost | 0 lost |
| 921 600 | on | 87.6 KiB/s, 0 lost | **0 lost**, 50.5 KiB/s | 90.2 KiB/s, 0 lost | 0 lost |
| 2 000 000 | off | 176.0 KiB/s, 0 lost | **76 296 B lost** (13 overflows) | 196.0 KiB/s, 0 lost | 0 lost |
| 2 000 000 | on | 176.0 KiB/s, 0 lost | **0 lost**, 77.3 KiB/s | 196.0 KiB/s, 0 lost | 0 lost |

- Bulk runs at about 95 % (H2→S3) and 88 % (S3→H2, USB scheduling) of the
  line rate; the S3 UART never overflowed (`info`: 0 FIFO overflows, 0
  dropped, 0 frame errors over all runs).
- Without flow control an H2 that stops reading for 30 ms loses data at every
  rate (its RX FIFO is 128 bytes: 2.8 ms at 460 800, 0.64 ms at 2 Mbaud).
  With RTS/CTS nothing is lost and the S3 simply waits.
- The host-pause case shows no loss either way: macOS keeps reading the CDC
  endpoint into its tty buffer while the application sleeps, so it does not
  reach the S3. An S3-side stall is covered by design (the bridge waits for
  the host, the 16 KiB ring fills, RTS pauses the H2) but was not measured.


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
cargo test                                       # host tests: hub-core, hub-ncp, hubctl (stable Rust)
```

### Flashing the S3 (hwlab only, no buttons)

Phase 0 keeps the stock partition table and bootloader and replaces only
**ota_0 at 0x20000** (otadata already selects ota_0). Verified cycle
(0.1.2 → 0.1.3 on 2026-11-18):

1. Bridge console: `s3 usj` (or `s3 download`, or `stty -f
   /dev/cu.usbmodem7CDFA1FE67F03 1200`). The bridge ports vanish and
   `espusbjtag/303a:1001:7C_DF_A1_FE_67_F0` / `uart/cu.usbmodem1474301`
   appear within about 1 s. The app keeps running with the bridge off.
2. hwlab's program guard needs a verified backup that matches the current
   flash, so take one first (both cores are halted for it), then program:

   ```sh
   hwlab target flash backup --yes --device espusbjtag/303a:1001:7C_DF_A1_FE_67_F0 --arg dir=$DIR
   hwlab target flash program --yes --device espusbjtag/303a:1001:7C_DF_A1_FE_67_F0 \
     --config allow_flash=true --config allow_erase=true \
     --arg path=$HOME/.hwlab/artifacts/zigbee-rs-hub/tbr-bridge-<ver>-app.bin --arg address=0x20000 \
     --arg confirm=program-flash --arg backup=$DIR/esp-flash-5e4016-4096K-<sha>.bin
   ```
3. Boot it with the USJ's RTS reset through hwlab's `uart` plugin (MCP):
   `capture_start` on `uart/cu.usbmodem1474301` (baud 115200, no `dtr`/`rts`
   config), then `device_invoke uart SerialTransport.set_lines {dtr:false,
   rts:false}`. Opening the port asserts both lines; dropping DTR first
   passes through (DTR=0, RTS=1) = reset, and releasing RTS boots the app.
   The USJ port disappears and the bridge ports come back. (hwlab refuses a
   SoC reset on the S3, so this is the only button-free reset.)

**Do not reboot the S3 into ROM download mode from firmware.**
`RTC_CNTL_FORCE_DOWNLOAD_BOOT` (0x6000812C bit 0) survives USJ chip resets
(`rst:0x15`) and the S3 ROM does not clear it. Bridge 0.1.0–0.1.2 used it for
`s3 download`, and afterwards every RTS reset came back in `boot:0x23
DOWNLOAD`. If that happens: with the S3 in the ROM loader, send the loader
`SYNC` and then `WRITE_REG 0x6000812C = 0` through the hwlab `uart` plugin
(`SerialTransport.write`, SLIP frames), then do the RTS reset. Done once on
2026-11-18. The same symptom after BOOT + RESET comes from the latched boot
strap; only a plain RESET or a power cycle clears that. For ROM download mode
without buttons, use the USJ's esptool sequence instead (DTR=1/RTS=0, then
DTR=0/RTS=1, then both 0), which is not sticky.

### Recovery (getting hwlab back to the S3)

The PHY routing bits live in RTC_CNTL and survive software and panic resets,
so the bridge hands the PHY back to the USJ explicitly:

| Path | Result |
|---|---|
| console `s3 usj` / `s3 download`, or the console port set to 1200 baud | PHY → USJ, the app keeps running with the bridge off (verified) |
| USB not enumerated within 20 s of boot | PHY → USJ |
| boot after a panic or watchdog reset | safe mode: TinyUSB never starts, PHY → USJ |
| power cycle, or S3 BOOT held + RESET pressed | cold boot / ROM download on USJ |

The hand-back makes the host see a real detach and a new attach. It drops the
D+ pull-up (TinyUSB `tud_disconnect` + pull overrides) for 500 ms, re-routes
the PHY, then puts the USJ pull-up registers back to their reset state
(`DP_PULLUP` = 1, no override).

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
- 0.1.1 (`bd5ac931…`) ran on hardware: three full H2 reads (two of them
  through 0.1.1), identical SHA-256, 8.4 MB bridged with 0 bytes dropped
  despite 10 802 SET_LINE_CODING requests.
- 0.1.1's hand-back detached but the USJ never re-attached: it left the USJ
  `DP_PULLUP` bit at 0 (reset value 1). Fixed in 0.1.2, verified: the USJ is
  back within about 1 s.
- 0.1.2's `s3 download` left `FORCE_DOWNLOAD_BOOT` set (see "Flashing the
  S3"). In **0.1.3** (`b3406de0…`) `s3 download` is a
  plain hand-back. Verified without buttons: hand-back → hwlab backup and
  program in app mode → RTS reset → bridge → hand-back → RTS reset → bridge;
  `esptool flash-id` on the H2 afterwards.
- Buttons are only needed for a stuck boot strap (after BOOT + RESET: plain
  RESET), or if the bridge firmware itself no longer runs (BOOT + RESET for
  ROM download).
- **0.2.x** (2026-11-19): RTS/CTS on the reserved lines, `gpio` test command,
  UART error counters, `diag` and the hang supervisor. 0.2.0 hung in
  `h2 boot`: the console called `uart_flush_input`, which waits for
  the UART driver's RX mutex, and the priority-20 reader thread re-took that
  mutex every time it released it (a FreeRTOS mutex is not handed to a
  waiting lower-priority task), so the console starved. The 0.2.1 supervisor
  caught it (`console loop stalled`, console blocked in a command); 0.2.2
  lets the reader thread do the flush on request. **0.2.2** (`7797265d…`) is
  on the board, flashed without buttons.
- **0.2.3** (not flashed): the bridge became a library as well, so `tbr-hub`
  runs it as its default mode; a console extension hook and a CDC 0
  "bridge enabled" switch (always on in `tbr-bridge`). No functional change.

## Phase 1: ZRHP host (`hub-ncp`, `hubctl`, `tbr-hub`)

One host core, two places: on macOS against the real NCP through the
bridge (fast development loop) and on the S3 talking to UART1 directly.

```text
 hubctl (macOS)  ── serial ── tbr-bridge CDC 0 ── UART ──┐
                                                         ├── H2: ZRHP NCP
 tbr-hub (S3, hub mode) ── UART1 921 600 RTS/CTS ────────┘
      both: hub-ncp = driver (zigbee-zrhp Link) + lifecycle (Hub) + store
```

### `crates/hub-ncp`

| Piece | What it does |
|---|---|
| `transport` | `PortRx` (reader thread) / `PortTx` (driver: writes, line rate, RTS/CTS), `NcpControl` (EN/BOOT), in-memory pipe |
| `driver` | One thread owns `zigbee_zrhp::link::Link<8, 8>` (embedded profile: RX window 8, LOG credit 0, BULK 2; credits returned on delivery). It answers `EVT_NCP_READY` with `SYS_NEGOTIATE` + `SYS_GET_INFO` itself, then reports the session. Requests are **gated** until the lifecycle has resumed the NCP, bounded by the 20 s resume budget. A dropped session makes transmitted requests "in doubt": same NCP cookie (`PEER_RESYNC`) ⇒ re-sent with the same TID and `RETRY` (the NCP's retry cache answers), new cookie ⇒ `LINK_RESET` (delivery unknown, spec 6.5). Two-phase commands wait for their completion event (`EVT_APS_DATA_CONFIRM`, `EVT_NETWORK_STATE`). Without a session the driver cycles 921 600 + RTS/CTS → 460 800 without flow control |
| `lifecycle` (`Hub`) | Seamless resume on every session: UART flow-control mismatch (NCP without RTS/CTS ⇒ host flow off), store identity (`ncp_ieee`), NCP cookie, `table_epoch`, NCP flash size (`SYS_GET_INFO.flash_size`, decision N4: picks the H2 slot size, 1.875 MiB on 4 MB, 896 KiB on 2 MB, so oversized images are refused before `FW_BEGIN`); network ownership (adopt into an empty store, refuse to touch a different network, report a lost one); `NWK_INIT` with the checkpoint-log floors (`last + 9 × fc_stride`); replay of host endpoints and the permit-join remainder; `TBL_SYNC`; NCP firmware check. Operations: `form` (PAN ID, EPID and key chosen and persisted by the hub, then `NWK_FORM`), `permit_join`, `add_endpoint`, `send_aps`, `fw_update`, `fw_confirm`, `reset_ncp` (soft/EN), `enter_rom_loader` |
| `fw` | `FW_BEGIN` → `FW_WRITE` (two in flight, resynchronised on `next_offset`) → `FW_FINISH` → `FW_ACTIVATE`, straight from the source (file, upload); the S3 keeps no NCP image. The next session confirms the hub's own trial image with `FW_CONFIRM` after the network resumed, or reports the H2 bootloader rollback. Trial images the hub did not activate are not confirmed (another host may own them) |
| `store` | Host store journal (spec 3.4/8.8): 32 × 4 KiB ring, records `len, type, commit, payload, CRC-32C`, committed by clearing the commit byte after a read-back; two-generation snapshot + deltas, compaction above half the ring. Holds the store identity, the owned network (incl. key), the FC checkpoint log (every 4th checkpoint persisted), a device registry, the pending NCP update and the host endpoints. Backends: `hubdata` partition, a file, RAM |
| `sim` | Simulated NCP (the real link on the NCP side, scripted command set incl. A/B slots, trial boot and rollback) for the tests and `hubctl --sim` |

Phase 1 sets `FC_CHECKPOINTS | RETRY_CACHE`, not `HOST_TABLES`
(host-backed tables and link keys are phase 2): `TBL_LOOKUP` is answered
`NOT_FOUND`, `TBL_WRITEBACK` `STORAGE_ERROR`.

### `tbr-hub` (S3)

| Mode | What runs |
|---|---|
| **bridge** (default; no NVS key) | `tbr-bridge` 0.2.3 unchanged: same USB identity and ports, CDC 0 bridge, console on CDC 1 |
| **hub** | `hub-ncp` on UART1 (921 600 + RTS/CTS, IRAM ISR, 16 KiB RX ring), H2 EN/BOOT on GPIO7/8; CDC 0 idle; console on CDC 1 with `hub …` commands; the bridge commands that touch the H2 UART or EN/BOOT are refused |

Console additions: `mode`, `mode bridge|hub` (persisted in NVS `hub/mode`,
reboot), `mode hub once` (one boot in hub mode, any later reset returns to
the persisted mode), `hub [status]`, `hub echo [n]`, `hub form [channel]`,
`hub permit <s>`, `hub fw slots`, `hub fw confirm`, `hub ncp reset [hard]`,
`hub rom` (manual NCP recovery: reboot into the bridge with the H2 in its ROM
loader, then esptool on CDC 0 with an operator-supplied image). USB recovery
(`s3 usj`, 1200 bps touch, mount timeout, crash safe mode) is the bridge's,
in both modes.

The host store uses the `hubdata` partition when the table has one
(`firmware/hub/partitions-4mb.csv`, `partitions-8mb.csv`; the 4 MB / 8 MB
layouts above) and checks that the table fits the detected flash; with the
stock table on the hwlab board it falls back to RAM. Flashing a new
partition table is a separate, explicit step.

### `hubctl` (desktop)

```sh
cargo run -p hubctl -- --sim status                     # simulated NCP
cargo run -p hubctl -- --sim form --channel 17
hubctl --serial /dev/cu.usbmodem7CDFA1FE67F01 --read-only status   # real NCP, observe only
hubctl --serial … --store hubdata.bin form --channel 17            # store in a file
hubctl --serial … fw-update ncp.bin --version 0x00010100           # stream, trial boot, confirm
hubctl --serial … soak 60                                          # echoes across NCP resets
```

`--read-only` sends nothing that changes the NCP: handshake, negotiation
and read-only queries only (no `NWK_INIT`, `TBL_SYNC` or replay). The bridge
follows the host's rate and does RTS/CTS towards the H2 itself.

### Building

```sh
cargo test                                        # host: hub-core, hub-ncp (11 tests), hubctl
. ~/export-esp.sh && cd firmware/hub && cargo build --release
cd ../.. && MAX_APP_BYTES=1572864 tools/make-app-image.sh release target/tbr-hub-app.bin tbr-hub
```

CI builds both firmwares and fails if `tbr-hub` exceeds the 1.5 MiB app
slot of the 4 MB layout.

### Sizes (release, Xtensa Rust 1.90.0.0, ESP-IDF v5.5.1)

| Image | App image | Slot | Use |
|---|---:|---:|---:|
| `tbr-bridge` 0.2.3 | 501 744 B | 1 572 864 B | 32 % |
| `tbr-hub` 0.1.0 | 738 416 B | 1 572 864 B | 47 % |

The stock table's `ota_0` (1.6 MB) holds either.

### Phase 1 status

- Host tests (`hub-ncp`, against the simulated NCP over the shared link):
  negotiation and `SYS_GET_INFO`; formation with host parameters, NCP
  reboot with a request held across it, `NWK_INIT` with the floors of the
  logged checkpoint, endpoint replay; `RETRY` after a resync; `LINK_RESET`
  after a restart; APS data with its confirm; `FW_*` staged, trial boot,
  `FW_CONFIRM`; a rolled-back update reported; the store surviving a hub
  restart; journal compaction, power cuts at every 7th programmed byte and
  a torn record.
- Hardware: not run yet. The H2 NCP is shared (the bridge port was busy
  with other work); `tbr-hub` has not been flashed: the bridge stays the
  board's image until the user decides.

## License

MIT OR Apache-2.0, like zigbee-rs.
