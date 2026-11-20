# zigbee-rs-hub

[Русская версия](README.ru.md)

A standalone Zigbee hub in Rust for the **Espressif Thread Border Router /
Zigbee Gateway board** (ESP32-S3 host + ESP32-H2 radio). The H2 runs the
[zigbee-rs](https://github.com/faronov/zigbee-rs) NCP; the S3 runs the hub and
talks to it with **ZRHP v1** (zigbee-rs host protocol, spec:
`docs/book/src/advanced/host-protocol.md` in zigbee-rs) over the board UART.

ZRHP stays a USB/UART protocol (no network transport). The hub's own external
interface will be a WebSocket JSON API.

The same hub also runs on classic-ESP32 LAN gateways with an EFR32MG24
radio, first the **SONOFF Dongle Max (Dongle-M)**, and on an ESP32-S3
SuperMini + EFR32MG24 bench setup; see "Supported hardware".

## Phases

| Phase | Content | Status |
|---|---|---|
| **0** | S3 firmware `tbr-bridge`: USB ↔ H2 UART bridge with esptool auto-reset, so the H2 can be flashed and spoken to from a desktop; flash backups of both chips | **done**: bridge 0.2.2 on the board; esptool through the auto-reset, verified backups of both chips, spinel round trip; S3 reflash cycle through hwlab without buttons; RTS/CTS on the reserved lines, measured up to 2 Mbaud (see Status) |
| **1** | ZRHP host on the S3 using `zigbee-zrhp` (`std`): session, ARQ, credits, resume; H2 NCP lifecycle: flash-size detection, `FW_*` updates streamed into the H2's inactive slot with trial boot, confirm and **H2-side rollback** (A/B slots; the S3 stores no NCP images); H2 ROM-loader recovery only as a manual path with an operator-supplied image | **in progress** (branch `phase1`): host core `hub-ncp` tested against a simulated NCP, `hubctl` on macOS, `tbr-hub` firmware (hub mode + the bridge as default mode); hardware runs pending (see "Phase 1") |
| **2** | Standalone hub: host-backed tables (paging, host store journal), WebSocket JSON API, Wi-Fi and W5500 Ethernet, PTA coexistence enabled automatically while Wi-Fi is up | **in progress** (branch `stage2`): engine on the host core, device registry in `hubdata`, WebSocket JSON API on a desktop (`hubctl serve`) and on the S3 over Wi-Fi, catalog verifier, 8 MB production / 4 MB dev layouts; the S3 application build is 2.1 MiB, see "Stage 2" |

## Supported hardware

| Board | Host | NCP | Uplink | Console | Flash / PSRAM | NCP firmware update | Firmware | Status |
|---|---|---|---|---|---|---|---|---|
| Espressif Thread BR / Zigbee Gateway | ESP32-S3 | ESP32-H2 | Wi-Fi (W5500 daughter board not driven yet) | TinyUSB CDC | 8 MB (4 MB early samples) / 2 MB | `FW_*` into the H2's A/B slots | `firmware/hub` (`tbr-hub`) | on the hwlab bench |
| SONOFF Dongle Max (Dongle-M) | ESP32-D0WDR2-V3 (classic Xtensa LX6) | EFR32MG24 | Ethernet (IP101 RMII) + Wi-Fi | UART0 through the CP2102N | 16 MB / 2 MB | Gecko bootloader (UART XMODEM), images and the last-known-good one in `hubdata` | `firmware/donglem` (`zrh-donglem`) | builds in CI; hardware not yet received |
| ESP32-S3 SuperMini + RF-star RF-BM-MG24B1 (dev) | ESP32-S3FH4R2 | EFR32MG24A410F1536IM48 | Wi-Fi | USB-Serial-JTAG | 4 MB / 2 MB | J-Link SWD | `firmware/s3-mg24` (`zrh-s3-mg24`) | builds in CI; wiring to be confirmed |

Every board is a `hub_core::profile::BoardProfile` (`THREAD_BR`, `DONGLE_M`,
`S3_MG24_DEV`): NCP UART pins, rate and flow control, NCP reset and
serial-loader lines with polarity and open drain, Ethernet, LEDs, button,
console, flash and PSRAM, checked on the host (pin conflicts, flash/PSRAM
and USB pins, input-only pins, strapping pins). The gateway firmware
(`firmware/gateway`, a library; `firmware/donglem` and `firmware/s3-mg24`
are its two binaries) runs the same ZRHP host, application and WebSocket
API as `tbr-hub`, from the profile. ZRHP gets no network transport and
there is no serial-over-TCP stream server: the NCP is only reachable
through the hub (decided). Details: "Gateway firmware" below.

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
- **Engine** ([`crates/hub-devices`](crates/hub-devices/README.md), host
  tests only so far): interview → fingerprint → resolution → entities with
  stable uids `<device>:<endpoint>:<key>`, the generic ZCL tier with
  reporting defaults, definitions loaded as zdefc CBOR through `zdef`
  (expressions on the zdef VM), Tuya EF00 with time sync and the magic
  packet, self-description over ZRDS chunks, Green Power (generic tier, GP
  definitions, GP bindings on-device through ZRHP 0x0A08–0x0A0D or
  hub-side), availability, events, and the WebSocket API model. It is
  sans-IO behind a `ZigbeeTransport` trait; the adapter to the phase-1 host
  core is the next step.

## Layout

| Path | What |
|---|---|
| `crates/hub-devices` | Device/entity engine (`std`, sans-IO): ZCL/ZDO codec, interview, generic tier, resolution, entity runtime, Tuya EF00, Green Power and GP bindings, WebSocket API model; golden scenarios and property tests |
| `crates/hub-core` | Portable `no_std` core: Thread BR board wiring (`board`), board profiles (`profile`), flash layouts and their boot check (`layout`), esptool DTR/RTS → EN/BOOT logic (`lines`), re-export of `zigbee-zrhp` (git dependency on faronov/zigbee-rs, pinned rev) |
| `crates/hub-app` | Stage 2: the engine on the host core: event/request conversion, transport, device registry in the host store, app thread with the API sessions, catalog verifier (trusted keys) |
| `crates/hub-ncp` | Phase 1: portable ZRHP host core (`std`): link driver, sessions/resume, client API, NCP lifecycle, `FW_*` streaming, Gecko bootloader client for EFR32 NCPs (`gecko`: GBL checker, XMODEM-CRC, radio image banks, update/rollback job, simulated bootloader), host store journal, simulated NCP (`sim`) |
| `tools/hubctl` | Phase 1: `hub-ncp` on a desktop: the real NCP through the bridge, or `--sim` |
| `firmware/bridge` | Phase-0 S3 firmware (`tbr-bridge`, esp-idf-svc + esp_tinyusb); also a library, the default mode of `tbr-hub` |
| `firmware/hub` | Phase-1 S3 firmware (`tbr-hub`): bridge mode (default) and hub mode; 4 MB / 8 MB partition tables for provisioning |
| `firmware/gateway` | Gateway firmware library for board profiles (NCP UART and lines, Ethernet, Wi-Fi, WebSocket API, radio banks, LED/button, console); 16 MB partition table |
| `firmware/donglem`, `firmware/s3-mg24` | Its binaries: `zrh-donglem` (classic ESP32, SONOFF Dongle-M), `zrh-s3-mg24` (ESP32-S3 SuperMini + MG24) |
| `tools/make-app-image.sh` | ELF → ESP app image (same `elf2image` arguments as ESP-IDF; `CHIP=esp32` for the Dongle-M), optional size limit |
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
rollback; ZRHP decision N6 as amended 2026-11-18). (The Dongle-M is different: its
EFR32MG24 is updated through the Gecko bootloader, so the ESP32 keeps the
staged and the last-known-good `.gbl` in `hubdata`; see "Gateway
firmware".)

**8 MB boards are the product** (decision 2026-11-20): two 3 MiB app slots
with A/B OTA and one data partition. **4 MB early samples** (the hwlab
board) get a **development layout**: one app slot, no OTA (flashing and
recovery through hwlab / USB-Serial-JTAG), the phase-0 bridge kept as a
separate image for H2 work. The hub detects the flash size at boot and
checks that the table belongs to it (`hub_core::layout`): a hub table for
another size, or partitions beyond the flash, is fatal (the hub does not
start); a foreign table (the stock one during bring-up) runs with the host
store in RAM. Tables: `firmware/hub/partitions-8mb.csv`,
`partitions-4mb-dev.csv`; binaries with `tools/make-partition-table.sh`.

| Partition | 8 MB production | 4 MB dev |
|---|---|---|
| bootloader | 0x000000, 32 KiB | same |
| partition table | 0x008000, 4 KiB | same |
| `nvs` (Wi-Fi, PHY calibration, small settings) | 0x009000, 24 KiB | same |
| `otadata` | 0x00F000, 8 KiB | — |
| `phy_init` | 0x011000, 4 KiB | 0x00F000, 4 KiB |
| core dump | 0x012000, 56 KiB | 0x2F0000, 64 KiB |
| hub app | `ota_0` 0x020000 and `ota_1` 0x320000, **3 MiB** each | `factory` 0x010000, **2 944 KiB** |
| `hubdata` (log-structured store) | 0x620000, 1 920 KiB | 0x300000, 1 MiB |

`hubdata` on the dev layout holds the 896 KiB areas of device-support 16.1
(ZRHP host store journal 128 KiB, configuration 32 KiB, converter database
2 × 256 KiB, local definitions 64 KiB) plus spare. CI checks the hub app
against both slots (3 MiB and 2 944 KiB); the application build is
2.1 MiB (see "Stage 2"). The stock `esp_ot_br` is 1.6 MB *with* OpenThread,
which the hub does not need.

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
cd ../.. && MAX_APP_BYTES=3014656 tools/make-app-image.sh release target/tbr-hub-app.bin tbr-hub
```

CI builds both firmwares and checks `tbr-hub` against the app slots (3 MiB
production, 2 944 KiB dev).

### Sizes (release, Xtensa Rust 1.90.0.0, ESP-IDF v5.5.1)

| Image | App image |
|---|---:|
| `tbr-bridge` 0.2.3 | 455 760 B |
| `tbr-hub` 0.1.0, ZRHP host core only (`--no-default-features`) | 664 560 B |

(Fat LTO at opt-level `z` since stage 2; 501 744 B and 738 416 B before. The
application build is in "Stage 2".)

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
- Hardware, 2026-11-20, `hubctl --read-only` on macOS through bridge 0.2.2
  to the shared H2 NCP (quick checks only): session at 921 600 in ~15 ms,
  `SYS_NEGOTIATE`/`SYS_GET_INFO` (fw 0.1.0, 4 MB flash, RTS/CTS), PAN 0x1C06
  on channel 17 up, slots (0: 0x00010000 running, 1: 0x00010001 inactive),
  frame counters; 200 `SYS_ECHO`s, mean 11.4 ms, worst 15 ms, no
  retransmissions (the NCP's ACK arrives ~7 ms after a request over the USB
  bridge; the host core adds < 1 ms, 0.7 ms against the simulated NCP).
- Not run on hardware yet: NCP resets/resume, formation, joins, `FW_*`
  (the NCP is shared), and `tbr-hub` on the S3 (not flashed: the bridge
  stays the board's image until the user decides).

## Stage 2: the application on the host core (`hub-app`, branch `stage2`)

The device engine (`hub-devices`) runs on the ZRHP host core (`hub-ncp`):

| Piece | What it does |
|---|---|
| `convert` | ZRHP events → engine inputs (APS indications with source IEEE and group, admitted joins, `EVT_GP_*` incl. commissioned sink records and remote sink/translation tables); engine APS/GP requests → wire (`APS_DATA_REQUEST`, `GP_*` 0x0A01–0x0A0D) |
| `transport` | The engine's `ZigbeeTransport` on `hub_ncp`: APS data non-blocking (`Client::start`, held across an NCP resume, outcomes counted), GP/group/remove/permit-join as calls only while the session is ready |
| `registry` | Device registry in the host store journal (device-support 16.1): `dev/<id>` = deflated JSON `DeviceSnapshot` (identity, fingerprint, descriptors, attribute support and the values resolution reads, self-description fragment as zdef CBOR, user settings and entity overrides, configure hash/results, GP sink record), `udef/<id>` = user definitions. Written on device changes (2 s coalescing) and in a 30 s sweep; identical values are skipped and snapshots hold no measured values, so reports never write flash. The journal stores values > 2000 B as parts, applied only when complete |
| `app` | One thread: engine, `ApiServer` sessions (any `SessionSink`), registry, 1 s tick; registers the hub endpoint 1 (HA profile) once (replayed by `hub-ncp` on every resume) and the coordinator IEEE; built-in catalog = the definition examples until banks exist |
| `catalog` | Trusted keys of the definitions catalog: **slot 0** = `keys/zdef-catalog-ed25519.pub.pem` (`ff931767…63c6a2`, kid `66f64ac1cbfc9e27`), **slot 1** reserved for a rotation (add the successor in a firmware release, sign with it, retire slot 0 later); `verify_manifest` / `verify_bank` (COSE_Sign1 Ed25519, `zdef`) |

`hub-devices` gained `persist` (`snapshot_device` / `restore_device`) and
`ApiServer::pump_events`; `hub-ncp` gained key/value records, PTA
`coex_enable` following the Wi-Fi state (decision N7, replayed on resume),
`group_membership`, `remove_device`, `permit_join_on`. All zigbee-rs crates
are pinned to one revision (master `7c4f96c`).

**Desktop.** `hubctl --serial … --store hubdata.bin serve [--listen
127.0.0.1:8099] [--token T]` runs the application with its WebSocket JSON
API (`ws://…/`); `hubctl ws-call URL --token T JSON…` is a minimal client;
`hubctl catalog-verify FILE…` checks catalog banks and `manifest.cose` with
the firmware's keys. `--sim` works for all of them.

**S3.** `tbr-hub` (hub mode) runs the application, the
WebSocket API on `ws://<ip>/api` (esp_http_server, one session per socket,
pushes through detached senders) once Wi-Fi credentials are stored
(console `wifi set <ssid> <password>`, `wifi`, `token`, `catalog`), and the
same API as JSON lines on CDC 0 while a host holds the port (a test path
without network).

**Size.** The application is the default `tbr-hub` build; it targets the
8 MB production layout (3 MiB slots) and the 4 MB dev layout (2 944 KiB).

| Build | App image | 3 MiB slot | 2 944 KiB dev slot |
|---|---:|---:|---:|
| `tbr-hub` (application, default) | 2 208 864 B | 70 % | 73 % |
| `tbr-hub --no-default-features` (ZRHP host core only) | 664 560 B | 21 % | 22 % |

Rust is ~1.2 MB of the application build (engine, `zdef`, `hub-ncp`, serde
and the `core`/`std`/`alloc` monomorphisations), C ~670 KB (ESP-IDF, Wi-Fi,
lwIP, mbedTLS, httpd). Size work so far: fat LTO at opt-level `z` (both
firmwares), std without backtrace symbolisation (−138 KB, panic messages
kept), the smaller `zdef` (zdef-size). `panic_immediate_abort` would save
another ~250 KB but loses panic messages; not used.

**Toolchain workaround.** The Xtensa LLVM of Rust 1.90.0.0 fails
instruction selection ("Cannot select: i32 = Constant<5376>") on the btree
node code of `resolve`'s merge map (5 376-byte leaf nodes); its values are
boxed.

**Tests (host).** `hub-app`: event and GP translation-payload conversion,
an API session against the simulated NCP (auth, `devices/list`,
subscription, a join reaching the engine and its Node_Desc_req going out
as `APS_DATA_REQUEST`), the registry surviving an app restart (name
override restored, removal deletes the record), the catalog key slots
(slot 0 = the committed PEM; a foreign key is refused by its kid).
`hub-devices`: snapshot round trip, measured values excluded, restore of a
self-describing device without traffic. `hub-ncp`: multi-part values
across compaction and a torn put.

## Gateway firmware (`firmware/gateway`: Dongle-M, S3 SuperMini + MG24)

One library, two binaries, selected by board profile and target:
`firmware/donglem` → `zrh-donglem` (`xtensa-esp32-espidf`, profile
`DONGLE_M`), `firmware/s3-mg24` → `zrh-s3-mg24` (`xtensa-esp32s3-espidf`,
profile `S3_MG24_DEV`). Boot: NCP control lines released first (on the
Dongle-M GPIO12 low, see "Strapping"), console, NVS, board overrides,
strapping/eFuse report, partition-table check, a pending radio install
through the Gecko bootloader, then the ZRHP host (`hub-ncp`, host store in
`hubdata`), the application (`hub-app`, `AppConfig::for_board`), Ethernet
and/or Wi-Fi with the WebSocket API on `ws://<ip>/api`, LED and button.

Console commands (`help`): `info` (board, heap, layout, straps), `board`,
`board set <k=v …>` (NCP wiring overrides, next boot), `hub status|echo|
form|permit|ncp reset`, `radio`, `radio install [A|B]`, `radio check A|B`,
`net`, `wifi set|clear`, `token`, `restart`.

### SONOFF Dongle-M

Sources: SONOFF's guide "Running ESPHome firmware on Dongle-M" (GPIO table,
Ethernet block, MG24 bootloader script), the eWeLink forum post "ESPHome on
SONOFF Dongle Max" (adds GPIO5 = PHY reset/power) and the Dongle-M port of
esp-thread-br (`sdkconfig.defaults.sonoff_dongle_m`: IP101, RMII clock in
on GPIO0, PHY address 1, the fixed RMII data pins; that port names GPIO12
"reset" and GPIO15 "boot" — SONOFF's own table and bootloader script say the
opposite, and the profile follows SONOFF).

