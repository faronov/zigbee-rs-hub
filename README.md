# zigbee-rs-hub

[Русская версия](README.ru.md)

A standalone Zigbee hub in Rust for the **Espressif Thread Border Router /
Zigbee Gateway board** (ESP32-S3 host + ESP32-H2 radio). The H2 runs the
[zigbee-rs](https://github.com/faronov/zigbee-rs) NCP; the S3 runs the hub and
talks to it with **ZRHP v1** (zigbee-rs host protocol, spec:
`docs/book/src/advanced/host-protocol.md` in zigbee-rs) over the board UART.

ZRHP stays a USB/UART protocol (no network transport). The hub's own external
interface will be a WebSocket JSON API.

The same hub application also runs on the **SONOFF Dongle Max** (classic
ESP32 + EFR32MG24 with SONOFF's stock EmberZNet NCP) through the EZSP
backend ([docs/dongle-max.md](docs/dongle-max.md)).

## Hardware

| Hardware | Host | Radio / backend | Image | Install | State |
|---|---|---|---|---|---|
| Espressif Thread BR / Zigbee Gateway board, 8 MB S3 | ESP32-S3, PSRAM quad or octal | ESP32-H2, zigbee-rs NCP / ZRHP | `tbr-hub` 8 MB (MQTT, advanced console, Zigbee OTA with the image store) | web flasher | released |
| same board, 4 MB S3 sample | ESP32-S3, 4 MB flash | same | `tbr-hub` 4 MB dev (MQTT) | web flasher | released |
| SONOFF Dongle Max (Dongle-M) | ESP32-D0WDR2-V3, 16 MB flash, 2 MB PSRAM, Ethernet (PoE) + Wi-Fi | EFR32MG24, SONOFF's EmberZNet 7.4.5 NCP / EZSP 13 | `zrh-donglem` (MQTT, advanced console, Zigbee OTA with the image store in SONOFF's `files` partition once claimed, self-update) | SONOFF's web console (into `ota_1`; SONOFF's table, bootloader and application stay), or USB | runs on one lab unit (EZSP session in 2.6 s, a sleepy end device joined and interviewed); HIL checklist in `docs/dongle-max.md` |

## Phases

| Phase | Content | Status |
|---|---|---|
| **0** | S3 firmware `tbr-bridge`: USB ↔ H2 UART bridge with esptool auto-reset, so the H2 can be flashed and spoken to from a desktop; flash backups of both chips | **done**: bridge 0.2.2 on the board; esptool through the auto-reset, verified backups of both chips, spinel round trip; S3 reflash cycle through hwlab without buttons; RTS/CTS on the reserved lines, measured up to 2 Mbaud (see Status) |
| **1** | ZRHP host on the S3 using `zigbee-zrhp` (`std`): session, ARQ, credits, resume; H2 NCP lifecycle: flash-size detection, `FW_*` updates streamed into the H2's inactive slot with trial boot, confirm and **H2-side rollback** (A/B slots; the S3 stores no NCP images); H2 ROM-loader recovery only as a manual path with an operator-supplied image | **in progress** (branch `phase1`): host core `hub-ncp` tested against a simulated NCP, `hubctl` on macOS, `tbr-hub` firmware (hub mode + the bridge as default mode); on hardware: resets/resume, soak, `FW_*` update with rollback and confirm, `tbr-hub` on the S3 (see "Phase 1 status") |
| **2** | Standalone hub: host-backed tables (paging, host store journal), WebSocket JSON API, Wi-Fi and W5500 Ethernet, PTA coexistence enabled automatically while Wi-Fi is up | **in progress** (branch `stage2`): engine on the host core, device registry in `hubdata`, WebSocket JSON API on a desktop (`hubctl serve`) and on the S3 over Wi-Fi, catalog verifier, 8 MB production / 4 MB dev layouts; the S3 application build is 2.1 MiB, see "Stage 2" |

## Releases and the web flasher

Firmware for the board comes from tagged GitHub releases (`hub-v*` in this
repository for the S3, `ncp-v*` in zigbee-rs for the H2) and installs from
the browser: **<https://faronov.github.io/zigbee-rs/flasher/>** (Chrome or
Edge on a desktop, Web Serial, [ESP Web Tools](https://esphome.github.io/esp-web-tools/);
details in the zigbee-rs book, "Web Flasher").

| Flasher entry | Port | Image |
|---|---|---|
| Thread Border Router board, S3 (4 MB / 8 MB, detected) | S3, USB2 | `tbr-hub`: production layout (A/B OTA) on 8 MB, dev layout (no OTA) on 4 MB |
| Thread Border Router board, H2 | H2, USB1 | ZRHP NCP, UART 921 600 with RTS/CTS |

- **Flash size:** the S3 entry first reads the chip's flash ID and partition
  table (*Connect and read the flash size*) and selects the largest image
  that fits; an 8 MB image is never offered for a 4 MB chip (it would not
  boot). A different partition table on the chip (e.g. 4 MB dev → 8 MB)
  forces erase: back up the network first (web UI or `network/backup`).
- **Update (default)** writes the bootloader, partition table, blank
  `otadata` (8 MB) and the app; `nvs` (Wi-Fi credentials, boot mode, API
  token) and `hubdata` (device registry, ZRHP host store) stay. Leave *Erase
  device* unticked in the ESP Web Tools dialog.
- **Erase** (tick it on the page) for the first install over Espressif's
  firmware or tbr-bridge on the stock layout, or to reset the hub. The H2 and
  its network are not touched.
- While tbr-hub or tbr-bridge runs, USB2 shows the bridge's two ports, not
  the S3's USB-Serial-JTAG: switch first with the page's button (1200 bps
  touch on the second, console port) or the console command `s3 usj`. After
  the install the S3 starts in bridge mode; `mode hub` and
  `wifi set <ssid> <password>` on the console make it a hub. The browser
  console <https://faronov.github.io/zigbee-rs/flasher/console/> does this
  with a form (*Set up Wi-Fi*: credentials, hub mode, address and API token
  as a link to the web UI) and is a terminal for the second (console) port.
- The 8 MB images have not run on an 8 MB board yet (only the 4 MB sample
  so far).

**Making a release:** set the version in `firmware/hub/Cargo.toml` and
`CONFIG_APP_PROJECT_VER` (`firmware/hub/sdkconfig.defaults`), and the same
version in `firmware/donglem` (`zrh-donglem`: the hub images of one release
share its version), update `Cargo.lock` (`cargo update -p tbr-hub -p
zrh-donglem`), add the release's section to [CHANGELOG.md](CHANGELOG.md),
commit, then `git tag hub-vX.Y.Z && git push origin hub-vX.Y.Z`.
`hub-release.yml` checks the versions (also the ones in the built apps) and
the changelog section, builds with pinned tools and attaches per layout the
full flash image (`…-merged.bin`, offset 0, padded to the flash size),
bootloader, partition table, blank `otadata` (8 MB), app, the same set
for the 8 MB **Developer edition** (`…-8mb-devtools[-opi]-…`, tbr-hub
feature `dev`: the web UI's developer tools always on; built once
`firmware/hub` has the feature, its own entry in the web flasher), plus the
`tbr-bridge` app; for the SONOFF Dongle Max (experimental) the app for
`ota_1` (`zrh-donglem-X.Y.Z-app.bin`, also the file for SONOFF's web
console; `zrh-donglem-X.Y.Z-devtools-app.bin` for its developer
edition) and the `otadata` sector that selects it
(`zrh-donglem-X.Y.Z-otadata-ota1.bin`, at 0x5D000); `release-manifest.json`
and `SHA256SUMS`. The release notes are the changelog section, then the
esptool commands. Both flash sizes carry the MQTT bridge; the 8 MB images
also enable the advanced console and the Zigbee OTA image store
(`features` per board in the manifest; `tools/build-hub-firmware.sh`). A
suffix (`hub-v0.2.0-rc.1`) makes a pre-release. The repository variable
`HUB_RELEASE_DONGLEM=false` leaves the Dongle Max assets out. A pull
request that touches the release files, or `gh workflow run
hub-release.yml --ref <branch>`, runs the same workflow as a dry run
(nothing published; `dist/` and the notes are the run's `hub-release`
artifact). The flasher picks the release up within six hours, or at once
when this repository has the secret `FLASHER_DISPATCH_TOKEN` (fine-grained
token, *Contents: read and write* on faronov/zigbee-rs). It lists the
Dongle Max without an install button (`"web_install": false`: ESP Web
Tools' erase would wipe SONOFF's firmware). **Tag only once the live
flasher knows it**: the `revision` in
<https://faronov.github.io/zigbee-rs/flasher/sensor/status.json> must
include faronov/zigbee-rs#15 (a master CI deploy, 10–20 min after the
merge); a tagged run with the Dongle Max checks that revision's
`flasher_site.py` and the live page and fails otherwise.

### Image variants and memory (after release 0.2.0)

Hardware/backend requirements and the feature/readiness matrix:
[Firmware profiles and coordinator backends](docs/coordinator-profiles.md).
Moving from Zigbee2MQTT or ZHA without pairing the devices again:
[Migrating from Zigbee2MQTT or ZHA](docs/migration.md).

