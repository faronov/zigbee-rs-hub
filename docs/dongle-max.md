# SONOFF Dongle Max (Dongle-M)

**Status: build-tested, not hardware-tested.** `firmware/donglem` is the
board bring-up stage of the Dongle-M port: network, LED, button, MG24
lines and a read-only EZSP probe. It runs no coordinator backend and no hub
application yet; those are wired after the backend selection lands. The
layout, sdkconfig and memory budget below are planned for the product
feature set. Nothing here is a Zigbee or interoperability claim.

## Product target

Decided 2026-11-24; wired after the backend selection (coordinator-boundary
PR H) and the EZSP backend.

| Feature | Plan |
|---|---|
| Hub application: devices, catalog, groups, bindings, reporting, automations | yes |
| Web UI with the advanced console, WebSocket API | yes |
| MQTT with Home Assistant discovery | yes |
| Zigbee device OTA | yes, with a dedicated image store (`otastore`) |
| ESP32 self-update | yes, A/B app slots with a trial boot |
| Ethernet (also PoE-powered) and Wi-Fi | yes; Ethernet first, Wi-Fi station as the fallback |
| Network backup/restore (Open Coordinator Backup, EZSP key export) | yes |
| Green Power through EZSP | later |
| MG24 firmware update through the Gecko bootloader | later; flash reserved (`radio`), code not compiled |
| TCP bridge to the MG24 UART (port 6638, as the stock firmware) | **no** |
| Bluetooth / BLE | **no** (`CONFIG_BT_ENABLED=n`, its RAM stays with the heap) |

## Hardware

| Item | Value |
|---|---|
| Host | ESP32-D0WDR2-V3 rev v3.1: dual core, 240 MHz, 2 MB in-package quad PSRAM |
| Flash | 16 MiB (Puya, JEDEC 85 20 18), no flash encryption, no secure boot |
| Console | CP2102N USB-UART on UART0; its DTR/RTS drive EN/GPIO0 (classic auto-reset); USB product "SONOFF Dongle Max MG24" |
| Radio | EFR32MG24A420F1536IM48; stock firmware "1.0.0" (EmberZNet NCP, EZSP over ASH), 115 200 Bd, no RTS/CTS; Gecko bootloader at 115 200 |
| Stock ESP32 firmware | "Dongle-M" 1.0.7, ESP-IDF v5.2.1, in `ota_0`; second-stage bootloader ESP-IDF v5.2.1 (silent log) |

Pins (`hub_core::profile::DONGLE_M`, the single source the firmware uses;
SONOFF's guide "Running ESPHome firmware on Dongle-M" and its ESPHome
configuration):

| Function | ESP32 GPIO | Notes |
|---|---|---|
| MG24 UART RX (MG24 → ESP32) | 13 | UART1, internal pull-up |
| MG24 UART TX (ESP32 → MG24) | 17 | |
| MG24 nRESET | 15 | active low, 100 ms pulse; MTDO strap (pull-up at reset: the MG24 runs while the ESP32 boots) |
| MG24 bootloader entry | 12 | high across an MG24 reset enters the Gecko bootloader; **MTDI strap, see below** |
| RGB LED red / green / blue | 4 / 14 / 2 | LEDC PWM, active high; GPIO2 is a strap (released at reset) |
| User button | 34 | active low, input only, no internal pull-up (board pull-up) |
| Ethernet MDC / MDIO | 23 / 18 | IP101, PHY address 1 |
| Ethernet REF_CLK | 0 | 50 MHz from the board's oscillator, `CLK_EXT_IN`; GPIO0 is the download strap |
| Ethernet PHY power | 5 | driven high after boot, before the EMAC starts |
| RMII data (fixed) | 19, 21, 22, 25, 26, 27 | TXD0, TX_EN, TXD1, RXD0, RXD1, CRS_DV |
| Reserved | 6–11, 16 | SPI flash; GPIO16 = the in-package PSRAM's CS (ESP-IDF `esp_psram_impl_quad.c`) |

**GPIO12 (MTDI)** selects the flash/PSRAM supply voltage at every ESP32
reset, and this unit has no eFuse override (`VDD_SDIO_FORCE` = 0): it must
be low at reset. The firmware drives it low before anything else and high
only for the ~0.7 s of a Gecko bootloader entry (`mg24 bootloader`); an
ESP32 reset releases the pin. `info` reports the straps.