| Function | ESP32 GPIO | Notes |
|---|---|---|
| MG24 UART: ESP TX → MG24 RX | 17 | UART1; no RTS/CTS on this board |
| MG24 UART: ESP RX ← MG24 TX | 13 | internal pull-up (MG24 TX floats in reset) |
| MG24 RESET_N | 15 | active low, push-pull; MTDO strap (pull-up at reset = MG24 runs) |
| MG24 bootloader entry | 12 | active high; MTDI = flash-voltage strap, see below |
| Ethernet MDC / MDIO | 23 / 18 | IP101GA, PHY address 1 |
| Ethernet REF_CLK in | 0 | external 50 MHz oscillator |
| PHY reset / power (oscillator enable) | 5 | driven high only after boot |
| RMII data | 19, 21, 22, 25, 26, 27 | fixed by the ESP32 EMAC |
| RGB LED red / green / blue | 4 / 14 / 2 | active high |
| Button | 34 | active low, input only (board pull-up) |
| Console | UART0 | CP2102N USB bridge, 115 200 |

**NCP line.** 460 800 8N1 without flow control by default (ZRHP's ARQ
recovers what an overrun loses), 115 200 as the hunting fallback (stock
images, bootloader rate); 921 600 only once it is verified on the hardware.
The hub's lines: `board set baud=921600` etc.

