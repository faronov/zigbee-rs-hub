# zigbee-rs hub for Home Assistant

A Home Assistant custom integration (`zigbee_rs_hub`) for
[zigbee-rs-hub](../../README.md): the ESP32-S3 hub with an ESP32-H2 NCP, or
`hubctl serve` on a desktop. It is a thin bridge over the hub's WebSocket
JSON API (`docs/device-support.md`, sections 4 and 14–15): no Zigbee logic and
no per-device code in Home Assistant; the hub decides which entities a device
has, and the integration maps them 1:1 onto HA platforms.

- Config flow (host, port, API token), reauthentication and reconfiguration.
- One persistent WebSocket connection (HA's aiohttp session), `local_push`:
  a snapshot on (re)connect, then the hub's deltas; no polling. Reconnects
  with exponential back-off (1 s → 60 s); entities are unavailable while the
  hub is away. An application-level keepalive (`hub/info` every 60 s, 10 s
  timeout) drops a hub that stopped answering.
- A HA device per Zigbee/GP device (`identifiers` = hub device id,
  manufacturer, model, model ID, software/hardware version, IEEE or GPD
  source ID as serial number, `via_device` = the hub), plus a hub device with
  a **Connection** sensor and a **Permit join** button. With API 1.1 hubs the
  hub device also has **Permit join** (on while the network accepts joins,
  closing on time even if the closing event is missed), **Permit join ends**
  (a timestamp HA shows as a countdown), **Network state** (with PAN ID,
  extended PAN ID, update ID, ownership and NCP session as attributes) and
  **Channel**; its hardware version shows the NCP board and firmware.
- Entities for all platforms the hub serves: `sensor`, `binary_sensor`,
  `switch`, `light`, `number`, `select`, `button`, `text`, `event`, `cover`,
  `climate`, `lock`, `fan`, `siren`, `valve`, `scene`.
- Zigbee groups of the hub (device-support 14.9): a device per group with
  members (`grp:<id>`, model "Zigbee group"), its `light` (or `switch`) with
  the aggregated state, commands sent as one groupcast, and its scenes as HA
  scenes (`scene.turn_on` → Recall Scene, with `transition`). Deleting the
  group device deletes the group on the hub.
- **Firmware** (`update`), one per device with a Zigbee OTA client, on hubs
  with the OTA server (`ota` in `hub/info`, device-support 14.11): installed
  version (the device's OTA file version, `0x…` as ZHA shows it), the
  newest image stored on the hub for it, install (`ota/start`; the device is
  notified, a sleepy one when it wakes) and download progress. A newer
  image in the OTA index the hub names (`index_url`, Koenkk/zigbee-OTA by
  default; fetched daily by Home Assistant, not by the hub) counts as the
  latest version: Install downloads it and checks its SHA-512, then
  uploads it (hubs with an image store: the 8 MB images) or **streams** it
  (a hub built without the store: Home Assistant serves the blocks while
  the device downloads; the file is kept in `.storage/zigbee_rs_hub_ota/`
  until the update is done and offered again after any reconnect, so a
  hub or HA restart does not lose the download). Images can also be given
  from the hub's web UI (Updates tab) or with `hubctl ota-upload` /
  `ota-stream`. The 4 MB images have no Zigbee OTA (no `ota` in
  `hub/info`): no update entities.
- Services: `zigbee_rs_hub.permit_join` (duration, Green Power window, a
  router to join through), `zigbee_rs_hub.reinterview_device`,
  `zigbee_rs_hub.reconfigure_device`, `zigbee_rs_hub.update_catalog`.
- Definitions catalog (hubs with the `catalog/*` API): the hub downloads
  nothing itself, so the integration fetches the signed release files from
  the mirror set on the hub (`catalog/config` `base_url`, default
  `https://faronov.github.io/zigbee-rs-definitions/latest`) with HA's aiohttp
  session and uploads them as `hubctl catalog-push` does: `manifest.cose`,
  then the bank and web tools the hub `wants`, then the `shard-hh.cbor`
  files of devices that resolve to generic only. Uploads go in 4 KiB chunks
  (the most the hub accepts) and continue after a dropped connection from
  `upload.written`. `zigbee_rs_hub.update_catalog` (optional
  `config_entry_id`, `force` to pass the bank anti-rollback check, `base_url`
  to use another mirror for this run) runs it now; automatically it runs two
  minutes after set-up and then every 24 h when the hub's `auto_update` or
  `auto_fetch` is on (each enables its part), and on the hub's
  `definition_needed` event when `auto_fetch` is on (at most once an hour
  per device). A **Catalog version** diagnostic sensor on the hub device
  shows `catalog_date.serial` of the installed bank (`none` without one),
  the manifest, missing definitions and the last check's error.
- Hub automations (API 1.3 hubs, `automations` in `hub/info`): the rules
  that run on the hub itself (device-support 14.8) appear on the hub device
  as an **Automation …** switch each (on = enabled; `automations/enable` /
  `disable`; attributes: ID, last run and result, run count, why a schedule
  is suspended) and a **Run …** button (`automations/run`, a test run of
  the actions). They follow the hub's `automations` topic: created, renamed
  and removed at run time, re-listed after every reconnect. The rules are
  edited in the hub's web UI or over the API, not in HA.
- HA's **Delete device** removes the device from the network
  (`devices/remove`); devices removed on the hub disappear from HA.
- Diagnostics (entry and device; the device download includes the hub's
  `devices/diagnostics`), with the token and any keys redacted.
- English and Russian translations.

## Requirements

- Home Assistant 2025.1 or newer (tested with 2026.10).
- Hub firmware with the WebSocket API in hub mode (`tbr-hub`), Wi-Fi
  configured (console `wifi set <ssid> <password>`). The API token is shown
  by the hub console command `token` (CDC 1).
- The hub does not advertise itself over mDNS yet, so the hub is added
  manually (no zeroconf discovery).

## Installation

This directory is not a HACS repository of its own (HACS needs
`custom_components/` at a repository root); `hacs.json` is ready for a
future split. Install manually:

1. Copy `custom_components/zigbee_rs_hub` into Home Assistant's
   configuration directory, so that the result is
   `/config/custom_components/zigbee_rs_hub/manifest.json`. For example, with
   the Samba or SSH add-on:

   ```sh
   scp -r integrations/home-assistant/custom_components/zigbee_rs_hub \
       root@<ha-host>:/config/custom_components/
   ```

2. Restart Home Assistant (*Settings → System → Restart*); custom
   integrations are only loaded at start-up.
3. *Settings → Devices & services → Add integration → zigbee-rs hub*:
   host = the hub's IP address, port = `80` (`8099` for `hubctl serve`),
   API token = the output of the hub console `token`.

Updating is the same copy followed by a restart. To remove the integration,
delete the entry, then the directory, then restart.

## How the hub API maps onto Home Assistant

| Hub (WebSocket API) | Home Assistant |
|---|---|
| `auth_required` → `auth {token}` → `auth_ok` | config entry data (`host`, `port`, `token`); `auth_invalid` → reauth flow |
| `hub/info` `coordinator_ieee` | config entry unique ID (hubs without `hub/info`: `host:port`, moved to the IEEE once the hub reports it); hub device serial number |
| `hub/info` `name`, `ncp_product` | hub device firmware version (`zigbee-rs-hub 0.2.4` → `0.2.4`) and model |
| `subscribe ["devices", "entities", "events"]` (+ `"network"` on API 1.1) snapshot | devices, entities, states and network facts (again after every reconnect) |
| `network` topic (API 1.1): `permit_join {remaining_s}`, `network_state {state}` | Permit join / Permit join ends, Network state |
| `network/permit_join` from HA (API 1.1) | Permit join on at once for the requested duration (whole network only), then reconciled with `network/info` `permit_join_s`; hubs need not send a `permit_join` event for a window the host opened |
| network/info `time` (the hub's clock) | timestamps from a hub whose clock is not synchronised yet (seconds since boot) are converted with the offset to HA's clock |
| device record (4.1) | device registry entry; `available: false` → all its entities unavailable; `name` → device name (a name set in HA stays HA-only) |
| entity record (4.2) | `uid` → `unique_id`; `platform`, `device_class`, `unit`, `state_class`, `category` → entity category, `enabled_default`, `capabilities` → options, limits, colour modes, features, `event_types` |
| entity name | record `name`, else the device name for the main entity (`light`, `switch`, `cover`, …), else the translation key, else HA's device-class name, else the key |
| `entity_state` | entity state and attributes (`available: false` → unavailable) |
| `entity_added` / `entity_updated` / `entity_removed` | entities created at run time / record changes (`orphaned` → unavailable) / registry entry removed |
| `device_added` / `device_updated` / `device_removed` | device registry |
| `entity_event` | `event` entity fires `event_type` with the attributes |
| HA service call | `entities/command {uid, command, params}` with HA's service and parameter names (`turn_on {brightness, transition, color_temp_kelvin, xy_color, hs_color}`, `set_value {value}`, `select_option {option}`, `press`, `set_temperature`, `set_cover_position`, …); hub errors become `HomeAssistantError` |
| `network/permit_join` | Permit join button (254 s, Green Power included) and the `permit_join` service |
| `devices/remove`, `devices/reinterview`, `devices/reconfigure`, `devices/diagnostics` | Delete device, services, device diagnostics |
| group devices `grp:<id>` and their entities (14.9) | ordinary devices and entities; platform `scene` → HA `scene` (`activate`) |
| `catalog/status`, `catalog/upload_chunk`, `catalog/commit`, events `definition_needed`, `catalog_*` | `update_catalog` service, automatic checks, **Catalog version** sensor |
| `automations/list`, topic `automations` (`automation_added`/`_updated`/`_removed`/`_run`) (API 1.3) | per rule: `switch` `<entry>_automation_<id>` (enabled), `button` `<entry>_automation_<id>_run`; `automations/enable`, `disable`, `run` |

## Development and tests

The tests use
[pytest-homeassistant-custom-component](https://github.com/MatthewFlamm/pytest-homeassistant-custom-component)
(it pins a Home Assistant release and needs the same Python, currently 3.14)
and a fake hub (`tests/fake_hub.py`, an aiohttp WebSocket server speaking
the hub API) for the config flow, entity creation, pushed state,
availability, commands on every platform, runtime entities and devices,
reconnect with resync, the keepalive, services, device removal,
diagnostics and the catalog client (with a local HTTP mirror; the fake hub
refuses chunks over 4 KiB). `tests/test_e2e_hubctl.py` runs against the
real API server, `hubctl --sim serve` (simulated NCP; with
`--sim-devices` simulated plugs for the automations test), when `HUBCTL`
names a hubctl binary; its catalog test also needs `zdefc` (next to hubctl, or
`ZDEFC`) to build and sign a test release.

```sh
cd integrations/home-assistant
python3.14 -m venv .venv            # or: uv venv --python 3.14 .venv
.venv/bin/pip install -r requirements-test.txt
.venv/bin/ruff check . && .venv/bin/ruff format --check .
.venv/bin/python -m pytest
# end to end against the simulated hub:
cargo build -p hubctl -p zdefc
HUBCTL=$PWD/../../target/debug/hubctl .venv/bin/python -m pytest
```

CI runs the same in the `home-assistant` job of `.github/workflows/ci.yml`.

## Limitations

- No zeroconf discovery until the hub advertises `_zigbee-rs-hub._tcp`.
- No device triggers yet; remotes and GP switches are `event` entities.
- Renaming a device in HA does not rename it on the hub (`devices/update`).
- User definitions and Green Power bindings are not exposed in HA, nor are
  the catalog settings (`catalog/config`); use the hub's own tools for them.
