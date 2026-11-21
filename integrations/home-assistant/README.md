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
  `climate`, `lock`, `fan`, `siren`, `valve` (`update` is not served by the
  hub yet).
- Services: `zigbee_rs_hub.permit_join` (duration, Green Power window, a
  router to join through), `zigbee_rs_hub.reinterview_device`,
  `zigbee_rs_hub.reconfigure_device`.
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

## Development and tests

The tests use
[pytest-homeassistant-custom-component](https://github.com/MatthewFlamm/pytest-homeassistant-custom-component)
(it pins a Home Assistant release and needs the same Python, currently 3.14)
and a fake hub (`tests/fake_hub.py`, an aiohttp WebSocket server speaking
the hub API) for the config flow, entity creation, pushed state,
availability, commands on every platform, runtime entities and devices,
reconnect with resync, the keepalive, services, device removal and
diagnostics. `tests/test_e2e_hubctl.py` runs against the real API server,
`hubctl --sim serve` (simulated NCP), when `HUBCTL` names a hubctl binary.

```sh
cd integrations/home-assistant
python3.14 -m venv .venv            # or: uv venv --python 3.14 .venv
.venv/bin/pip install -r requirements-test.txt
.venv/bin/ruff check . && .venv/bin/ruff format --check .
.venv/bin/python -m pytest
# end to end against the simulated hub:
cargo build -p hubctl
HUBCTL=$PWD/../../target/debug/hubctl .venv/bin/python -m pytest
```

CI runs the same in the `home-assistant` job of `.github/workflows/ci.yml`.

## Limitations

- No zeroconf discovery until the hub advertises `_zigbee-rs-hub._tcp`.
- No device triggers yet; remotes and GP switches are `event` entities.
- Renaming a device in HA does not rename it on the hub (`devices/update`).
- Definitions, catalog and Green Power bindings are not exposed in HA; use
  the hub's own tools for them.