**GPIO0/GPIO5**: the 50 MHz oscillator is powered by GPIO5, which the
firmware drives only after boot, as SONOFF's ESPHome configuration does.
Whether a software restart with the oscillator running always boots from
flash is checked on hardware (HIL list below).

## Image and sdkconfig

`firmware/donglem` (`zrh-donglem`: board profile, sdkconfig, partition
tables) on `firmware/gateway` (`zrh-gateway`: the board-generic firmware),
target `xtensa-esp32-espidf`, ESP-IDF v5.5.1, Xtensa Rust 1.90 (`esp`).

| Setting | Value | Why |
|---|---|---|
| Image header | DIO, 40 MHz, 16 MB, chip v3.0–v3.99 | as the stock bootloader and application: the stock bootloader stays and boots it |
| PSRAM | quad, 40 MHz, `CONFIG_SPIRAM_USE_MALLOC`, C mallocs > 1 KiB and Wi-Fi/lwIP buffers to PSRAM, 32 KiB internal kept for DMA | 80 MHz PSRAM would need 80 MHz flash; heap policy of `firmware/hub` and `hub_core::resources` |
| `CONFIG_ESP32_REV_MIN_3` | v3.0 | no PSRAM cache workaround: smaller, faster code |
| Rust heap | prefers PSRAM (`firmware/hub/src/alloc.rs`, shared) | as `firmware/hub` |
| Atomics | `-C target-feature=-s32c1i` + the same compile-time guard | S32C1I is not atomic between the cores on PSRAM; ESP-IDF's `__atomic_*` lock PSRAM addresses (`CONFIG_STDATOMIC_S32C1I_SPIRAM_WORKAROUND`, on for SPIRAM on the ESP32). The build has no S32C1I in Rust code; `atomics` measures it |
| Bluetooth | off | not in the feature set |
| Panic | recorder in RTC memory (`firmware/hub/src/cpanic.rs`, shared, `--wrap=esp_panic_handler`); core dump (ELF, 4 tasks) to `coredump` | the stock table has no `coredump`: there only the recorder |
| Self-update | `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE`; the image confirms itself once started | whether the stock bootloader has rollback support is unknown (silent log): without it a new image boots without the automatic fallback (open question) |
| Web/MQTT | WebSocket support in esp_http_server; esp-mqtt plain TCP only; 3 SNTP servers; 4 KiB tcpip stack | as `firmware/hub` |
| Internal RAM | no IPv6, ROM printf (nano), silent asserts | as `firmware/hub` |
| NVS | own namespace `zrh`; no erase on init errors; Wi-Fi in RAM; PHY data not stored | while the image may share the stock NVS (bring-up); on the product table NVS is the hub's own |

Build (the first build downloads ESP-IDF into `.embuild`):

```sh
. ~/export-esp.sh && cd firmware/donglem && cargo build --release && cd ../..
CHIP=esp32 FLASH_SIZE=16MB FLASH_FREQ=40m MIN_REV_FULL=300 MAX_APP_BYTES=4194304 \
  tools/make-app-image.sh release target/zrh-donglem-app.bin zrh-donglem
python3 tools/make-otadata.py --seq 2 -o target/otadata-donglem-ota1.bin
```

The bring-up image is 0.92 MB. CI checks it against the 4 MiB product slot.

## Product layout (`firmware/donglem/partitions-16mb.csv`)

| Partition | Offset | Size | Content |
|---|---|---|---|
| (bootloader) | 0x1000 | ≤ 28 KiB | the stock ESP-IDF v5.2.1 bootloader, kept |
| (partition table) | 0x8000 | 4 KiB | |
| `nvs` | 0x9000 | 80 KiB | settings, Wi-Fi, PHY calibration |
| `otadata` | 0x1D000 | 8 KiB | A/B selection |
| `phy_init` | 0x1F000 | 4 KiB | |
| `coredump` | 0x20000 | 64 KiB | last core dump (ELF, 4 tasks) |
| `ota_0` | 0x30000 | 4 MiB | application slot A |
| `ota_1` | 0x430000 | 4 MiB | application slot B |
| `hubdata` | 0x830000 | 2 MiB | host store journal, catalog banks, definitions, web tools: the 8 MB map of `hub_core::layout` (1 728 KiB) + 320 KiB spare |
| `otastore` | 0xA30000 | 4 MiB | Zigbee device OTA images (type 0x41) |
| `radio` | 0xE30000 | 1 856 KiB | two 928 KiB banks for MG24 `.gbl` images (current, previous; type 0x42), blank until the MG24 updater |

