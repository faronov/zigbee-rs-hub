# Firmware profiles, coordinator backends and feature matrix

**Status: implementation plan and support matrix, not a release announcement.**
The only implemented coordinator backend is ZRHP (zigbee-rs on the Thread
BR's ESP32-H2). EZSP and ZNP images do not exist yet; their rows below are
requirements, not support claims.

Published **hub-v0.2.0** predates the image variants described here: its
4 MB images have no MQTT and include the ZCL console. Do not infer the
contents of that release from the next-release defaults below.

## 1. Goals and boundaries

Keep the hub application independent of the radio's vendor and firmware:

```text
device/entity engine, definitions, automations, WebSocket/HA, MQTT
                 |
       coordinator capabilities + events
          /             |              \
        ZRHP          EZSP/ASH         ZNP/MT
          |             |              |
      zigbee-rs      EmberZNet       TI Z-Stack
```

Four choices are independent:

| Choice | Examples | Owns |
|---|---|---|
| Host board | Thread BR, S3 with an external NCP, Dongle-M | ESP target, wiring, reset/boot lines, network interfaces |
| Host memory | 4/8/16 MB flash; Quad or Octal PSRAM of 2–16 MB (required by the application) | partitions, application options, buffer limits |
| Coordinator backend | ZRHP, EZSP, ZNP | framing, commands, callbacks, network lifecycle |
| Radio update method | ZRHP `FW_*`, Gecko/XMODEM, TI ROM bootloader | target checks, safe programming and recovery |

A radio chip does not uniquely select a backend. An MG24 with EmberZNet
speaks EZSP; an MG24 with a future zigbee-rs NCP speaks ZRHP. Its update
method can remain Gecko in both cases. The onboard H2 cannot run TI's or
Silicon Labs' NCP firmware.

Every image, at every flash size, compiles **one backend**. There is no
automatic protocol probing that resets an unknown coordinator. Do not fork
the device engine or scatter vendor checks through the UI.

## 2. Target hardware and readiness

Identifiers in this table are descriptive planning names, not Cargo
features. Order of work: ZRHP (existing), then EZSP, then ZNP.

| Target | Host | Radio firmware / backend | Built-in radio update | Current state / release blocker |
|---|---|---|---|---|
| Thread BR + H2, product | ESP32-S3, 8 MB flash (retail boards), PSRAM per module | zigbee-rs / ZRHP | H2 `FW_*`, trial boot and A/B rollback | Released (8 MB images) |
| Thread BR + H2, early sample | ESP32-S3, 4 MB flash: development layout, one app slot, no hub OTA | zigbee-rs / ZRHP | as above | Released as `…-4mb-dev` (0.3.0: MQTT, no console). Octal image: Zigbee on the N4R8 lab board. Quad image (main 002658d, 2,797,888 B): ran on a bare S3 devkit with 4 MB flash and 2 MB Quad PSRAM, no radio (in-place upgrade from 0.1.3 with `hubdata`, Wi-Fi and token kept; PSRAM detected; web page and WebSocket API under 4 parallel clients; internal heap min 121,351 B; clean restart, no crash). Zigbee on a 4 MB board still ran only on the N4R8 lab board |
| S3 + MG24 | ESP32-S3; actual board/memory to be identified | EmberZNet NCP / EZSP-ASH | Gecko bootloader / XMODEM | **First external backend.** Not implemented; UART and reset/boot wiring unspecified |
| S3 + CC2652P7 | ESP32-S3; actual board/memory to be identified | TI Z-Stack ZNP / ZNP-MT | TI ROM serial bootloader | After EZSP. Not implemented; updater blocked until reset and bootloader-backdoor lines are specified |
| Dongle-M + MG24, stock NCP | Classic ESP32-D0WDR2-V3, 16 MB flash / 2 MB PSRAM | EmberZNet NCP / EZSP-ASH | Gecko bootloader / XMODEM (from the ESP32, not yet on hardware) | Hub image `zrh-donglem` (experimental asset of the hub releases since 0.3.0): the app on `Backend::ezsp`, data on SONOFF's `files` after a claim, self-update; SONOFF's stock partition table kept (install through SONOFF's web console). Runs on one lab unit (network formed, a sleepy device joined, interviewed and reporting; network backup export and same-network restore); the rest of its HIL checklist pending (`docs/dongle-max.md`) |
| Dongle-M + MG24, zigbee-rs NCP | Same Dongle-M host | zigbee-rs / ZRHP | Gecko bootloader / XMODEM, not H2 `FW_*` | Hub side in `zrh-donglem` (host-tested): the ZRHP backend chosen at boot (`auto` asks the MG24), the GBL flashed from the ESP32 with a kept backup and the network restored after the switch (`docs/dongle-max.md`, "Replacing the MG24 firmware"). The NCP's 38.4 MHz radio for the Dongle-M is in zigbee-rs PR #21 (in review); no hardware run yet |

PSRAM: Thread BR modules seen so far are N4R8 (lab, 8 MB Octal), N4R2
(2 MB Quad) and, at retail, N8R2/N8R8. Quad/Octal is the interface; the
image must match it, the size does not matter. Every image detects PSRAM
at boot (section 4).

The Dongle-M pins are `hub_core::profile::DONGLE_M` (SONOFF's published
pinout; `docs/dongle-max.md`). The `donglem` branch also contains an S3
SuperMini + RF-BM-MG24B1 board profile, not ported: prior work to
reconcile, not permission to assume that wiring for every external S3/MG24
assembly.

Before creating flashable external-NCP images, record:

- exact ESP board/module, flash and PSRAM interface;
- exact radio module and crystal/firmware variant;
- UART TX/RX, RTS/CTS or an explicitly qualified no-flow-control mode;
- reset and boot-entry GPIOs, polarities and electrical ownership;
- installed bootloader, its configuration, protected nonvolatile regions
  and recovery path.

The first supported EZSP and ZNP firmware versions must be pinned and
reported in this table before a support claim. A protocol name alone is
not a compatibility range.

## 3. Features and first-stage acceptance

Legend: **existing** is implemented in the ZRHP path; **partial** exists
with the stated gap; **required** is a first-stage requirement for a new
backend; **later** is intentionally not part of its first stage. Required
does not mean implemented.

| Feature | ZRHP | EZSP first stage | ZNP first stage |
|---|---|---|---|
| Identify NCP, query network and report capabilities | Existing | Required | Required |
| Create a network on an explicit request | Existing | Required | Required |
| Resume/adopt a compatible existing network without erasing it (below) | Existing | Required | Required |
| Permit join, interview, endpoint registration | Existing | Required | Required |
| ZCL reads/writes, normal commands, reports and Tuya definitions | Existing | Required | Required |
| Request outcome: accepted / delivered or unknown / ZCL response | Partial: failures and ZCL responses, no positive delivery | Required | Required |
| Groups, bindings and normal scene commands | Existing | Required | Required |
| NCP restart, request cancellation and unknown delivery outcomes | Existing | Required | Required |
| Read-only network backup export (Open Coordinator Backup) | Existing | Required, before the updater | Required, before the updater |
| Built-in NCP firmware updater | Existing H2 path | **Required in first stage** | **Required in first stage**; blocked on wiring |
| Backup restore within the same backend | Existing | Later, after the core stage | Later, after the core stage |
| Migration of an existing network between different stacks | No general claim | Separate project | Separate project |
| Import of a Zigbee2MQTT or ZHA network with its device records (`network/import`, [migration.md](migration.md)) | Existing (host-tested; IEEE override by the NCP) | Existing (host-tested; IEEE through the token rules of the restore) | — |
| Green Power sink/proxy operations | Existing ZRHP capabilities | Later / capability-gated | Later / capability-gated |
| Vendor-specific tables, diagnostics and tuning | Existing where exposed | Capability-gated | Capability-gated |

**Capacity** is part of each backend's capabilities and is shown before a
user picks an image: child table, address table, source routes, multicast
(group) table, APS in-flight requests. Values come from the pinned
firmware's configuration (e.g. `EZSP_CONFIG_*`, Z-Stack NV items), not
from the protocol.

**Compatible existing network**: the NCP reports a formed network whose
identity (extended PAN ID, plus the network-key hash when both sides know
it) is known to the hub's store, or the operator explicitly adopts it. PAN
ID, channel and key sequence are informative: PAN ID conflict resolution,
channel changes and key rotation rewrite them. Adopting a network
without its previous controller's device database is supported only with
rediscovery: NCP child/address tables, Device Announce and traffic. Sleepy
devices may take hours to reappear; the UI says so.

Host-side features stay common: device definitions, the entity model,
local automations, WebSocket/HA and MQTT. They must not imply that every
backend supports every radio operation. Unsupported operations return an
explicit error and are not advertised by the API/UI.

### Host image options

| Option | Standard 4 MB BR | Standard 8 MB BR | Dongle Max 16 MB (`docs/dongle-max.md`) |
|---|---|---|---|
| ESP-IDF esp-mqtt | Included; runtime opt-in | Included; runtime opt-in | Included, with HA discovery |
| Manual ZCL/Tuya console and live frame tap | Off; `advanced` opt-in | On | On |
| Normal ZCL/Tuya/GP device support | Kept | Kept | Shared engine; radio capabilities still apply |
| Local automations, groups/scenes/bindings | Kept | Kept | Kept (shared engine) |
| Signed catalog and user definitions | Compact catalog/storage areas | Larger areas | The 8 MB areas, in SONOFF's `files` region once claimed |
| Hub application A/B slots | No: existing single-slot layout | Yes | Yes: `ota_0`/`ota_1` of SONOFF's table (6 MiB each); the first self-update replaces SONOFF's application |
| Zigbee device OTA server (`ota`; branch `zigbee-ota-v2`) | **Not included** (it would fill the slot to 95 %); `ota/*` answers `unsupported`, the hub endpoint does not advertise 0x0019 | Included: streamed images and the image store in the inactive app slot (`ota-store`) | Yes: 1 MiB store in the claimed `files` region, larger images streamed; separate OTA qualification |
| Wi-Fi | Open and WPA2/WPA3-Personal | same | same, as the fallback of Ethernet; no Bluetooth |

The `advanced` flag does not control normal Tuya datapoint decoding or
entity commands. Host application OTA, Zigbee device OTA and NCP firmware
updates are three different operations.

### Network ownership and the hub's store

Two ownership models exist and the backend boundary names them:

- **ZRHP**: the hub's store holds the owned network (key, counters
  checkpoint) and restores it onto a blank or replaced NCP
  (`hub_ncp::lifecycle::Ownership`).
- **EZSP/ZNP**: the NCP's NVM3/NV is authoritative for the network; the
  hub keeps device metadata and application settings, plus the read-only
  backup export.

`hubdata` survives reflashing the ESP. Its store is tagged with the backend
and the network identity (extended PAN ID and a network-key hash). An image
of another backend, or an NCP with another network, does not use a foreign
registry without an explicit operator action.

### NCP firmware updater safety

The built-in updater is a release gate for the first EZSP/ZNP stage. The
H2 path is safe because it writes the NCP's inactive A/B slot. The Gecko
standalone bootloader and the TI ROM bootloader write the single
application in place, and the 4 MB layout has no room to stage an image on
the ESP, so the update streams and an interrupted one leaves the NCP in its
bootloader. Requirements:

- **export first**: a read-only network backup (keys, identity, counters,
  link keys) is written and offered for download before any write;
- **allow-list manifest**: each update names the source firmware
  (version/hash) and target image it applies to, the protected NV/NVM3
  regions and the allowed write ranges; anything else is refused;
- **protocol bounds**: only targets whose EZSP/MT version the backend's
  codec speaks; no downgrade without an explicit operator action;
- **refuse** encrypted GBLs, GBLs carrying bootloader or Secure Engine
  upgrades, and images touching NV pages or the CCFG/backdoor settings
  beyond the manifest;
- **persisted state**: the update phase is stored on the ESP; at boot the
  hub checks for an NCP waiting in its bootloader before it declares the
  NCP dead or forms a network, and resumes or reports the update;
- bounded attempts, verification of the written image and of the running
  NCP's identity/version, and qualification of power loss in every phase.

EZSP can enter the bootloader with `launchStandaloneBootloader`; recovery
after a partial write depends on the installed Gecko bootloader's
configuration, which is recorded per supported firmware. The TI ROM
bootloader needs reset and backdoor lines and a check of the image's CCFG;
until they are specified the ZNP updater stays blocked. Radio-firmware
conversion that cannot preserve the current network is a separate,
explicit operator action, not an ordinary update.

## 4. Architecture, builds and budgets

The current `ZigbeeTransport` abstraction is useful but insufficient:
`hub-app` administration, registry, last values and catalog settings also
depend on `hub_ncp::Hub` and its `with_store` operations, and `hub-app`
depends on `hub-ncp`/`zigbee-zrhp` unconditionally (about 109 KB an
EZSP/ZNP image must not carry).

The extraction separates:

1. **Hub-owned storage**: registry, definitions, automations, user settings
   and last values, independent of the wire protocol, tagged as above.
2. **Coordinator operations/events**: normalized network identity, endpoint
   registration, APS requests/confirmations/indications, joins/departures,
   explicit capabilities (in-flight capacity, maximum ASDU, groups,
   coexistence, capacity limits).
3. **Protocol-specific lifecycle and storage**: ZRHP resume/table epochs,
   EZSP policies/security and ZNP configuration/NV remain in their adapters,
   each behind a Cargo feature.
4. **Update mechanism**: selected by board and radio firmware capabilities,
   not by assuming that every ZRHP NCP is an H2.

Operations on the app's owning thread never wait for a serial response or
a worker: every request is a ticket with a deadline, completed by an event.
Errors are normalized (`unsupported`, `busy`, `link_down`, `no_ack`,
`timeout`, `delivery_unknown`, backend code) instead of ZRHP status bytes.

### Commands available now

These commands build only the existing Thread BR/ZRHP implementation:

```sh
RUSTUP_TOOLCHAIN=esp tools/build-hub-firmware.sh --layout 4mb --locked
RUSTUP_TOOLCHAIN=esp tools/build-hub-firmware.sh --layout 4mb --psram octal --locked
RUSTUP_TOOLCHAIN=esp tools/build-hub-firmware.sh --layout 4mb --locked -- --features advanced
```

There are no EZSP/ZNP build commands yet. Add them only with actual
implementations and validated board mappings; a profile that merely
returns `unsupported` for its core operations is not a backend.

The 4 MB variant keeps `partitions-4mb-dev.csv`: application limit
**3,014,656 bytes**, 1 MiB `hubdata`. With esp-mqtt and without the
advanced console it measures 2,754,560 B (Quad) / 2,755,728 B (Octal),
about 259 KB of headroom (the hand-written API request parser saved
93,760 B against the derived one). An EZSP or ZNP image replaces `hub-ncp`
and `zigbee-zrhp` (about 109 KB), so size is not on the backends' critical
path. These are build results, not hardware results.

Release 0.4.2 (CI dry run, quad / octal): 4 MB 2,966,656 / 2,967,824 B
(98.4 % of the 2,944 KiB slot, ~46 KiB left); 8 MB 3,117,344 /
3,118,512 B and its developer edition 3,118,032 / 3,119,216 B (99.2 % of
the 3 MiB slot, ~26 KiB left); `zrh-donglem` about 3.5 MB (55 % of its
6 MiB slot). Release 0.4.1: 4 MB 2,934,864 B (~78 KiB left), 8 MB
developer edition 3,087,456 B (~57 KiB left), so 0.4.2 added about
32 KB to every S3 image. The 8 MB headroom is the tightest: the size pass
must come before the next feature lands.

Every image detects PSRAM at boot. Without it the application is not
started (hardware run on the N4R8 lab board with a Quad image: ~35 KB of
internal heap left, crash once clients connected); the hub core keeps the
Zigbee network and the web page, `/api` (503 `psram_missing`) and the
console name the image to flash. An internal-RAM application mode needs
about 100 KB less memory and is future work. A subscription snapshot is
sent as fragments of one WebSocket message, each made after the earlier
ones are sent. Thread stacks and driver allocations always use internal
RAM.

For every candidate, record final app-image size, slot headroom, loaded
ELF sections, static IRAM/DRAM, task-stack reservations and runtime
heap/stack high-water separately. Keep identical paths, toolchains and
unrelated features when comparing sizes. Do not count debug ELF data or
NOLOAD flash address reservations as image bytes. SDK-level TLS removal is
not pursued: esp-tls is required by esp-mqtt, and a TLS client-only
configuration without Wi-Fi Enterprise saves only ~7 KB.

## 5. Delivery and qualification

1. Qualify the 4 MB variant on hardware (done on the N4R8 lab board with
   PSRAM: fragmented snapshots, five concurrent clients, heap stable;
   without PSRAM: the wrong-image mode; the Quad image on a 2 MB Quad-PSRAM
   S3 devkit without radio: upgrade in place, web and API under four
   parallel clients, heap stable). MQTT under load and Zigbee on a Quad
   board remain.
2. Extract the coordinator/storage boundary while keeping the existing
   ZRHP wire behavior, persistence and recovery unchanged; ZRHP adapter
   conformance tests. Design and PR sequence: `coordinator-boundary.md`.
3. Implement and host-test ASH/EZSP: core stage, read-only backup export,
   then the updater against explicitly supported firmware versions.
   Core stage: `crates/hub-ezsp`, host-tested; not wired, not hardware-tested.
4. ZNP/MT core may follow in parallel; its updater waits for the wiring.
5. Supply the external-board wiring and integrate actual ESP image targets,
   one backend per image.
6. Qualify join/interview/reporting/commands, groups, reboot/resume,
   existing-network preservation, reconnect storms and update recovery
   (power loss in every phase) on each physical combination.
7. Publish only qualified combinations. Keep MG24/ZRHP marked blocked until
   the zigbee-rs radio bring-up is complete.

Feature reports and release manifests distinguish **planned**,
**host-tested**, **build-tested**, **hardware-tested**, and **released**.
A passed image-size check is not a hardware or Zigbee conformance claim.

## 6. Quick answers

- *Which image fits an unmodified Thread BR?* A retail board (8 MB flash):
  the 8 MB image for its PSRAM interface. An early sample with 4 MB flash:
  the `4mb-dev` image. `esptool chip-id` shows flash size and embedded PSRAM.
- *Does MG24 mean EZSP?* No: stock EmberZNet speaks EZSP, a future
  zigbee-rs NCP speaks ZRHP; the Gecko bootloader updates both.
- *Can a 4 MB image include MQTT and omit the console?* Yes, that is the
  next standard 4 MB image.
- *What happens to an existing network at startup or update?* It is kept;
  an unknown network is reported, never cleared automatically, and an
  update first exports a backup.
- *What blocks the external TI/SL images?* The board wiring, pinned
  firmware versions and the backends themselves.
- *What blocks Dongle-M with ZRHP?* The zigbee-rs MG24 radio, independently
  of the classic-ESP32 host port.