**Strapping.** GPIO12 (MTDI) selects VDD_SDIO at reset: high = 1.8 V, which
does not boot the 3.3 V flash unless the eFuse forces the voltage. The
firmware configures GPIO12 low before anything else, drives it high only
for the ~0.7 s bootloader entry (BOOT high, 100 ms, RESET pulse 100 ms,
500 ms, BOOT low), never across a reset it starts itself (`esp_restart`
releases the pin), and reports the eFuse state at boot (`info`: forced
3.3 V / 1.8 V / off, or "strap"). `espefuse.py set_flash_voltage 3.3V`
removes the hazard for good; it is irreversible and optional — check the
module's flash voltage and back up first. GPIO0 (download-mode strap) is
the RMII clock input: the oscillator is enabled by GPIO5, which the firmware
drives high only after boot. GPIO2, GPIO5, GPIO15 are released at reset.

**Flash layout (16 MB, `firmware/gateway/partitions-16mb.csv`).** The hub
checks it at boot (`hub_core::layout`, `Layout::Production16`).

| Partition | Offset | Size |
|---|---|---|
| bootloader | 0x001000 | 28 KiB (classic ESP32: at 0x1000) |
| partition table | 0x008000 | 4 KiB |
| `nvs` | 0x009000 | 24 KiB |
| `otadata` | 0x00F000 | 8 KiB |
| `phy_init` | 0x011000 | 4 KiB |
| core dump | 0x012000 | 56 KiB |
| `ota_0`, `ota_1` (A/B) | 0x020000, 0x420000 | 4 MiB each |
| `hubdata` | 0x820000 | 8 064 KiB |