Sizing:

- **App slots, 4 MiB.** The S3 8 MB image with MQTT and the advanced
  console is 2.80 MB (main 6fc074d); Zigbee device OTA adds ~0.14 MB, the
  EZSP backend replaces ZRHP (`hub-ncp` + `zigbee-zrhp`, ~0.11 MB),
  Ethernet adds ~0.04 MB, the S3's TinyUSB bridge goes; self-update,
  backup/restore, Green Power and the MG24 updater ~0.15 MB. Estimate
  3.0–3.2 MB: ≥ 1 MB (≈ 25 %) headroom. Both slots hold the application;
  the store does not borrow the inactive slot (the S3 8 MB image does), so
  a self-update never drops device images and the previous image stays for
  a rollback.
- **`otastore`, 4 MiB.** Real images (Koenkk index, 1 034 images): median
  251 KB, 90 % ≤ 446 KB, largest 2.1 MB: the largest plus several typical
  ones, or ~15 typical ones. Same store format as `docs/ota.md` (index A/B
  + images), on its own partition (follow-up in `hub-app`).
- **`hubdata`, 2 MiB**: is a `Layout::Production` with the 8 MB map (the
  journal stays at the start; tested in `hub_core::layout`); the spare
  allows larger catalog banks later.
- **`radio`**: the MG24 has 1 536 KiB of flash; EmberZNet NCP images are a
  fraction of it (to be measured for the pinned firmware). Two banks keep
  the running image for a rollback, as the updater requirements in
  `coordinator-profiles.md` need.
- `hub_core::layout::check` accepts the table on 16 MB flash; the stock
  table is `Foreign` (no `hubdata`: host store in RAM).

## RAM budget (2 MB PSRAM)

Static RAM of the bring-up build: IRAM 101 KB of 128 KB (`.iram0.text`
99 991 B + vectors), DRAM `.data` + `.bss` 32.9 KB. The application code
runs from flash; IRAM grows little with it (levers if it ever runs out:
`CONFIG_ESP_WIFI_IRAM_OPT`, `CONFIG_ESP_WIFI_RX_IRAM_OPT`, FreeRTOS and heap
functions to flash, at some speed).

**Internal (byte-accessible DRAM).** Estimates, calibrated against the S3
hub's measurements (README: ~295 KB at boot, 118–131 KB free with Wi-Fi,
the application, the HTTP server and a WebSocket session); measured on
hardware with `info` (HIL step 1).

| Item | KB |
|---|---:|
| DRAM heap at boot (SRAM2 192 KiB + SRAM1 128 KiB, minus static data and ROM areas; BT off) | ~299 |
| ESP-IDF start-up: system tasks, timers, event loop, console and NCP UART drivers | −25 |
| lwIP + tcpip task | −10 |
| Ethernet: EMAC DMA (10 + 10 × 512 B, descriptors), RX task, netif | −16 |
| Wi-Fi station, only when running (driver, 10 static RX DMA buffers, task); dynamic buffers in PSRAM | −50 |
| Application thread (40 KiB stack as on the S3) and its internal-only allocations | −45 |
| EZSP backend: host thread, UART rings 4 + 2 KiB | −16 |
| HTTP server (web UI, WebSocket API) | −15 |
| MQTT, when enabled: bridge thread 16 KiB, esp-mqtt task 5 KiB, buffers | −23 |
| Zigbee OTA server, self-update, console, LED | −12 |
| Other (difference between this model and the S3 measurements) | −25 |
| **Free: Ethernet uplink, MQTT on** | **≈ 110** |
| **Free: Wi-Fi uplink, MQTT on** | **≈ 75** |
| **Free: both up** | **≈ 60** |

Ethernet first keeps the Wi-Fi station (~50 KB) off on a cabled dongle.
Target: ≥ 40 KB minimum free in every case. Levers if the measurement is
lower: Wi-Fi static RX buffers 10 → 6 (−6 KB), no A-MPDU RX, the
application thread's stack from its measured high water, fewer HTTP
sockets (the `hub_core::resources` limits).

**PSRAM (2 MB).** `hub_core::resources` gives a 2 MB board the same limits
as any PSRAM size; the worst case fits with margin:

