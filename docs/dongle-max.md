# SONOFF Dongle Max (Dongle-M)

**Status: the hub image (`zrh-donglem` 0.2.0) runs on one lab unit**
(results below; the rest of the HIL checklist at the end). It runs the hub
application on SONOFF's stock EmberZNet NCP through
the EZSP backend (`docs/ezsp-backend.md`), with its data on SONOFF's
`files` partition once claimed, on SONOFF's unchanged partition table.
The board bring-up image (0.1.0) ran on one unit. Nothing here is a Zigbee
or interoperability claim.

**Hardware results of the bring-up image** (0.1.0; lab unit, ESP32-D0WDR2-V3 rev v3.1, image in `ota_1`
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
- Not yet run: Ethernet link, `stock confirm`, installation through
  SONOFF's web console (HIL checklist at the end).

**Hardware results of the hub image** (0.2.0, the same unit, `ota_1`):

- First boot: the automatic claim of `files` (4 blocks of SONOFF's data
  copied, 2.4 s); the app on the EZSP backend; internal heap 170 KiB free
  after start (minimum 166), PSRAM 1903 / 2048 KiB; no panic. Wi-Fi station
  from the stored credentials; the hub formed its network (channel 20) and
  runs as a second hub in Home Assistant.
- **EZSP session start: 2.6 s** from the app's start to the session up
  with the network up (reset 2.2 s: an RST, the NCP's boot RSTACK, the RST
  again, its answer after the NCP's ~1.1 s reboot; version 10 ms,
  configuration 0.2 s, the rest 0.1 s). It took ~52 s at first: the UART
  read waited for 256 bytes or a whole timeout of silence, up to 1 s per
  command (`uart.rs`). `hub` on the console shows the phases and the link:
  94 commands, ASH retransmissions, NAKs, CRC errors, ACK timeouts and UART
  overruns all 0.
- The NCP's memory holds the multicast table at its own 8 (16 refused:
  the hub endpoint's groups are limited to 8), the address table at 12 of
  16 asked, no trust center address cache (`docs/ezsp-backend.md`).
- A sleepy end device (the lab's ESP32-C6 zigbee-rs sensor) joins with
  permit join and is interviewed `complete`: Basic, endpoint 1 with
  temperature and humidity, bindings and reporting; its reports arrive.
  Before the fix its interview ended `partial` with nothing learned: the
  NCP appends a byte to the incoming messages of a device that joined in
  its session, and the backend refused them (`docs/ezsp-backend.md`, "On
  the Dongle Max").

## Features

Decided 2026-11-24; `zrh-donglem` 0.2.0:

| Feature | In the image |
|---|---|
| Hub application: devices, catalog, groups, bindings, reporting, automations | yes: `hub-app` on `Backend::ezsp` (features `ezsp`, `advanced`, `ota-store`) |
| Web UI with the advanced console, WebSocket API | yes: the page with the gateway cards (`hub-web` feature `gateway`, 29,795 B of the 36 KiB budget; with its four tab modules 40,074 B of the 44 KiB total); the ZCL console with developer mode (Settings › Hub); a developer edition (devtools, feature `dev`, CI-built) has it always |
| MQTT with Home Assistant discovery | yes (esp-mqtt, plain TCP; off until enabled, settings in NVS `zrh/mqtt`) |
| Zigbee device OTA | yes: the 1 MiB image store in the claimed `files` region (`ota_store`), larger images streamed from a client; streamed only until the claim |
| ESP32 self-update | yes: `firmware/*` requests, Settings › Hub firmware; into the other app slot (the first one replaces SONOFF's application, after a confirmation) |
| Ethernet (also PoE-powered) and Wi-Fi | yes: Ethernet first, the Wi-Fi station after 10 s without an Ethernet address |
| Network backup/restore (Open Coordinator Backup) | the requests exist; the EZSP backend answers `unsupported` until key export lands (`docs/ezsp-backend.md`, follow-ups) |
| Green Power through EZSP | later |
| MG24 firmware update through the Gecko bootloader | later; the `radio` area is reserved, no code |
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
| `otadata` | 0x5C000 | 8 KiB | seq 1 → `ota_0` | selects `ota_1` (installation) or the slot a self-update wrote, `ota_0` again on `stock confirm` |
| `phy_init` | 0x5E000 | 4 KiB | | unchanged (blank) |
| `ota_0` | 0x60000 | 6 MiB | SONOFF's application 1.0.7 | kept until the hub's first self-update |
| `ota_1` | 0x660000 | 6 MiB | blank | the hub |
| `files` | 0xC60000 | 3 712 KiB | SONOFF's LittleFS: `config.json`, `device_info.json`, `indicator.json`, MG24 info and factory IEEE config, staging of MG24 updates | read only until the claim; then the hub's data region (next section) |

### Installation

**Product: SONOFF's web console**, Firmware → ESP32-D0WDR2-V3 → Upload &
Flash Firmware, with `zrh-donglem-app.bin` (CI artifact `donglem-firmware`).
SONOFF's application writes it into `ota_1` with ESP-IDF's OTA functions
and selects it. What the stock handler checks, from its strings: the
image magic, the image length against the partition, the chip revision
range, the appended SHA-256 (ESP-IDF `esp_ota_end`), and a checksum the
web page computes. Our image is a plain ESP32 app image: chip ESP32,
revision v3.0–v3.99 (the unit is v3.1), DIO 40 MHz, SHA-256 appended,
about 2.96 MB of the 6 MiB slot. Its project name is `libespidf`
(esp-idf-sys); SONOFF's guide says ESPHome images are accepted. **To verify
on hardware.**

**Lab: over USB with hwlab esprom** (below), `ota_1` and one `otadata`
sector.

The first boot claims SONOFF's `files` partition for the hub's data
(SONOFF's files are copied into it first; next section) and generates the
API token. The console (CP2102N, 115 200 8N1) prints the token (`token`);
the web UI is at `http://<address>/` once Ethernet or Wi-Fi has an address
(`net`, `wifi set <ssid> <password>` or the UI). If the claim was refused,
the log, `files` and a banner in the web UI say why: the hub then keeps its
data in RAM only.

**API token**: 12 random bytes from the ESP32's RNG with an entropy source.
The classic ESP32's RNG is a true RNG only while the radio runs or the
SAR-ADC entropy is on, and a dongle on Ethernet never starts its radio: the
token (at the first boot, and `hub/token/rotate`) is read with
`bootloader_random_enable` around it unless the Wi-Fi station runs; the
Wi-Fi start waits for such a read (`firmware/gateway/src/entropy.rs`). The
firmware uses neither the ADC nor I2S. Network keys, PAN ID and EPID are
the host's random bytes XOR the MG24's TRNG (`docs/ezsp-backend.md`).

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

### Self-update

Settings › Hub firmware in the web UI, or a client with the API
(`firmware/*`, maintenance feature `firmware`):

| Request | Fields | Result |
|---|---|---|
| `firmware/status` | | `running`, `version`, `target` (the slot an update writes), `target_holds`, `replaces_stock`, `state` (`idle`, `uploading`, `ready`), `written`, `size`, `ready_version`, `chunk` |
| `firmware/begin` | `size`, `sha256` (hex, optional), `replace_stock` | `target`, `chunk` (4096) |
| `firmware/chunk` | `offset` (sequential), `data` (base64, at most 4 KiB) | `written` |
| `firmware/end` | | `version`; the image is selected for the next boot |
| `firmware/abort` | | |

The image goes into the other app slot with ESP-IDF's OTA functions
(sequential writes: each sector is erased when it is reached); `firmware/end`
checks the upload's SHA-256 when one was given, the image itself
(`esp_ota_end`: header, chip, segments, appended SHA-256) and that it is a
`zrh-donglem` image (`App version` prefix), then selects it; `hub/reboot`
(which saves the registry first) starts it. The hub runs from `ota_1` after
its installation, so **its first update writes over SONOFF's application
in `ota_0`**: `firmware/begin` refuses (`confirm_required`) unless
`"replace_stock": true`, and the web UI asks for a tick. From then on the
two slots alternate between hub images, and the way back to SONOFF's
firmware is USB only. The hub's data are not touched by an update.

### Back to SONOFF's firmware

`stock` on the console says what happens, `stock confirm` does it, while
`ota_0` holds SONOFF's application (project `Dongle-M`):

1. if the hub claimed `files` (or a claim or return was interrupted): the
   hub application stops (it saves the registry; refused while a
   maintenance job runs, `network/jobs`: a scan, a channel change, a
   network reset or restore writes the records from its own thread), the
   gate of every window of the region the hub holds closes (the journal,
   the OTA image store, the catalog: a write under way ends first, every
   later one fails), SONOFF's blocks are written back from the copy and the
   copy is erased (next section); the hub's data on the region are gone;
2. `ota_0` is selected in `otadata` (ESP-IDF verifies the image first), the
   first boot's claim is forgotten (`zrh/files_auto`: a later installation
   claims again) and the dongle restarts into SONOFF's firmware with its
   settings in `nvs` and `files`.

If writing back fails, `ota_0` is not selected (`stock confirm` again, or
`restart`: the hub then runs with its data in RAM). After a self-update has
replaced SONOFF's application, the way back is SONOFF Dongle Flasher (USB
or web serial) or the esprom backup; the console says so.

## The hub's data region: SONOFF's `files` partition

With the table unchanged, `files` is the only space for the hub's data
(`hub_core::layout::dongle_files`; claim and return in `hub_store::files`,
host-tested):

| Area | Offset in `files` | Size | Content |
|---|---|---|---|
| `claim` | 0x0 | 8 KiB | claim records, two blocks written alternately (`ZRHF`), where LittleFS keeps its superblock pair |
| `hubdata` | 0x2000 | 1 920 KiB | the 8 MB `hubdata` map of `hub_core::layout`: host store journal (128 KiB, first), catalog state, manifest, banks, local definitions, web tools |
| `ota_store` | 0x1E2000 | 1 MiB | Zigbee device images (format of `docs/ota.md`): 92 % of real images are ≤ 512 KiB; larger ones are streamed |
| `radio` | 0x2E2000 | 640 KiB | one MG24 `.gbl` image, reserved for the Gecko updater (erased by the claim, not used yet) |
| `stock_copy` | 0x382000 | 120 KiB | SONOFF's blocks in use at the claim: an index block and up to 29 blocks |

### The first boot claims it

**Decision (2026-11-25): the hub claims `files` by itself on its first
boot**, before the application starts, when the claim's preconditions hold
(below); SONOFF's files are copied first, as for every claim. The attempt
is kept in NVS (`zrh/files_auto`: `claimed`, or `refused: <why>`): a
refusal is not retried at every boot, an interrupted first claim is
resumed, a claim cut short after its commit is finished (any boot), a
return to stock cut short is left for `stock confirm`
(`hub_store::files::boot_action`, host-tested). A completed `stock
confirm` forgets the attempt, so a later installation is a first boot
again.

**If the claim is refused, the hub runs with its data in RAM only**, and
says so loudly: the boot log (`!!!`), `files`, `hub` and `info` on the
console, `hub/system` and `storage/status` (`ram_only`: why;
`first_boot_claim`), and a banner over every page of the web UI. Every
feature works, but the device registry (names, interviews), groups,
bindings the hub made, automations, reporting settings, the ownership tag
and the hub's other records are lost at every restart. What survives: the
Zigbee network itself (in the MG24's NVM: after a restart the hub takes the
NCP's network again and imports the devices the NCP knows), and what is in
NVS (`zrh`: Wi-Fi, API token, MQTT settings, LED). No catalog can be
installed (the built-in examples are there) and the Zigbee OTA image store
is off (streamed images work). Once the cause is gone (e.g. SONOFF's
settings reset in its web console), the claim can be made by hand.

### The claim

At the first boot, or by hand: console `files claim` (what it would do),
`files claim confirm`; API `storage/claim` with `"confirm": true` (without
it: `confirm_required`), Settings › Storage › Claim storage. A claim by
hand runs on its own thread and restarts the hub when it is done (the RAM
data of this boot are lost). `storage/status` reports `content`,
`claimed`, `state`, `copy`, `in_use`, `ram_only`, `first_boot_claim`,
`busy`, `last`.

1. **Scan.** SONOFF's blocks are those its LittleFS uses: from the
   superblock pair {0, 1} the metadata pairs are followed through their
   tails and through the directory entries, of each pair the newer block
   with a valid commit (CRC) replayed (a torn commit after the last valid
   one is ignored, as littlefs does), and the CTZ skip-lists of its files
   walked, as littlefs's own traversal does (`hub_store::littlefs`, read
   only; host tests compare it with upstream littlefs's `lfs_fs_traverse`
   on images made by littlefs v2.9 configured as esp_littlefs). Following
   the directory entries too covers a power cut while littlefs relocates a
   directory (the parent already points at the new pair, the tails still at
   the old one; littlefs reads the new pair, its traversal misses it): a
   test cuts littlefs off after every program and erase of such a
   relocation and checks that every file still reads the same with only
   the walked blocks kept. Blocks LittleFS freed are not counted (it
   does not erase them: an MG24 update staged and installed by SONOFF
   leaves ~70 written blocks behind). If the walk fails (a CRC that never
   matches, a pointer beyond the file system, a cycle, no superblock),
   every block that is not erased counts instead. Refused, writing
   nothing, when more than 29 blocks are in use outside `stock_copy`, when
   SONOFF's file system uses a block of `stock_copy` (without the walk:
   any written block there), or when the region holds neither LittleFS
   nor erased flash.
2. **Copy.** Written blocks of `stock_copy` that SONOFF's file system freed
   are erased; the index (`ZRHS`: block numbers and CRC-32C of each block,
   and whether they came from the walk; its body first, its magic last)
   into the first block of `stock_copy`, the blocks into the slots after
   it, each read back, then the index's `DONE` word. SONOFF's file system
   is still intact.
3. **Commit.** A claim record (`erasing`) over block 0, the LittleFS
   superblock: from here the region is the hub's. Block 1 and the hub's
   areas (`hubdata`, `ota_store`, `radio`; blocks already erased are only
   read) are erased, a second record (`ready`) goes to block 1.

**Power cuts**: the claim and the return are idempotent, every write and
erase is a safe stopping point (host tests cut each one, with torn writes
and half-erased blocks, then run the claim or the return to stock again
and compare the region byte for byte, or mount the littlefs image and read
its files back). Before the commit SONOFF's file system is untouched: the
stock firmware still works, a new claim starts over (an interrupted copy,
also a torn first write of the index, is recognised as the hub's and
erased), and `stock confirm` erases the copy. After the commit the next
boot finishes the erase on its own. A copy whose source blocks changed
since (SONOFF's firmware ran in between) is made again.

On the lab unit SONOFF's LittleFS uses blocks 0, 1 and 411 (inside
`hubdata`), so the copy is three blocks (littlefs allocates from 411 on a
fresh file system of this size, as the tests show).

### Return to stock after a claim

A record `restoring`; every block outside `stock_copy` is erased (if it is
not) and the copied blocks are written back to their block numbers and read
back, the claim blocks last (the one holding the newest record at the very
end); then `stock_copy` is erased, its index last. Blocks SONOFF's file
system had freed come back erased. A power cut anywhere:
`stock confirm` again finishes it (or `files claim confirm` claims the
region again: the copy stays until the return is complete); the hub boots
with its data in RAM meanwhile. A copy that does not verify is never
written back: the way back is then USB. **To verify on hardware:** SONOFF's
firmware mounts the restored blocks (what it does with a `files` region
that is not a valid LittleFS is presumably a format with factory settings:
web and AP passwords, device name, the MG24 info and factory IEEE config
files gone).

## The hub on the dongle

`firmware/gateway` (`zrh-gateway`, the board-generic firmware) with
`firmware/donglem` (`zrh-donglem`: profile, sdkconfig, the stock table) and
`firmware/espapp` (`zrh-espapp`: the ESP-IDF glue shared with the S3 hub:
PSRAM heap, panic recorders, HTTP server, WebSocket API, log ring, SNTP,
catalog flash, MQTT transport). Boot: NCP lines released (GPIO12 low),
heap, panic recorders, console, settings, boot report, NCP UART and an MG24
reset, `files` (a committed claim is finished), the hub, network, LED and
button, console.

| Part | What |
|---|---|
| Coordinator | `Backend::ezsp` on UART1 (TX GPIO17, RX GPIO13, 115 200 Bd, no flow control; the backend owns the UART for the whole boot), `EzspOptions` defaults (worker 16 KiB stack) |
| Store | the host store journal in `files` `hubdata` once claimed, RAM before; `NcpNvRecords` decide ownership (`docs/ezsp-backend.md`) |
| Catalog | the `hubdata` area mapped (`zrh_espapp::catalog::HubData::window`) once claimed |
| Zigbee OTA | the server with streams (4 at once) and, once claimed, the image store in `ota_store` |
| HTTP | esp_http_server on every interface from the first address: the page, the web tools, `/api` (7 sockets, LRU purge) |
| MQTT | `zrh_espapp::mqtt` (esp-mqtt), settings in NVS `zrh/mqtt` |
| Platform | `hub/system` with `board`, `ethernet`, `wifi`, `storage`, `firmware`; Wi-Fi set from the API at once; token rotation; reboot; logs; time; no boot modes |
| Requests | `storage/*` (feature `storage`), `firmware/*` (feature `firmware`) besides the hub's own |

**Button**: a short press (under 3 s) opens joining for 180 s (as the web
UI's Permit join; the LED blinks yellow while it is open). A press of 3 s
or more does nothing; held 10 s or more the LED turns white and still
nothing happens: no factory reset or other destructive action from the
button (the console and the web UI do that, with confirmations). Held
while the dongle powers up or resets, it ignores the stored board overrides
for that boot.

**LED** (brightness `led <0-100>`):

| Colour | Meaning |
|---|---|
| white flash | button pressed (white steady: held 10 s or more) |
| magenta, fast blink | MG24 probe or bootloader banner running (bring-up boot) |
| yellow, blinking | joining open |
| red, blinking | the MG24 does not answer (no EZSP session for 30 s), or the hub did not start |
| blue, blinking | no network (Ethernet and Wi-Fi without an address) |
| cyan | network up, no Zigbee network (none formed yet: Settings › New network, or `network/form`) |
| green | network and Zigbee network up |

## Image and sdkconfig

Target `xtensa-esp32-espidf`, ESP-IDF v5.5.1, Xtensa Rust 1.90 (`esp`).

| Setting | Value | Why |
|---|---|---|
| Image header | DIO, 40 MHz, 16 MB, chip v3.0–v3.99 | as SONOFF's bootloader and application: the stock bootloader stays and boots it |
| PSRAM | quad, 40 MHz, `CONFIG_SPIRAM_USE_MALLOC`, C mallocs > 1 KiB and Wi-Fi/lwIP buffers to PSRAM, 32 KiB internal kept for DMA | 80 MHz PSRAM would need 80 MHz flash; heap policy of `firmware/hub` and `hub_core::resources` |
| `CONFIG_ESP32_REV_MIN_3` | v3.0 | no PSRAM cache workaround: smaller, faster code |
| Rust heap | prefers PSRAM (`zrh_espapp::alloc`, shared with the S3) | the application's data, WebSocket queues and the log ring live in PSRAM |
| Atomics | `-C target-feature=-s32c1i` + the same compile-time guard | S32C1I is not atomic between the cores on PSRAM; ESP-IDF's `__atomic_*` lock PSRAM addresses (`CONFIG_STDATOMIC_S32C1I_SPIRAM_WORKAROUND`, on for SPIRAM on the ESP32). CI checks the target features and disassembles the ELF: no S32C1I/SCOMPARE1 attributed to a Rust source (`tools/check-s32c1i.sh`); `atomics` measures it on hardware |
| Bluetooth | off | not in the feature set |
| Panic | recorders in RTC memory (`zrh_espapp::cpanic`: ESP-IDF panic through `--wrap=esp_panic_handler`, the Rust panic message, failed allocations); no core dump | the stock table has no `coredump` partition |
| App rollback | off | as SONOFF's build (above) |
| Web/MQTT | WebSocket support in esp_http_server; esp-mqtt plain TCP only; 3 SNTP servers; 4 KiB tcpip stack; 13 lwIP sockets (the HTTP server's 7 + 3, MQTT, SNTP, DNS) | as `firmware/hub` |
| Wi-Fi | 6 static RX buffers (internal, DMA), dynamic buffers in PSRAM, modem sleep off | internal RAM; the station runs only without Ethernet |
| Internal RAM | no IPv6, ROM printf (nano), silent asserts | as `firmware/hub` |
| NVS | SONOFF's partition, shared: own namespace `zrh`; no erase on init errors; Wi-Fi config in RAM; PHY data not stored | SONOFF's settings stay as they are |

Build (the first build downloads ESP-IDF into `.embuild`):

```sh
. ~/export-esp.sh && cd firmware/donglem && cargo build --release && cd ../..
CHIP=esp32 FLASH_SIZE=16MB FLASH_FREQ=40m MIN_REV_FULL=300 MAX_APP_BYTES=6291456 \
  tools/make-app-image.sh release target/zrh-donglem-app.bin zrh-donglem
python3 tools/make-otadata.py --seq 2 -o target/otadata-donglem-ota1.bin
```

(A local build whose ESP-IDF Python check fails on a constraint the
package index cannot meet yet builds with `IDF_PYTHON_CHECK_CONSTRAINTS=no`.)

Size: about **2.96 MB of the 6 MiB slot** (the bring-up image was 0.92
MB; CI prints the exact number and fails above the slot). Static RAM:
IRAM 101.5 KB of 128 KB, DRAM `.data` + `.bss` 33.2 KB (bring-up 30.9 KB).

## RAM budget (2 MB PSRAM)

**Internal (byte-accessible DRAM)**: an estimate from the bring-up
measurement and the image (thread stacks and internal allocations counted
from the code); `info`, `hub/system` `heap` and `cpu` measure it (HIL).

| Item | KiB |
|---|---:|
| Free at the bring-up console (measured: Ethernet driver, NCP UART, LED, console) | 247 |
| Static DRAM of the hub image (+2.3 KB) | −2 |
| Application thread 40, EZSP worker 16, HTTP server 8, LED +1 | −65 |
| Thread control blocks, esp_http_server and lwIP sessions, small C allocations (≤ 1 KiB stay internal) | −6 |
| **Free: Ethernet uplink, MQTT off** | **≈ 174** |
| MQTT on: bridge thread 16, esp-mqtt task 5, its buffers 2 | −23 |
| **Free: Ethernet uplink, MQTT on** | **≈ 151** |
| Wi-Fi station up (driver task, 6 static RX buffers, driver state, the station thread 6) | −40 |
| **Free: Wi-Fi uplink, MQTT on** | **≈ 110** |
| A maintenance job running (network map, scans: 12 KiB stack), a claim (8 KiB) | −12 |

Target: ≥ 40 KB minimum free in every case. The rest of the application
(device registry and models, WebSocket queues, catalog, EZSP host buffers,
the log ring) is Rust data in PSRAM. Levers if the measurement is lower:
no A-MPDU RX, Wi-Fi static RX buffers 6 → 4, the application thread's
stack from its measured high water, fewer HTTP sockets.

**PSRAM (2 MB)**: `hub_core::resources` gives a 2 MB board the same
limits as any PSRAM size (7 HTTP sockets with LRU purge, 128 KiB queued per
WebSocket client and 384 KiB in all, 8 KiB snapshot pieces, the built-in
catalog, MQTT queue 64 KiB and device model 256 KiB, 4 OTA streams); the
worst case fits with margin:

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

The lab unit gets its images over USB, not through the web console: only
two regions are written, both blank before the first flash (later flashes
rewrite `ota_1` only).

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
changed since. **After a claim** of `files`, `stock confirm` (it writes
SONOFF's blocks back) or a restore of the `files` partition from the backup
comes first: SONOFF's firmware on a claimed region would format it.

## Console (UART0, 115 200 8N1, the CP2102N port)

| Command | What it does |
|---|---|
| `info` | firmware, chip, memory mode, partitions and layout verdict, running/boot slot, what `ota_0` holds, what `files` holds, straps, last panics, failed allocations, hub status |
| `hub` | EZSP session, Zigbee network, permit join, devices, sessions, APS counters, where the data are; the EZSP link: sessions, failed starts, the last start phase by phase, ASH/EZSP/UART counters (retransmissions, NAKs, CRC errors, ACK timeouts, bad frames, callbacks with trailing bytes, overruns), the configuration values the NCP's memory did not hold |
| `permit [s]` | open joining (default 180 s, 0 closes; the result is logged) |
| `token` | the API token (generated at the first boot with the hub; `hub/token/rotate` changes it) |
| `files` | what `files` holds: claimed, interrupted, the copy; where the hub's data are; a running claim |
| `files claim` | what the claim would copy (reads the region: blocks used by the LittleFS walk or written), or why it is refused |
| `files claim confirm` | the claim by hand when the first boot's was refused (own thread), then a restart |
| `board`, `board set <k=v …>`, `board clear` | profile pins; NCP wiring overrides (e.g. `baud=460800`), next boot. Overrides that take flash/PSRAM or console pins are refused; **holding the button while the dongle powers up or resets ignores stored overrides for that boot** (then `board clear`) |
| `net`, `wifi set <ssid> [pw]`, `wifi clear` | Ethernet/Wi-Fi state; station credentials, used at once (the station runs when Ethernet has had no address for 10 s); `clear` from the next boot |
| `led <0-100>` | LED brightness |
| `mg24` | line levels, UART rate and errors |
| `mg24 reset`, `mg24 bootloader`, `mg24 ezsp info [init]`, `mg24 ezsp config [ID=v …]`, `mg24 baud <rate>` | the bring-up probes (MG24 reset; Gecko bootloader banner and menu, nothing uploaded; read-only EZSP probe, `init` resumes a stored network then resets the MG24; every configuration value of the NCP, after setting the given ones in its RAM only (IDs hex or decimal, e.g. `0x06=16`), then an MG24 reset; UART rate): only in a bring-up boot, the hub's backend owns the UART otherwise |
| `stock` | whether `ota_0` still holds SONOFF's application (project `Dongle-M`), what `stock confirm` does with `files`, or why it cannot |
| `stock confirm` | SONOFF's `files` data back (if claimed), `ota_0` selected in `otadata` (ESP-IDF verifies the image first), restart into SONOFF's firmware |
| `cpu [ms]`, `atomics` | CPU use per task; lost atomic updates between the cores (Rust atomics must lose none) |
| `restart` | ESP32 restart |
| `restart bringup` | one boot without the hub (the bring-up console with the MG24 probes); any later reset boots the hub |

After an ASH reset an EmberZNet NCP has no network up until the host calls
`networkInit`: `mg24 ezsp info` normally shows `NO_NETWORK` even with a
stored network (the hub's backend calls it at every session start).

## Not in this image, limits

- **Network backup/restore through EZSP** (key export): `network/backup`
  answers `unsupported` for now.
- **Green Power through EZSP**: GP frames are ignored.
- **MG24 firmware updates** through the Gecko bootloader: the `donglem`
  branch's updater (radio banks in its own `hubdata`, ZRHP-based
  verification) does not port to an EZSP NCP, and the updater
  requirements of `coordinator-profiles.md` (backup export first,
  allow-list manifest, persisted state) are not met. Not compiled; the
  `radio` area is reserved for it.
- A SONOFF file system that the LittleFS walk cannot read is counted by
  its written blocks (refused above 29); more than 29 blocks in use is
  refused either way (RAM only, the reason in the banner).
- No app rollback (SONOFF's bootloader): a hub image that does not start
  needs USB.
- No SoftAP setup: the first network access is Ethernet (DHCP) or Wi-Fi
  credentials from the console.
- The ESP32-S3 SuperMini + MG24 bench profile of the `donglem` branch.

## HIL checklist (with the user's confirmation, after a fresh backup check)

Done on the lab unit: 1 (4 blocks claimed), 2 (session start 2.6 s), the
Wi-Fi part of 3, forming and a sleepy end device's join and interview of
5, `restart bringup` and the MG24 probes of 9.

1. Flash the hub image over USB as above (`ota_1`, `otadata` sector 1);
   boot log: PSRAM 2 MB, `memory mode psram`, stock table `Foreign`,
   running `ota_1`, `ota_0` = SONOFF Dongle-M, `files` = SONOFF's
   LittleFS; the first boot's claim (three blocks on the lab unit, walked
   by LittleFS), `files` claimed and in use; `info` heap figures for the
   RAM budget, at the console and after a minute.
2. EZSP session on the stock NCP: `hub` shows the session up, `network/info`
   (NO_NETWORK on a fresh NCP), the LED cyan with a network address.
3. Ethernet link and DHCP, the web UI and `/api` with the token; Wi-Fi
   fallback with Ethernet unplugged (`wifi set`, then the UI's Wi-Fi), the
   heap with Wi-Fi up.
4. `hub/system` `store` not volatile; restart: data kept. On a second
   unit if available: a power cut during the first boot's claim, and a
   refused claim (RAM-only banner).
5. Form a network (Settings › New network), join a device with the button
   (yellow LED), interview, control, restart: the device and its name stay.
6. MQTT with a broker and HA discovery; the catalog upload; a Zigbee OTA
   image into the store.
7. Self-update with the same image: `replace_stock` asked, SONOFF's
   application replaced, the hub boots from `ota_0` with its data (only on
   a unit whose way back is the USB backup).
8. On another run, before 7: `stock confirm` after the claim: SONOFF's
   firmware boots with its settings and its files (config, device info),
   stays selected across resets (no rollback); installation of
   `zrh-donglem-app.bin` through SONOFF's web console; the hub boots from
   `ota_1`.
9. `atomics`; `restart` with Ethernet up (GPIO0 clock at a software reset);
   `restart bringup` and the MG24 probes; `cpu` under API load.