`hubdata` areas (`hub_core::layout::hubdata16`, offsets inside the
partition):

| Area | Offset | Size |
|---|---|---|
| Host store journal | 0x000000 | 256 KiB (128 KiB used today) |
| Configuration | 0x040000 | 64 KiB |
| Converter bank A / B | 0x050000 / 0x150000 | 1 MiB each (the full catalog with room) |
| Local definitions | 0x250000 | 512 KiB |
| Radio banks A + B | 0x2D0000 | 2 × 1 792 KiB (4 KiB header + `.gbl`) |
| Free | 0x650000 | 1 600 KiB |

**RAM (2 MB PSRAM).** `CONFIG_SPIRAM_USE_MALLOC` with allocations above
4 KiB in PSRAM, Wi-Fi/lwIP buffers allowed in PSRAM, 48 KiB of internal RAM
reserved for DMA (EMAC, UART) and stacks. Thread stacks must stay internal:
ESP-IDF asserts on flash writes from a task whose stack is in PSRAM, and the
store, registry and radio banks write flash. Measured static use (release
ELF): IRAM 102.7 KiB, DRAM `.data` + `.bss` 32.7 KiB (S3 SuperMini build:
92.6 KiB, 35.6 KiB). Internal stacks: console/main 8, ZRHP driver 32,
lifecycle 12, app 64, HTTP server 8, Ethernet 4, Wi-Fi 6 (only with Wi-Fi),
LED 3, UART events 3, radio verify 6 (after an install) KiB, about 146 KiB,
plus ESP-IDF tasks (~16 KiB), EMAC DMA buffers (10 + 10 × 512 B), UART rings
(12 KiB) and Wi-Fi static RX buffers (16 KiB, only with Wi-Fi): roughly
200 KiB of the classic ESP32's ~270 KiB internal heap (estimate, to be
measured: `info` prints free / minimum / largest internal block and
PSRAM). PSRAM holds the engine (200 devices ≈ 100–300 KiB,
device-support 16.4), the catalog index and API frames. If internal RAM is
short on hardware: the ZRHP driver stack (`HubConfig.driver.stack_size`),
the app stack (`hub_app::APP_STACK_ESP`), `CONFIG_ESP_WIFI_STATIC_RX_BUFFER_NUM`.

