# Changelog

User-facing changes of the hub firmware releases (`hub-v*` tags). The
release workflow (`.github/workflows/hub-release.yml`) puts the release's
section at the top of its GitHub release notes, above the generated
install instructions and the list of merged pull requests. A pre-release
`X.Y.Z-rc.N` uses its own section if there is one, else `X.Y.Z`'s.

## 0.3.0

### Highlights

- **Crash fix (important for every 0.2.0 hub).** Rust atomics now go through
  ESP-IDF's PSRAM-safe functions: the S3's compare-and-set instruction is
  not atomic between the two cores on PSRAM, where the hub keeps its data.
  0.2.0 hubs could crash or reboot under API load (`LoadProhibited`,
  `Cache error`, mutex asserts; on the lab board after 40–80 network
  diagnostics); with the fix it ran 1000 in a row without a crash.
  Updating every 0.2.0 hub is recommended.
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
  99.6 % of its fingerprints; white labels by model ID and by software
  build (e.g. Paulmann `RGBWW` lights), `date_code` clauses; a *generic*
  badge and *Find definition* for unmatched devices.
- "Find definition" no longer fails with `SHA-256 differs from the
  installed manifest`: the catalog is updated before shards are fetched.
- Automations: device-then-entity pickers that tell identical devices
  apart; the network map lays out small networks readably.
- `network/info` reports the coordinator's table sizes (`capacity`).
- Hub records are tagged with the network they belong to (`hub/owner`):
  `network/info.registry`, `network/registry/adopt` and
  `network/registry/clear` (both need `"confirm": true`).
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
- **4 MB boards:** no Zigbee OTA (no Updates tab, `ota/*` answers
  `unsupported`) and no advanced console any more; MQTT is new there.
  **8 MB boards:** OTA, the advanced console and MQTT.
- **Home Assistant:** integration **0.5.0**, from this release's source
  (`integrations/home-assistant`): it adds the firmware `update` entities
  of the Zigbee OTA server (8 MB hubs).
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
  through SONOFF's web console not yet verified on hardware; no network
  backup/restore through EZSP, no Green Power, no MG24 firmware update; at
  most 8 groups on the hub's endpoint (the NCP's multicast table); if the
  first boot cannot claim SONOFF's `files`
  partition the hub keeps its data in RAM only (a banner says so). Not
  offered by the web flasher (its erase option would wipe SONOFF's
  firmware): use SONOFF's web console or esptool with the two USB parts.