| Item | KB |
|---|---:|
| Application baseline: definitions, decoded example catalog (~90 KB), registry (S3 SuperMini 2 MB, measured) | ~160 |
| Devices: ~2–3.4 KB per device in the registry and models | 100 devices: ≤ 350 |
| WebSocket queues (≤ 128 KB per client, ≤ 384 KB total) | ≤ 384 |
| MQTT queue (≤ 64 KB) and device model (≤ 256 KB) | ≤ 320 |
| Wi-Fi/lwIP dynamic buffers | 30–100 |
| HTTP/OTA/self-update chunks (4 KiB), OTA streams (fixed, ≤ 4), snapshot pieces (8 KB) | < 64 |
| **Worst case** | **≈ 1 280 of 2 048** |

## Flash layouts in use

**Bring-up (first flash): the stock table, unchanged.**
`firmware/donglem/partitions.csv` reproduces the stock table at 0x8000
byte for byte (CI checks its SHA-256). Only two regions are written:

| Region | Offset | Size | First flash |
|---|---|---|---|
| bootloader (ESP-IDF v5.2.1) | 0x1000 | 27 KiB | kept |
| partition table | 0x8000 | 4 KiB | kept |
| `nvs` (stock settings, credentials) | 0x1C000 | 256 KiB | kept (see runtime writes) |
| `otadata` sector 0 (stock entry: seq 1 → `ota_0`) | 0x5C000 | 4 KiB | kept |
| **`otadata` sector 1** | **0x5D000** | **4 KiB** | **written: seq 2, VALID → `ota_1`** (blank before) |
| `phy_init` | 0x5E000 | 4 KiB | kept |
| `ota_0` (stock application 1.0.7) | 0x60000 | 6 MiB | kept |
| **`ota_1`** | **0x660000** | **6 MiB** | **written: `zrh-donglem-app.bin`** (blank before) |
| `files` (SONOFF LittleFS: config, MG24 info) | 0xC60000 | 3 712 KiB | kept |

The bootloader picks the valid `otadata` entry with the highest sequence
number, so erasing sector 1 again boots the untouched stock application.
Runtime writes: only NVS keys of the namespace `zrh` in the stock `nvs`,
and only when a setting is changed (`wifi set`, `board set`, `led`). NVS is
opened without the erase-on-error fallback; Wi-Fi keeps its configuration
in RAM and the PHY calibrates at every boot, so the stock namespaces are
neither read nor written. Nothing writes the MG24.