The 4 MB image keeps the existing partition table, `nvs`, `hubdata` and
boot-mode selection. Standard images use feature `mqtt` (4 MB) and
`mqtt,advanced,ota-store` (8 MB): normal ZCL, Tuya and Green Power devices,
groups/scenes/bindings, local automations, the catalog, WebSocket/HA and
ESP-IDF's esp-mqtt in both. The manual ZCL/Tuya console and live frame tap
are the optional `advanced` feature; an image without it answers those API
requests with `unsupported`. `hub/info.advanced` reports whether they are
present. The Zigbee device OTA server (`ota`; `ota-store` adds the image
store in the 8 MB layout's second app slot and implies it) is in the 8 MB
images only: the 4 MB images are built without it (no `ota` in
`hub/info`, `ota/*` answers `unsupported`, no Updates tab).

```sh
RUSTUP_TOOLCHAIN=esp tools/build-hub-firmware.sh --layout 4mb --locked
# Only the octal variant, or an advanced 4 MB image:
RUSTUP_TOOLCHAIN=esp tools/build-hub-firmware.sh --layout 4mb --psram octal --locked
RUSTUP_TOOLCHAIN=esp tools/build-hub-firmware.sh --layout 4mb --locked -- --features advanced
```

Every image marks its features and PSRAM mode in the binary;
`tools/release-images.py` refuses an image whose marker differs from its
board entry (e.g. an 8 MB build left in the 4 MB target directory).

Quad/Octal is the fitted memory's interface, not a PSRAM capacity; 2, 4, 8
and 16 MB PSRAM get the same limits: seven HTTP sockets, 128 KiB queued per
WebSocket client (384 KiB in all). A subscription snapshot goes as the
fragments of one WebSocket message, each made once the earlier ones are
sent, so it needs no large contiguous buffer. `hub/system` reports
`memory_mode`, the image's `build` (features, PSRAM mode) and these
`limits`.

Every image detects PSRAM at boot. Without it (the image was built for the
other interface, or the module has none) the application is **not
started**: on the lab N4R8 board a Quad image left ~35 KB of internal heap
and crashed as soon as clients connected, since the application's data and
the Wi-Fi/lwIP buffers otherwise live in PSRAM. The hub core keeps the
Zigbee network (NCP session, host store); the web page, `/api` (HTTP 503,
`psram_missing`) and the console's `info` say which image to flash. An
application mode on internal RAM alone needs about 100 KB less memory and
is future work.

The 4 MB app still has a 2,944 KiB slot and no hub A/B update; this change
adds neither SoftAP nor Zigbee OTA. Release **0.2.0** predates these
variants: its 4 MB images omit MQTT and include the console.

### Coordinator backends (cargo features)

The application runs on a coordinator backend chosen at build time
(`docs/coordinator-boundary.md` §6, `docs/ezsp-backend.md`):

| Build | Backend features | Notes |
|---|---|---|
| `tbr-hub` (Thread BR board) | `zrhp` (default; exactly one, else `compile_error!`) | the H2 NCP speaks ZRHP; not marked in the image (`app`, `mqtt`, `advanced`, `ota`, `ota-store` are) |
| `hub-app` | `zrhp` (default), `ezsp`, both, or none | `App::start_backend(Backend::zrhp(hub) / Backend::ezsp(…))`; `App::start`/`start_with` need `zrhp` |
| `hub-mqtt`, `hub-ota` | none | link no hub-ncp or zigbee-zrhp (CI checks the tree) |
| `hubctl` | `zrhp` + `ezsp` | `--sim` / `--serial` (ZRHP); `--backend ezsp --sim` (the EZSP simulated NCP, `serve`) |
| `zrh-donglem` (Dongle Max) | `ezsp` (hub-app `ezsp`, `advanced`, `ota-store`) | the hub app on SONOFF's stock EmberZNet NCP (`docs/dongle-max.md`); no ZRHP code |

```sh
cargo test -p hub-app                                         # ZRHP, OTA store (defaults)
cargo test -p hub-app --no-default-features --features zrhp   # the 4 MB images' app
cargo test -p hub-app --features ezsp                         # + EZSP: one suite on both backends
cargo build -p hub-app --no-default-features                  # an app without a backend
hubctl --backend ezsp --sim --sim-devices serve               # the app on the EZSP simulated NCP
```

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
| `crates/hub-coord` | Coordinator backend boundary (`std`, no backend protocol crate): requests and outcomes, events, errors, capabilities, network identity, the administration traits, and the APS/Green Power types the engine exchanges (re-exported by `hub-devices`); see `docs/coordinator-boundary.md` |
| `crates/hub-core` | Portable `no_std` core: board wiring (`board`) and board profiles (`profile`: Thread BR, SONOFF Dongle-M), flash layouts (`layout`), buffer limits (`resources`), esptool DTR/RTS → EN/BOOT logic (`lines`), re-export of `zigbee-zrhp` behind feature `zrhp` (git dependency on faronov/zigbee-rs, pinned rev) |
| `crates/hub-web` | The embedded web UI: a static page (`web/index.html`) and the tabs it loads on demand (`web/ui/*.html`), gzip-compressed by `build.rs` |
| `crates/hub-ota` | Zigbee OTA Upgrade server (cluster 0x0019): OTA file format, streamed images, image store (inactive app slot, 8 MB), server state machine, simulated OTA client ([`docs/ota.md`](docs/ota.md)) |
| `crates/hub-app` | Stage 2: the engine on a coordinator backend (`Backend`: ZRHP with feature `zrhp`, EZSP with `ezsp`), transport, device registry in the host store and its ownership (`NcpNv` decision for EZSP), app thread with the API sessions, catalog verifier (trusted keys) |
| `crates/hub-ezsp` | ASH + EZSP host for EmberZNet NCPs (EZSP 13–18), simulated NCP, the EZSP coordinator backend ([`docs/ezsp-backend.md`](docs/ezsp-backend.md)) |
| `crates/hub-mqtt` | Zigbee2MQTT-compatible MQTT bridge: topics, payloads and exposes from the hub's entities, HA MQTT discovery, the bridge thread with reconnects; a desktop MQTT client and a test broker |
| `crates/hub-ncp` | Phase 1: portable ZRHP host core (`std`): link driver, sessions/resume, client API, NCP lifecycle, `FW_*` streaming, host store journal, simulated NCP (`sim`) |
| `tools/hubctl` | Phase 1: `hub-ncp` on a desktop: the real NCP through the bridge, or `--sim`; the hub app on the EZSP simulated NCP (`--backend ezsp --sim`) |
| `firmware/bridge` | Phase-0 S3 firmware (`tbr-bridge`, esp-idf-svc + esp_tinyusb); also a library, the default mode of `tbr-hub` |
| `firmware/hub` | Phase-1 S3 firmware (`tbr-hub`): bridge mode (default) and hub mode; 4 MB / 8 MB partition tables for provisioning |
| `firmware/espapp` | ESP-IDF glue shared by the hub firmwares (`zrh-espapp`): PSRAM heap, panic recorders, flash windows, HTTP server, WebSocket API, log ring and SNTP, catalog flash, MQTT transport |
| `firmware/gateway`, `firmware/donglem` | SONOFF Dongle Max (classic ESP32 + EFR32MG24) firmware: the hub app on the EZSP or the ZRHP backend (chosen at boot), its data on SONOFF's `files` partition after a claim, Ethernet/Wi-Fi, self-update, the MG24's firmware replaced through its Gecko bootloader (EmberZNet ⇄ the zigbee-rs NCP; not yet on hardware), LED, button (permit join), MG24 lines and probes, return to SONOFF's firmware; runs on SONOFF's unchanged partition table (`docs/dongle-max.md`) |
| `tools/make-app-image.sh` | ELF → ESP app image (same `elf2image` arguments as ESP-IDF; `CHIP=esp32` for the Dongle-M), optional size limit |
| `tools/make-otadata.py` | One `otadata` sector selecting an OTA slot (the Dongle-M's first flash) |
| `tools/check-s32c1i.sh` | Fails if Rust code in a firmware ELF has an S32C1I compare-and-set (SCOMPARE1 written, then S32C1I within 4 instructions in one straight run; in each function's own bytes, from its symbol's address and size; not on literal words; Rust by line table or mangled name). Data decoded as instructions (lone S32C1I/SCOMPARE1) is listed, not counted. CI runs it on every S3 and Dongle-M ELF and on a positive control (`tools/s32c1i-control`, built with S32C1I: each of its six atomics must be found) |
| `tools/size-report.py` | Firmware size breakdown of an ELF: bytes per ESP-IDF component / library and per Rust crate (flash code, rodata, IRAM, DRAM), largest symbols |
| `tools/flow/` | Reserved-line check, RAM-only H2 UART test program and the flow-control measurements (`reserved_lines.py`, `h2-uart-test/`, `flow_test.py`); button-free S3 reflash `s3_reflash.sh IMAGE TAG [4mb-dev or 8mb]` (the app offset comes from the partition table on the device via `s3_partition.py`; a different layout is refused) |
| `docs/device-support.md` | Device support design: tiers, definition format, expression language, self-description, Green Power, entity model, budgets |
| `docs/ota.md` | Zigbee OTA Upgrade server: behaviour, image store, clients and the online index, NCP findings, OTA files for zigbee-rs devices |
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

esp-idf-svc is pinned to exactly `=0.53.0` in every firmware crate: the MQTT
transport (`record_error` in `firmware/espapp/src/mqtt.rs`) reads the raw
esp-mqtt error event through 0.53.0's private `EspMqttEvent` layout, because
0.53 reports every MQTT error event as `ESP_FAIL`. **TODO on upgrading to
0.54+:** use its public `EventPayload::Error(&EspMqttError)`
(`connect_return_code()`, `esp_transport_sock_errno()` …) and delete the
`transmute_copy`, the `unsafe` and the size assert.

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
  PSRAM: the lab board's module is an **ESP32-S3-WROOM-1 N4R8** (marking
  M0N4R8: 4 MB flash, **8 MB octal PSRAM** in the ESP32-S3R8 die). Builds up
  to hub 0.1.3 were quad-PSRAM only, so it ran without PSRAM. Other boards
  carry N4R2 (2 MB quad, seen at a user) and, at retail, N8R2/N8R8; the
  module marking or `esptool chip-id` ("Embedded PSRAM 8MB"/"2MB") tells.
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
rollback; ZRHP decision N6 as amended 2026-11-18).

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