**LED.** Blue blinking: no network; yellow: network, no NCP session; green:
network and NCP session; white blinking: joining open; magenta fast blink:
radio install; red: partition-table error or the last radio update failed.
**Button:** a short press opens joining for 180 s.

### Flashing the Dongle-M (back up the stock firmware first)

The ESP32 is behind the CP2102N (macOS `/dev/cu.SLAB_USBtoUART` or
`/dev/cu.usbserial-*`, Linux `/dev/ttyUSB0`). esptool v5 syntax; images from
the CI artifact `gateway-firmware` or a local build.

```sh
PORT=/dev/cu.usbserial-XXXX
# 1. Identify, record the eFuses (VDD_SDIO, chip revision) and back up all 16 MB.
esptool --chip esp32 --port $PORT flash-id
espefuse --chip esp32 --port $PORT summary > dongle-m-efuse.txt
esptool --chip esp32 --port $PORT --baud 921600 read-flash 0 0x1000000 dongle-m-stock-16mb.bin
esptool --chip esp32 --port $PORT --baud 921600 verify-flash 0 dongle-m-stock-16mb.bin
shasum -a 256 dongle-m-stock-16mb.bin   # keep the file and the hash safe (it holds Wi-Fi credentials)

# 2. Flash the hub (whole chip erased: the stock NVS/data do not belong to this layout).
esptool --chip esp32 --port $PORT erase-flash
esptool --chip esp32 --port $PORT --baud 921600 write-flash --flash-mode dio --flash-freq 80m --flash-size 16MB \
  0x1000 bootloader-donglem.bin 0x8000 partitions-16mb.bin 0x20000 zrh-donglem-app.bin

# 3. Console (115 200): wifi set <ssid> <password>, token, hub status, radio …
python3 -m serial.tools.miniterm $PORT 115200

# Restore the stock firmware at any time:
esptool --chip esp32 --port $PORT --baud 921600 write-flash 0 dongle-m-stock-16mb.bin
```