**Product (with the hub application): `partitions-16mb.csv`.** Moving to
it rewrites everything except the bootloader: partition table, `nvs`
(erased: the hub's own), `otadata`, `coredump`, both app slots (the stock
application and SONOFF's `files` are gone), `hubdata`, `otastore`, `radio`
(erased). The way back to stock is the full backup (below). That step is
for the release with the hub application and needs explicit confirmation.

## Flashing the bring-up image with hwlab `esprom`

Verified backup first (already taken for the lab unit:
`~/.hwlab/artifacts/sonoff-dongle-max/esprom-flash-esp32-<mac>-16384K-<sha>.bin`,
16 MiB, read twice and checked against the chip's MD5; it holds the
device's credentials: **never commit or share it**).

```sh
D=cp210x-<CP2102N serial>
BK=~/.hwlab/artifacts/sonoff-dongle-max/esprom-flash-esp32-<mac>-16384K-<sha>.bin
hwlab target bsl identify --plugin esprom --device $D
# Plan only (reads, no gate; the chip stays in the loader):
hwlab target flash program --plugin esprom --device $D --path target/zrh-donglem-app.bin \
  --arg address=0x660000 --arg dry_run=true --yes
# 1. The application into ota_1 (an app partition: not protected).
hwlab target flash program --plugin esprom --device $D --path target/zrh-donglem-app.bin \
  --arg address=0x660000 --token program-flash --arg backup=$BK --config allow_flash=true --yes
# 2. otadata sector 1 (protected, raw data): boot ota_1.
hwlab target flash program --plugin esprom --device $D --path target/otadata-donglem-ota1.bin \
  --arg address=0x5D000 --arg raw=true --arg confirm_protected=modify-protected-region \
  --token program-flash --arg backup=$BK --config allow_flash=true --yes
# 3. Run, with 8 s of console.
hwlab target bsl reset --plugin esprom --device $D --config allow_reset=true --arg console_ms=8000 --yes
```

Write the application before `otadata`: a failed first step leaves the
stock application selected.

## Back to the stock firmware

```sh
# Stock application again: otadata sector 1 back to blank (from the backup).
hwlab target flash restore --plugin esprom --device $D --arg backup=$BK \
  --arg address=0x5D000 --arg length=0x1000 --arg confirm_protected=modify-protected-region \
  --token restore-flash --config allow_flash=true --yes
# Optional: ota_1 back to blank.
hwlab target flash restore --plugin esprom --device $D --arg backup=$BK --arg partition=ota_1 \
  --token restore-flash --config allow_flash=true --yes
```

The `zrh` NVS keys stay (the stock firmware ignores them). A whole-flash
restore (`hwlab target flash restore … --arg backup=$BK`, with
`confirm_protected`; the only way back from the product layout) also puts
back the NVS of the backup's date: it rolls back stock settings changed
since. SONOFF Dongle Flasher can also reinstall the stock firmware.

## Console (UART0, 115 200 8N1, the CP2102N port)

| Command | What it does |
|---|---|
| `info` | firmware, chip, memory mode, partitions and layout verdict, running/boot slot, straps, last panic |
| `board`, `board set <k=v …>`, `board clear` | profile pins; NCP wiring overrides (e.g. `baud=460800`), next boot |
| `net`, `wifi set <ssid> [pw]`, `wifi clear` | Ethernet/Wi-Fi state; station credentials (the station starts when Ethernet has had no address for 10 s; next boot) |
| `led <0-100>` | LED brightness |
| `mg24` | line levels, UART rate and errors |
| `mg24 reset` | MG24 reset into its application (bootloader line low) |
| `mg24 bootloader` | bootloader line high, reset pulse, print the Gecko bootloader's output and menu after a newline, reset back into the application. Nothing is uploaded |
| `mg24 ezsp info` | ASH reset (the NCP restarts), EZSP version negotiation (`hub-ezsp` host core), `getValue(VERSION_INFO)`, `getEui64`, `getMfgToken` (string, board name), `networkState` and, if joined, `getNetworkParameters`/`getNodeId`. Read-only |
| `mg24 ezsp info init` | also `networkInit` (resumes a stored network; no form, leave or configuration), then an MG24 reset so the network does not keep running without a host |
| `mg24 baud <rate>` | MG24 UART rate until the next boot |
| `cpu [ms]`, `atomics` | CPU use per task; lost atomic updates between the cores (Rust atomics must lose none) |
| `restart` | ESP32 restart |

LED: blue blinking = no network, green = network up, magenta blinking =
probe or bootloader banner running, white flash = button. The button is
reported only (log, `info`) until the application gives it a function.

After an ASH reset an EmberZNet NCP has no network up until the host calls
`networkInit`: `mg24 ezsp info` normally shows `NO_NETWORK` even with a
stored network.

## Not in this stage

- The hub application, the EZSP coordinator backend, the store on
  `otastore`, self-update, backup/restore: after the backend selection.
- **MG24 firmware updates** through the Gecko bootloader: the `donglem`
  branch's updater (radio banks in its own 16 MB `hubdata`, ZRHP-based
  verification) does not port to an EZSP NCP, and the updater
  requirements of `coordinator-profiles.md` (backup export first,
  allow-list manifest, persisted state) are not met. Not compiled; the
  `radio` partition is reserved for it.
- The ESP32-S3 SuperMini + MG24 bench profile of the `donglem` branch.

## HIL checklist (with the user's confirmation, after a fresh backup check)

1. Flash the bring-up image as above; capture the boot log: PSRAM 2 MB
   found, `memory mode psram`, stock table `Foreign`, running `ota_1`,
   straps; `info` heap figures for the RAM budget.
2. `atomics`: Rust atomics lose nothing on PSRAM (bare S32C1I may).
3. Ethernet link and DHCP; Wi-Fi fallback with Ethernet unplugged (heap
   with Wi-Fi up); LED.
4. `mg24 bootloader` banner (version), `mg24 ezsp info` (EZSP 13, EmberZNet
   7.4.x, EUI-64), with and without `init`.
5. `restart` with Ethernet up (GPIO0 clock at a software reset).
6. Back to stock (otadata sector 1 restore): the stock firmware boots and
   works.