Through the hwlab **MCP** server a hub image (2.4–2.5 MB) does not fit one
call: programming runs at about 11 KB/s and the MCP request times out after
about 110 s, leaving a half-written slot (verified 2026-11-21). Program it
in parts instead: the image from offset 0x1000 on in raw parts of at most
640 KiB (`raw: true`, at 0x11000, 0xB0000, …), then take a fresh backup and
program the whole image at 0x10000; hwlab skips identical sectors, so only
the header sector is written (about 40 s). Do not `halt` the running app
first: a halted core trips the watchdog and the S3 resets.

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
| boot after a panic or watchdog reset | persisted hub mode: the hub restarts (`info` shows the panic, the last ESP-IDF panic with its PC, caller, task and how many boots ago (RTC memory; the coredump can stay empty), and the crash streak); bridge mode, or a fourth crash in a row each within 10 min of boot: safe mode, TinyUSB never starts, PHY → USJ |
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
- **0.2.4** (not flashed yet): fix for the "5 s silence after opening the
  bridge port" (zigbee-rs Z14). With flow control on and DTR set, the UART
  reader kept retrying a CDC 0 write while the host had not drained the TX
  FIFO, so it stopped reading the UART until the port was reopened. Now it
  waits at most 200 ms without progress, then flushes the TX FIFO, counts
  a stall and goes on; a DTR edge (port opened or closed) flushes the FIFO
  too. `info` shows both counters (`stalls`, `DTR flushes`).
- **0.2.5** (hub release 0.3.0): no change in the bridge's code or in the
  `hub-core` parts it uses. Rebuilt with the release, its bytes differ
  from the 0.2.4 asset of hub release 0.2.0 (build metadata, dependency
  versions), so it gets its own version: one name, one image.
- **0.2.6** (hub release 0.4.0): esp-idf-svc pinned to 0.53.0, as in every
  firmware crate; no functional change. The image no longer carries a
  build time (`CONFIG_APP_COMPILE_TIME_DATE=n`; until 0.2.5 every release
  build stamped its commit time, so the file changed with each release):
  two clean builds with different `SOURCE_DATE_EPOCH` gave byte-identical
  images and ELFs, and `tools/release-images.py` refuses a bridge image
  with a build time. The bridge now changes version only with its code.

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
| `driver` | One thread owns `zigbee_zrhp::link::Link<8, 8>` (embedded profile: RX window 8, LOG credit 0, BULK 2; credits returned on delivery). Once a session is up it grants LOG 2, so the NCP's log reaches the hub's (`network/ncp_log`; not the backend probe, which leaves the NCP's boot records to the backend). It answers `EVT_NCP_READY` with `SYS_NEGOTIATE` + `SYS_GET_INFO` itself, then reports the session. Requests are **gated** until the lifecycle has resumed the NCP, bounded by the 20 s resume budget. A dropped session makes transmitted requests "in doubt": same NCP cookie (`PEER_RESYNC`) ⇒ re-sent with the same TID and `RETRY` (the NCP's retry cache answers), new cookie ⇒ `LINK_RESET` (delivery unknown, spec 6.5). Two-phase commands wait for their completion event (`EVT_APS_DATA_CONFIRM`, `EVT_NETWORK_STATE`). Without a session the driver cycles 921 600 + RTS/CTS → 460 800 without flow control |
| `lifecycle` (`Hub`) | Seamless resume on every session: UART flow-control mismatch (NCP without RTS/CTS ⇒ host flow off), store identity (`ncp_ieee`), NCP cookie, `table_epoch`, NCP flash size (`SYS_GET_INFO.flash_size`, decision N4: picks the H2 slot size, 1.875 MiB on 4 MB, 896 KiB on 2 MB, so oversized images are refused before `FW_BEGIN`); network ownership (adopt into an empty store, refuse to touch a different network, report a lost one); `NWK_INIT` with the checkpoint-log floors (`last + 9 × fc_stride`); replay of host endpoints and the permit-join remainder; `TBL_SYNC`; NCP firmware check. Operations: `form` (PAN ID, EPID and key chosen and persisted by the hub, then `NWK_FORM`), `permit_join`, `add_endpoint`, `send_aps`, `fw_update`, `fw_confirm`, `reset_ncp` (soft/EN), `enter_rom_loader`. Maintenance (`netops`): `energy_scan`, `change_channel` (store follows the new channel and update ID), `set_tx_power` (persisted, replayed on every resume), `diag_counters`, `ncp_status`, the coordinator's neighbours/routes/children (all pages), `backup_export` / `backup_restore` (ZRHP record streams, spec 9; the restored network is adopted with its counters as floors), `leave_reset` |
| `inventory` | Devices the NCP already knows: `TBL_READ` of the `DEVICE` table, `DIAG_GET_CHILDREN` and `DIAG_GET_NEIGHBORS` (paged; previous children skipped; `UNSUPPORTED_COMMAND` tolerated and reported), merged by IEEE with the store's device records, the coordinator left out |
| `fw` | `FW_BEGIN` → `FW_WRITE` (two in flight, resynchronised on `next_offset`) → `FW_FINISH` → `FW_ACTIVATE`, straight from the source (file, upload); the S3 keeps no NCP image. The next session confirms the hub's own trial image with `FW_CONFIRM` after the network resumed, or reports the H2 bootloader rollback. Trial images the hub did not activate are not confirmed (another host may own them) |
| `store` | Host store journal (spec 3.4/8.8): 32 × 4 KiB ring, records `len, type, commit, payload, CRC-32C`, committed by clearing the commit byte after a read-back; two-generation snapshot + deltas, compaction above half the ring. Holds the store identity, the owned network (incl. key), the FC checkpoint log (every 4th checkpoint persisted), a device registry, the pending NCP update and the host endpoints. Backends: `hubdata` partition, a file, RAM |
| `sim` | Simulated NCP (the real link on the NCP side, scripted command set incl. A/B slots, trial boot and rollback, energy scan, channel change, TX power, DIAG tables and counters, backup export/restore, remote routers answering Mgmt_Lqi/Mgmt_Rtg; any command can be made `UNSUPPORTED_COMMAND`) for the tests and `hubctl --sim` |

Phase 1 sets `FC_CHECKPOINTS | RETRY_CACHE`, not `HOST_TABLES`
(host-backed tables and link keys are phase 2): `TBL_LOOKUP` is answered
`NOT_FOUND`, `TBL_WRITEBACK` `STORAGE_ERROR`.

### `tbr-hub` (S3)

| Mode | What runs |
|---|---|
| **bridge** (default; no NVS key) | `tbr-bridge` (0.2.6) unchanged: same USB identity and ports, CDC 0 bridge, console on CDC 1 |
| **hub** | `hub-ncp` on UART1 (921 600 + RTS/CTS, IRAM ISR, 16 KiB RX ring), H2 EN/BOOT on GPIO7/8; CDC 0 idle; console on CDC 1 with `hub …` commands; the bridge commands that touch the H2 UART or EN/BOOT are refused |