`flash-id` first checks that esptool resets the ESP32 into its ROM loader
through the bridge (SONOFF Dongle Flasher uses the same port and can always
reinstall the current stock firmware). Do not continue without a verified
backup.

**Never remove the MG24 Gecko bootloader.** Without it the MG24 can only
be recovered over SWD. The hub refuses images with a bootloader or Secure
Engine upgrade; do not mass-erase the MG24 with Simplicity Commander. Keep a
copy of SONOFF's stock MG24 `.gbl` (Zigbee NCP or Thread RCP) to go back:
it installs through the same path as any other image.

### Radio firmware through the Gecko bootloader (Dongle-M)

The alternative to `FW_*` A/B for NCPs whose flash belongs to the Gecko
bootloader (`hub_ncp::gecko`). Upload, then install:

```sh
TOKEN=…   # console: token
curl -H "Authorization: Bearer $TOKEN" --data-binary @ncp.gbl http://<ip>/radio/gbl
curl -X POST -H "Authorization: Bearer $TOKEN" http://<ip>/radio/install   # or the console: radio install
curl http://<ip>/radio                                                    # banks, job, last result
```

1. **Upload** streams the image into the radio bank that is not the
   last-known-good one, checks it (GBL header and end tag CRC-32, no
   bootloader / Secure Engine upgrade, ≤ 1 788 KiB), reads it back and
   records it as staged.
