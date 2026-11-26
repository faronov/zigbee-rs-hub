# Changelog

User-facing changes of the hub firmware releases (`hub-v*` tags). The
release workflow (`.github/workflows/hub-release.yml`) puts the release's
section at the top of its GitHub release notes, above the generated
install instructions and the list of merged pull requests. A pre-release
`X.Y.Z-rc.N` uses its own section if there is one, else `X.Y.Z`'s.

## Unreleased

### Other changes

- **Web UI: tabs in a sidebar.** On screens 900 px wide or more the tabs
  are a sidebar on the left; on phones a menu button (☰, with the open
  tab's title) opens them over the page (a tab, Escape or a tap on the
  dimmed page closes it). No horizontal scroll at 320 px any more (the
  device grid overflowed there).
- **Device cards: status at the top.** Battery, LQI, RSSI, last seen and
  power source sit right under the card's title, in the device page's
  order, in the same place on every card (they were under the entities).

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
