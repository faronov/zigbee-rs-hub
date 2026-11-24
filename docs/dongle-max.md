# SONOFF Dongle Max (Dongle-M)

**Status: board bring-up hardware-tested on one unit (2026-11-24), no hub
application yet.** `firmware/donglem` is the board bring-up stage of the
Dongle-M port: network, LED, button, MG24 lines, a read-only EZSP probe and
the way back to SONOFF's firmware. It runs no coordinator backend and no hub
application yet; those are wired after the backend selection lands. The
data region, sdkconfig and memory budget below are planned for the product
feature set. Nothing here is a Zigbee or interoperability claim.

**Hardware results** (lab unit, ESP32-D0WDR2-V3 rev v3.1, image in `ota_1`
flashed over USB with hwlab esprom, SONOFF's bootloader and `ota_0` kept):

- Boots from SONOFF's v5.2.1 bootloader; the bootloader itself prints
  nothing on the CP2102N, and it leaves UART0 off GPIO1/3, so the
  application re-routes the console first (`console::route`).
- Console, `info`, `net` (Ethernet driver up, no cable; Wi-Fi not set),
  `atomics` (0 lost on PSRAM and internal RAM), `stock`; LED blinks blue
  without network, button press flashes white.
- MG24 (stock "1.0.0"): ASH v2, EZSP 13 after two version exchanges,
  EmberZNet 7.4.5.0, EUI-64 `B0E8E8FFFE5D4D03`, mfg "SONOFF", board
  "Dongle-M", `networkInit` NOT_JOINED (no stored network), no ASH
  retransmissions or errors at 115200 Bd without flow control.
- Gecko Bootloader v2.04.02 at 115200 Bd (menu: upload gbl / run / ebl
  info).
- Heap at the console: internal 247 KiB free (largest block 92 KiB), PSRAM
  2039 / 2048 KiB.
- Not yet run: Ethernet link, Wi-Fi station, `stock confirm`, installation
  through SONOFF's web console.

## Product target

Decided 2026-11-24; wired after the backend selection (coordinator-boundary
PR H) and the EZSP backend.

| Feature | Plan |
|---|---|
| Hub application: devices, catalog, groups, bindings, reporting, automations | yes |
| Web UI with the advanced console, WebSocket API | yes |
| MQTT with Home Assistant discovery | yes |
| Zigbee device OTA | yes: 1 MiB image store in the hub's data region, larger images streamed from a client |
| ESP32 self-update | yes, into the other app slot of SONOFF's table |
| Ethernet (also PoE-powered) and Wi-Fi | yes; Ethernet first, Wi-Fi station as the fallback |
| Network backup/restore (Open Coordinator Backup, EZSP key export) | yes |
| Green Power through EZSP | later |
| MG24 firmware update through the Gecko bootloader | later; space reserved, code not compiled |
| Installation through SONOFF's web console | yes: **SONOFF's partition table and bootloader never change** |
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
| Reserved | 6–11, 16; 1, 3 | SPI flash; GPIO16 = the in-package PSRAM's CS (ESP-IDF `esp_psram_impl_quad.c`); GPIO1/3 = UART0, the console (no override may take them) |

**GPIO12 (MTDI)** selects the flash/PSRAM supply voltage at every ESP32
reset, and this unit has no eFuse override (`VDD_SDIO_FORCE` = 0): it must
be low at reset. The firmware drives it low before anything else and high
only for the ~0.7 s of a Gecko bootloader entry (`mg24 bootloader`); an
ESP32 reset releases the pin. `info` reports the straps.

**GPIO0/GPIO5**: the 50 MHz oscillator is powered by GPIO5, which the
firmware drives only after boot, as SONOFF's ESPHome configuration does.
Whether a software restart with the oscillator running always boots from
flash is checked on hardware (HIL list below).

## Slots, installation and the way back

SONOFF's stock partition table is the product layout
(`firmware/donglem/partitions.csv` generates it byte for byte; CI checks
its SHA-256):

| Region | Offset | Size | Stock firmware | With the hub |
|---|---|---|---|---|
| bootloader (ESP-IDF v5.2.1) | 0x1000 | 27 KiB | | unchanged |
| partition table | 0x8000 | 4 KiB | | unchanged |
| `nvs` | 0x1C000 | 256 KiB | SONOFF's settings, Wi-Fi, credentials | shared: the hub adds its own namespace `zrh`, nothing else |
| `otadata` | 0x5C000 | 8 KiB | seq 1 → `ota_0` | selects `ota_1` (installation) or `ota_0` (`stock confirm`, self-update) |
| `phy_init` | 0x5E000 | 4 KiB | | unchanged (blank) |
| `ota_0` | 0x60000 | 6 MiB | SONOFF's application 1.0.7 | kept until the hub's first self-update |
| `ota_1` | 0x660000 | 6 MiB | blank | the hub |
| `files` | 0xC60000 | 3 712 KiB | SONOFF's LittleFS: `config.json`, `device_info.json`, `indicator.json`, MG24 info and factory IEEE config, staging of MG24 updates | the hub's data region, only after an explicit claim (next section); this image only reads it |

**Installation (product): SONOFF's web console**, Firmware →
ESP32-D0WDR2-V3 → Upload & Flash Firmware, with `zrh-donglem-app.bin`.
SONOFF's application writes it into `ota_1` with ESP-IDF's OTA functions
and selects it. What the stock handler checks, from its strings: the
image magic, the image length against the partition, the chip revision
range, the appended SHA-256 (ESP-IDF `esp_ota_end`), and a checksum the
web page computes. Our image is a plain ESP32 app image: chip ESP32,
revision v3.0–v3.99 (the unit is v3.1), DIO 40 MHz, SHA-256 appended,
0.91 MB of the 6 MiB slot. Its project name is `libespidf` (esp-idf-sys);
SONOFF's guide says ESPHome images are accepted. **To verify on hardware**
(the lab unit is first flashed over USB instead, below).

**No trial boot.** SONOFF's application was built without ESP-IDF's app
rollback: its `esp_ota_ops` carries the error strings that every build has
(e.g. "OTA image has invalid magic byte") but not the one only a rollback
build has ("Running app has not confirmed state"). The bootloader comes
from the same ESP-IDF v5.2.1 build, so it very likely has no rollback
either (its log is silent; **to verify**). An image installed through the
web console is therefore selected for good: an image that does not start
cannot fall back to SONOFF's, and recovery needs USB (SONOFF Dongle
Flasher, or esprom with the backup). The hub's sdkconfig has no rollback
either, so the entries it writes (`stock confirm`, self-update) are
"undefined", which every bootloader boots and never reverts; it still
confirms itself once its core runs (`esp_ota_mark_app_valid_cancel_rollback`),
which only matters if a rollback bootloader ever put it on trial.