2. **Install** stores the job in NVS and restarts the ESP32. Before the hub
   opens the UART, the next boot switches the UART to 115 200, enters the
   bootloader (GPIO12 high, GPIO15 pulse 100 ms, GPIO12 low after 500 ms),
   probes the menu (`BL >`), sends `1`, uploads the image by XMODEM-CRC
   (128-byte packets, stop-and-wait, retransmission on NAK or timeout, no
   flow control needed), waits for `Serial upload complete` and sends `2`.
3. **Verify:** the hub starts; within 90 s a ZRHP session must come up and
   `SYS_NEGOTIATE.build_id` must match the image's zigbee-rs metadata. Then
   the bank becomes last-known-good. An image without that metadata is
   accepted on the session alone ("unverifiable", logged).
4. **Rollback:** a failed install or verification marks the bank bad and
   installs the last-known-good bank the same way (once; a failed rollback
   ends the job with an error and the LED red). The first update over the
   stock MG24 firmware has no last-known-good image: keep SONOFF's `.gbl`.

Host tests run the client against a simulated bootloader
(`hub_ncp::gecko::sim`: menu, `C` start, XMODEM with injected NAKs and lost
ACKs, block-number wrap, rejected images, missing bootloader) and the
bank/job logic on RAM flash.

**What the hub expects from the MG24 NCP image** (to agree with the
zigbee-rs MG24 port): ZRHP v1 on the MG24 USART wired to the ESP32 (Dongle-M:
the pins of SONOFF's stock image), 460 800 8N1 without flow control as the
default (115 200 accepted, the hub hunts both); `SYS_GET_INFO.transport = 2`,
`uart_flow = 0`, `flash_size = 1 536 KiB` (no `FW_*` slots); a new `chip`
value for EFR32MG24 (proposal: 12); the release `.gbl` with the application
tag (version) and a metadata tag (`commander gbl create --metadata`) holding
the record `"ZRHP", u8 1, [8] build_id, u8 n, [n] fw` with the `build_id` and
`fw` the image reports in `SYS_NEGOTIATE` (`hub_ncp::gecko::gbl::ZrhpMeta`).

### ESP32-S3 SuperMini + RF-BM-MG24B1 (bench)

Default wiring (`S3_MG24_DEV`; `board set` changes it, checked against the
S3's flash/PSRAM and USB pins, applied at the next boot):

| S3 GPIO | MG24 (RF-BM-MG24B1) | Function |
|---|---|---|
| 4 | PA06 (RX) | S3 TX → MG24 RX |
| 5 | PA05 (TX) | S3 RX ← MG24 TX |
| 6 | RESET_N | open drain, active low |
| 7 | PA08 (CTS) | S3 RTS (optional) |
| 8 | PA07 (RTS) | S3 CTS (optional) |
| 0 | — | BOOT button: opens joining |

Default line 460 800 without flow control; with RTS/CTS wired:
`board set flow=1 baud=921600 fallback=460800`. Example rewiring:
`board set tx=10 rx=11 rts=none cts=none reset=12`. The MG24 is flashed with
a J-Link over SWD (no Gecko bootloader needed). The S3 uses the 4 MB dev
layout of the Thread BR board:

```sh
esptool --chip esp32s3 --port $PORT write-flash 0x0 bootloader-s3-mg24.bin 0x8000 partitions-4mb-dev.bin \
  0x10000 zrh-s3-mg24-app.bin
```

### Building

```sh
. ~/export-esp.sh            # espup install --toolchain-version 1.90.0.0 --targets esp32,esp32s3 --std
(cd firmware/donglem && cargo build --release)
(cd firmware/s3-mg24 && cargo build --release)
CHIP=esp32 FLASH_SIZE=16MB MIN_REV_FULL=300 MAX_APP_BYTES=4194304 \
  tools/make-app-image.sh release target/zrh-donglem-app.bin zrh-donglem
CHIP=esp32s3 FLASH_SIZE=4MB MAX_APP_BYTES=3014656 \
  tools/make-app-image.sh release target/zrh-s3-mg24-app.bin zrh-s3-mg24
tools/make-partition-table.sh firmware/gateway/partitions-16mb.csv target/partitions-16mb.bin 16MB
```

Sizes (release, ESP-IDF v5.5.1, 2026-11-20): `zrh-donglem` 2 274 016 B
(2.17 MiB of the 4 MiB slot), `zrh-s3-mg24` 2 218 064 B (of the 2 944 KiB
dev slot). CI (`gateway` job) builds both, checks them against their slots
and uploads images, bootloaders and partition tables. Dongle-M build:
flash DIO 80 MHz and PSRAM 80 MHz as in SONOFF's ESPHome configuration
(GPIO9/10 carry the in-package PSRAM, so no QIO), `CONFIG_ESP32_REV_MIN_3`
(V3 silicon: no PSRAM cache workaround), console on UART0.

## License

MIT OR Apache-2.0, like zigbee-rs.
