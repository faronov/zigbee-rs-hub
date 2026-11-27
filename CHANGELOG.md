# Changelog

User-facing changes of the hub firmware releases (`hub-v*` tags). The
release workflow (`.github/workflows/hub-release.yml`) puts the release's
section at the top of its GitHub release notes, above the generated
install instructions and the list of merged pull requests. A pre-release
`X.Y.Z-rc.N` uses its own section if there is one, else `X.Y.Z`'s.

## Unreleased

### New

- **Dongle Max: the MG24's firmware can be replaced from the hub**
  (experimental, not yet run on hardware). Settings › Zigbee radio
  firmware (MG24) uploads a GBL file and flashes it through the MG24's
  Gecko bootloader, from the ESP32, without USB: SONOFF's EmberZNet NCP by
  the zigbee-rs ZRHP NCP, and back. The image is checked first (never one
  that would write the bootloader, an encrypted or compressed one, or a
  damaged one); the network is backed up with its keys and kept on the hub
  before anything is written; after the flash the hub restarts on the new
  firmware and puts the network back, under the same coordinator IEEE, so
  the devices keep working without pairing again. A failed or interrupted
  flash leaves the MG24 in its bootloader, which is never written: the hub
  starts anyway, says so and flashes again. Images without an
  application (an NV clear) write data only, leave the MG24's application
  running and restore nothing: the kept network waits for the next
  application image. The way back to SONOFF's firmware (an NV clear,
  then SONOFF's GBL, with their checksums) is in `docs/dongle-max.md`. The
  zigbee-rs NCP needs its Dongle-M radio (38.4 MHz, in review).
- **Dongle Max: the hub runs on either NCP firmware.** The image carries
  both coordinator backends (EZSP and ZRHP); at boot it asks the MG24
  which firmware it runs (setting `auto`, the default; `ezsp` or `zrhp` to
  fix it). `hub` on the console and `ncp/update/status` show what answered.
- **Dongle Max: `mg24 ezsp info` shows the custom EUI-64 and CTUNE
  tokens** (bring-up boot): whether the coordinator IEEE is the chip's own,
  and the crystal tuning, before the firmware is replaced.
- `network/backup` takes `keep`: the backup is also kept on the hub as a
  replaced network's (`network/backup/replaced`), whatever backend runs
  later.
- **The zigbee-rs NCP's log in the hub's log** (ZRHP backend). The hub
  now lets the NCP send its log: the records it kept since its reset
  (crystal tuning, where its IEEE comes from, NV state) and the ones after
  go into the hub's log (`hub/logs`, the console) as `ncp`, up to info by
  default, rate-limited. `network/ncp_log` (and `ncp log on|off|level <n>`
  on the Dongle Max console) sets the level, kept across restarts;
  `network/diagnostics` shows it with its counters, and whether the NCP
  keeps an EmberZNet backup's hashed link key root (`ezsp_hashed_tclk`).
- zigbee-rs crates at master `0607d31` (zigbee-rs PR #22: restored
  EmberZNet devices are admitted by the zigbee-rs NCP's Trust Center;
  late LOG credit is delivered at once).

### Fixes

- **Dongle Max: the hub no longer crashes when it asks the MG24 for the
  zigbee-rs NCP.** The probe ran the ZRHP link driver on a stack smaller
  than the driver needs (its thread entry reserved 16 KB for code it never
  ran), so the check after flashing the zigbee-rs NCP, and every boot
  after it, crashed the hub. The driver's entry now takes 720 bytes and
  the probe runs it as the hub's own ZRHP backend does. A ZRHP probe or
  backend that crashes the hub at two boots in a row is now skipped: the
  hub starts without it (UART parked), says why, and stays reachable to
  flash another image; after 10 minutes without a crash the next boot
  tries ZRHP once more, and a hub update starts afresh.
- **Dongle Max: `mg24 bootloader` shows the whole menu.** The bootloader
  prints its menu by itself on entry; the wait for the menu after the
  newline stopped at that first copy and cut the second one short.