**Self-update (follow-up)** writes the other slot. The hub runs from
`ota_1`, so **its first self-update replaces SONOFF's application in
`ota_0`**; from then on the two slots alternate between hub images.
Before that, the hub warns and asks for confirmation.

**Back to SONOFF's firmware**: `stock` / `stock confirm` on the console
(the web UI later) while `ota_0` holds SONOFF's application: it selects
`ota_0` and restarts. SONOFF's settings in `nvs` and `files` are
untouched as long as `files` has not been claimed (after a claim, the
copy in the region's `stock_copy` area is written back first, follow-up).
After a self-update has replaced SONOFF's application, the way back is
SONOFF Dongle Flasher (USB or web serial) or the esprom backup.

## The hub's data region: SONOFF's `files` partition

With the table unchanged, `files` is the only space for the hub's data:
`hubdata` (host store journal, catalog, definitions), the Zigbee device
OTA store and the MG24 image. Plan (`hub_core::layout::dongle_files`,
tested):

| Area | Offset in `files` | Size | Content |
|---|---|---|---|
| `claim` | 0x0 | 8 KiB | claim record, two sectors written alternately (`ZRHF`), where LittleFS keeps its superblock pair |
| `hubdata` | 0x2000 | 1 920 KiB | the 8 MB `hubdata` map of `hub_core::layout` (journal first), as on the S3 8 MB images |
| `ota_store` | 0x1E2000 | 1 MiB | Zigbee device images (format of `docs/ota.md`): 92 % of real images are ≤ 512 KiB; larger ones are streamed |
| `radio` | 0x2E2000 | 640 KiB | one MG24 `.gbl` image, reserved for the Gecko updater (to be sized against the pinned EmberZNet image) |
| `stock_copy` | 0x382000 | 120 KiB | SONOFF's LittleFS blocks in use at the claim: an index sector and up to 29 blocks |

**Claim (follow-up; this image never writes `files`).** Only on an
explicit confirmation in the console or web UI, never automatically, and
only when `files` is SONOFF's LittleFS or blank (`info` shows which; an
unknown content is refused):