Console additions: `mode`, `mode bridge|hub` (persisted in NVS `hub/mode`,
reboot), `mode hub once` (one boot in hub mode, any later reset returns to
the persisted mode), `hub [status]`, `hub echo [n]`, `hub form [channel]`,
`hub permit <s>`, `hub fw slots`, `hub fw confirm`, `hub ncp reset [hard]`,
`hub crash` (a deliberate panic), `hub rom` (manual NCP recovery: reboot into the bridge with the H2 in its ROM
loader, then esptool on CDC 0 with an operator-supplied image). USB recovery
(`s3 usj`, 1200 bps touch, mount timeout, crash safe mode) is the bridge's,
in both modes; a persisted hub restarts after a crash and leaves it to the
safe mode only in a crash loop. Back to the bridge: `mode bridge` on the
console (CDC 1), or `s3 usj` and reflash.

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
| `tbr-bridge` 0.2.4 | 456 256 B |
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
- Hardware, 2026-11-20 (board handed over for the hub runs; H2 = master
  NCP BR image, PAN 0x1C06, channel 17, the C6 sensor 0x93B2 joined):
  - `hubctl` with a store on macOS through the bridge: adopted the NCP's
    network; a 600 s soak with three H2 EN resets from the bridge console
    found that a rebooted NCP reports its network **formed but down**
    (`network_state` 1) and `NWK_GET_INFO` has no PAN/EPID until
    `NWK_INIT`, so the hub took it for a foreign network and left it down.
    Fixed: a formed-down network is started first (`NWK_INIT` with floors),
    then compared. After the fix: 330 s soak with three EN resets, 28 856
    echoes, 0 failures, 3 echoes interrupted by the resets (outcome unknown,
    as specified), each resume ~200 ms with `NWK_INIT` from the store's
    floors; `reset` (SYS_RESET) and `reset --hard` (EN) resumed in < 1 s;
    the frame-counter floors move forward. The ~3 000 bad frames per run
    are the H2 boot log at the wrong baud during resets (2 without resets).
  - Permit join (30 s) and listen: the C6's temperature and humidity reports
    every 10–60 s. `hubctl serve` against the real NCP: WebSocket API up,
    but the engine knows no devices: devices that joined before the hub
    adopted the network are not imported from the NCP tables yet (gap).
  - `FW_*` with an ECDSA-P256-signed envelope (master 33cdace BR build,
    726 200 B, ~30.8 s at 921 600): staged in slot 1, trial boot, held
    unconfirmed (`--hold-trial`), reset → the H2 rolled back to slot 0 with
    the network and counters kept; second run with `FW_CONFIRM` → slot 1
    confirmed, survives a reset. Fixed: rollback is detected by slot and
    version (the NCP reports the digest of the staged ESP image, not of the
    envelope), and `Hub::shutdown` waits for a store commit in progress.
    H2 backup before the update: `esp32h2-br-flash-…-40f581eccab2`.
  - `tbr-hub` 0.1.0 core-only (521 632 B) in S3 `ota_0` of the stock table
    (hwlab backup and program, RTS reset): boots in bridge mode (the bridge
    works for the Mac as before); `mode hub once`: ZRHP session at 921 600
    with RTS/CTS, network owned (host store in RAM: the stock table has no
    `hubdata`), 200 echoes mean 11.3 ms / worst 15 ms (same as through the
    bridge: the NCP's ACK delay dominates), `hub ncp reset` resumed;
    `s3 reset` → bridge mode again.
- Not run on hardware yet: formation by the hub, joins handled by the
  engine, the application build on the S3 (needs the 4 MB dev partition
  table).

## Stage 2: the application on the host core (`hub-app`, branch `stage2`)

The device engine (`hub-devices`) runs on the ZRHP host core (`hub-ncp`):

| Piece | What it does |
|---|---|
| `convert` | ZRHP events → engine inputs (APS indications with source IEEE and group, admitted joins, `EVT_GP_*` incl. commissioned sink records and remote sink/translation tables); engine APS/GP requests → wire (`APS_DATA_REQUEST`, `GP_*` 0x0A01–0x0A0D) |
| `transport` | The engine's `ZigbeeTransport` on `hub_ncp`: APS data non-blocking (`Client::start`, held across an NCP resume, outcomes counted), GP/group/remove/permit-join as calls only while the session is ready |
| `registry` | Device registry in the host store journal (device-support 16.1): `dev/<id>` = deflated JSON `DeviceSnapshot` (identity, fingerprint, descriptors, attribute support and the values resolution reads, self-description fragment as zdef CBOR, user settings and entity overrides, configure hash/results, GP sink record), `udef/<id>` = user definitions. Written on device changes (2 s coalescing) and in a 30 s sweep; identical values are skipped and snapshots hold no measured values, so reports never write flash. The journal stores values > 2000 B as parts, applied only when complete |
| `laststate` | Last known values (`st/<id>`: entity states, `last_seen`, LQI, RSSI; 0.2–0.5 KB per device): a device at most every 15 min when its states changed, every 6 h for diagnostic-only changes, everything on a clean reboot or stop; restored at start with `attributes.restored: true` until the next report (the web UI shows them faded, HA gets the attribute) |
| `app` | One thread: engine, `ApiServer` sessions (any `SessionSink`), registry, 1 s tick; registers the hub endpoint 1 (HA profile) once (replayed by `hub-ncp` on every resume) and the coordinator IEEE; built-in catalog = the definition examples until a bank is installed (the catalog store and service: `catalog`); once per NCP session imports devices the NCP knows and the engine does not (joined before the hub adopted the network) and interviews them; after a start, reads the still-unknown states of restored devices once (`refresh_unknown`; a sleepy device gets the reads right after its next frame), since snapshots hold no measured values and a device may report only on change (the C6 sensor: temperature only after a 0.5 °C change or 15 min) |
| `catalog` | Trusted keys of the definitions catalog: **slot 0** = `keys/zdef-catalog-ed25519.pub.pem` (`ff931767…63c6a2`, kid `66f64ac1cbfc9e27`), **slot 1** reserved for a rotation (add the successor in a firmware release, sign with it, retire slot 0 later); `verify_manifest` / `verify_bank` (COSE_Sign1 Ed25519, `zdef`); the store in `hubdata` (`store`: bank slots A/B, active-bank record, manifest; `local`: fetched definitions; `data`: the partition, memory-mapped) and the service (`service`: chunked uploads of manifest, banks, web tools and shards, what clients should fetch, `catalog/*` API); `web`: the web tools served at `/zdefc/` |
| `admin` | Network and hub maintenance (API 1.2, device-support 14.6): energy scan with a channel recommendation that avoids the hub's own Wi-Fi channel, channel change, TX power, NCP diagnostics, the network map, Open Coordinator Backup export/restore, reset + form; Wi-Fi credentials, API token rotation, reboot, boot mode, system facts and a log tail through the platform. NCP work runs on job threads in two lanes, one job each (heavy: scans, map, channel change, backup/restore/reset; light: diagnostics, TX power; `busy` otherwise), so the app thread keeps serving |
| `jobs` | The job board: running jobs per lane, progress (time estimate, measured passes, the NCP's optional `EVT_SCAN_PROGRESS`), cancellation, the last result of each kind for clients that connect later |
| `ocb` | `zigpy/open-coordinator-backup` v1 ⇄ ZRHP record streams (byte orders and Z2M/zigpy quirks of ZRHP 9.1); the export is written as text, not a JSON tree (RAM) |
| `topology` | Network map: the coordinator's `DIAG_GET_NEIGHBORS`/`ROUTES`/`CHILDREN`, then Mgmt_Lqi_req/Mgmt_Rtg_req to every router found, breadth first |
| `automations` | Local automations (API 1.3, device-support 14.8): rules of triggers (entity state equals/not equals/above/below/changed, entity events incl. remotes and GP commands, device availability, a cron schedule, hub start), conditions (state, local time window, weekday, and/or/not) and actions (entity command, any allowed API request for groups/scenes, delay, notify, enable/disable a rule) on the app thread, sans-IO through a `World`; bounded (rules, runs, traces, runs per minute) with self-trigger loops stopped through a causal chain; delays on the monotonic clock, schedules wait for a synchronised wall clock; rules in the journal as `auto/<id>` (compact JSON, format `v` 1) |
| `platform` | `Platform` (what the app needs from the S3 or the desktop) and `LogRing` (a fixed-size ring of recent log lines) |

`hub-devices` gained `persist` (`snapshot_device` / `restore_device`) and
`ApiServer::pump_events`; `hub-ncp` gained key/value records, PTA
`coex_enable` following the Wi-Fi state (decision N7, replayed on resume),
`group_membership`, `remove_device`, `permit_join_on`. All zigbee-rs crates
are pinned to one revision (master `cb9b5c0e`).

**Desktop.** `hubctl --serial … --store hubdata.bin serve [--listen
127.0.0.1:8099] [--token T] [--hubdata FILE]` runs the application with its WebSocket JSON
API (`ws://…/`) and the catalog store in a 1 MiB `hubdata` image (RAM
without `--hubdata`); `hubctl catalog-push URL --token T [--from URL | --dir
DIR]` uploads the signed catalog to a hub; `hubctl ws-call URL --token T JSON…` is a minimal client;
`hubctl catalog-verify FILE…` checks catalog banks and `manifest.cose` with
the firmware's keys. `--sim` works for all of them.

**S3.** `tbr-hub` (hub mode) runs the application, the
WebSocket API on `ws://<ip>/api` (esp_http_server, one session per socket,
pushes through detached senders) once Wi-Fi credentials are stored
(console `wifi set <ssid> <password>`, `wifi`, `token`, `catalog`), and the
same API as JSON lines on CDC 0 while a host holds the port (a test path
without network).
`hub/info` returns the hub name, API version, features, current `seq`,
device/entity counts and, once the NCP session is up, `coordinator_ieee`,
`ncp_board` and `ncp_product` (the Home Assistant integration's unique ID).
The S3 implements the hub side of the maintenance API (1.2,
`firmware/hub/src/platform.rs`): `hub/wifi/set` stores the credentials in
NVS and the station reconnects with them at once, or starts if the hub
booted without any (console `wifi set` does the same in hub mode);
`hub/token/rotate` stores the new token in NVS; `hub/reboot`;
`hub/mode/set`; `hub/system` (internal heap free, minimum and largest block,
uptime, Wi-Fi SSID/IP/RSSI/channel, reset reason); `hub/logs` from a 3 KiB
RAM ring of Rust log lines up to Info (ESP-IDF's C logs are not in it),
stamped with the local time once the clock is set. **Time:** SNTP
(`esp_sntp`, default `pool.ntp.org`, up to three servers) starts with the
first Wi-Fi connection; the S3 keeps its clock over a software restart, so
after the first synchronisation the time is known at boot (`source`
`rtc`). The engine's clock stays monotonic for its timers; API times
(`time`, `last_seen`, events) become Unix time through an offset once the
time is known (`time_synced`, `time_utc`). The hub endpoint serves the ZCL
Time cluster (0x000A: Time, TimeStatus Master|Synchronized|MasterZoneDst,
TimeZone, DstStart/End/Shift, StandardTime, LocalTime, LastSetTime) to
thermostats and clocks, and Tuya devices' time requests (EF00 0x24) get the
local time of the configured zone. The energy-scan advice avoids the AP's channel. Maintenance jobs run on one
long-lived 12 KiB worker thread started with the app; a job that starts
while another runs (a diagnostics read during a scan) gets a thread of its
own, only while the internal heap has its stack in one block plus 16 KiB
(`busy`, "low memory", otherwise). Before, every job got a new thread: after
10 h of uptime the hwlab board had 51 KB free but no 12 KiB block, so every
job failed with `job thread: Not enough space (os error 12)`, and each
thread left about 300 B behind. With the worker (hwlab, 2026-11-22) the
heap stays level over repeated jobs; the jobs use at most 4 KiB of the
stack (high water logged after each job); a 5-pass scan with two sessions
reading diagnostics meanwhile runs, the extra reads are refused as low
memory. Kept job results take at most 8 KB. On the hwlab board (2026-11-21): `hub/system`, `hub/logs`,
`hub/reboot`, the network map (the C6 as the coordinator's child), backup,
and channel change 17 → 25 → 17 (about 9 s each; the C6 followed within a
minute) work; `hub/wifi/set` and `hub/mode/set` were not tried on the shared
board. The first try of `network/diagnostics` there ran the heap out
(minimum free 12.7 KB, then an abort and a reboot: its result was a
serde_json tree, about 0.7 KB per counter object); maintenance results are
now written as text into the result frame with fallible allocation
(`no_space` instead of an abort). Afterwards: 46 counters in a 4 KB frame in
170 ms, minimum free heap 27.5 KB, 22 KB with the web UI's map scan and
three sessions.

**Definitions catalog.** The hub keeps the signed catalog of
`faronov/zigbee-rs-definitions` in `hubdata` but downloads nothing itself
(no TLS in the firmware): the release is mirrored to GitHub Pages
(`faronov.github.io/zigbee-rs-definitions/latest/`), and clients with
Internet access — the web UI, the Home Assistant integration, `hubctl
catalog-push` — fetch the files and upload them in resumable chunks
(`catalog/upload_chunk` + `catalog/commit`, targets `manifest`, `bank`,
`web`, `shard`). The hub streams each upload into flash (one chunk in RAM),
checks it in place (Ed25519 COSE signature of the manifest with the
built-in key and its version; the SHA-256 the manifest lists for every
other file; a bank also by its own signature and body hash, its
definitions when a lookup loads them) and switches banks A/B atomically (`hub_core::layout::HubdataMap`,
device-support 9 and 16). `catalog/status` tells clients what to fetch:
`wants` (a newer bank for this slot size, the web tools) and `missing`
(devices that resolve to generic only, with the shards holding their
keys; event `definition_needed`). Shards are checked against the installed
manifest, so the web UI installs the mirror's newer manifest (and the bank
and web tools it then wants) before fetching shards — for Find definition,
Fetch, and for `definition_needed` and on opening the Catalog tab when
`auto_update` is on — and once more after a shard refused as not matching,
retrying the shards once; an older or the same manifest is not uploaded,
an interrupted bank or web tools install of the installed release
continues where it stopped, and a busy hub gets one retry 30 s later.
Lookups read the memory-mapped bank:
format 2 banks (zdefc default since 2026-11-21) keep even their string
table in flash, so a lookup only allocates the inflater, a 4 KiB block
buffer and the decoded candidates (7.3 KB median, 33.7 KB for the largest
of 2 289 definitions). Tested on the host end to end (all upload targets,
an interrupted upload resumed, restart, anti-rollback, a corrupted bank;
`hubctl catalog-push` installed the real signed `catalog-2026.11.21` on the
simulated hub). On the BR board (S3 without PSRAM, 2026-11-21) `hubctl
catalog-push` installed the same release in 4 KiB chunks (the 218 KB bank in
about 10 s, commit 0.6 s; lowest free heap 24 KB with Wi-Fi, the NCP and a
joined device running); manifest and bank survive a reboot and
`definitions/get` reads the bank in place. Not yet run on hardware: the web
tools upload and a device resolved from the bank. Cost on the S3: app image
+119.5 KB (Ed25519/COSE verification ~32 KB, bank reader ~15 KB, store and
upload service ~50 KB), static RAM +32 B.

**Web UI.** `http://<ip>/` serves a static page (`crates/hub-web/web/index.html`:
HTML, CSS and JS, no framework, no CDN) that talks only to the WebSocket API
on `/api`: network (state, PAN ID, channel, extended PAN ID, NCP firmware,
IEEE and link), permit join with a countdown, compact device cards (picture,
name, availability; right under the title, in the same place on every card
and in the device page's order, battery, a signal dot and LQI, RSSI, last
seen and power source; then the main sensors, controls and events — a
remote's last action and when — with their live states; subscription to `devices`,
`entities` and `network`) and, one click on a card away, the device page
`#device/<id>` (model, IEEE, NWK, LQI/RSSI, last seen, power source,
definition — its model and source, partial when the catalog could not
translate all of it —, every entity under a section header: sensors,
controls, events, configuration and diagnostic; rename, re-interview, find definition, bindings, reporting per
attribute — min and max in seconds, the change in the entity's units —,
diagnostics — what the device reported (Basic: manufacturer, model ID,
application/stack/HW version, SW build ID, date code, ZCL version, power
source; logical type, endpoints and clusters) and what the hub made of it
(definition and layers, catalog definitions this firmware cannot run,
unknown attributes, Tuya DPs), with the JSON of `devices/diagnostics`,
the record, entities and states to copy or download — and
remove with confirmation; back returns to the list where it was), controls for switches and lights (on/off, brightness), numbers,
selects and buttons, connection status with automatic reconnect; mobile
layout, light/dark from `prefers-color-scheme`. The page is split into
tabs (`#/devices`, …), listed in a sidebar on screens 900 px wide or more
and, on narrower ones, in a menu that the button ☰ (with the open tab's
title) opens over the page: each `<div class="tab" data-tab="id" data-title="…">`
in `<main>` becomes one, in document order, and `onTab[id]` runs when it is
shown, so a feature adds its own tab without touching the others. The
rarely used tabs — Converters, Catalog, the ZCL console and Updates — and
rarely used parts of others — Settings' migration wizard, the device page's
Diagnostics — are modules (`crates/hub-web/web/ui/<name>.html`: the tab's markup, then its
script; the tab's `data-mod`) that the page loads when the tab is first
opened, or when it needs one: the device page's Find definition and a
`definition_needed` event with fetching for new devices on load the
Catalog's code. They come from the hub (`/ui/<build>/<name>.js`, never
from the internet) as classic scripts run in the page's scope, and only of
the page's own build: the build hash is in the path (cached for good,
`immutable`) and in the module's first line, which the page checks before
running it, so a page left open over a firmware update reloads once instead
of mixing builds (an unknown path answers 404). An image without a module's
feature has neither the module nor its tab.
The **Green Power** tab (`#/gp`) opens and closes the GP commissioning
window (duration, proxies, exit on first pairing) with a countdown, lists the
GPDs in bidirectional commissioning, recent failures with their reason and the
GP devices (ID, type, security level, frame counter, sink mode, an
`unsecured` badge, remove with or without the proxies), and manages GP
bindings (create, edit, delete, verify; target probe; executor auto, device
or hub; per-target state) and the sink's security level and communication
mode (topic `gp`); API 1.2 added `gp/commissioning/status` and
`gp/config/get` for it.
The **ZCL** tab (`#/zcl`) is a console for writing and debugging
definitions: per device, endpoint and cluster (or any cluster ID) with an
optional manufacturer code it reads and writes attributes (the type comes
from a read or the resolved definition, or is given), configures and reads
reporting, discovers attributes and received/generated commands and sends
any cluster or foundation command with a hex payload; answers are shown
decoded. A live frame tail (topic `frames`, filter by cluster) and a Tuya
EF00 datapoint viewer (DP, type, value; send DP) follow the device; read
attributes and seen DPs can be copied as definition snippets
(device-support 14.7). In the standard images (feature `advanced`: the 8 MB
ones and the gateway) it is a developer tool: the tab appears with
**Developer mode** (Settings › Hub; `hub/developer/set`, kept by the hub,
`developer_mode` in `hub/info`, a `hub_updated` event tells the other open
pages) and the hub serves its module only then (403 otherwise,
`Cache-Control: no-cache` so a browser asks again). Developer mode only
shows the tab and serves its module: the console's API (`zcl/*`,
`tuya/send`, topic `frames`) stays available with the API token, which
already grants full control (Home Assistant services may use it). The
**developer edition** ("devtools": cargo feature `dev`, the
`tbr-hub-<ver>-8mb-devtools[-opi]` and Dongle-M devtools images; `edition`
`devtools` in `hub/info`, `standard` otherwise; its further developer tools
come later) always has it, without the switch; the 4 MB images have no
developer tools.

The **Groups** tab (`#/groups`) creates, renames and deletes Zigbee groups,
adds and removes member endpoints (ZCL Groups, with their state: pending,
ok, failed, or waiting for a sleepy device to wake), switches the group's
light or switch entity (one groupcast per command), stores the members'
current state as a scene, recalls, re-captures and removes scenes, and
identifies the group; each device page has a **Bindings** section: the
device's binding table (read with Mgmt_Bind_req), pending requests, and
bind/unbind of a source endpoint's clusters (presets remote → light: On/Off,
Level, Color) to a device endpoint or a group (topic `groups`,
device-support 14.9).
It asks for the API token
once (console `token`; a link ending in `#token=…` stores it too) and keeps
it in the browser. `build.rs` minifies the page line by line (indentation,
blank lines and `//` comment lines in the script and style; `src/minify.rs`,
which refuses constructs it cannot keep intact) and the modules alike, gzips
each at build time (deterministic, pure Rust, no node toolchain) and derives
strong ETags and the build hash (in every image variant the page must stay
within 36 KiB gzipped, `hub_web::PAGE_BUDGET`, each module within its
`MODULE_BUDGETS` entry and the page with all its modules within
`TOTAL_BUDGET`, 44 KiB — `DEV_TOTAL_BUDGET`, 64 KiB, in the developer
edition, devtools —, checked by the crate's tests, which also run every module in
Node in one scope with the page); the hub answers
`Content-Encoding: gzip`, `Cache-Control: no-cache` for the page and `304` on a matching
`If-None-Match`. `hubctl serve` serves the same page on `/` and its modules
(the WebSocket on `/api`; `--features dev` builds the developer edition,
devtools), so the UI can be tried on a desktop, also with `--sim`. API 1.1
added what the page needed: `network/info`, the `network` topic
(`permit_join`, `network_state`), `time` in it for relative times, and `nwk`
in device records (device-support 14.2); the header and the hub facts
(coordinator IEEE, API version, counts) come from `hub/info`.

**Maintenance API (1.2).** Network settings and upkeep over the same
WebSocket (device-support 14.6): `network/energy_scan` (per-channel energy,
the channels under the hub's own Wi-Fi channel marked, a recommendation),
`network/active_scan` (the Zigbee networks around: PAN ID, EPID, LQI, permit
join; the scans and the map run as background jobs: the request answers
with a job ID, progress and results come as events, `network/jobs` lists
the running ones, `network/job/cancel` stops them), `network/change_channel`, `network/set_tx_power` (re-applied after every NCP
restart), `network/diagnostics` (NCP status incl. PTA, DIAG counters, frame
counters, host link counters), `network/topology` (map from the
coordinator's tables and Mgmt_Lqi/Mgmt_Rtg over the air),
`network/backup` / `network/backup/last` / `network/restore` (Open ZigBee
Coordinator Backup, as zigpy/ZHA and Z2M write it; restore in device batches to a
fresh NCP), `network/reset` (confirm `RESET`: automatic backup, leave,
form with chosen channel(s), PAN ID, EPID, TX power), and the hub itself:
`hub/system`, `hub/logs`, `hub/wifi/set`, `hub/token/rotate`, `hub/reboot`,
`hub/mode/set`, `hub/time` / `hub/time/set` (time zone as a POSIX TZ
string with an optional IANA name, SNTP servers; the web page picks an IANA
zone and converts it in the browser), `hub/developer/set` (developer mode: the web UI's
developer tools, images with the advanced console; event `hub_updated`).
`hubctl serve` implements the hub side for a desktop
(credentials and token in memory, its own log tail; `--wifi-channel N` for
the scan advice); `hubctl --sim --sim-demo serve` forms a network on the
simulated NCP with three routers that answer Mgmt_Lqi/Mgmt_Rtg, and
`--sim-unsupported 0x0106,0x0108` mimics an NCP image without energy scan
and TX power. The web UI has a **Map** tab (an SVG drawn by the page: the
coordinator in the middle, rings by hop count, end devices next to their
parent, links coloured by LQI, a table with the best LQI per node) and a
**Settings** tab (network facts, channel change, TX power with the applied
and requested value, the chip's range and a regulatory note, a channel scan:
utilization in % per channel over 1–10 rounds with a progress bar
(pass k of N, elapsed, time left) and a Stop button, averaged or peak, the hub's
Wi-Fi channel and any others the user lists marked, a recommendation with
its reasons and the Zigbee networks heard per channel, backup download,
the last automatic backup, restore from a file in device batches, a new-network form
that needs `RESET` typed, hub facts, Wi-Fi, token rotation, reboot, bridge
mode, NCP diagnostics and the log tail); controls the hub or its NCP lack
are hidden (`maintenance` in `hub/info`); the page stays usable while a
scan runs, and a page opened meanwhile follows it. The page is 21.9 KB minified and gzipped
with the other tabs (Green Power included; 23.3 KB without the minification).

**Automations (API 1.3).** Rules that run on the hub itself, without Home
Assistant or the internet (device-support 14.8): `automations/list`, `get`,
`validate`, `create`, `update`, `delete`, `enable`, `disable`, `run` (a test
run) and `traces` (the last 20 runs: trigger, conditions evaluated, each
action's result, also results that arrive later), events on the
`automations` topic, `automations` in `hub/info`. A rule is triggers →
conditions → actions (example: motion after 18:00 → light on, 90 s, off;
`mode` single or restart; `max_per_min`). Runs are deterministic and
bounded: 64 rules, 16 actions each, 16 runs waiting in delays, a rate limit
per rule, and a rule that would trigger itself (directly or through other
rules, within 10 s of its command) is stopped as a loop. Delays use the
monotonic clock; `time` triggers (cron) and time/weekday conditions wait
until the wall clock is synchronised (the list says so: `time_synced`,
`suspended`). Rules are journaled in `hubdata` and restored at start;
`start` triggers run once the NCP session is up. The web UI has an
**Automations** tab (the rules with an enable toggle, Run and Edit, a form
for a simple rule — one trigger, an optional condition, a command or
notification and an optional delayed second command —, the JSON of the
whole rule for everything else, run history with the trace of each run);
the Home Assistant integration shows each rule as a switch (enabled) and a
Run button on the hub device. `hubctl --sim --sim-devices serve` adds two
simulated On/Off plugs and a temperature sensor (the engine's device
simulator answering interview, reads and commands) for trying them on a
desktop; `tools/hubctl/tests/automations_e2e.rs` drives a rule chain, a
loop, a delay and a hard restart through it. Cost on the S3: app image +62.5 KB (2 568 464 → 2 630 992 B: code +55.5 KB, read-only
data +7.0 KB, of which the web page +2.7 KB, now 25.5 KB gzipped), no static
RAM; heap about 1.2 KB per rule and as much per trace with its waiting run
(nothing without rules); a hub with less than 48 KiB of free data heap takes
no new rules.

**RAM.** Until hub 0.1.3 the hwlab board's S3 ran **without PSRAM** (its
N4R8 module has 8 MB of octal PSRAM, which the quad-only builds did not
enable), so the application lived in ~295 KB of internal heap. On the first hardware run
(2026-11-20, 4 MB dev table) it ran out of memory within a second: the
registry's deflate compressor (~250 KB) and the decoded example catalog
(~90 KB). Now the registry stores plain JSON (deflated values of earlier
builds are still read), the example catalog loads only with PSRAM, the app
thread has 40 KiB of stack and the hub-mode UART rings are 4/2 KiB. In hub
mode, after the C6 sensor was imported and interviewed: ~106 KB internal
heap free (minimum 103 KB), Wi-Fi not yet started. PSRAM support is on
(`CONFIG_SPIRAM`, ignored when absent). ESP-IDF fixes the PSRAM mode at build
time, so from 0.1.4 every layout has two images: quad (`…-app.bin`, N4R2/N8R2,
S3FH4R2, or no PSRAM) and octal (`…-opi-app.bin`, N4R8/N8R8/N16R8, the lab
board); the web flasher picks by the chip's embedded PSRAM, a wrong pick boots
without PSRAM. In 0.1.3 a board *with* PSRAM ran out of internal RAM (N4R2:
4 KB free a minute after boot, every maintenance request refused): `malloc`
keeps requests below `CONFIG_SPIRAM_MALLOC_ALWAYSINTERNAL` internal, and the
hub's data are many small Rust allocations, while 2 MB of PSRAM stayed empty.
From 0.1.4 Rust allocations prefer PSRAM (`firmware/espapp/src/alloc.rs`; C
mallocs above 1 KiB and Wi-Fi/lwIP buffers too; thread stacks stay internal).
On PSRAM the S32C1I instruction behind every atomic read-modify-write is
not atomic between the two cores (ESP-IDF builds its C code without it,
`CONFIG_STDATOMIC_S32C1I_SPIRAM_WORKAROUND`). Rust code used it for every
`Arc` count, channel slot and lazily created `Mutex` in that heap; cross-core
races there corrupt memory, which fits the crashes after 40–80
`network/diagnostics` in a row (a zeroed channel `Sender`, failed mutex
unlocks, cache and MMU faults). The hub is now built with `-C target-feature=-s32c1i`
(`firmware/hub/.cargo/config.toml`): Rust atomics call ESP-IDF's `__atomic_*`
functions, which lock for PSRAM addresses (+2.6 KB image, +1 KB IRAM).
`hub atomics` on the console counts lost updates of adds from both cores.
Hardware (2026-11-24, lab Thread BR board, ESP32-S3 N4R8 with 8 MB octal
PSRAM, 4 MB dev layout, image af13d65 with the fix): 1,000 consecutive
`network/diagnostics` calls over the WebSocket and USB without a crash,
where the 0.2.0 image crashed after 40–80; `hub atomics` on the same board:
Rust `AtomicU32` on PSRAM 200,000 of 200,000 increments, a bare S32C1I
loop on PSRAM lost 15,562 of 200,000 (another run 3,268), on internal RAM
none.
Measured on an ESP32-S3 SuperMini (S3FH4R2, 2 MB quad) in hub mode without an
NCP, Wi-Fi up: 0.1.3 8.4 KB internal free; 0.1.4 120 KB internal free
(minimum 119 KB while installing the whole catalog), PSRAM 160 KB used. For the next
crash, `info` shows the previous boot's start-up stage, free heap per stage,
the last stack/heap watch, the last Rust panic message and the crash reset
reason (RTC memory; hub-mode logs go to UART0, which the lab host cannot
see); `hub stacks` shows the live picture. In 0.1.9 the main task ends
after start-up (its 8 KiB stack returns to the heap) and lwIP has no IPv6:
+13 KB internal heap. Measured on the lab board (2026-11-23, Wi-Fi, two
devices, one API session): octal build 117.8 → 131.0 KB free (minimum
113.0 → 125.9 KB, largest block 45 → 55 KB); the quad build on the same
board, i.e. without PSRAM, 23.5 → 36.5 KB free after boot (minimum 17.3 →
30.0 KB). Without PSRAM a catalog search (`definitions/list` with a query)
made 0.1.8 fail a 10.5 KB allocation and restart; 0.1.9 answers it (minimum
5 KB free while searching, backup and energy scan included).

With Wi-Fi (2026-11-21): the default Wi-Fi configuration left 14–22 KB of
internal heap, so the hub build uses fewer Wi-Fi buffers (4 static RX,
8 dynamic RX/TX, no A-MPDU) and keeps Wi-Fi, FreeRTOS and heap code in
flash (`firmware/hub/sdkconfig.defaults`): with Wi-Fi, the HTTP server and
a WebSocket session up, **~65 KB free (minimum 57 KB)**. `ws://<ip>/api` from
the Mac: `devices/list` (the C6), `entities/states` with live temperature
and humidity, `network/permit_join` — all answered. Found on the way: the
first WebSocket connection deadlocked the app, because the app sent the
`auth_required` frame through esp_http_server's detached sender (which
needs the server thread) while that thread waited in the handler for the
app's reply. `App::connect` now never waits for the app thread (the session
ID comes from a shared counter), since any blocking send of the app (events,
keepalive pings) can meet the server thread in a handler; sessions of
closed connections are dropped on a Close frame, when the sender reports
the socket closed, or when the 30 s WebSocket ping fails. The `/api`
endpoint is registered on raw esp_http_server calls (`firmware/espapp/src/ws.rs`)
with control frames passed to the handler: with esp-idf-svc's `ws_handler`
(0.53) esp_http_server leaves the rest of a client PONG unread, the next
frame is parsed from inside it: the connection drops, or esp-idf-svc panics
on a reserved opcode — a client answering the hub's ping (browsers, the HA
integration) could crash the hub. Wi-Fi modem sleep (the ESP-IDF default)
is off: the hub is mains powered, and with it frames to the hub waited for
the next DTIM beacon — ping from the Mac avg 91 ms (max 334 ms), `hub/info`
round trip median 144 ms / p90 456 ms; with `WIFI_PS_NONE` ping avg 23 ms
(max 90 ms), `hub/info` median 68 ms / p90 134 ms. API frames are queued to
the server thread without waiting (a client that stopped reading held up
every other client for 1–2 s; now it is closed when 32 frames behind or
after a 2 s blocked send), WebSocket sockets use `TCP_NODELAY`, and page
responses close their socket (idle browser keep-alive sockets made the
server purge the least recently used socket — the Home Assistant
WebSocket — at its limit, now 7). Then: WebSocket ping/pong median 27 ms,
`hub/info` median 57 ms / p90 73 ms (the Mac's own Wi-Fi: ~17 ms to the
access point). PTA: the H2 NCP image does not implement it (no `PTA_COEX`
capability), so `coex_enable` is not sent; Wi-Fi is on channel 11, Zigbee
on 17 (no overlap). On macOS, Local Network privacy
may block an unsigned `hubctl` from reaching LAN addresses ("No route to
host" while `curl` works): allow it in System Settings, or relay through
`nc` on localhost.

**Recorded release 0.2.0 sizes.** The application targets the 8 MB production layout (3 MiB
slots; images with the MQTT bridge, feature `mqtt`) and the 4 MB dev layout
(2 944 KiB; without it). `tools/build-hub-firmware.sh` builds all four
images (quad and octal PSRAM).

| Build (hub 0.2.0) | App image | 3 MiB slot | 2 944 KiB dev slot |
|---|---:|---:|---:|
| 8 MB image: `tbr-hub --features mqtt` (octal: +1 168 B) | 2 858 192 B | 91 % | — |
| 4 MB dev image: `tbr-hub` (application, default; octal: +1 184 B) | 2 618 688 B | — | 87 % |
| `tbr-hub --no-default-features` (ZRHP host core only) | 543 168 B | 17 % | 18 % |

Over 0.1.10 (2 488 544 B) the 4 MB image grew 130 KB: about 179 KB for
time, last values, the ZCL console, automations and groups, less 49 KB of
silent ESP-IDF asserts; the MQTT bridge with esp-mqtt adds 240 KB in the
8 MB images.

`tools/size-report.py` breaks an ELF down by owner. Application build:
Rust code 1.49 MB (`hub-devices` 432 KB, `core` 232 KB, `alloc` 151 KB, `std`
148 KB, `zdef` 113 KB, `hub-app` 108 KB, `hub-ncp` 93 KB, serde and
`serde_json` 76 KB, `ed25519-compact` 17 KB), C 0.61 MB (Wi-Fi libraries
175 KB, mbedTLS 56 KB, `wpa_supplicant` 54 KB, lwIP 51 KB, newlib 29 KB,
httpd and its parser 22 KB, TinyUSB 13 KB), and 0.43 MB of constants and
literal pools no symbol covers (mostly Rust strings and tables). The largest
single items are the thread bodies (101 KB), the API request parser (serde,
internally tagged: 66 KB), `ApiServer::dispatch` (53 KB) and 11 copies of
std's stable sort (45 KB, most from `BTreeMap`/`BTreeSet` `collect()`).

Size work: fat LTO at opt-level `z` (both firmwares; `s` is 81 KB larger),
std without backtrace symbolisation (−138 KB, panic messages kept), the
smaller `zdef` (zdef-size), and in 0.1.9 the C printf family from ROM
(newlib "nano", −27 KB; only ESP-IDF's own UART0 log lines use it), no IPv6
in lwIP (the hub never had an IPv6 address; −20 KB) and the minified page,
together −55 KB (2 568 416 B before); in 0.1.10 the `zdef` encoders and the
fingerprint order their few keys with `zdef::small_sort` (a stable
insertion sort, same order) instead of 8 copies of std's sort (−25 KB). Not used: std's `optimize_for_size`
(−63 KB of sort code, but its stable sort is inlined into each caller with a
4 KiB stack buffer: the app thread needed 13 KB more stack, and two
start-ups crashed on the hardware), `panic_immediate_abort` (~250 KB, no panic
messages) and `-Zlocation-detail=none` (−57 KB, panics without file and
line). ESP-IDF's C `assert` is silent
(`CONFIG_COMPILER_OPTIMIZATION_ASSERTIONS_SILENT`: −49 KB flash, −5 KB
internal RAM): a failed one aborts at its PC without the expression and
file; the UART0 backtrace and the core dump still locate it with the ELF.
Rust panics keep their messages.

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

## MQTT (Zigbee2MQTT-compatible)

The hub can publish its devices on an MQTT broker with **Zigbee2MQTT's
topics and payloads**, so tools built for Zigbee2MQTT work with it:
Node-RED (`node-red-contrib-zigbee2mqtt`), ioBroker, openHAB, Home
Assistant's MQTT discovery, scripts. `crates/hub-mqtt` is a client of the
hub's own WebSocket API (an in-process session), so it sees what the web
UI and the HA integration see.

- **Plain MQTT 3.1.1 over TCP, no TLS** (the hub does no TLS; esp-mqtt's
  SSL and WebSocket transports are compiled out): use a broker on your LAN
  (mosquitto, the HA add-on …), optionally with a user name and password.
- **Settings**: web UI → Settings → MQTT, or the API (`mqtt/config/get`,
  `mqtt/config/set`, `mqtt/status`; device-support 14.10); stored in NVS.
  Off until enabled. Broker host/port, client ID (default
  `zigbee-rs-hub-<6 hex of the coordinator IEEE>`), credentials, base
  topic (`zigbee2mqtt`), keepalive, HA discovery and its prefix
  (`homeassistant`), retained states/availability, QoS.
- **Anonymous or with a login**: empty user name and password fields
  connect anonymously (the CONNECT carries neither; a password without a
  user name is not sent). Many brokers refuse anonymous clients:
  Mosquitto 2.x unless `allow_anonymous true`, and **Home Assistant's
  Mosquitto add-on always** — its log then shows `received null username
  or password for unpwd check` and `… disconnected: not authorised.` For
  the add-on, create a Home Assistant user (Settings → People → Users;
  not `homeassistant` or `addons`) or an entry in the add-on's `logins`,
  and enter it in the hub. `mqtt/status` `last_error` says why a
  connection failed: `connection refused: not authorized` (CONNACK 5),
  `connection refused: bad user name or password` (CONNACK 4), `the broker
  closed the connection after CONNECT (it may require a user name and
  password)` (closed without a CONNACK); the web UI adds a hint when an
  anonymous client is refused. Checked on the lab S3 (main 21354727): an
  anonymous connect to the Home Assistant add-on shows `connection
  refused: not authorized`.
- **Topics**: `<base>/<friendly_name>` (JSON state, keys = Zigbee2MQTT
  expose names: `temperature`, `humidity`, `battery`, `voltage` (mV),
  `state` `ON`/`OFF`, `brightness` 0–254, `color_temp` (mired), `color`,
  `action`, `linkquality` …), `…/set` and `…/get` (also `…/set/<property>`
  and endpoint forms like `state_l1` or `<name>/l1/set`),
  `…/availability` (`{"state":"online"}`), `bridge/state` (retained,
  LWT `offline`), `bridge/info`, `bridge/devices` (with `definition` and
  `exposes`), `bridge/event` (joined, interview, leave), `bridge/logging`,
  `bridge/request/permit_join`, `…/device/rename`, `…/device/remove`,
  `…/health_check` with `bridge/response/…` and transaction IDs. Groups:
  later.
- **Friendly names** are the hub device names (unique-suffixed if two are
  equal); renaming anywhere (MQTT, web UI, HA) clears the old retained
  topics, like Zigbee2MQTT.
- **Home Assistant**: use **either** the hub's integration
  ([ha-zigbee-rs-hub](https://github.com/faronov/ha-zigbee-rs-hub)) **or**
  MQTT discovery (opt-in), not both
  — each creates the same devices. Discovery follows Zigbee2MQTT
  (`unique_id` `<ieee>_<object>_<base>`, availability via `bridge/state`
  and the device topic).
- **Robustness**: reconnect with backoff (1–60 s), everything republished
  after a reconnect or an HA restart (`homeassistant/status`), outgoing
  messages rendered lazily into a byte-bounded queue (12 KB without PSRAM),
  `bridge/devices` without exposes beyond 16 KB on boards without PSRAM,
  which start the bridge only with at least 64 KB of free internal heap
  (`mqtt/status` state `low_memory` until then).
- **Desktop**: `hubctl --sim --sim-z2m-devices serve --mqtt HOST[:PORT]
  [--mqtt-discovery] [--mqtt-base T] [--mqtt-user U --mqtt-password P]
  [--mqtt-settings FILE]` runs the bridge with four simulated devices;
  `hubctl mqtt-broker` is a small broker for tests (not for production).
- **Firmware**: both **4 MB and 8 MB images** select cargo feature `mqtt`
  through `tools/build-hub-firmware.sh`; the release manifest lists it
  for each image. Builds without it answer `mqtt/*` with `unsupported`.
  Release 0.2.0 included MQTT only in 8 MB images.
  `mqtt` uses ESP-IDF's esp-mqtt; `mqtt-lite` (an option, in no standard image)
  the bridge's own MQTT client on lwIP sockets (esp-mqtt links esp-tls and
  the mbedtls TLS layer even for plain TCP; −62 KB). Size: +240 KB (the
  bridge ~180 KB, esp-mqtt with esp-tls/mbedtls ~62 KB). Static RAM +1.5 KB. At run time, only when
  enabled: the bridge thread (16 KB stack), esp-mqtt's task (5 KB stack)
  with 2 KB of buffers and up to 8 KB of QoS 1 outbox, the queue (≤ 12 KB
  without PSRAM), the device model (3.4 KB per device measured on a 64-bit
  host, `crates/hub-mqtt/tests/heap.rs`; less on the S3) and, briefly, the
  API snapshot the bridge parses one record at a time.

## Device firmware updates (Zigbee OTA)

Draft (branch `zigbee-ota-v2`, [docs/ota.md](docs/ota.md), API in
device-support 14.11): the hub is the OTA Upgrade server (cluster 0x0019)
of its network and downloads nothing itself. Two image sources:
**streaming**: a connected client — Home Assistant (preferred), a browser
tab (**Updates** on a build without the store, "keep this tab open"),
`hubctl ota-stream` — offers a file it holds and answers
`ota/block_needed` with `ota/block_data`; the hub keeps a 4 KiB window per
stream (at most 4 streams with PSRAM, `Resources::ota_streams`), answers
the device WAIT_FOR_DATA while a chunk is on its way and ends the transfer
cleanly when the client goes away (the device retries later). **Image
store** (8 MB images only, feature `ota-store`): uploads (web UI, Home
Assistant, `hubctl ota-upload`; resumable, `replace` at begin, checked in
flash: header, sub-elements, SHA-256) go to the **inactive app slot**
(3 MiB) — locked against hub self-updates both ways, lost when the hub
updates itself, re-checked at boot. **Zigbee OTA is in the 8 MB images
only** (feature `ota`, implied by `ota-store`): the 4 MB images are built
without it (`ota/*` answers `unsupported`, the hub endpoint does not list
0x0019 — an endpoint stored by an older image is registered again at
start — and a device's Query Next Image gets only the Default Response it
asks for, as before the server). The
Koenkk/zigbee-OTA index is fetched by Home Assistant (the web page has no
index search; the page budget is 36 KiB gzip, the Updates module's 3 KiB). Per device: policy
`manual` (default), `auto` (schedule window) or `off` (never: `ota/start`
refused, a running job aborted; decided); Image
Notify (sleepy devices through the queue); blocks of min(device, 48 B,
NCP `max_asdu` − 17); page requests answered one block at a time (at most
128, decided; sharing the pacing slots with the other devices); pacing; at most 2
downloads at once; abort (an aborted or unapproved Upgrade End gets
ABORT, never "install now"); progress events; resumable after a hub
restart. Home Assistant: one `update` entity per device (upload to the
store or streaming). Host tests: simulated OTA client behind the
simulated NCP, Home Assistant end to end with `hubctl --sim --sim-ota`.
Hardware (octal 4 MB lab board, lab build with `ota` of this branch
at 6efb7ab): the zigbee-rs C6 updated twice by streaming, with the client
away and a hub restart mid-download, no panic, internal heap flat
(results in [docs/ota.md](docs/ota.md#hardware-results)); not yet on
hardware: the 8 MB store. App image vs main (3c777d2, quad PSRAM, CI):
8 MB (`mqtt,advanced,ota-store`) 2 933 504 B, +131 232 B, 93.3 % of the
3 MiB slot; 4 MB (`mqtt`, no Zigbee OTA) 2 785 520 B, +3 632 B (generic
plumbing kept in every image: app clusters and their outcomes, events
serialized once), 92.4 % of the 2 944 KiB slot; no static RAM. Open: hub self-OTA must take the
app-slot lock. zigbee-rs devices advertise OTA manufacturer code 0x6D6B
(devices on older firmware: one transition file with 0x1234).

## Home Assistant integration

The custom integration `zigbee_rs_hub` has its own repository,
**[faronov/ha-zigbee-rs-hub](https://github.com/faronov/ha-zigbee-rs-hub)**
(up to its 0.6.0 it was `integrations/home-assistant` here), and installs
and updates through HACS: HACS › ⋮ › *Custom repositories*, add
`https://github.com/faronov/ha-zigbee-rs-hub` with the type *Integration*,
download *zigbee-rs hub*, restart Home Assistant, then *Settings › Devices &
services › Add integration › zigbee-rs hub* (host, port `80`, the API token
from the hub console `token`). A copied install moves to HACS without losing
its config entry (same domain). Each hub release's notes name the
integration version to use; the integration's README has the compatibility
table, the installation and migration steps, tests (end to end against
`hubctl --sim serve` built from a hub release) and the API mapping.

In short: one persistent WebSocket connection to
`ws://<hub>/api` (token from the hub console `token`), a HA device per
Zigbee/GP device, entities created from the hub's entity model (device-support
section 4) with live state pushed by the hub, commands through
`entities/command`, permit join (button and service), device removal,
diagnostics. It identifies the hub by `hub/info` (`coordinator_ieee`, added for
it; minor API addition). Zigbee groups of the hub appear as devices with a
group light or switch and their scenes as HA scenes (device-support 14.9);
deleting a group device in HA deletes the group.

The integration is also a catalog client: since the hub downloads nothing,
Home Assistant fetches the signed release files from the mirror set on the
hub (`catalog/config` `base_url`, GitHub Pages by default) and uploads them
over the API in 4 KiB chunks, resuming after a dropped connection. Service
`zigbee_rs_hub.update_catalog` (optional `config_entry_id`, `force`,
`base_url`) runs it on demand; otherwise it runs every 24 h (first two
minutes after set-up) as the hub's `auto_update` / `auto_fetch` allow, and on
`definition_needed` events when `auto_fetch` is on. A **Catalog version**
sensor shows the installed bank.

## License

MIT OR Apache-2.0, like zigbee-rs.