## 0.4.1 (2026-11-27)

A fix release for 0.4.0: an interview that ended partial finishes when the
device rejoins, backups restore routers as routers and no longer roll the
frame counters back on the Dongle Max, and the Dongle Max can take its
storage back after SONOFF's firmware reformatted it. Same boards, images
and web flasher entries as 0.4.0; an update keeps all data.

### Fixes

- **A partial interview resumes when the device rejoins.** A device whose
  interview ended `partial` (a descriptor or Basic read failed or timed out,
  e.g. a router busy right after its join) stayed so until
  `devices/reinterview` was called by hand. Now its next rejoin or
  Device_annce (the moment a sleepy device is awake) resumes the interview
  asking only the answers it lacks (e.g. just the Node descriptor of a
  device whose endpoints were read), so a failure then loses nothing it
  knew; the user's names and settings stay. Works on both coordinator
  backends. A device that keeps failing is retried at most 5 times per hub
  run, the first at once, then no sooner than 10, 20, 40 and 80 min after
  the previous try, however often it announces; a complete interview is
  never repeated. `devices/diagnostics` counts the resumptions
  (`interview_resumes`). A join and the Device_annce of the same rejoin no
  longer send each interview request twice, and an Active_EP_rsp with an
  error status now makes the interview partial (it was taken as a device
  without endpoints).