1. read every non-blank 4 KiB block into PSRAM; more than 29 → refused
   (an MG24 update staged by SONOFF can be that large: reset SONOFF's
   settings first, or take a USB backup and confirm discarding them);
   the `stock_copy` blocks must be free in SONOFF's file system;
2. write the index and the blocks into `stock_copy`, read them back
   (SONOFF's file system is still intact: these blocks are free in it);
3. write the claim record over block 0, then block 1 (SONOFF's LittleFS is
   now invalid), then erase `hubdata`, `ota_store` and `radio` as they
   are first used.

On the lab unit SONOFF's LittleFS uses blocks 0, 1 and 411 (inside
`hubdata`), so the copy is three blocks.

**Return to stock after a claim (follow-up):** erase the region, write the
copied blocks back to their block numbers, check them, then `stock
confirm`. **To verify on hardware:** what SONOFF's firmware does with a
`files` region that is not a valid LittleFS (presumably formats it and
starts with factory settings: web and AP passwords, device name, the MG24
info and factory IEEE config files gone), and that it mounts the restored
blocks.

## Image and sdkconfig

`firmware/donglem` (`zrh-donglem`: board profile, sdkconfig, the stock
table) on `firmware/gateway` (`zrh-gateway`: the board-generic firmware),
target `xtensa-esp32-espidf`, ESP-IDF v5.5.1, Xtensa Rust 1.90 (`esp`).

| Setting | Value | Why |
|---|---|---|
| Image header | DIO, 40 MHz, 16 MB, chip v3.0–v3.99 | as SONOFF's bootloader and application: the stock bootloader stays and boots it |
| PSRAM | quad, 40 MHz, `CONFIG_SPIRAM_USE_MALLOC`, C mallocs > 1 KiB and Wi-Fi/lwIP buffers to PSRAM, 32 KiB internal kept for DMA | 80 MHz PSRAM would need 80 MHz flash; heap policy of `firmware/hub` and `hub_core::resources` |
| `CONFIG_ESP32_REV_MIN_3` | v3.0 | no PSRAM cache workaround: smaller, faster code |
| Rust heap | prefers PSRAM (`firmware/hub/src/alloc.rs`, shared) | as `firmware/hub` |
| Atomics | `-C target-feature=-s32c1i` + the same compile-time guard | S32C1I is not atomic between the cores on PSRAM; ESP-IDF's `__atomic_*` lock PSRAM addresses (`CONFIG_STDATOMIC_S32C1I_SPIRAM_WORKAROUND`, on for SPIRAM on the ESP32). CI checks the target features and disassembles the ELF: no S32C1I/SCOMPARE1 attributed to a Rust source (`tools/check-s32c1i.sh`); `atomics` measures it on hardware |
| Bluetooth | off | not in the feature set |
| Panic | recorder in RTC memory (`firmware/hub/src/cpanic.rs`, shared, `--wrap=esp_panic_handler`); no core dump | the stock table has no `coredump` partition |
| App rollback | off | as SONOFF's build (above) |
| Web/MQTT | WebSocket support in esp_http_server; esp-mqtt plain TCP only; 3 SNTP servers; 4 KiB tcpip stack | as `firmware/hub` |
| Internal RAM | no IPv6, ROM printf (nano), silent asserts | as `firmware/hub` |
| NVS | SONOFF's partition, shared: own namespace `zrh`; no erase on init errors; Wi-Fi config in RAM; PHY data not stored | SONOFF's settings stay as they are |

