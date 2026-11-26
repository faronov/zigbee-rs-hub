# Migrating from Zigbee2MQTT or ZHA

A Zigbee2MQTT (Z2M) or ZHA (zigpy) network moves to the hub without pairing
its devices again: the hub restores the network (its key, PAN, channel and
frame counters) from the source's backup and takes the devices from the
source's records, so they appear at once with their entities and names. The
same radio or another radio, the same chip family or another
(Z-Stack/EmberZNet/deCONZ/ZBOSS → the hub's EZSP or ZRHP backend): the
devices only ever see the same network.

**Never run the old coordinator again on this network** once the hub runs it:
two coordinators with one network key break frame-counter safety and
address resolution. Stop Zigbee2MQTT, or disable ZHA, before you import.

Contents: [What you need](#what-you-need) · [From Zigbee2MQTT](#from-zigbee2mqtt) ·
[From ZHA](#from-zha) · [The same radio or another](#the-same-radio-or-another) ·
[What carries over](#what-carries-over) · [Limits](#limits) ·
[How it works](#how-it-works) · [The import document](#the-import-document-zigbee-rs-hubimport-v1) ·
[API](#api) · [Where the files are read](#where-the-files-are-read-and-why)

## What you need

- A hub with an **API token** (the import restores network keys; a hub
  without a token refuses, `unauthorized`) and a backend with network
  restore (`restore` in `hub/info` `maintenance`: ZRHP and EZSP both).
- The source's **network backup** with its network key (`coordinator_backup.json`
  of Z2M, the newest backup in ZHA's `zigbee.db` or ZHA's *Download
  backup*). An import without the network part is refused
  (`network_required`).
- The source's **device records**: Z2M's `database.db`, ZHA's `zigbee.db`.
- Optional: the names (Z2M `configuration.yaml` or `devices.yaml`/`groups.yaml`;
  for ZHA, Home Assistant's device registry).

## From Zigbee2MQTT

1. Stop Zigbee2MQTT (`docker stop zigbee2mqtt`, the add-on's Stop): it
   writes `coordinator_backup.json` and `database.db` on the way down, so
   the copies are current. Do not start it again.
2. Take from its data directory: `coordinator_backup.json`, `database.db`,
   `configuration.yaml` (and `devices.yaml`/`groups.yaml` if the
   configuration names them).
3. Either:
   - **Web UI**: Settings › *Migrate from Zigbee2MQTT or ZHA* › Start, pick
     the files, *Preview*: the network (extended PAN ID, what it replaces,
     key, frame counter, link keys), and per device its model, the
     definition it resolves to (or *generic*) and its status (`new`,
     `replace`, `keep`, `skip`). Then *Import*: the page asks the
     confirmations the hub needs (replace the running network, re-tag the
     registry, write an EZSP IEEE once) and saves the replaced network's
     backup.
   - **hubctl**: `hubctl import z2m /path/to/data --url ws://HUB/api --token T`
     previews; add `--apply --force` (and `--confirm`,
     `--confirm-ieee-write-once` if the preview says so) to import.
     Without `--url` it writes the import document (`--out doc.json`).
4. The devices are verified as they talk ([How it works](#how-it-works)).
5. Names in Home Assistant: the hub's devices take Z2M's friendly names.
   Areas of Z2M's MQTT devices in Home Assistant: call the integration's
   `zigbee_rs_hub.copy_names_areas` service (it also copies names you set
   in HA).

Z2M's friendly names are also what the hub's Zigbee2MQTT-compatible MQTT
bridge uses for its topics, so MQTT automations keep their topics.

## From ZHA

With the zigbee-rs hub integration in the same Home Assistant (as an
administrator: the migration services refuse other users):

1. Settings › Developer tools › Actions: `zigbee_rs_hub.import_from_zha`
   (preview). It reads ZHA's `zigbee.db` (default `<config>/zigbee.db`,
   field `database`), its newest network backup (or `backup`, a file of
   ZHA's *Configure › Download backup*), the names you gave ZHA's devices
   and their areas, and answers the hub's preview (also as a persistent
   notification).
2. Disable ZHA: Settings › Devices & services › Zigbee Home Automation › ⋮ ›
   Disable. The service refuses to apply while ZHA runs.
3. `zigbee_rs_hub.import_from_zha` with `apply: true` (plus `force`, and
   `confirm` or `confirm_ieee_write_once` when the preview's `needs` or the
   restore check names them). The hub's new devices take ZHA's names and,
   in Home Assistant, ZHA's areas.
4. Once everything works, delete the ZHA integration (its devices and
   entities leave HA; automations that named ZHA's entities must be pointed
   at the new ones). Keep it disabled until then — never enable both.

Without Home Assistant: `hubctl import zha zigbee.db [--backup backup.json]
[--ha-storage /config/.storage]` (names and areas from HA's
`core.device_registry`/`core.area_registry`) previews or applies like
`import z2m`, or writes the document for the web UI.

ZHA's database schemas v12 to v17 are read (zigpy 0.5x to 2.5, November
2026: v16 in zigpy 2.4, v17 in 2.5).

## The same radio or another

| Case | Coordinator IEEE | What happens to the devices |
|---|---|---|
| The same radio, reflashed with the hub's NCP firmware or used through the EZSP backend | the radio's own, the same as before | nothing: binds, reporting and the IAS CIE address still name the coordinator; verified on first contact (identity, unknown states) |
| Another radio that takes the backup's IEEE (ZRHP ESP32-H2: repeatable override; EZSP with the rewritable token) | the backup's | as above |
| Another EmberZNet radio with only the one-time custom EUI-64 token | the backup's, **written once** with `confirm_ieee_write_once` (irreversible: the token can never take another IEEE) | as above |
| Another radio that keeps its own IEEE (`allow_ieee_change: false`, `--keep-ieee`, an EZSP token already burnt with another IEEE) | the radio's | the devices know the trust center by the old IEEE: they are **configured again** when they talk (binds and reporting to the new IEEE, IAS enrolment); see the limits |

`reconfigure`: `auto` (default: only when the coordinator IEEE changed),
`always`, `never`.

## What carries over

| | Z2M | ZHA | Notes |
|---|---|---|---|
| Network key, PAN ID, extended PAN ID, channel, update ID | yes | yes | from the backup |
| NWK frame counter | backup + 2²⁰ | backup + 2²⁰ | the files may be days old: a large margin costs nothing in 32 bits |
| APS (trust center) frame counter | unknown in Z2M's files: + 2²⁰ | backup + 2¹⁶ | |
| Unique trust center link keys | Z-Stack backups list them; EmberZNet backups carry the hashed root (`stack_specific.ezsp.hashed_tclk`) | bellows: hashed root and key table; zigpy-znp: the key table | per backend below |
| Children of the coordinator | the backup's `is_child` | the backup's children | restored into the NCP's child table |
| Devices: endpoints, clusters, Basic identity, power source, manufacturer code | `database.db` | `zigbee.db` | the hub resolves the same definitions an interview would |
| Node descriptor (rx-on, mains) | not kept by Z2M: routers listen, battery end devices sleep | `node_descriptors` | |
| Cached values the definitions read (colour capabilities, metering and electrical scaling, thermostat limits, battery) | `database.db` cluster caches | `attributes_cache` | |
| Names | friendly names | names set in HA | |
| Areas | — (Z2M has none; `copy_names_areas` takes HA's from Z2M's MQTT devices) | HA areas (HA integration, hubctl `--ha-storage`) | the hub has no areas; Home Assistant gets them |
| Groups and their members | `database.db` groups, names from the configuration | `groups`, `group_members` | members already hold the group (device-side); the hub endpoint joins |
| Bindings between devices | — | — | device-side: they survive as they are |

Unique link keys per backend:

- **EZSP** (Dongle Max): Z-Stack/explicit keys go to the NCP's key table
  with `importLinkKey`, as many as its `KEY_TABLE_SIZE` (12 on the Dongle
  Max's stock NCP, `network/info` `capacity.link_keys`; the rest are
  `ignored` in the restore report's notes); EmberZNet sources bring the
  hashed root, so every device's derived key is the same as before.
  Devices whose key did not fit keep working on network security; their
  APS-encrypted exchanges with the trust center (key updates, a trust
  center rejoin) need a new pairing.
- **ZRHP** (zigbee-rs NCP): the keys go into the restore stream (`DEVICE`
  link blocks): the NCP caches them or keeps them host-backed
  (`link_keys_on_ncp`, `link_keys_host_backed`). The EZSP hashed root is
  stored; deriving keys from it waits for gate H5 of the host protocol
  (ZRHP 9.6), so devices that hold hashed keys of an EmberZNet source
  lose APS-level security with the trust center until then.

## Limits

- **Green Power devices** are not imported (`skip`, `green_power`): their
  sink table entries are the source's; commission them again (a button
  press). Z2M's GP security key in `database.db` is not used.
- **Devices that cache the old trust center address**: when the
  coordinator IEEE changes, devices keep the old one as their trust center
  address and in their bind tables. Reports bound by IEEE usually still
  arrive (the coordinator is always NWK 0x0000), and the hub binds them to
  the new IEEE; a trust center rejoin, key updates and some security
  checks fail until the device is paired again. Prefer a radio that takes
  the backup's IEEE.
- **Sleepy devices** are verified, and configured again where needed, when
  they next wake (a report, a poll); until then the hub shows them
  `interview: imported` and their last state unknown.
- **Unfinished interviews at the source** (Z2M `interviewState: FAILED` or
  `interviewCompleted: false`, zigpy status `NEW`/`ZDO_INIT`, a record
  without a model or with an endpoint without clusters) are imported, but
  the device is interviewed when it is first heard from.
- **Z2M external converters and device options** are Z2M's own: the hub
  uses its definitions catalog (`definitions/fetch`, user definitions);
  devices without a definition get generic entities.
- **Z2M's or ZHA's quirks/device settings** (calibration offsets,
  per-device reporting) are not imported; the devices keep what was
  written to them.
- One import takes 500 devices and 256 groups.
- Devices of the network an import replaces stay in the hub's registry
  (re-tagged, unavailable) unless `remove_missing`; their network's
  backup is kept (`network/backup/replaced`).
- An import over the network the radio runs already needs the hub's
  device registry to be this network's (`network/info` `registry.state`
  `owned` or `untagged`): records of another backend or network
  (`foreign`, `unverified`) are adopted or cleared first
  (`network/registry/adopt`, `network/registry/clear`), else the import
  answers `registry_read_only` — its devices could not be stored.

## How it works

`network/import` runs on the job thread, in this order:

1. The backend's restore check: a refusal (an IEEE the radio cannot take,
   counters past 2³²) changes nothing.
2. The running network's backup, kept as `bkp/replaced/<ext_pan_id>`
   before the network is left (as `network/restore`, `docs/ezsp-backend.md`).
3. The restore (EZSP: IEEE token, counters, security state, key table,
   `formNetwork`, child table; ZRHP: `BKP_RESTORE_*`), the records tagged
   with the imported network (registry `owned`); the job waits (30 s at
   most) until the registry is writable, else it ends with
   `registry_read_only` and no device imported (run it again).
4. The devices and groups, on the app thread: first the stored records
   the restore opened are loaded (records that were `foreign` or
   `unverified` at the start: a device of the same IEEE the hub
   interviewed is kept); then each imported device appears as the hub
   would have interviewed it, `interview: imported`; the registry is
   written at once (`stored`; the clients report `stored: false` as an
   error: the devices would be lost at the next restart). Meanwhile the
   hub does not import the NCP's own device tables (that would interview
   every device); afterwards they add nothing. An app thread that does not
   take them in 2 minutes ends the job with `timeout`; running the import
   again finishes it.

Nothing is sent to an imported device at import. When it is first heard
from (any frame, an announce, the answer to a probe):

- the same coordinator IEEE: the definition's configure is taken as done
  (the previous coordinator did it), the identity (DateCode, SWBuildID)
  and the states the hub does not know are read;
- another coordinator IEEE: the definition's configure runs, two awake
  devices at a time (a sleepy device that finds both turns taken tries
  again at its next wake; a sleeping one's requests wait for its wake and
  do not hold a turn);
- a record of an interview the source never finished (see the limits):
  the device is interviewed.

Mains devices not heard from are probed (a Basic read; `Active_EP_req`
for an unfinished record or one without Basic), one every 3 s, so two
hundred routers take ten minutes, not a burst. A probe without an
answer is repeated after 10, 20 and 40 minutes; after four the device
waits until it is heard from, and an unavailable device is not probed.
A device then shows `interview: complete`.

**Idempotent, resumable.** The same document again over the running
network skips the restore (`network.action: "same"`) and changes no
device the hub verified (`keep`; a missing name is taken); an imported
device not verified yet is replaced with the user's settings kept. A run
that stopped after the restore (power cut, lost session) is finished by
running it again; a hub that restarted meanwhile may have interviewed
some devices from the NCP's tables: those are kept. A restart while a
device is configured again verifies it again on its next contact.

## The import document (`zigbee-rs-hub/import` v1)

What every client (web UI, hubctl, the HA integration) produces from the
source's files; the hub reads nothing else. JSON:

```json
{
  "format": "zigbee-rs-hub/import", "version": 1,
  "source": {"kind": "z2m", "name": "Zigbee2MQTT (zigbee-herdsman@3.2.0)"},
  "network": {"metadata": {"format": "zigpy/open-coordinator-backup", "version": 1, "...": "..."}},
  "devices": [{
    "ieee": "0017880100f1e2d3", "nwk": 31248, "name": "Hall light", "area": "Hall",
    "type": "router", "manufacturer": "Signify Netherlands B.V.", "model": "LCT015",
    "manufacturer_code": 4107, "power_source": 1, "app_version": 2, "stack_version": 1,
    "hw_version": 1, "zcl_version": 1, "date_code": "20191218", "sw_build_id": "1.50.2_r30933",
    "node_descriptor": {"logical_type": 1, "mac_capabilities": 142, "manufacturer_code": 4107,
      "max_buffer_size": 82, "max_incoming_transfer": 82, "server_mask": 11264, "max_outgoing_transfer": 82},
    "endpoints": [{"id": 11, "profile": 260, "device_id": 269, "in": [0, 3, 4, 5, 6, 8, 768], "out": [25]}],
    "attributes": [{"ep": 11, "cluster": 768, "attr": 16394, "value": 31}],
    "interviewed": true, "source_id": 2
  }],
  "groups": [{"id": 4097, "name": "Hall", "members": [{"ieee": "0017880100f1e2d3", "ep": 11}]}]
}
```

| Field | Meaning |
|---|---|
| `network` | the Open ZigBee Coordinator Backup v1 (zigpy's and Z2M's file, `docs/device-support.md` 14.6 `network/restore`); required |
| `devices[].ieee` | 16 hex digits, most significant first; `0x` and `:`/`-` accepted; required |
| `nwk` | short address, a number or hex; a device without it takes the network part's `nwk_address`, else it is skipped (`no_nwk`) |
| `type` | `router`, `end_device`; `coordinator` and `green_power` are skipped |
| `name` | at most 64 characters; the hub's device name (a name the hub has wins) |
| `area` | informational: the hub has no areas (the HA integration applies them) |
| `manufacturer`, `model`, `manufacturer_code`, `power_source` (Basic 0x0007 number, or zigbee-herdsman's names), `app_version`, `stack_version`, `hw_version`, `zcl_version`, `date_code`, `sw_build_id` | the fingerprint (`docs/device-support.md` 3.2) |
| `node_descriptor` | when known: rx-on-when-idle and mains from `mac_capabilities`; without it routers and mains end devices listen, other end devices sleep |
| `endpoints[]` | `id` (1–240, 242), `profile`, `device_id`, `in`/`out` cluster lists; none: interviewed on first contact |
| `attributes[]` | cached values, only those the hub keeps (`persistent_value`), 64 at most |
| `interviewed` | `false`: the source's interview did not finish, the device is interviewed when heard from (so are records without `model` or with an endpoint without clusters); absent: as its record shows |
| `source_id` | informational |
| `groups[]` | `id` (1–0xFFF7), `name`, `members` [{`ieee`, `ep`}]; the coordinator's membership is the hub endpoint's |

## API

`docs/device-support.md` 14.6. Both requests need an API token on the hub
and take the document in requests of a few kilobytes: the first carries
`document` (its header, `network` — its `devices` may be left out —,
`groups`, maybe `devices`), the following ones any of `network_devices`
(OCB `devices[]` entries), `devices` and `groups`; `more: true` on all but
the last. The options go with the first. One staging at a time: it
belongs to its session and ends with its last request, when the session
closes, or 10 minutes after its last request (another session's request
meanwhile answers `busy`); staging is refused (`busy`) when the hub's heap
runs low. Clients send at most 6 KB of entries per request, the groups in
batches too (the hub takes 16 KiB frames).

| Type | Parameters | Result |
|---|---|---|
| `network/import/preview` | as above; options as `network/import` (they decide `needs`) | per request: `devices` (rows of its entries: `ieee`, `id`, `name`, `manufacturer`, `model`, `status` `new`/`replace`/`keep`/`skip`, `reason` of a skip: `green_power`, `coordinator`, `no_nwk`, `duplicate`, `invalid`; `definition` {`source`, `id`} or null for generic, `entities`, `endpoints`, `interview` `imported`/`on_first_contact`), `staged_devices`; the first: `network` {`ext_pan_id`, `pan_id`, `channel`, `coordinator_ieee`, `nwk_frame_counter`, `tclk_frame_counter`, `nwk_margin`, `aps_margin`, `devices`, `link_keys`, `hashed_tclk`, `stack`, `created`, `made_by`, `action` (`restore`, or `same`: the hub runs it already), `running` {`ext_pan_id`, `up`}, `coordinator` (`same`, `differs`, `unknown`: against the radio's IEEE now), `backend`, `needs` (`force`, `confirm`)}. The last also: `summary` {`new`, `replace`, `keep`, `skip`}, `groups`, `not_imported` {`count`, `devices` (the hub's devices the document does not list, at most 100), `removed`}, `restore_check` (`ok`, `not_needed`, or {`code`, `message`} of the backend's refusal, e.g. "set confirm_ieee_write_once"). Nothing changes |
| `network/import` | the document; `force` (the radio runs another network), `confirm` (EZSP: the records are another network's), `confirm_ieee_write_once`, `allow_ieee_change` (default true), `skip_backup`, `remove_missing` (forget the hub's devices the document does not list), `reconfigure` (`auto`, `always`, `never`) | rows as the preview for every request but the last; the last starts the job and answers `network` {`action` `restored`/`same`, `pan_id`, `channel`, `ext_pan_id`, `coordinator_ieee`, `devices`, and after a restore `nwk_fc_start`, `aps_fc_start`, `link_keys_on_ncp`, `link_keys_host_backed`, `children`, `notes` (as `network/restore`)}, `coordinator_changed`, `reconfigure`, `devices` {`new`, `replace`, `keep`, `skip`}, `failed`, `removed`, `groups`, `stored` (the registry written; `false` is reported as an error by the clients), `pending_verification`, `previous_network_backed_up`, `backup` (the replaced network's, with an API token) |

Errors: `unauthorized` (no API token), `network_required`,
`invalid_value` (not the format, a bad network part, "set force"),
`registry_read_only` ("set confirm"; over the running network: the
registry is not this network's, adopt or clear it; after a restore: it did
not become writable), `unsupported` (no restore on this NCP), `too_large`
(more than 500 devices, 256 groups, or 384 KiB of staged device and group
records), `backup_incomplete` (the running network's backup has no key:
`skip_backup`), `busy` (another job, an import being applied, another
session's staging, low memory), the backend's restore refusals
(`docs/ezsp-backend.md`), `timeout` (the devices not taken in time: run
the import again). A failure after the leave answers
the error with the replaced network's `backup`, as `network/restore`.
Event `network_updated` {`reason: "imported"`}. The device record's
`interview` is `imported` until the device is verified.

## Where the files are read, and why

The hub takes the normalized document only; the source's formats are read
where they are at home:

- **Zigbee2MQTT** (`database.db` is JSON lines, the backup JSON, the names
  YAML): in the browser (the web UI's migration module, loaded only when
  used: no cost in the page) and in `hubctl`.
- **ZHA** (`zigbee.db` is SQLite): in the Home Assistant integration
  (Python's `sqlite3`, on the machine that has the file, with HA's
  registries for names and areas) and in `hubctl` (SQLite compiled in).
  Not on the hub: an SQLite reader costs hundreds of kilobytes of
  firmware, and the file is on the HA machine anyway. Not in the browser:
  SQLite in WebAssembly is a megabyte.

One document format keeps the hub's part small (no YAML, no SQLite, no
per-source quirks in the firmware) and testable; each converter is tested
against generated files of its source (`tools/hubctl/src/import`,
`crates/hub-web/web/ui/mig.html`,
`integrations/home-assistant/custom_components/zigbee_rs_hub/migrate.py`).