- **Dongle Max: a refused storage claim says why, and the hub's region
  can be claimed again after SONOFF's firmware reformatted it.** Every
  refusal of `storage/claim` (and of the first boot's claim) now has a
  code and a message, in the request's error and in `storage/status`
  `last` (now `{"code", "message"}`); 0.4.0 refused the claim after such
  a reformat without a word. That state (the hub's claim recorded in NVS,
  which every boot that finds the region the hub's now writes if it is
  missing; SONOFF's new file system over the hub's data; the claim's copy
  of SONOFF's files intact) is shown as `reformatted` in `storage/status`
  and in the RAM-only banner, and `storage/claim` with `"recover": true`
  (console `files recover confirm`; Settings › Storage asks for it)
  claims the region again, keeping the copy of SONOFF's original files
  for `stock confirm`. The hub restarts with an empty store, takes the
  MG24's network and imports its devices; the most recent hub backup
  restored onto it brings back its groups and devices (not names or
  automations: `docs/dongle-max.md`, "Recovery after SONOFF's firmware
  reformatted `files`"). Make the first update a self-update: the
  bootloader's fallback is then a hub image, not SONOFF's firmware.
- **EZSP: a backup restored over its own running network no longer rolls
  the frame counters back.** The restore started from the backup's NWK
  and APS frame counters plus 10 000; the coordinator sends thousands of
  frames a day, so a backup a day or two old made the devices drop its
  frames. It now starts from the NCP's own counters when they are higher
  (read before the network is left; note `floor_raised`), and a stored
  network whose counters the NCP does not report gets a margin of 2²⁰
  (note `counters_unknown`). A zigbee-rs NCP (ZRHP) already started above
  the counters it used.
- **EZSP: after a restore the NCP resumes above the frame counters used
  before it, and the report says where.** EmberZNet stores the counters a
  restore writes truncated to 4096 and resumed from there at the
  restore's reset (lab unit, 7.4.5: `nwk_fc_start` 96 058 reported,
  94 208 in use): up to 4095 below the intended margin, and below the
  counters the restored network had already sent (devices drop such
  frames). The start is now on a 4096 boundary and the NCP is reset a
  second time: the session after that reset starts above every counter
  used before (lab Dongle Max: start 110 592 written, the NCP resumed at
  118 785), and `nwk_fc_start`/`aps_fc_start` are read back then. The
  short session between the two resets may still reuse a few counters
  (its many-to-one route requests and link status only).
- **Backups restore routers as routers.** A zigbee-rs NCP (the Thread BR
  board's H2) lists a router that joined directly through the coordinator
  as its child, and `network/backup` wrote it so (`is_child`). Restoring
  that backup made the router a sleepy end-device child: the NCP held
  every frame to it for a poll a router never sends (seen on hardware: the
  interview hung, ZCL requests got no APS ACK, `devices/remove` never
  reached it). New backups write such a router as a plain device (as
  zigpy and Zigbee2MQTT mean `is_child`), and a restore or import of an
  older file gives no child entry to a device the hub knows as a router
  (its node descriptor) or the file lists as one (`type: "router"`), on
  both backends. End devices stay children.

### Upgrading from 0.4.0

- **Data:** an update keeps `nvs` (Wi-Fi, mode, API token) and the hub's
  data (`hubdata` on the S3, the claimed `files` region on the Dongle
  Max), as from 0.3.0 to 0.4.0. The web flasher has the same entries as
  for 0.4.0.
- **Make a new backup after the update** (Settings › Backup). A backup
  made by 0.4.0 or earlier on the Thread BR board lists routers that
  joined directly through the coordinator as children. 0.4.1 restores
  such a file correctly onto a hub that knows those devices as routers
  (e.g. the same hub, its devices interviewed); restored onto an empty
  hub, such a router would still become a sleepy child.
- **Frame counters on restore:** a restore starts the network's frame
  counters from the higher of the backup's and the NCP's own counters,
  plus a margin (10 000; 2²⁰ for a counter the NCP stores but does not
  report, note `counters_unknown`). The NCP's counters count on EZSP
  since 0.4.1 (note `floor_raised`); a zigbee-rs NCP already kept them.
  On EZSP the start is rounded up to a multiple of 4096 and the NCP
  resumes above it after the restore's second reset, so the reported
  `nwk_fc_start` is higher than with 0.4.0. An older backup restored
  onto the same network is therefore safe. A restore
  onto another radio cannot know what the old coordinator sent after the
  backup: take the backup last and do not run the old coordinator again.
- **SONOFF Dongle Max:** download a hub backup (Settings › Backup) before
  every update over USB or through SONOFF's web console, and prefer the
  hub's self-update (Settings › Hub firmware; the first one with *Replace
  SONOFF's firmware* ticked): the bootloader's fallback is then a hub
  image, not SONOFF's firmware, which would reformat the hub's region. If
  that happened (`storage/status` and the banner say `reformatted`):
  [Recovery after SONOFF's firmware reformatted `files`](https://github.com/faronov/zigbee-rs-hub/blob/hub-v0.4.1/docs/dongle-max.md#recovery-after-sonoffs-firmware-reformatted-files).
- **Home Assistant:** integration **0.6.0** as in 0.4.0 (unchanged).
  **tbr-bridge 0.2.6** as in 0.4.0 (unchanged, the same bytes).

### Tested on hardware

- Thread BR board (lab S3, zigbee-rs NCP on the H2): install, devices,
  `interview_resumes` in `devices/diagnostics`, a backup (an end device
  listed as a child) and a `network/restore` of a fresh backup.
- SONOFF Dongle Max (stock EmberZNet 7.4.5): the new `storage/status`
  fields, the claim refusals `confirm_required` and `already_claimed`; a
  restore over its own network: start 110 592 written, the NCP resumed at
  118 785 after the second reset (one 4096 step above the model, the safe
  side), its counter at or above the start 1 and 5 min later, a sleepy
  end device (C6) reporting and answering reads after it, a normal reboot
  resuming ahead.

### Known limitations

- The 8 MB images (and the OTA image store) have not run on an 8 MB board
  yet. The 4 MB octal image ran with Zigbee on the lab board; the 4 MB
  quad image ran on a 2 MB quad-PSRAM S3 devkit without a radio, so Zigbee
  on a quad-PSRAM board is still untested.
- Resuming a partial interview is tested on the simulated backends only
  (a failing interview step cannot be forced on hardware).
- Migration: host-tested on both simulated backends (ZRHP and EZSP) with
  Zigbee2MQTT and ZHA files. On hardware a Zigbee2MQTT-format import of
  the Dongle Max's own network ran (preview `same`, names applied, devices
  complete); an import onto another radio or network and a ZHA import
  have not. Green Power devices are not imported (commission them again);
  after a change of the coordinator IEEE some devices keep the old trust
  center address (prefer a radio that takes the backup's IEEE); Z2M/ZHA
  per-device settings and external converters are not imported
  (`docs/migration.md`, Limits).
- No hub A/B self-update on the 4 MB dev layout; no SoftAP setup (Wi-Fi
  from the console or the browser console).
- Definitions that need the vendor template `tuya` are not run yet
  (TS011F devices for which Zigbee2MQTT picks `TS011F_plug_2`/`_3`,
  multi-gang strips, DIN relays): they use `TS011F_plug_1`, marked
  **approx.**, and the device's diagnostics name Zigbee2MQTT's definition.
- An rx-on end device restored or imported from a backup becomes a
  sleepy child of the coordinator (the backup format has no capability).
- A zigbee-rs NCP (the Thread BR board's H2) may keep a stale child table
  entry for a device that moved to another network: backups list it, and
  a restore re-creates it as a partial device (remove it with
  `devices/remove`; a fix is tracked).
- SONOFF Dongle Max: experimental. Ran on the lab unit: the network,
  joining and interviews, reports, a network backup export and restore
  onto the same network (0.3.0; the joined sensor kept working), the hub
  name and version, a Zigbee2MQTT-format import of its own network, the
  0.4.1 items above. Not yet run: Ethernet, `stock confirm`, the
  self-update, installation through SONOFF's web console, a restore or
  import onto another radio, the recovery claim of 0.4.1 itself (it needs
  a region SONOFF's firmware reformatted; host-tested, also with real
  LittleFS). The stock EmberZNet 7.4.5 does not report the APS frame
  counter: its backups have no TCLK counter, and a restore gives the APS
  counter a margin of 2²⁰ (note `counters_unknown`). No Green Power, no
  MG24 firmware update; at most 8 groups on the hub's endpoint (the NCP's
  multicast table); if the first boot cannot claim SONOFF's `files`
  partition the hub keeps its data in RAM only (the banner says why). Not
  offered by the web flasher (its erase option would wipe SONOFF's
  firmware): use SONOFF's web console or esptool with the two USB parts,
  after the otadata check.
- SONOFF Dongle Max, while SONOFF's firmware is still in `ota_0` (USB or
  web-console install): a hub image in `ota_1` that fails to boot (e.g. an
  interrupted USB write) makes the bootloader start SONOFF's firmware,
  which reformats the `files` partition. The hub's devices, names,
  automations and settings are lost (the Zigbee network stays in the MG24
  and the devices come back through the NCP import, unnamed). The
  recovery claim and a hub backup bring back the devices and groups;
  names, areas, automations and settings have to be set again.

## 0.4.0

### Highlights

- **Migration from Zigbee2MQTT and ZHA without pairing again.** The hub
  restores the network (key, PAN, channel, frame counters) from the
  source's backup and takes the devices from its records: they appear at
  once with their names, areas and entities, and nothing is sent to them
  until they are heard from (with the same coordinator IEEE their
  configuration is kept; on another radio they are configured again).
  Three ways in, each with a preview first (`network/import/preview`; the
  running network's backup is kept before an apply):
  - web UI: Settings › *Migrate from Zigbee2MQTT or ZHA* (reads Z2M's
    `database.db`, `coordinator_backup.json` and names in the browser);
  - Home Assistant: the admin services `zigbee_rs_hub.import_from_zha`
    (ZHA's `zigbee.db` with HA's names and areas; apply refused while ZHA
    runs) and `zigbee_rs_hub.copy_names_areas` (names and areas from ZHA or
    Zigbee2MQTT devices);
  - `hubctl import z2m DIR` / `hubctl import zha zigbee.db`.

  Both backends (the Thread BR board's zigbee-rs NCP and the Dongle Max's
  EmberZNet NCP). Never run the old coordinator again on that network.
  What carries over and the limits: `docs/migration.md`.

### Fixes

- **MQTT: why the broker refused the bridge.** `mqtt/status` `last_error`
  now says `connection refused: not authorized`, `bad user name or
  password` or `the broker closed the connection after CONNECT (it may
  require a user name and password)` instead of `disconnected`, and the
  web UI points to a Home Assistant user or the Mosquitto add-on's logins
  when an anonymous client is refused (the add-on does not accept
  anonymous clients; checked against it). A password without a user name
  is no longer sent (MQTT 3.1.1 forbids it).
- **Dongle Max: hub version.** `hub/info` `name` carries the image's
  version (0.3.0 said `zigbee-rs-hub 0.2.0`), and the Home Assistant
  integration reads the version from the name on every board (the Dongle
  Max's hub device had no software version).
- **Tuya TS011F plugs get their definition** (`TS011F_plug_1` and its white
  labels, e.g. NEO NAS-WR01B for `_TZ3000_gjnozsaz`): the hub implements
  the vendor template `tuya_metering_zero_debounce` they need. As in
  Zigbee2MQTT, a zero power, current or voltage reading is published only
  if it stands for about 11 s (spurious zeros are dropped), and power is 0
  while the plug is off. TS011F devices for which Zigbee2MQTT picks a
  definition that needs template `tuya` (not implemented yet:
  `TS011F_plug_2`/`_3`, multi-gang strips, DIN relays) use `TS011F_plug_1`
  meanwhile, marked **approx.** on the device card and page (the tooltip
  names Zigbee2MQTT's definition and what it needs; `definition_approximate`
  in the device record and diagnostics). The definitions a device could
  not run stay with it across restarts and are checked again, shard by
  shard, after a catalog update or a firmware that gains the feature.
  After the update
  use Find definition on the device page (or Catalog › Fetch missing
  definitions) to fetch it.
- **A removed device stays removed.** A device removed with
  `devices/remove` (with or without `force`) that the coordinator still
  listed came back at the next hub restart or NCP reset, as a partial
  device without a name (the hub imports the devices the NCP knows). The
  removal now purges the NCP's tables (ZRHP: the NCP's Trust Center record
  and link key, `TBL_INVALIDATE`, and the host store's device record;
  EZSP: an APS Remove-Device to a router, the device's key table entry),
  and the hub remembers the last 64 removed devices so that the NCP's
  child or neighbor entries (kept until the device ages out) do not bring
  them back. A removed device that joins or announces itself again is
  added as before. Forming, resetting or restoring the network forgets
  them. Checked on hardware: a force-removed sensor stayed removed after
  an NCP hard reset and a hub reboot.
- **Find definition no longer says "the catalog has no definition" for a
  definition the firmware cannot run.** Catalog definitions that need a
  feature the hub lacks (a vendor template, e.g. every Tuya TS011F plug
  definition in 0.3.0) were dropped without a word, so such devices stayed
  generic for no visible reason. The hub now names them: Find definition
  says which definition matched and what it needs, and the device's
  diagnostics list it (`catalog_refused`).
- A definition fetched from one catalog shard no longer keeps a better one
  from a later shard out (a device's keys span several shards).
- A catalog shard or definition shared by several devices is no longer
  checked or fetched for the first of them only: uploading it for one
  device does not end the check of the others.

### Other changes

- **Web UI: tabs in a sidebar.** On screens 900 px wide or more the tabs
  are a sidebar on the left; on phones a menu button (☰, with the open
  tab's title) opens them over the page (a tab, Escape or a tap on the
  dimmed page closes it). No horizontal scroll at 320 px any more (the
  device grid overflowed there).
- **Device cards: status at the top.** Battery, LQI, RSSI, last seen and
  power source sit right under the card's title, in the device page's
  order, in the same place on every card (they were under the entities).
- **Device diagnostics in the web UI.** The device page's **Diagnostics**
  button shows what the device reported (manufacturer, model ID,
  application, stack and HW version, SW build ID, date code, ZCL version,
  power source, logical type, endpoints with their clusters) and what the
  hub made of it (definition and its layers, catalog definitions this
  firmware cannot run, unknown attributes, Tuya DPs), with **Copy JSON**
  and **Download JSON** (`zigbee-rs-hub-diagnostics-<IEEE>.json`: the
  record, entities, states and `devices/diagnostics`, as Home Assistant's
  "Download diagnostics"). A module loaded on the first click.
- Internal: every firmware crate pins esp-idf-svc 0.53.0 (the MQTT error
  reporting reads its event layout). The zigbee-herdsman-converters parity
  test also resolves the definitions that need features the hub lacks,
  with every feature, and lists them by feature.

### Upgrading from 0.3.0

- **Data:** an update keeps `nvs` (Wi-Fi, mode, API token) and `hubdata`
  (devices, settings, network backup), as from 0.2.0 to 0.3.0. The web
  flasher has the same entries as for 0.3.0 (standard images, the
  Developer edition, the Dongle Max listed without an install button).
- **Home Assistant:** integration **0.6.0** (`integrations/home-assistant`):
  the migration services above and the hub version read from the name.
- **tbr-bridge 0.2.6** (the S3 app for bridge mode on Espressif's stock
  layout): no functional change (esp-idf-svc pinned). Its image no longer
  carries a build time, so later releases rebuild it byte for byte and it
  keeps its version until its code changes.

### Known limitations

- The 8 MB images (and the OTA image store) have not run on an 8 MB board
  yet. The 4 MB octal image ran with Zigbee on the lab board; the 4 MB
  quad image ran on a 2 MB quad-PSRAM S3 devkit without a radio, so Zigbee
  on a quad-PSRAM board is still untested.
- Migration: host-tested on both simulated backends (ZRHP and EZSP) with
  Zigbee2MQTT and ZHA files. On hardware a Zigbee2MQTT-format import of
  the Dongle Max's own network ran (preview `same`, names applied, devices
  complete); an import onto another radio or network and a ZHA import
  have not. Green Power
  devices are not imported (commission them again); after a change of the
  coordinator IEEE some devices keep the old trust center address (prefer
  a radio that takes the backup's IEEE); Z2M/ZHA per-device settings and
  external converters are not imported (`docs/migration.md`, Limits).
- No hub A/B self-update on the 4 MB dev layout; no SoftAP setup (Wi-Fi
  from the console or the browser console).
- Definitions that need the vendor template `tuya` are not run yet
  (TS011F devices for which Zigbee2MQTT picks `TS011F_plug_2`/`_3`,
  multi-gang strips, DIN relays): they use `TS011F_plug_1`, marked
  **approx.**, and the device's diagnostics name Zigbee2MQTT's definition.
- SONOFF Dongle Max: experimental. Ran on the lab unit: the network,
  joining and interviews, reports, a network backup export and restore
  onto the same network (0.3.0; the joined sensor kept working), the hub
  name and version, a Zigbee2MQTT-format import of its own network. Not
  yet run: Ethernet, `stock confirm`, the self-update, installation
  through SONOFF's web console, a restore or import onto another radio.
  No Green Power, no MG24 firmware update; at most 8
  groups on the hub's endpoint (the NCP's multicast table); if the first
  boot cannot claim SONOFF's `files` partition the hub keeps its data in
  RAM only (a banner says so). Not offered by the web flasher (its erase
  option would wipe SONOFF's firmware): use SONOFF's web console or
  esptool with the two USB parts, after the otadata check.
- SONOFF Dongle Max, while SONOFF's firmware is still in `ota_0` (USB or
  web-console install): a hub image in `ota_1` that fails to boot (e.g. an
  interrupted USB write) makes the bootloader start SONOFF's firmware,
  which reformats the `files` partition. The hub's devices, names,
  automations and settings are lost (the Zigbee network stays in the MG24
  and the devices come back through the NCP import, unnamed). Download a
  hub backup (Settings › Backup) before updating over USB; after a hub
  self-update the fallback is the previous hub image. Re-claiming after
  such a reformat is refused without a reason (fixed in 0.4.1: `recover`).

## 0.3.0

### Highlights

- **Crash fix (important for every 0.2.0 hub).** Rust atomics now go through
  ESP-IDF's PSRAM-safe functions: the S3's compare-and-set instruction is
  not atomic between the two cores on PSRAM, where the hub keeps its data.
  0.2.0 hubs could crash or reboot under API load (`LoadProhibited`,
  `Cache error`, mutex asserts; on the lab board after 40–80 network
  diagnostics); with the fix it ran 1000 in a row without a crash
  (measurements in the README, `hub atomics`). Updating every 0.2.0 hub
  is recommended.
- **Zigbee device OTA** (Upgrade server, cluster 0x0019) in the 8 MB
  images: an Updates tab, `ota/*` API, Home Assistant `update` entities,
  an image store in the inactive app slot, larger images streamed from a
  client. The 4 MB images are built without it.
- **Reporting configuration:** editable defaults per value class
  (Settings › Reporting defaults), per-entity overrides on the device
  page (`min`, `max`, `change` in the entity's units, or `off`), and
  readback of what the device applied.
- **Device page** (`#device/<id>`) with compact device cards on the
  dashboard; event entities (remotes) show their last action; grouped
  sections (Sensors, Controls, Events, Configuration, Diagnostic).
- **Developer edition (devtools)** of the 8 MB images
  (`tbr-hub-0.3.0-8mb-devtools[-opi]-…`, its own entry in the web flasher)
  and of the Dongle Max (`zrh-donglem-0.3.0-devtools-app.bin`): the web UI's
  developer tools (the ZCL/Tuya console) always on, `edition: "devtools"`
  in `hub/info`. For developers; everyone else keeps the standard image.
  Same layout and data as the standard image: switching keeps `nvs` and
  `hubdata`.
- **Last panic in RTC memory:** the console's `info` shows the last
  ESP-IDF panic and how many boots ago it happened.
- **SONOFF Dongle Max (experimental):** the hub application on the
  dongle's stock EmberZNet NCP (`zrh-donglem`, EZSP backend), installed
  into `ota_1` of SONOFF's unchanged partition table. It runs on one lab
  unit (network formed, a sleepy device joined, interviewed and reporting;
  EZSP session up in 2.6 s); the rest of its hardware checklist is pending
  (`docs/dongle-max.md`).

### Other changes

- 4 MB images: the Zigbee2MQTT-compatible MQTT bridge is now included;
  the advanced ZCL/Tuya console and frame tap are 8 MB only (feature
  `advanced`). `hub/system` reports the memory mode, build features and
  limits. A board without the PSRAM its image was built for keeps the
  Zigbee network but does not start the application and says which image
  to flash.
- Definitions: device selection matches zigbee-herdsman-converters for
  6,081 of its 6,082 fingerprint vectors. White labels by model ID,
  software build (Paulmann `RGBWW`), logical type, hardware version and
  endpoints (SONOFF SNZB-03/SNZB-05, BTicino K4027C), with zhc's exact
  names; `date_code` clauses. Tuya week-schedule thermostats (ENGO EONE,
  TS0601_thermostat_2, …) get their definition instead of staying generic.
  The interview keeps the Green Power endpoint 242 for matching, so 13
  more models match (e.g. Shelly 2PM, OSRAM DALI, Gledopto GL-C-007-1ID).
  A *generic* badge and *Find definition* for unmatched devices.
- SWBuildID and DateCode are read again after an OTA upgrade and on
  rejoin, so the definition and white label follow the new firmware
  without a re-interview.
- Web UI: the rarely used tabs (Converters, Catalog, ZCL console,
  Updates) load when first opened, which keeps the page small; a page left
  open over a firmware update reloads once.
- "Find definition" no longer fails with `SHA-256 differs from the
  installed manifest`: the catalog is updated before shards are fetched.
- Automations: device-then-entity pickers that tell identical devices
  apart; the run mode is labelled and explained in the editor (*single*
  ignores triggers while a run is in progress, *restart* starts over); the
  network map lays out small networks readably.
- **Time zone picker** in Settings › Hub: search the zone by name or take
  the browser's (the hub keeps the IANA name, `hub/time` `tz_name`, next to
  its POSIX string); Home Assistant can keep the hub's zone in sync with
  its own (integration option). Settings no longer scrolls sideways on
  phones.
- `network/info` reports the coordinator's table sizes (`capacity`).
- SONOFF Dongle Max: network backup and restore through EZSP (Open
  Coordinator Backup, the format zigpy/ZHA and Zigbee2MQTT use, so a
  network can move between them and the hub); host-tested against the
  simulated NCP (EZSP 13–18), not yet run on a Dongle Max.
- Hub records are tagged with the network they belong to (`hub/owner`):
  `network/info.registry`, `network/registry/adopt` and
  `network/registry/clear` (both need `"confirm": true`). After an adopt,
  or once the records are readable again, groups, bindings, last values,
  automations and OTA states load too (a start-up race left them hidden
  until the next restart).
- Internal: the coordinator boundary (host store, coordinator traits, the
  non-blocking ZRHP pump, ticketed device transport) and the EZSP backend
  as a build option. The TBR board images stay on ZRHP and need no NCP
  update (ZRHP 1.0, H2 NCP 0.1.2).

### Upgrading from 0.2.0

- **Data:** an update keeps `nvs` (Wi-Fi, mode, API token) and `hubdata`
  (devices, settings, network backup). At its first boot 0.3.0 adds one
  record to the host store, the owner tag (`hub/owner`, derived from the
  stored network); nothing else is rewritten and no data are lost. Going
  back to 0.2.0 is safe: it ignores the tag.
- **Web flasher:** pick the board, leave *Erase* unticked; the image is
  chosen from the detected flash and PSRAM. Changing the layout (4 MB dev
  ↔ 8 MB) still needs an erase: back up the network first.
- **ZCL/Tuya console (8 MB, Dongle Max):** the tab is now a developer
  tool: switch on **Developer mode** (Settings › Hub) to see it; the
  console's API (`zcl/*`, `tuya/send`, topic `frames`) stays available with
  the API token. The developer edition always shows it.
- **4 MB boards:** no Zigbee OTA (no Updates tab, `ota/*` answers
  `unsupported`) and no advanced console any more; MQTT is new there.
  **8 MB boards:** OTA, the advanced console and MQTT.
- **Home Assistant:** integration **0.5.0**, from this release's source
  (`integrations/home-assistant`): it adds the firmware `update` entities
  of the Zigbee OTA server (8 MB hubs) and the option to sync the hub's
  time zone from Home Assistant.
- **tbr-bridge 0.2.5** (the S3 app for bridge mode on Espressif's stock
  layout): no functional change; a new version because the rebuilt image
  differs from the 0.2.4 file of release 0.2.0.

### Known limitations

- The 8 MB images (and the OTA image store) have not run on an 8 MB board
  yet. The 4 MB octal image ran with Zigbee on the lab board; the 4 MB
  quad image ran on a 2 MB quad-PSRAM S3 devkit without a radio (in-place
  upgrade with the data kept, web UI and API under load, no crash), so
  Zigbee on a quad-PSRAM board is still untested.
- No hub A/B self-update on the 4 MB dev layout; no SoftAP setup (Wi-Fi
  from the console or the browser console).
- SONOFF Dongle Max: experimental, full hub hardware test pending
  (Ethernet, `stock confirm`, self-update not yet run); installation
  through SONOFF's web console not yet verified on hardware; network
  backup/restore through EZSP host-tested only, not yet run on a Dongle
  Max; no Green Power, no MG24 firmware update; at
  most 8 groups on the hub's endpoint (the NCP's multicast table); if the
  first boot cannot claim SONOFF's `files`
  partition the hub keeps its data in RAM only (a banner says so). Not
  offered by the web flasher (its erase option would wipe SONOFF's
  firmware): use SONOFF's web console or esptool with the two USB parts.