Build (the first build downloads ESP-IDF into `.embuild`):

```sh
. ~/export-esp.sh && cd firmware/donglem && cargo build --release && cd ../..
CHIP=esp32 FLASH_SIZE=16MB FLASH_FREQ=40m MIN_REV_FULL=300 MAX_APP_BYTES=6291456 \
  tools/make-app-image.sh release target/zrh-donglem-app.bin zrh-donglem
python3 tools/make-otadata.py --seq 2 -o target/otadata-donglem-ota1.bin
```

The bring-up image is 0.91 MB; the full application is estimated at
3.0–3.2 MB (the S3 8 MB image with MQTT and the advanced console is 2.80
MB; Zigbee device OTA ~0.14 MB; the EZSP backend replaces ZRHP, ~0.11 MB;
Ethernet ~0.04 MB; no TinyUSB bridge; self-update, backup/restore, Green
Power, MG24 updater ~0.15 MB): ~3 MB headroom in the 6 MiB slot.

## RAM budget (2 MB PSRAM)

Static RAM of the bring-up build: IRAM ~101 KB of 128 KB, DRAM `.data` +
`.bss` ~33 KB (CI prints both). The application code runs from flash;
IRAM grows little with it (levers if it ever runs out:
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

## First flash of the lab unit (esprom, USB)

The lab unit gets the bring-up image over USB, not through the web
console: only two regions are written, both blank before.

| Region | Offset | Size | Written |
|---|---|---|---|
| `ota_1` | 0x660000 | 6 MiB | `zrh-donglem-app.bin` |
| `otadata` sector 1 | 0x5D000 | 4 KiB | seq 2, VALID → `ota_1` (`tools/make-otadata.py`) |

Everything else stays (bootloader, table, `nvs`, `otadata` sector 0,
`phy_init`, SONOFF's application in `ota_0`, `files`). The bootloader picks
the valid `otadata` entry with the highest sequence number, so erasing
sector 1 again boots SONOFF's untouched application.

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

Back to SONOFF's firmware: `stock confirm` on the console, or from the
backup over USB:

```sh
# otadata sector 1 back to blank: SONOFF's application boots again.
hwlab target flash restore --plugin esprom --device $D --arg backup=$BK \
  --arg address=0x5D000 --arg length=0x1000 --arg confirm_protected=modify-protected-region \
  --token restore-flash --config allow_flash=true --yes
# Optional: ota_1 back to blank.
hwlab target flash restore --plugin esprom --device $D --arg backup=$BK --arg partition=ota_1 \
  --token restore-flash --config allow_flash=true --yes
```

(After `stock confirm`, `otadata` sector 0 holds the newer entry; restore
the whole `otadata` partition to return to the backup's state.) The `zrh`
NVS keys stay (SONOFF's firmware ignores them). A whole-flash restore also
puts back the NVS of the backup's date: it rolls back SONOFF's settings
changed since.

## Console (UART0, 115 200 8N1, the CP2102N port)

| Command | What it does |
|---|---|
| `info` | firmware, chip, memory mode, partitions and layout verdict, running/boot slot, what `ota_0` holds, what `files` holds, straps, last panic |
| `board`, `board set <k=v …>`, `board clear` | profile pins; NCP wiring overrides (e.g. `baud=460800`), next boot. Overrides that take flash/PSRAM or console pins are refused; **holding the button while the dongle powers up or resets ignores stored overrides for that boot** (then `board clear`) |
| `net`, `wifi set <ssid> [pw]`, `wifi clear` | Ethernet/Wi-Fi state; station credentials (the station starts when Ethernet has had no address for 10 s; next boot) |
| `led <0-100>` | LED brightness |
| `mg24` | line levels, UART rate and errors |
| `mg24 reset` | MG24 reset into its application (bootloader line low) |
| `mg24 bootloader` | bootloader line high, reset pulse, print the Gecko bootloader's output and menu after a newline, reset back into the application. Nothing is uploaded |
| `mg24 ezsp info` | ASH reset (the NCP restarts), EZSP version negotiation (`hub-ezsp` host core), `getValue(VERSION_INFO)`, `getEui64`, `getMfgToken` (string, board name), `networkState` and, if joined, `getNetworkParameters`/`getNodeId`. Read-only |
| `mg24 ezsp info init` | also `networkInit` (resumes a stored network; no form, leave or configuration), then an MG24 reset so the network does not keep running without a host |
| `mg24 baud <rate>` | MG24 UART rate until the next boot |
| `stock` | whether `ota_0` still holds SONOFF's application (project `Dongle-M`) and what `stock confirm` does, or why it cannot |
| `stock confirm` | select `ota_0` in `otadata` (ESP-IDF verifies the image first) and restart into SONOFF's firmware |
| `cpu [ms]`, `atomics` | CPU use per task; lost atomic updates between the cores (Rust atomics must lose none) |
| `restart` | ESP32 restart |

LED: blue blinking = no network, green = network up, magenta blinking =
probe or bootloader banner running, white flash = button. The button is
reported only (log, `info`) until the application gives it a function.

After an ASH reset an EmberZNet NCP has no network up until the host calls
`networkInit`: `mg24 ezsp info` normally shows `NO_NETWORK` even with a
stored network.

## Not in this stage

- The hub application, the EZSP coordinator backend, the `files` claim and
  its way back, the OTA store on it, self-update, backup/restore: after
  the backend selection.
- **MG24 firmware updates** through the Gecko bootloader: the `donglem`
  branch's updater (radio banks in its own `hubdata`, ZRHP-based
  verification) does not port to an EZSP NCP, and the updater
  requirements of `coordinator-profiles.md` (backup export first,
  allow-list manifest, persisted state) are not met. Not compiled; the
  `radio` area is reserved for it.
- The ESP32-S3 SuperMini + MG24 bench profile of the `donglem` branch.

## HIL checklist (with the user's confirmation, after a fresh backup check)

1. Flash the bring-up image over USB as above; capture the boot log: PSRAM
   2 MB found, `memory mode psram`, stock table `Foreign`, running `ota_1`,
   `ota_0` = SONOFF Dongle-M, `files` = LittleFS, straps; `info` heap
   figures for the RAM budget.
2. `atomics`: Rust atomics lose nothing on PSRAM (bare S32C1I may).
3. Ethernet link and DHCP; Wi-Fi fallback with Ethernet unplugged (heap
   with Wi-Fi up); LED.
4. `mg24 bootloader` banner (version), `mg24 ezsp info` (EZSP 13, EmberZNet
   7.4.x, EUI-64), with and without `init`.
5. `restart` with Ethernet up (GPIO0 clock at a software reset).
6. `stock confirm`: SONOFF's firmware boots with its settings and stays
   selected across resets (no rollback).
7. From SONOFF's firmware: install `zrh-donglem-app.bin` through the web
   console (Firmware → ESP32-D0WDR2-V3 → Upload & Flash Firmware); the hub
   boots from `ota_1`.
