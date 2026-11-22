# Device support: generic ZCL, declarative definitions, self-description, Green Power

Design specification of 2026-11-19, with the user's decisions D1–D23 of the
same day. All open questions of the drafts are decided.
The definition format (sections 3.2–3.3, 6–9, 12.1, 13.3, 14.4) is
implemented in `crates/zdef` and `crates/zdefc` (host-tested; see
[`zdef.md`](zdef.md)); the rest is design only. Code fragments are
illustrations, not APIs, except where they name those crates.

Scope: how the hub (ESP32-S3, ZRHP host) turns joined Zigbee and Green Power
devices into entities, without reflashing the hub for new devices. It covers
the three support tiers, the definition format (YAML source, CBOR on the hub),
the expression language, Tuya EF00, the zigbee-rs self-description protocol,
Green Power, the entity model and its 1:1 mapping onto Home Assistant, the
WebSocket API surface, storage budgets, the zigbee-herdsman-converters
generator with measured coverage, testing, and open questions.

Sources and conventions:

- ZRHP = the zigbee-rs host protocol, `docs/book/src/advanced/host-protocol.md`
  in faronov/zigbee-rs (cited as "ZRHP 7.11" etc.). GP NCP state:
  `docs/book/src/advanced/green-power.md` there ("GP doc").
- ZCL = Zigbee Cluster Library revision 8. Cluster and attribute IDs below were
  checked against the zigbee-reference knowledge base and zigbee-herdsman
  (`zspec/zcl/definition/cluster.ts`).
- zhc = zigbee-herdsman-converters (MIT). Device facts cite the zhc source file
  and model; the coverage numbers in section 17 were measured on zhc 26.130.0
  (the newest npm release on 2026-11-19; git master was 26.134.0, whose
  sources were used for the examples).
- Hex IDs are `0xHHHH`. "Hub" is the S3 application, "NCP" the H2 running the
  zigbee-rs coordinator NCP.
- Normative words (must, should, may) apply to the hub implementation.

## Contents

1. [Decisions and goals](#1-decisions-and-goals)
2. [Architecture](#2-architecture)
3. [Device lifecycle: interview, match, entities](#3-device-lifecycle-interview-match-entities)
4. [Entity model and the Home Assistant mapping](#4-entity-model-and-the-home-assistant-mapping)
5. [Tier a: generic ZCL](#5-tier-a-generic-zcl)
6. [Tier b: declarative definitions](#6-tier-b-declarative-definitions)
7. [Expression language](#7-expression-language)
8. [CBOR encoding, versioning and hashing](#8-cbor-encoding-versioning-and-hashing)
9. [Catalog: banks, signatures, on-demand fetch, OTA, user definitions](#9-catalog-banks-signatures-on-demand-fetch-ota-user-definitions)
10. [Tuya EF00](#10-tuya-ef00)
11. [Example: an Efekta-style settings device](#11-example-an-efekta-style-settings-device)
12. [Self-description for zigbee-rs devices](#12-self-description-for-zigbee-rs-devices)
13. [Green Power devices](#13-green-power-devices)
14. [WebSocket API surface](#14-websocket-api-surface)
15. [Boundary to a Home Assistant integration](#15-boundary-to-a-home-assistant-integration)
16. [Storage and RAM budgets](#16-storage-and-ram-budgets)
17. [herdsman generator and measured coverage](#17-herdsman-generator-and-measured-coverage)
18. [Testing strategy](#18-testing-strategy)
19. [Open questions](#19-open-questions)
20. [Appendix: measured sizes of the examples](#20-appendix-measured-sizes-of-the-examples)

---

## 1. Decisions and goals

User decisions (2026-11-19) this spec implements:

| # | Decision |
|---|---|
| D1 | Three tiers, none needs a hub reflash: **(a)** generic ZCL auto-exposure with automatic reporting and bindings (ZHA-like, no definition data); **(b)** declarative device definitions (data, not code) matched on manufacturer/model and fingerprints, with manufacturer-specific attributes, Tuya EF00 datapoints, custom commands/events and a small bounded expression language compiled to bytecode; **(c)** definitions are authored in YAML, compiled to deterministic, schema-versioned, content-hashed CBOR; a generator translates the declarative part of zigbee-herdsman-converters with a coverage report. |
| D2 | WASM plugins are a later option. The format reserves a hook (8.5, 6.9); this spec does not define it. |
| D3 | The hub stores a curated catalog in the converter bank (2 × 256 KiB on 4 MB, A/B with atomic switch). Definitions for unknown devices are fetched on demand from a signed definitions repository. User definitions uploaded through the WebSocket API take precedence over built-ins. |
| D4 | zigbee-rs devices describe their own settings and entities over the air (self-description), so the hub needs no external definition for them. |
| D5 | The entity model maps 1:1 onto Home Assistant platforms, device classes, units, state classes and entity categories, so a later HA custom integration over the WebSocket API is a thin bridge. |
| D6 | Green Power devices (switches and sensors) are first-class: identity, generic GP tier, declarative GP definitions, commissioning through the WebSocket API. The NCP runs the GP sink (`NCP_SINK`); the hub handles the GPD commands. |
| D7 | Budgets: ~150–300 B per simple definition; ~1 600–2 000 compressed definitions per 256 KiB bank; definitions loaded lazily per device; the index in PSRAM. |

User decisions (2026-11-19, second batch), answering the first draft's open
questions:

| # | Decision | Where |
|---|---|---|
| D8 | **Manufacturer code 0x6D6B for everything zigbee-rs**: Zigbee devices (Node descriptor, manufacturer-specific frames), Green Power devices (Manufacturer ID), the self-description and settings clusters. It replaces the 0x1234 placeholder of the zigbee-rs products. 0x6D6B is **not registered with the CSA** (it is not in zigbee-herdsman's `ManufacturerCode` table, checked 2026-11-19); it is already used as GP Manufacturer ID by DIYRuZ FlowerX GP and EfektaLab BGT1 GP. **CSA registration is not planned** (D17). Caveat that remains: the CSA could assign 0x6D6B to another member at any time; frames with this code would then be ambiguous for stacks that key behaviour on the code (the hub itself keys self-description on cluster 0xFC6B plus a valid SchemaFormat, 12.3, and GP models on Manufacturer ID + Model ID, so it would only need catalog entries for such a device). Cluster IDs: Device Schema **0xFC6B**, zigbee-rs Settings **0xFC6C** (12.2). | 12, 13.3 |
| D9 | **Definitions repository `faronov/zigbee-rs-definitions`**, separate from the hub. The catalog (bank files, manifest, index shards) is published as **GitHub Releases** assets. The Ed25519 signing key lives in that repository's **GitHub Actions secrets**; the user owns it. | 9.3, 9.4 |
| D10 | **64 KiB local-definition area** on 4 MB boards (user uploads, fetched definitions, self-description schemas); free space in `hubdata` becomes 160 KiB. | 9.2, 16.1 |
| D11 | **Self-description above the catalog**; a catalog definition can still demote a known-buggy firmware (`self_description: under` / `ignore`). | 3.3, 12.5 |
| D12 | **4 MB bank curation by popularity, fully translated definitions only** (no partial definitions in the bank). | 17.2 |
| D13 | **Vendor template order: Hue light, then Lumi, then Tuya specifics** (schedule codecs, time-sync variants). | 6.6, 17.4 |
| D14 | **GP sink security level stays 0x00**, with a UI warning on every GPD that is commissioned unsecured (or below its definition's `security_min`). | 13.1, 13.7 |
| D15 | **YAML is compiled client-side** (`zdefc` CLI and its WebAssembly build in the UI); **the hub accepts CBOR only**. | 14.4 |
| D16 | GP facts from the user's ZHA quirks become definitions (FlowerX GP, BGT1 GP, zigbee-rs GP sensor and switch), and the **GP model-ID space under 0x6D6B** is tracked in an allocation table. Every accepted GP report refreshes `last_seen` and the entity states even if the values are unchanged. | 13.3, 13.6 |

User decisions (2026-11-19, third batch), closing the remaining questions:

| # | Decision | Where |
|---|---|---|
| D17 | **No CSA registration of 0x6D6B is planned.** The collision caveat of D8 stays documented. | D8, 12.2 |
| D18 | **Popularity for the 4 MB curation = model mentions in Zigbee2MQTT / ZHA GitHub issues**, counted by `tools/herdsman-coverage/popularity.py` (method in 17.2). | 17.2 |
| D19 | **FlowerX GP / BGT1 GP report intervals are unknown**: they keep the 25 h unavailability default for GP sensors; any definition can set `gp.report_interval_s` or `device.availability.timeout_s` to change it. | 4.6, 13.6 |
| D20 | **8 MB layout approved**: banks 2 × 576 KiB, local definitions 128 KiB. | 16.2 |
| D21 | The other proposals of the drafts are accepted: on-demand fetch on by default (with a setting); availability defaults of 4.6; zhc expose property names as entity keys; the HA integration's domain and distribution are decided when it is built. | 4.6, 9.3, 15, 17.2 |
| D23 | **GP bindings (2026-11-19):** a GPD button or command can be bound to a device endpoint or a group with a ZCL command and payload. Two executors, one per binding: **(a) on-device**, programmed into a target that is a GP sink with CT-based commissioning and a Translation Table (gpsFunctionality bits 11 and 18) through ZRHP remote GP provisioning (GP Pairing Configuration + GP Translation Table Update); **(b) hub-side virtual binding**, the hub turning the NCP's `EVT_GP_FRAME` into ZCL commands to any light or group, including proxy-only lamps. Automatic choice, user override, listing, removal, drift repair, persistence in `hubdata`; GP event entities always still fire. | 13.8, 14.5 |
| D22 | **Source formats (2026-11-19):** the canonical form in the definitions repository is **strict YAML 1.2** (core schema); `zdefc` also accepts **JSON, JSONC and JSON5** (one data model, one JSON Schema, identical CBOR). JSON5 is the alternative for authors who prefer explicit typing: it has comments, hexadecimal numbers and no implicit booleans. Catalog CI normalises contributed JSON/JSON5 files to the canonical YAML. | 6.1, 14.4 |
| D24 | **S3 layouts (2026-11-20):** 8 MB boards are the product (two 3 MiB app slots, A/B OTA, `hubdata` 1 920 KiB); 4 MB samples get a development layout (one 2 944 KiB app slot, no OTA, `hubdata` 1 MiB holding the 896 KiB areas of 16.1); no 1.5 MiB app target. | 16.1, 16.2, 16.5 |

Design goals derived from these:

- **One entity runtime, three sources of entities.** Generic rules,
  definitions and self-descriptions all produce the same entity records
  (section 4). There is no second code path per tier.
- **Data never runs unbounded code.** Every computation in a definition is
  either a built-in behaviour selected by name from a closed, versioned set,
  or a verified bytecode expression with a static size and step bound.
- **Stable identities.** An entity's unique ID depends only on the device
  identity, endpoint and entity key, never on which tier produced it, so a
  catalog update, a new definition or a firmware update does not orphan HA
  entities.
- **Forward compatible.** Definitions carry the schema version and the
  hub features they need (`requires`); an older hub skips what it cannot
  run and falls back to the next layer instead of misbehaving.

Non-goals (v1): arbitrary JavaScript, porting herdsman's custom converters
one by one, Zigbee Light Link touchlink, hub-side automations beyond what
event entities need (GP bindings, D23, are a bounded binding table, not a
rule engine), an HA integration itself (only its boundary, 15).

## 2. Architecture

```text
                    ESP32-S3 hub application
 +---------------------------------------------------------------------+
 |  WebSocket JSON API (14)          catalog OTA / fetch (9)           |
 |      |   ^ state/events                ^                            |
 |      v   |                             |                            |
 |  +----------------+   resolve   +--------------+   load   +--------+ |
 |  | Device manager |-----------> |  Resolver    |--------->| Def.   | |
 |  | (registry,     |  fingerprint|  (3.3 layers)|  lazily  | store  | |
 |  |  interview,    |<----------- |              |<---------| (16)   | |
 |  |  availability) |  resolved   +--------------+   CBOR   +--------+ |
 |  +----------------+  entity set        |                            |
 |      |   ^                              v                           |
 |      v   |                     +-------------------+                |
 |  +----------------------+      | Entity runtime    |  bytecode VM   |
 |  | ZCL / EF00 / GP codec |<--->| (bindings, value  |  (7), named    |
 |  | (frames <-> values)   |     |  transforms, state)|  codecs (6.6)  |
 |  +----------------------+      +-------------------+                |
 |      |   ^                                                          |
 |      v   | ZRHP v1 (UART 460 800): APS_DATA_REQUEST / EVT_APS_DATA_  |
 |          |   INDICATION, ZDO over APS, EVT_DEVICE_*, GP_* / EVT_GP_* |
 +----------|----------------------------------------------------------+
            v
       ESP32-H2 NCP: Zigbee PRO coordinator + Trust Center + GP combo
       (proxy, NCP sink, TempMaster)
```

Responsibilities:

| Component | Does | Does not |
|---|---|---|
| NCP (ZRHP) | Network, security, APS, ZDO server, GP sink (commissioning, security, counters, de-duplication, Sink Table) | Interpret ZCL application payloads; translate GPD commands (no translation endpoint, ZRHP 7.11) |
| Device manager | Device registry, interview, fingerprint, availability, sleepy-device send queue, configure (bind/report) | Know device-specific behaviour |
| Resolver | Choose and merge layers (generic, catalog, self-description, user) into one resolved entity set per device | Talk to devices |
| Entity runtime | Decode frames into entity states, encode commands, run expressions and named codecs, keep last state | Store definitions |
| Definition store | Banks, index, local definitions, self-description cache, signature checks | Decide precedence |

Two rules keep this bounded on the S3:

1. Definitions are **resolved and compiled once per device** (at join, at
   boot from the persisted resolution, and when a layer changes). The runtime
   then works on small per-device tables; the CBOR is dropped.
2. Frame dispatch is table-driven: `(ieee, endpoint, cluster, attribute or
   command)` → list of entity bindings. No per-frame search through
   definitions.

## 3. Device lifecycle: interview, match, entities

### 3.1 Join and interview (Zigbee devices)

Triggered by `EVT_DEVICE_JOINED` with `decision` admitted, by an unknown
`Device_annce` (forwarded as `EVT_APS_DATA_INDICATION`, ZRHP 7.2), or by a
user "re-interview" request.

| Step | Request (over `APS_DATA_REQUEST`, ZDO on profile 0) | Stored |
|---|---|---|
| 1 | Node_Desc_req | logical type, manufacturer code, MAC capabilities (RxOnWhenIdle), max buffer sizes |
| 2 | Active_EP_req | endpoint list |
| 3 | Simple_Desc_req per endpoint (skip 242 = GP) | profile, device ID, version, input and output clusters |
| 4 | Basic (0x0000) Read Attributes on the first endpoint with Basic: 0x0004 ManufacturerName, 0x0005 ModelIdentifier, 0x0007 PowerSource, then 0x0000 ZCLVersion, 0x0001 ApplicationVersion, 0x0002 StackVersion, 0x0003 HWVersion, 0x0006 DateCode, 0x4000 SWBuildID (one request per ≤ 5 attributes; UNSUPPORTED_ATTRIBUTE tolerated) | identity strings and versions |
| 5 | Resolve (3.3) with what is known so far | candidate definition |
| 6 | Definition-requested reads (e.g. Tuya "magic packet", 10.4) | — |
| 7 | If a server cluster is the zigbee-rs Device Schema cluster (12): read the schema hash, fetch the schema if not cached | self-description |
| 8 | Final resolve, materialize entities (4), configure (5.3): binds, reporting, initial reads, IAS enrollment | resolved entity set, configure hash |

Sleepy end devices (Node descriptor RxOnWhenIdle = 0) are awake for a few
seconds after joining; steps 1–8 run back to back with
`APS_DATA_REQUEST.tx_options` bit5 (extended timeout). What does not finish
is kept in a per-device **pending queue** and sent when the device next talks
to the hub (any indication from it, a Poll Control Check-in, 5.3), like
herdsman's `sendPolicy: "queue"`. Settings writes from the UI to sleepy
devices use the same queue; the entity shows the requested value as
`pending` until the write response arrives.

A device whose interview cannot complete gets tier-a entities from what is
known (for example only the Node descriptor and Simple descriptors) and a
`interview: partial` flag; resolution reruns when more is learned.

### 3.2 Fingerprint

The fingerprint is the canonical, hashable record of the interview:

```text
Fingerprint {
  manufacturer_name: Option<String>, model_id: Option<String>,
  mfr_code: u16, logical_type, power_source, rx_on_when_idle,
  app_version, stack_version, hw_version, zcl_version, date_code, sw_build_id,
  endpoints: [(ep, profile, device_id, in_clusters[], out_clusters[])],
  self_description: Option<SchemaHash>,
}
```

Green Power devices have their own fingerprint from the commissioning
payload (13.2). The fingerprint hash (SHA-256 over its canonical CBOR, first
16 bytes; the CBOR is a map with keys 0 manufacturer_name, 1 model_id,
2 mfr_code, 3 logical_type, 4 power_source, 5 rx_on_when_idle,
6 app_version, 7 stack_version, 8 hw_version, 9 zcl_version, 10 date_code,
11 sw_build_id, 12 endpoints as `[ep, profile, device_id, [in], [out]]`
sorted by endpoint, 13 self_description; absent fields omitted) is stored with the resolution; a different hash after a
re-interview or a rejoin with a new firmware triggers re-resolution.

### 3.3 Resolution: layers and precedence

Each device is resolved into one entity set by merging **layers** from
bottom (lowest precedence) to top:

| Order | Layer | Source | Present when |
|---:|---|---|---|
| 1 | Generic | Tier-a rules (5) over the fingerprint's clusters | always (unless a higher layer sets `generic.disable`) |
| 2 | Catalog | Best-matching catalog definition: bank (built-in) or fetched-on-demand cache (9) | a definition matches |
| 3 | Self-description | The device's own schema (12) | the device publishes one and no higher layer disables it |
| 4 | User definition | Definition uploaded through the API (14.4); matched like catalog definitions | the user uploaded one that matches |
| 5 | Device overrides | Per-device user settings: names, disabled entities, option values, a pinned definition | the user changed something |

Precedence rules:

- **Choosing the definition for layers 2 and 4.** Match clauses (6.2) are
  scored by specificity; the highest score wins:

  | Score | Clause content |
  |---:|---|
  | 600 | GP: SrcID / IEEE range plus DeviceID, or manufacturer ID + model ID (13.3) |
  | 500 | manufacturer name exact AND model exact |
  | 400 | model exact AND manufacturer prefix (`{prefix: _TZE2}`) |
  | 300 | model exact only (herdsman `zigbeeModel`) |
  | 200 | fingerprint only: endpoints/clusters/device IDs, optionally Node descriptor manufacturer code |
  | +0…+50 | each extra constraint (app version range, SW build pattern, power source) adds 10 |

  Shapes the table does not list: manufacturer exact + model prefix = 400;
  any other names-only combination (model prefix, manufacturer only) = 250;
  a GP clause without DeviceID and without manufacturer + model ID = 100. Extra
  constraints (+10 each, at most +50): `mfr_code` (part of the base for
  fingerprint-only clauses), each version range, `sw_build`, `power`,
  `logical_type`, and `endpoints` when names are present. GP clauses take
  the table of 13.3 without extras (the FlowerX clause, DeviceID +
  manufacturer + masked model, scores 600). A clause without any field never
  matches.

  Ties: the higher explicit `priority` field, then the user source over the
  catalog, then the newer `revision`. A definition may be **pinned** per device
  (layer 5) to override scoring.
- **Self-description over catalog** (decided, D11). A device's own schema is generated from
  its firmware and versioned by hash, so it is more precise than a catalog
  entry written for some firmware. A catalog definition can still win where a
  firmware is known to describe itself wrongly: `self_description: ignore`
  (drop layer 3) or `self_description: under` (put layer 3 below layer 2).
- **User over everything published.** A user definition replaces the catalog
  definition for the matched device (it does not merge with it); it merges
  with generic and self-description like a catalog definition would.
- **Merging** is by entity **key** within an endpoint:
  - a higher layer entity with an existing key **replaces** the lower one
    (fields not given are *not* inherited, except when the layer uses
    `generic.patch`, which is an explicit field-level patch);
  - `generic.exclude: [key...]` removes generic entities, `generic.disable:
    true` drops layer 1, `generic.only_endpoints` restricts it;
  - entities with new keys are added;
  - device-level fields (`device:` power, battery curve, check-in) are
    field-wise overridden by higher layers.
- **Requires.** A definition whose `requires` (8.5) lists a feature this hub
  build lacks is skipped as if it did not match, and the device's diagnostics
  say why (`definition_skipped: requires codec.tuya_schedule_day`).

### 3.4 Materialization and stability

The merged set is compiled into the per-device runtime tables and persisted
in the host store journal as a **resolution record**: layer sources (ids,
revisions, content hashes), the fingerprint hash, the configure hash, and the
entity table (key, platform, endpoint, binding, flags). At boot the hub
restores the record and only reloads the referenced definitions (lazily, on
the first frame or API access); it does not re-resolve unless a layer changed.

Re-resolution happens when: a referenced definition's hash changed (catalog
OTA, user upload), a better match appears (new catalog), the fingerprint
changed, the self-description hash changed, or the user asks. Effects:

- entities with unchanged key keep their unique ID and state;
- new keys appear. A key that vanishes while the fingerprint stayed the
  same (a catalog installed or updated, a user definition uploaded or
  deleted, a pin) is **removed**: `entity_removed`, and HA deletes it. A key
  that vanishes because the fingerprint changed (re-interview, firmware
  update) becomes **orphaned**: kept with `available: false` and
  `orphaned: true` until the user deletes it (its endpoint may come back);
- a generic Identify button exists once per device (the lowest endpoint
  with the cluster) unless a definition declares per-endpoint ones; an
  endpoint name is added to an entity's name only when the same key exists
  on several endpoints (herdsman endpoint maps name every endpoint);
- configure (binds, reporting) is redone only if the configure hash (the
  canonical CBOR of all binds, reporting configs and configure ops) changed.

## 4. Entity model and the Home Assistant mapping

### 4.1 Device record

| Field | Zigbee | Green Power | HA device registry |
|---|---|---|---|
| `id` | `zb:<ieee>` (16 hex) | `gp:<srcid>` (8 hex) or `gp:<ieee>:<ep>` | `identifiers: {(DOMAIN, id)}` |
| `ieee` / `src_id` | IEEE | SrcID or IEEE + endpoint | `connections: {("zigbee", ieee)}` (Zigbee only) |
| `nwk` | short address (API 1.1) | null | (diagnostic) |
| `manufacturer` | definition `vendor`, else Basic 0x0004 | definition `vendor`, else manufacturer ID as hex | `manufacturer` |
| `model` | definition `model`, else Basic 0x0005 | definition `model`, else "GPD 0x<device id>" | `model` |
| `model_id` | Basic 0x0005 | `0x<mfr>/0x<model>` | `model_id` |
| `sw_version` / `hw_version` | Basic 0x4000 / 0x0003 | — | `sw_version` / `hw_version` |
| `name` | user name, else `model` | same | `name` (user-editable in HA) |
| `via` | the hub | the hub | `via_device` |
| `power` | mains / battery / energy_harvesting | battery / energy_harvesting | (diagnostic) |
| `availability`, `last_seen`, `lqi`, `rssi` | 4.6 | 4.6 | device availability |
| `warnings` | e.g. configure failures | `gp_unsecured` (13.1) | repair issues (15) |
| `definition` | resolved layer sources and revisions | same | diagnostics only |

### 4.2 Entity record

```json
{
  "uid": "zb:00158d0001a2b3c4:1:temperature",
  "device": "zb:00158d0001a2b3c4", "endpoint": 1, "key": "temperature",
  "platform": "sensor",
  "device_class": "temperature", "unit": "°C", "state_class": "measurement",
  "category": null, "enabled_default": true,
  "name": null, "translation_key": "temperature",
  "precision": 1,
  "capabilities": {},
  "source": "generic"
}
```

- `uid` = `<device id>:<endpoint>:<key>` is the HA `unique_id` (stable, 3.4).
  GP devices use endpoint 242.
- `key`: snake_case, unique per device and endpoint. Generic keys are fixed
  by section 5 (`temperature`, `humidity`, `battery`, `light`, …); keys on
  several endpoints are disambiguated in names, not keys (the endpoint is in
  the uid). `endpoints: {2: left}` in a definition only changes the display
  name ("Switch left").
- `name: null` means HA derives the name from `device_class` or
  `translation_key` ("has_entity_name" style); a definition can give `name`.
- `capabilities` carries the platform-specific static data HA needs (4.3).
- `source`: generic, catalog, self_description, user (diagnostics only).

### 4.3 Platforms

Every entity has exactly one HA platform. The platform defines the state
type, the commands, and the `capabilities` fields.

| Platform | State | Commands | Capabilities (HA attribute) | Typical sources |
|---|---|---|---|---|
| `sensor` | number, string, enum label, timestamp | — | `options` (enum), `suggested_display_precision` | attributes, DPs, struct tags, GP reports |
| `binary_sensor` | bool | — | — | IAS zone bits, occupancy, DP bool |
| `number` | number | `set_value` | `native_min_value`, `native_max_value`, `native_step`, `mode` (auto/box/slider) | writable attributes, DPs |
| `select` | label | `select_option` | `options` | enum attributes, DPs |
| `switch` | bool | `turn_on`, `turn_off` | — | On/Off (non-light), bool settings |
| `button` | — | `press` | — | commands, one-shot writes |
| `text` | string | `set_value` | `native_min`, `native_max`, `pattern`, `mode` | string attributes, schedules |
| `light` | on/off + brightness, color mode, color temp, xy/hs, effect | `turn_on` (brightness, transition, color_temp_kelvin, xy_color, hs_color, effect), `turn_off` | `supported_color_modes`, `min/max_color_temp_kelvin`, `effect_list`, `supported_features` (transition, effect) | 0x0006 + 0x0008 + 0x0300 |
| `cover` | position, tilt, open/closed/opening/closing | `open_cover`, `close_cover`, `stop_cover`, `set_cover_position`, `set_cover_tilt_position` | `supported_features` | 0x0102, DPs |
| `climate` | hvac_mode, hvac_action, current/target temperatures, preset, fan_mode | `set_hvac_mode`, `set_temperature`, `set_preset_mode`, `set_fan_mode` | `hvac_modes`, `min_temp`, `max_temp`, `target_temperature_step`, `preset_modes`, `fan_modes`, `supported_features`, `temperature_unit` | 0x0201, DPs |
| `lock` | locked/unlocked/jammed/locking/unlocking | `lock`, `unlock`, `open` | `supported_features` | 0x0101, DPs |
| `fan` | on/off, percentage, preset | `turn_on`, `turn_off`, `set_percentage`, `set_preset_mode` | `speed_count`, `preset_modes`, `supported_features` | 0x0202 |
| `event` | — (fires) | — | `event_types` | received commands, GPD commands, DP actions |
| `siren` | on/off | `turn_on` (tone, duration, volume), `turn_off` | `available_tones`, `supported_features` | 0x0502 IAS WD |
| `valve` | position or open/closed | `open_valve`, `close_valve`, `set_valve_position` | `reports_position`, `supported_features` | DPs, 0x0006 on valves |
| `update` | installed/latest version | `install` | `supported_features` | 0x0019 OTA (later) |

Composite platforms (`light`, `cover`, `climate`, `lock`, `fan`, `siren`,
`valve`) are built from **parts**: each HA property is its own binding
(`parts:` in a definition, 6.3). Part names are the HA property names
(`hvac_mode`, `target_temperature`, `current_temperature`, `hvac_action`,
`preset`, `brightness`, `color_temp`, `position`, `tilt`, …), so the
mapping to HA is mechanical.

### 4.4 Device classes, units, state classes, categories

The hub uses **HA's own strings** for these and stores them as small
integers in CBOR (8.3). The tables are copied from HA's constants
(`SensorDeviceClass`, `BinarySensorDeviceClass`, `NumberDeviceClass`,
`ButtonDeviceClass`, `EventDeviceClass`, `CoverDeviceClass`,
`SwitchDeviceClass`, `ValveDeviceClass`; `UnitOf*` values; `SensorStateClass`;
`EntityCategory`), and the generator fails a build if a definition uses a
string not in the table. Rules:

- A `sensor` with a numeric state and a `state_class` must have a unit that HA
  accepts for its device class (HA rejects for example `°C` on `humidity`);
  the compiler checks the pairs against an HA-derived table.
- Energy and gas meters use `total_increasing` (or `total` with a reset
  attribute); power, temperature, humidity and similar use `measurement`.
- `category: config` for writable settings (numbers, selects, switches,
  buttons that configure the device); `category: diagnostic` for read-only
  device health (battery voltage, LQI, RSSI, firmware counters). Primary
  measurements and controls have no category.
- `enabled: false` sets HA's `entity_registry_enabled_default = False`
  (noisy diagnostics such as LQI, voltage when a percentage exists).

### 4.5 State updates and commands

State message (WebSocket, 14.3):

```json
{"uid": "zb:00158d0001a2b3c4:1:temperature", "state": 21.37,
 "attributes": {}, "available": true,
 "last_changed": 1795098123.402, "last_reported": 1795098123.402}
```

- `state` is the native value in the entity's unit (after transforms), or
  `null` for unknown (an invalid raw value, a failed expression).
- `last_reported` changes on every report; `last_changed` only when the value
  changes (HA semantics). The hub never suppresses a report with an
  unchanged value, because HA statistics and "last seen" depend on it: every
  received report (ZCL attribute report, Tuya DP report, GP attribute report)
  produces an `entity_state` message for each entity it carries, and updates
  the device's `last_seen` (D16; required for GP sensors such as the BGT1 GP,
  which report the same temperature for hours).
- Commands are HA service names with HA's parameter names
  (`{"uid": …, "command": "set_temperature", "params": {"temperature":
  21.5}}`). The hub validates them against the capabilities, encodes them
  through the binding, and answers with the ZCL/EF00 status.

### 4.6 Availability

| Device | Rule (defaults, per device overridable) |
|---|---|
| Mains Zigbee (router, RxOnWhenIdle) | Unavailable after 10 min without any frame **and** a failed active check (read Basic 0x0000 ZCLVersion, 3 tries over 30 s). Checks start after 10 min of silence. |
| Battery Zigbee | Unavailable after `max(2.5 × longest configured max reporting interval, 2.5 × check-in interval, 25 h)` without any frame. No active checks. |
| GP sensor with a known report interval | Unavailable after 3 × `gp.report_interval_s` without an accepted GPDF (13.6). |
| GP sensor without a known interval | Unavailable after 25 h without an accepted GPDF (D19; FlowerX GP and BGT1 GP today). |
| Any device, per definition | `device.availability.timeout_s` replaces the computed timeout; `gp.report_interval_s` sets 3 × that for GP sensors. |
| GP switch or energy harvesting | Never unavailable. `last_seen` only. |

`available` is per device; HA marks all its entities. `last_seen`, `lqi` and
`rssi` (from `EVT_APS_DATA_INDICATION` / `EVT_GP_FRAME`) are device fields and
also exposed as disabled-by-default diagnostic sensors (`linkquality`,
`rssi`, `last_seen` as a timestamp sensor).

### 4.7 Events

`event` entities fire `{event_type, attributes}`. HA's `event` platform
requires the full `event_types` list at entity creation, so every event
binding declares it (definitions) or derives it (generic rules from the
commands of the client cluster, 5.4; GP from the GPD command list, 13.4).
Event attributes are flat JSON scalars (button number, duration, raw
command ID for diagnostics). Device triggers in HA can be derived from
`event_types` by the integration (15).

## 5. Tier a: generic ZCL

Tier a runs for every Zigbee device and needs no data. Rules match server
(input) clusters, or client (output) clusters for remotes, on an endpoint,
and produce entities with fixed keys. A rule fires only if the cluster is in
the Simple descriptor; optional attributes are created only after a
successful read or report (an UNSUPPORTED_ATTRIBUTE read removes them).

### 5.1 Server-cluster rules

| Cluster | Condition | Entities (key: platform, device class, unit; ZCL attribute) | Notes |
|---|---|---|---|
| 0x0006 On/Off | endpoint device ID is a light (HA profile 0x0100, 0x0101, 0x0102, 0x010C, 0x010D; ZLL profile 0xC05E 0x0000, 0x0100, 0x0200, 0x0210, 0x0220) or 0x0300 is present | `light`: light (part on_off 0x0000) | merged with 0x0008/0x0300 into one light |
| 0x0006 On/Off | otherwise | `switch`: switch (device class `outlet` for device IDs 0x0009 mains power outlet, 0x0051 smart plug, 0x010A on/off plug-in unit; ZLL 0x0010) | |
| 0x0006 | attribute 0x4003 StartUpOnOff readable | `power_on_behavior`: select config {0 off, 1 on, 2 toggle, 255 previous} | |
| 0x0008 Level | with a light | light part brightness (0x0000 CurrentLevel, commands Move to Level with On/Off 0x04) | |
| 0x0008 | 0x0011 OnLevel, 0x0010 OnOffTransitionTime readable | `on_level`, `transition`: number config | disabled by default |
| 0x0300 Color | with a light | light parts: color_temp (0x0007, mireds ↔ kelvin), xy (0x0003/0x0004), hs (0x0000/0x0001, enhanced 0x4000), color_mode 0x0008 | color modes from ColorCapabilities 0x400A; limits from 0x400B/0x400C |
| 0x0402 Temperature | | `temperature`: sensor temperature °C, measurement; 0x0000 int16 / 100, invalid 0x8000 | |
| 0x0405 Humidity | | `humidity`: sensor humidity %, measurement; 0x0000 uint16 / 100, invalid 0xFFFF | |
| 0x0403 Pressure | | `pressure`: sensor pressure hPa; 0x0000 int16 (0.1 kPa = 1 hPa); if 0x0010 ScaledValue and 0x0014 Scale exist: ScaledValue × 10^Scale kPa → hPa | |
| 0x0400 Illuminance | | `illuminance`: sensor illuminance lx; 0x0000 uint16, lx = 10^((x − 1) / 10000), 0 → 0, 0xFFFF invalid | |
| 0x0406 Occupancy | | `occupancy`: binary_sensor occupancy; 0x0000 bit 0. `occupancy_timeout`: number config s, 0x0010 PIROccupiedToUnoccupiedDelay | |
| 0x040D CO2 (and 0x040C–0x042B concentration clusters) | | `co2`: sensor carbon_dioxide ppm; 0x0000 single × 10^6 | the other concentration clusters map to their HA device class (pm25, pm10, volatile_organic_compounds, carbon_monoxide …) where HA has one, else a sensor with the unit only |
| 0x042A PM2.5 | | `pm25`: sensor pm25 µg/m³; 0x0000 single | |
| 0x0408 Soil moisture | | `soil_moisture`: sensor moisture %; 0x0000 / 100 | |
| 0x0001 Power Configuration | battery powered (Node descriptor / Basic 0x0007) | `battery`: sensor battery %, 0x0021 / 2 (0.5 %), invalid 0xFF; `battery_voltage`: sensor voltage V, diagnostic, disabled, 0x0020 / 10 | definitions may derive % from voltage (`device.battery`) |
| 0x0002 Device Temperature | | `device_temperature`: sensor temperature °C, diagnostic, disabled; 0x0000 int16 | |
| 0x0500 IAS Zone | | binary sensors from ZoneType 0x0001: 0x000D motion → `motion`; 0x0015 contact → `contact` (door); 0x0028 fire → `smoke`; 0x002A water → `water_leak` (moisture); 0x002B CO → `carbon_monoxide`; 0x002D vibration → `vibration`; 0x0225/others → `alarm` (safety). Plus `tamper` (bit 2, diagnostic), `battery_low` (bit 3, battery, diagnostic) | state from ZoneStatus 0x0002 and Zone Status Change Notification (command 0x00), bit 0 Alarm1; enrollment 5.3 |
| 0x0502 IAS WD | | `siren`: siren (Start Warning 0x00, Squawk 0x01) | |
| 0x0201 Thermostat | | `thermostat`: climate; parts current_temperature 0x0000, target_temperature 0x0012 (heat) / 0x0011 (cool), hvac_mode 0x001C SystemMode, hvac_action from 0x0029 RunningState, min/max from 0x0015/0x0016; `local_temperature_calibration`: number config 0x0010 int8 / 10 °C; `pi_heating_demand`: sensor % 0x0008 | hvac modes from ControlSequenceOfOperation 0x001B |
| 0x0204 Thermostat UI | | `keypad_lockout`: select config 0x0001; `temperature_display_mode`: select config 0x0000 | |
| 0x0202 Fan Control | | `fan`: fan; FanMode 0x0000 {0 off, 1 low, 2 medium, 3 high, 4 on, 5 auto, 6 smart}, presets from FanModeSequence 0x0001 | |
| 0x0102 Window Covering | | `cover`: cover; position from 0x0008 CurrentPositionLiftPercentage, tilt from 0x0009; commands 0x00 Up/Open, 0x01 Down/Close, 0x02 Stop, 0x05 Go To Lift Percentage, 0x08 Go To Tilt Percentage | HA position = 100 − ZCL percentage (ZCL 0 % = open); `invert: true` in a definition flips it for devices that report the other way |
| 0x0101 Door Lock | | `lock`: lock; state 0x0000 LockState {0 not fully locked → jammed, 1 locked, 2 unlocked}; commands 0x00 Lock, 0x01 Unlock; `lock_event`: event from Operation Event Notification 0x20 | PIN management is out of scope for v1 |
| 0x0702 Metering | | `energy`: sensor energy kWh, total_increasing, 0x0000 CurrentSummationDelivered (uint48) × Multiplier 0x0301 / Divisor 0x0302, unit from 0x0300 UnitOfMeasure (0 kWh; 1 m³ → gas/water volume); `power`: sensor power W from 0x0400 InstantaneousDemand (int24, same scaling, kW → W) if no 0x0B04 | |
| 0x0B04 Electrical Measurement | | `power` W 0x050B ActivePower × 0x0604/0x0605; `voltage` V 0x0505 RMSVoltage × 0x0600/0x0601; `current` A 0x0508 RMSCurrent × 0x0602/0x0603; `power_factor` % 0x0510; `frequency` Hz 0x0300 × 0x0400/0x0401 | multipliers/divisors read once at configure and cached; definitions may fix them (`innr-sp120.yaml`) |
| 0x0003 Identify | | `identify`: button identify, config | |
| 0x0020 Poll Control | | — (behaviour, 5.3) | |
| 0x000F Binary Input, 0x000C Analog Input, 0x0012 Multistate Input | | `binary_input` binary_sensor 0x0055 PresentValue; `analog_input` sensor 0x0055 (single); `multistate_input` sensor 0x0055 | names come from 0x001C Description if present; low value without a definition, created disabled |
| 0x0B05 Diagnostics | | diagnostic sensors for a few counters, disabled | |
| 0x0019 OTA | | `update` (later) | |

Device-level entities for every device: `linkquality`, `rssi` (diagnostic,
disabled), `last_seen` (timestamp, diagnostic, disabled).

### 5.2 Reporting defaults

Configured on every reportable attribute a rule exposes, on the endpoint that
has it. `min`/`max` in seconds, `change` in raw attribute units. Modelled on
ZHA's `REPORT_CONFIG_*` and herdsman's `reporting.*` presets; a definition
can change any of them (`report: [min, max, change]`), or set `report: off`.

| Attribute(s) | min | max | change |
|---|---:|---:|---:|
| On/Off 0x0006/0x0000 | 0 | 900 | — |
| Level 0x0008/0x0000 | 1 | 900 | 1 |
| Color temp 0x0300/0x0007, X/Y 0x0003/0x0004, hue/sat | 1 | 900 | 1 |
| Temperature 0x0402/0x0000 | 30 | 900 | 50 (0.5 °C) |
| Humidity 0x0405/0x0000 | 30 | 900 | 100 (1 %) |
| Pressure 0x0403/0x0000 | 30 | 900 | 1 |
| Illuminance 0x0400/0x0000 | 30 | 900 | 500 |
| Occupancy 0x0406/0x0000 | 0 | 900 | — |
| CO2 0x040D/0x0000 | 30 | 900 | 0.000025 (25 ppm) |
| Battery % 0x0001/0x0021 | 3600 | 21600 | 2 (1 %) |
| Battery voltage 0x0001/0x0020 | 3600 | 21600 | 1 |
| ActivePower 0x0B04/0x050B | 5 | 900 | 10 (raw) |
| RMSVoltage 0x0505, RMSCurrent 0x0508 | 5 | 900 | 10 / 10 |
| CurrentSummationDelivered 0x0702/0x0000 | 10 | 900 | 1 |
| InstantaneousDemand 0x0702/0x0400 | 5 | 900 | 1 |
| Thermostat LocalTemperature 0x0201/0x0000 | 30 | 900 | 10 |
| Thermostat setpoints, SystemMode, RunningState | 0 | 900 | 1 / — |
| PIHeatingDemand 0x0201/0x0008 | 30 | 900 | 5 |
| Window covering 0x0102/0x0008, 0x0009 | 1 | 900 | 1 |
| Door lock state 0x0101/0x0000 | 0 | 900 | — |
| Fan mode 0x0202/0x0000 | 0 | 900 | — |

For battery devices the hub caps `max` at the device's check-in or 6 h,
whichever is shorter, so availability (4.6) has a signal. A failed Configure
Reporting (UNSUPPORTED_ATTRIBUTE, UNREPORTABLE_ATTRIBUTE) is recorded and the
entity is polled instead only if it is a primary measurement on a mains
device (every `max` seconds); sleepy devices are never polled.

### 5.3 Configure: binds, reporting, enrollment, sleepy devices

At configure (and when the configure hash changes), per endpoint:

1. **Bind** every server cluster that has a reported attribute, and every
   client cluster whose commands become events (5.4), to the hub's endpoint
   1 (`Bind_req`, destination = the coordinator IEEE, endpoint 1). Remotes
   that only send to groups get, in addition, a hub group: the hub adds its
   endpoint to group 0x0000 + n (`NWK_GROUP_MEMBERSHIP`) when a definition
   asks for it (`configure: - group: …`).
2. **Configure Reporting** (ZCL 0x06) per attribute, at most 3 records per
   frame so responses fit without fragmentation.
3. **Read** the initial value of every exposed attribute (≤ 5 per Read).
4. **IAS Zone enrollment:** write 0x0500/0x0010 IAS_CIE_Address = coordinator
   IEEE, answer Zone Enroll Request (command 0x01) with Zone Enroll Response
   (0x00, code 0 success, zone ID allocated per device), and read
   0x0000 ZoneState / 0x0001 ZoneType.
5. **Poll Control** (0x0020): bind, write 0x0000 Check-in Interval if a
   definition asks; answer Check-in (0x00) with Check-in Response
   (start fast poll = true only while the pending queue is non-empty,
   timeout 0x0028), then send the queue, then Fast Poll Stop (0x01).
6. Definition `configure:` ops (6.8) after the generic ones.

Every step is idempotent and its result is recorded per attribute, so a
failed configure is retried (mains: after 60 s, then exponential up to 1 h;
sleepy: on the next contact) without repeating what worked.

### 5.4 Client clusters → events (remotes)

On endpoints whose client (output) cluster list contains On/Off, Level,
Scenes, Color Control, Window Covering or IAS ACE, and which are bound to the
hub, received cluster commands become one `action` event entity per device
(keys `action`, endpoint in attributes), with event types:

| Cluster/command | Event types |
|---|---|
| 0x0006: 0x00 Off, 0x01 On, 0x02 Toggle, 0x40 Off with effect, 0x42 On with timed off | `off`, `on`, `toggle`, `off`, `on` |
| 0x0008: 0x01/0x05 Move (with on/off), 0x02/0x06 Step, 0x03/0x07 Stop, 0x00/0x04 Move to level | `brightness_move_up`/`_down` (mode byte 0/1), `brightness_step_up`/`_down`, `brightness_stop`, `brightness_move_to_level` (attribute `level`) |
| 0x0005: 0x05 Recall scene | `recall_<scene id>` for the scene IDs listed by the definition, else `recall_scene` with attribute `scene` |
| 0x0300: 0x4B Move color temp, 0x4C Step color temp, 0x0A Move to color temp | `color_temperature_move_up`/`_down`/`_stop`, `color_temperature_step_up`/`_down`, `color_temperature_move` |
| 0x0102: 0x00, 0x01, 0x02 | `open`, `close`, `stop` |

Event payload attributes are the decoded command fields (step size,
transition time, rate). Duplicates (the same APS counter and ZCL sequence
within 2 s, from retries) are dropped.

### 5.5 Knobs for definitions

Generic entities can be patched per key without redefining them (6.3):

```yaml
generic:
  exclude: [power_on_behavior]   # drop
  only_endpoints: [2]            # restrict
  patch:
    current: {div: 1000}         # field-level patch, e.g. fixed divisor
    temperature: {report: [60, 3600, 20]}
    occupancy_timeout: {max: 65535}
  disable: false                 # true = no generic entities at all
```

---

## 6. Tier b: declarative definitions

### 6.1 File and top-level structure

Source formats (D22): the canonical form is one YAML document per
definition; `zdefc` also accepts the same structure as JSON, JSONC or JSON5
(`.json`, `.jsonc`, `.json5`). All of them are parsed into one data model,
validated by one JSON Schema and compiled to identical CBOR. JSON5 keeps
comments and hexadecimal numbers (`0x0402`) without YAML's implicit typing;
multi-line expressions are written with `\` line continuations there.
The rules below apply to the YAML form.

One YAML document per definition (YAML 1.2 core schema; `on`, `off`, `yes`,
`no` must be quoted where they are values, and are never keys; the core
schema has no signed hexadecimal, so negative values are written in decimal,
e.g. `invalid: -32768` for 0x8000 of an int16). `zdefc` accepts only plain
scalars that YAML 1.1 and 1.2 parsers read the same way: null (`null`, `~`,
empty), `true`/`false`, integers `-?(0|[1-9][0-9]*)` and `0x…`, floats
`-?(0|[1-9][0-9]*).[0-9]+`; it rejects as ambiguous `y/n/yes/no/on/off` in
any case, leading-zero and `0o`/`0b` integers, `_` separators, exponents,
`+` signs, sexagesimal numbers, `.inf`/`.nan` and timestamps (quote them).
Anchors, aliases, tags, merge keys, duplicate keys and several documents
per file are errors. JSON files are RFC 8259; JSONC adds comments and
trailing commas; JSON5 is the full JSON5 grammar. Object keys that must be
integers (`map`, `endpoints`, `bitmap_flags`) are written as `"0"` or
`"0x10"` in JSON. The schema is `schema/zdef-v1.schema.json` (JSON Schema
2020-12, generated by `zdefc schema`). Examples are
in `definitions/examples/`.

| Key | Type | Required | Meaning |
|---|---|---|---|
| `zdef` | int | yes | schema major version, `1` (8.5). A self-description fragment uses `zdef_fragment: 1` and has no `match`. |
| `id` | string | yes | stable ID `vendor/model-slug`, `[a-z0-9._/-]{3,64}`; user definitions use `user/...` |
| `revision` | int | yes | increases with every published change of this `id` |
| `vendor`, `model` | string | yes | display identity (HA device manufacturer/model) |
| `description` | string | no | stripped from catalog banks (fetched with the UI help) |
| `match` | list | yes | match clauses, OR-ed (6.2) |
| `white_label` | list | no | `{vendor, model, manufacturer: [names]}`; selects the display identity by the device's manufacturer name |
| `requires` | list | no | hub features needed (8.5); computed by the compiler, may be written |
| `priority` | int | no | tie-break between equal scores (3.3) |
| `device` | map | no | `power` (mains/battery/energy_harvesting), `sleepy`, `battery` (`voltage_to_percent: [min_mV, max_mV]` linear, or a named curve `3V_2100`, `3V_1500_2800`, `CR2032`), `checkin_s`, `availability` (`timeout_s`), `ota` (bool) |
| `generic` | map | no | tier-a knobs (5.5) |
| `endpoints` | map | no | endpoint display names `{1: left, 2: right}` |
| `self_description` | enum | no | `ignore` / `under` / `over` (default `over`, 3.3) |
| `configure` | list | no | extra configure ops (6.8) |
| `ignore` | list | no | `{ep?, cluster, cmd?}`: frames that must not create generic events or warnings |
| `entities` | list | no | entity definitions (6.3) |
| `tuya` | map | no | EF00 flags (10.3) |
| `gp` | map | no | Green Power device fields (13.5) |
| `x-wasm` | map | no | **reserved** (6.9) |

### 6.2 Match clauses

A clause is a map; all its fields must match (AND); a definition matches if
any clause matches (OR).

| Field | Value | Compared with |
|---|---|---|
| `manufacturer` | string, `{any: [...]}`, `{prefix: "..."}` | Basic 0x0004, exact, case-sensitive, trailing NULs and spaces trimmed |
| `model` | same forms | Basic 0x0005 |
| `mfr_code` | int | Node descriptor manufacturer code |
| `app_version`, `stack_version`, `hw_version` | `{min, max}` | Basic 0x0001 / 0x0002 / 0x0003 |
| `sw_build` | string or `{prefix}` | Basic 0x4000 |
| `power` | mains / battery | Basic 0x0007 / Node descriptor |
| `logical_type` | coordinator / router / end_device | Node descriptor logical type |
| `endpoints` | `{<ep>: {profile, device_id, in: [...], out: [...]}}`; `in`/`out` must be subsets | Simple descriptors |
| `gp` | 13.3 | GP commissioning data |

Indexing (9.1): each clause yields one or more **index keys**: `(manufacturer,
model)` pairs for exact names (one key per name of an `any` list), `(*,
model)` for model-only and prefix clauses, `(manufacturer, *)` for
manufacturer-only clauses, and `(mfr code, profile, device ID)` of the
lowest fully specified endpoint for fingerprint-only clauses. Prefix and
range constraints are re-checked after the index lookup.

### 6.3 Entities

Common fields:

| Field | Meaning |
|---|---|
| `key` | entity key (4.2) |
| `platform` | 4.3 |
| `device_class`, `unit`, `state_class`, `category`, `name`, `enabled` | 4.4 |
| `ep` | endpoint; default: the lowest endpoint whose Simple descriptor has the bound cluster (server for attributes, either for commands) |
| `access` | letters `r` read, `w` write, `p` report/push; defaults per platform: sensor/binary_sensor `rp`, number/select/switch/text `rwp`, button `w` |
| `report` | `[min, max, change]` or `off`. Generic entities get the 5.2 defaults; definition entities get Configure Reporting only when `report` is given (`p` in `access` only says that the device pushes the value) |
| binding | exactly one of `zcl`, `tuya`, `struct`, `gp`, `hub`, or `parts` / `triggers` / `press` (below) |
| transforms | `mul`, `div`, `offset`, `precision`, `invalid`, `map`, `value_on`/`value_off`, `from`, `to`, `codec` (6.4) |
| limits | `min`, `max`, `step`, `mode` (number, text) |
| `reset_after` | expression (seconds); for binary sensors without a "clear" report: set back to `false` after that time (Aqara example) |
| `present_if` | expression over the fingerprint (`gpd.*`, `fp.*`); the entity exists only if true |

Bindings:

| Binding | Fields | Read path | Write path |
|---|---|---|---|
| `zcl` | `cluster`, `attr`, `type` (ZCL type name: `bool`, `u8`…`u48`, `i8`…`i32`, `enum8`, `enum16`, `map8`, `map16`, `f32`, `str`, `ostr`), `mfr` (manufacturer code), `ep` | Read Attributes Response, Report Attributes (any endpoint/direction rules of ZCL), Write Attributes Response status | Write Attributes (0x02) with the given type and mfr code |
| `tuya` | `[dp, type]`; type `raw`, `bool`, `value`, `string`, `enum`, `bitmap` | EF00 dataResponse / dataReport (10) | EF00 dataRequest (or `sendData`, 10.3) |
| `struct` | `cluster`, `attr`, `mfr`, `codec` (named struct codec, 6.6), `tag` | a struct attribute decoded by the codec; value = the tagged field | — |
| `gp` | `{report: {cluster, attr, type, mfr?}}` or `{cmd: 0xNN}` | GPD attribute reports (0xA0–0xA3) or a GPD command payload (13) | — (bidirectional GPD writes are later) |
| `hub` | `{default: value}` | value stored by the hub (device option, e.g. a calibration only the hub applies) | stored by the hub; usable in expressions as `opt.<key>` |
| `parts` | map part name → binding + transforms | per part | per part |
| `triggers` (event) | list of `{cmd \| gp_cmd \| tuya, when?, event, attrs?}` | received frame → `when` → `event` expression → fire | — |
| `press` (button) | `{cmd: {cluster, id, mfr?, ep?}, payload?: expr}` or `{write: {cluster, attr, type, value, mfr?}}` or `{tuya: [dp, type], value}` | — | sends it |

### 6.4 Value transforms and their order

Decode (device → state), applied in this order; each step is skipped if not
given:

1. `codec` (named codec decodes bytes into a value, 6.6) or the binding's
   native decode (ZCL type, Tuya type);
2. `invalid`: raw value equal to it → state `null`;
3. `map` (raw → label) for select, enum sensors, climate modes; a raw value
   not in the map → `null` and a diagnostic counter;
4. `value_on` / `value_off` for bool platforms (default: non-zero = on);
5. `from` expression (7), with `x` = the value so far;
6. `mul`, `div`, `offset`: `x * mul / div + offset` (exact integer ratio,
   evaluated in f64);
7. `precision`: round to that many decimals (display hint for HA too).

Encode (command → device) runs the inverse: `precision` is ignored;
`(v − offset) * div / mul`, rounded to the attribute type; then `to`
expression if given (required when `from` is not invertible); `map` and
`value_on/off` are inverted automatically; then type checks: the value must be
within `min`/`max` and the ZCL type range, else the command fails with
`INVALID_VALUE` before anything is sent.

### 6.5 Example: one setting

```yaml
- key: temperature_offset
  platform: number
  category: config
  device_class: temperature
  unit: °C
  zcl: {cluster: 0x0402, attr: 0x0410, type: i16}
  div: 10
  min: -50
  max: 50
  step: 0.1
```

Encoded: 56 bytes of CBOR (8.2 key table; 21 of them are the key string). With the attribute report
`0x0402/0x0410 = int16 -15` the state is `-1.5`; a `set_value 2.3` writes
int16 23.

### 6.6 Named codecs and built-in behaviours

Some formats are too irregular for a bounded expression but common enough to
implement once in the hub. They are selected by name and parameters and form
a **closed, versioned set**; adding one needs a hub release, and definitions
that use it declare it in `requires` (8.5). v1 set:

| Name | Parameters | What it does | Users |
|---|---|---|---|
| `zcl_tagged` | — | sequence of `(tag u8, ZCL type u8, value)` → map tag → value | Lumi 0xFCC0/0x00F7, genBasic 0xFF01 (`aqara-rtcgq14lm-motion-p1.yaml`) |
| `tuya_schedule_day` | `day`, `transitions`, `variant` (`multi_dp` default, `trv602z`) | Tuya per-day schedule DP with the day number in byte 0 ↔ text `HH:MM/T …` (`trv602z`: `HH:MM/T/P …`, P = 2 eco, 3 comfort) (herdsman `thermostatScheduleDayMultiDPWithDayNumber`, `…_TRV602Z_WithDayNumber`) | Tuya TRVs |
| `tuya_schedule_week` | `variant` (required), `transitions`, `scale`, `min`, `max` | schedule DPs without a day number: `multi_dp` = `thermostatScheduleDayMultiDPWithTransitionCount` (byte 0 = 0, then `hour, minute, temp×scale u16 BE` per transition; defaults 4 transitions, scale 10, 5–35 °C); `single_dp` = `thermostatScheduleDaySingleDP` (day bitmask byte, up to 10 × `10-min segment, temp×10 u16 BE`, padded with `144,0,180`; the text starts with the day selector `monday`…`sunday`, `mon_sun`, `mon_fri`); `trv602z` (6 × `0xC0|min>>8, min, preset<<4, temp×10`); `trv603wz` (`7, count, (hour×10 + min/10, temp×2)*`) | Tuya TRVs |
| `bcd_time`, `hhmm` | — | packed times | timers |
| `ascii`, `utf16be_hex` | — | strings in raw DPs (herdsman `utf8String`, `utf16BEHexString`) | Tuya |
| `bitmap_flags` | `{bit: label}` | bitmap → list of labels (text sensor) | fault codes |

Built-in behaviours (not codecs) that definitions switch on by flags: Tuya
time sync and magic packet (10.3), occupancy auto-reset (`reset_after`),
battery percent from voltage (`device.battery`), IAS enrollment, Poll Control
handling. The herdsman generator maps herdsman's named helpers to these where
semantics match (17.2).

### 6.7 Events and buttons

Event entity (`philips-rwl022-dimmer.yaml`):

```yaml
- key: action
  platform: event
  device_class: button
  event_types: [on_press, on_hold, ..., recall_0, recall_1]
  triggers:
    - cmd: {ep: 1, cluster: 0xFC00, id: 0x00, mfr: 0x100B}
      event: >-
        lookup(payload[0], {1: "on", 2: "up", 3: "down", 4: "off"}) + "_" +
        lookup(payload[4], {0: "press", 1: "hold", 2: "press_release", 3: "hold_release"})
      attrs: {hold_time: "payload[6]"}
```

- `cmd` matches cluster, command ID, manufacturer code (if given, the frame
  must carry it), endpoint (if given), and by default either direction.
- `payload` is the ZCL payload after the ZCL header.
- `event` must evaluate to one of `event_types`; anything else is dropped and
  counted. `when` (bool) filters before.
- Built-in de-duplication: same source, ZCL sequence number and command
  within 2 s → one event.

Button with a write (Efekta) or a command (zigbee-rs `reset_settings`):

```yaml
press: {write: {cluster: 0x040D, attr: 0x0202, type: bool, value: true}}
press: {cmd: {cluster: 0xFC6C, id: 0x00, mfr: 0x6D6B}, payload: 'pack("u8", 1)'}
```

### 6.8 Configure ops

```yaml
configure:
  - bind: {ep: 1, clusters: [0x0006, 0x0008, 0xFC00, 0x0001]}
  - write: {ep: 1, cluster: 0x0000, attr: 0x0031, type: map16, value: 0x000B, mfr: 0x100B}
  - read: {ep: 2, cluster: 0x0406, attrs: [0x0030], mfr: 0x100B}
  - report: {ep: 1, cluster: 0x0402, attr: 0x0000, type: i16, report: [10, 600, 10]}
  - cmd: {ep: 1, cluster: 0xEF00, id: 0x03}     # e.g. Tuya dataQuery (prefer tuya.query)
  - cmd: {cluster: 0xFC11, id: 0x01, payload: 'pack("u8", opt.sensitivity ?? 2)'}
  - group: {ep: 1, group: 0x0100}               # hub joins a group the remote sends to
  - checkin: {interval_qs: 14400}               # Poll Control 0x0020/0x0000, quarter-seconds
```

A `cmd` op's optional `payload` (sub-map key 62) is an expression yielding
bytes. It is evaluated when the configure plan is built, with `opt.*` and
`state.*` but without `x`, `payload` or time functions, so the op's
idempotence key (which includes the payload bytes) is stable; a payload that
fails to evaluate records the op as failed. In the canonical expression
order (constant pool) configure payloads come after `generic.patch` and
before `entities` (key order 10, 13, 14).

Ops run in order after the generic configure (5.3). Each op is idempotent;
failures are recorded, never fatal. The set is closed (no loops, no
conditions); the generator produces it by recording herdsman `configure`
functions against a mock device (17.2).

### 6.9 Reserved: WASM hook

`x-wasm: {module: "sha256:<hex>", exports: {decode: "...", encode: "..."},
fuel: n, memory_pages: n}` and the binding kind `wasm: {export: "..."}` are
reserved. A hub without WASM support treats any definition that `requires:
[wasm]` as non-matching (3.3), so later WASM-carrying definitions degrade to
the generic tier on older hubs. Semantics, sandbox limits and signing of
modules are out of scope here.

---

## 7. Expression language

A small pure language for value transforms (`from`, `to`), event selection
(`event`, `when`), timers (`reset_after`), payload construction (`payload`)
and fingerprint conditions (`present_if`). Design limits: no loops, no
recursion, no user functions, no mutation, no I/O, forward-only jumps;
programs are verified before use and their cost is bounded by their length.

### 7.1 Types and semantics

| Type | Values | Notes |
|---|---|---|
| `int` | i64 | checked arithmetic: overflow → `null` |
| `float` | f64 | software f64 on the S3 is fine at these rates |
| `bool` | true / false | |
| `str` | UTF-8, ≤ 64 bytes at run time | literals, `lookup` results, `+` concatenation, `str()` |
| `bytes` | read-only view of the frame payload or a raw value; ≤ 64 bytes when built by `pack` | |
| `null` | unknown | propagates through arithmetic; `??` and `isnull()` test it |

- `int op int` stays `int`, except `/` which always yields `float` (`//` is
  floor division, `%` floor modulo). Mixed `int`/`float` → `float`.
- Comparisons between `int` and `float` compare numerically; `==` across other
  types is `false`.
- Bit operations need `int`; shifts are 0..63 (else `null`); `>>` is
  arithmetic.
- `&&`, `||`, `!` take `bool` (or `null`, treated as false); expressions are
  pure, so both sides may be evaluated.
- Every run-time error (out-of-range byte access, wrong type, overflow,
  division by zero) makes the whole expression `null`, and increments a
  per-entity error counter shown in diagnostics. It never aborts the hub.
- The compiler type-checks statically (variable types per context, function
  signatures), so most errors are caught when a definition is built or
  uploaded.
- Details fixed by the implementation: `==`/`!=` never yield `null`
  (`null == null` is true); `<`, `<=`, `>`, `>=` with a `null` operand yield
  `null` and also compare strings bytewise; `<<` drops bits shifted out
  (only the shift count is checked); `?:` takes the else branch on `false`
  or `null`; a non-finite float result is a run-time error; `round(x)`,
  `floor`, `ceil` and `int` return `int` (rounding half away from zero),
  `round(x, d)` returns `float`; `str(float)` prints at most 15
  significant digits, rounded, trailing zeros dropped, as a plain decimal
  for 1e-6 ≤ |x| < 1e21 and as `d.ddde±N` otherwise (JavaScript layout;
  `str(0.1 + 0.2)` = `"0.3"`, `str(21.0)` = `"21"`), so the VM links no
  float formatter; `min`/`max` take 2–8 arguments; built-ins return `null` when an
  argument is `null` (except `isnull`).

### 7.2 Grammar

```ebnf
expr      = let_expr | cond ;
let_expr  = "let" ident "=" cond { "," ident "=" cond } "in" expr ;
cond      = coalesce [ "?" expr ":" expr ] ;
coalesce  = or { "??" or } ;
or        = and { "||" and } ;
and       = compare { "&&" compare } ;
compare   = bitor [ ( "==" | "!=" | "<" | "<=" | ">" | ">=" ) bitor ] ;  (* no chaining *)
bitor     = bitxor { "|" bitxor } ;
bitxor    = bitand { "^" bitand } ;
bitand    = shift { "&" shift } ;
shift     = sum { ( "<<" | ">>" ) sum } ;
sum       = term { ( "+" | "-" ) term } ;
term      = unary { ( "*" | "/" | "//" | "%" ) unary } ;
unary     = ( "-" | "!" | "~" ) unary | postfix ;
postfix   = primary { "[" expr "]" } ;               (* byte at index *)
primary   = number | string | "true" | "false" | "null"
          | name | call | "(" expr ")" ;
name      = ident [ "." ident ] ;                     (* x, payload, state.k, opt.k, gpd.f, fp.f *)
call      = ident "(" [ arg { "," arg } ] ")" ;
arg       = expr | map_literal ;                      (* map literal only for lookup() *)
map_literal = "{" [ literal ":" literal { "," literal ":" literal } ] "}" ;
number    = decimal | "0x" hex | float ;
string    = '"' { char } '"' ;
```

Precedence follows Rust, not C: bit operators bind tighter than
comparisons, so `x & 0x10 == 0x10` means `(x & 0x10) == 0x10`; comparisons
do not chain. At most 8 `let` names per expression; a `let` name must not
reuse a context variable name (7.3).

### 7.3 Variables by context

| Name | Type | Available in |
|---|---|---|
| `x` | the binding's decoded value (`int`, `float`, `bool`, `str`, `bytes`) | `from`, `to` (the value being written) |
| `payload` | `bytes` | event `when`/`event`/`attrs`, `gp {cmd}` bindings, `raw` DPs, `press.payload` |
| `state.<key>` | the current state of another entity of the same device | everything except `present_if`; read-only, no cycles (the compiler orders entities and rejects cycles) |
| `opt.<key>` | value of a `hub` binding entity | everything except `present_if` |
| `last` | `str`/`null`: the entity's previous event type | event `when`/`event` |
| `gpd.device_id`, `gpd.manufacturer_id`, `gpd.model_id`, `gpd.src_id` | `int` | GP definitions, `present_if` |
| `fp.app_version`, `fp.hw_version`, `fp.mfr_code`, `fp.sw_build`, `fp.stack_version`, `fp.zcl_version`, `fp.date_code`, `fp.manufacturer`, `fp.model` | `int`/`str` (names trimmed of trailing spaces and NULs) | `present_if` |

### 7.4 Built-in functions

| Group | Functions |
|---|---|
| bytes | `len(b)`, `b[i]` (u8), `u8(b,o)`, `i8(b,o)`, `u16le/u16be/i16le/i16be(b,o)`, `u24le/…/i24be(b,o)`, `u32le/…/i32be(b,o)`, `f32le/f32be(b,o)`, `ascii(b,o,n)`, `hex(b)`, `pack(fmt, args…)` with a literal `fmt` of `u8 i8 u16le u16be i16le i16be u24le u32le u32be i32le i32be` items (result ≤ 64 bytes) |
| bits | `bit(x,n)` → bool, `bits(x,lo,len)` → int, `sext(x,bits)`, `bcd(x)` |
| math | `min`, `max`, `clamp(x,lo,hi)`, `abs`, `round(x[,digits])`, `floor`, `ceil`, `int(x)`, `float(x)`, `pow10(x)`, `log10(x)` |
| lookup | `lookup(x, {k: v, …}[, default])` (`null` if absent and no default) |
| strings | `str(x)` (int/float/bool to string), `+` on two strings |
| misc | `isnull(x)`, `battery_percent(mV)` (uses `device.battery`), `now()` (Unix seconds) and `tz_offset()` (seconds) — only in `to`/`press.payload` |

### 7.5 Bytecode

An expression compiles to a CBOR byte string: a stack-machine program for
one expression. All immediates follow the opcode; integers are LEB128
(zig-zag for signed). The definition carries one **constant pool** (top-level
key 17) shared by its expressions: floats, strings, `lookup` maps, `pack`
formats, state keys.

| Opcode | Mnemonic | Stack effect / immediate |
|---|---|---|
| 0x01 | `X` | push input `x` |
| 0x02 | `I8 v` | push small int −128..127 |
| 0x03 | `INT z` | push zig-zag LEB128 int |
| 0x04 | `CONST i` | push constant `i` |
| 0x05–0x07 | `NULL`, `TRUE`, `FALSE` | |
| 0x08 / 0x09 | `LDL n` / `STL n` | let-locals 0..7 |
| 0x0A | `STATE i` | push state of the entity whose key is constant `i` |
| 0x0B | `OPT i` | push hub option `i` |
| 0x0C | `PAYLOAD` | push payload bytes |
| 0x0D | `LAST` | push previous event type |
| 0x0E | `GPD f` / 0x0F `FP f` | push a GP / fingerprint field |
| 0x10–0x16 | `ADD SUB MUL DIV IDIV MOD NEG` | 2→1 (NEG 1→1) |
| 0x20–0x25 | `AND OR XOR BNOT SHL SHR` | bitwise |
| 0x30–0x38 | `EQ NE LT LE GT GE LNOT LAND LOR` | |
| 0x3C–0x3F | `JMP o`, `JZ o`, `JNN o`, `POP` | forward jumps by `o` bytes (u8); `JZ` pops; `JNN` jumps if top is not null and keeps it (for `??`) |
| 0x40 | `INDEX` | bytes, int → u8 |
| 0x48 | `LOOKUP i` | value → mapped value from constant map `i`, or null |
| 0x70 | `CALL f n` | built-in function `f` (u8) with `n` args |

There is no `END`: the program ends at the end of the byte string, and the
single value left on the stack is the result.

**Verifier** (runs on the hub on every definition load, also for the
compiler's output): opcodes known to this hub; immediates in range; every
jump forward and inside the program, landing on an opcode boundary; stack
depth computed along both branches of every jump, equal at merge points,
never below 0, never above 16, exactly 1 at the end; `CALL` arity matches the
function; constant indices and their types fit the opcode (`LOOKUP` needs a
map, `STATE` a string); program ≤ 255 bytes. Because jumps only go forward,
an expression executes at most `len` instructions; each instruction is
O(1) except `pack`/`+` on strings (≤ 64 bytes). Worst-case run time on the
S3: well under 10 µs.

Interpreter: a 16-slot value stack on the Rust stack (`enum Value {Null,
Bool, Int(i64), Float(f64), Str(&str), Bytes(&[u8])}`), strings and `pack`
results in a 512-byte arena inside the `Vm`; no heap use. The verifier also
tracks a type set per stack slot and rejects operations whose operands can
never have an accepted type (`"a" - 1`), variables and functions not
available at the site (7.3, `now()` outside `to`/`payload`), and reads of
`let` locals not stored on every path.

**Constant pool (canonical).** Pool entries are text strings, floats,
integers or `lookup` maps (`[[key, value], …]` in source order). Floats in
the pool are always encoded as floats or tag-4 fractions, never as
integers, so `x * 2.0` keeps float semantics. `event_types` and `attrs`
names are pool string indices too. The compiler fills the pool in one
canonical traversal, independent of the key order in the source: top-level
keys in key-table order (`generic.patch` entries in encoded-key order, then
`entities`); in an entity `from`, `to`, `reset_after`, `parts` (part-key
order, recursively), `event_types`, `triggers` (per trigger: `attrs` sorted
by name, `when`, `event`), `press.payload`, `present_if`, `attrs` (sorted by
name); inside one expression in code-emission order. Identical constants
share one entry. Code generation does no constant folding (`-5` is `I8 5;
NEG`).

### 7.6 Examples

| Source | Bytecode (hex) | Bytes |
|---|---|---:|
| `x == 1` | `01 02 01 30` | 4 |
| `x > 130536 ? 0 : x - 65536` (Aqara illuminance) | `01 03 d0 f7 0f 34 3d 04 02 00 3c 06 01 03 80 80 08 11` | 18 |
| `(state.detection_interval ?? 30) + 2` | `0a 00 3e 03 3f 02 1e 02 02 10` | 10 |
| `bit(payload[0], 0) && last != "press"` | `0c 02 00 40 02 00 70 00 02 0d 04 00 31 37` | 14 |
| `x == 0 ? 0 : round(pow10((x - 1) / 10000))` (FlowerX illuminance) | `01 02 00 30 3d 04 02 00 3c 0f 01 02 01 11 03 a0 9c 01 13 70 20 01 70 1b 01` | 25 |
| `u16le(payload, 2)` | `0c 02 02 70 07 02` | 6 |

(Output of `zdefc expr`, identical to `tools/zdefc-proto/zdefc.py expr`.
Constant indices depend on the definition's constant pool.)

---

## 8. CBOR encoding, versioning and hashing

### 8.1 Rules

- CBOR (RFC 8949) with **core deterministic encoding** (§4.2.1): preferred
  (shortest) integer and length encodings, definite lengths only, map keys
  sorted by their encoded bytes, no duplicate keys, no indefinite-length
  items, no tags except 4 (decimal fraction) and, in catalog form only,
  shared-string references (8.4). Integral numbers are integers. Other numbers (YAML
  decimals such as `0.1`) use the shorter of the shortest exact IEEE float
  (f16/f32/f64) and a tag-4 decimal fraction `[exponent, mantissa]`
  (exponent −1…−9); on a tie the float. `step: 0.1` is then 4 bytes
  (`c4 82 20 01`) instead of 9. The fraction is derived from the exact
  shortest round-trip decimal of the f64 (`0.30000000000000004` stays a
  float).
- All map keys are small unsigned integers from the key tables (8.2); unknown
  keys are an error for a hub of the same `zdef` major version and are
  ignored only inside the reserved range 64–127 ("optional extensions"),
  which is reserved in every map. Exceptions with other keys: `endpoints`
  and `map`-like integer maps (integer keys), `generic.patch` (entity keys
  as text), `attrs` (constant-pool indices).
- Enumerations (platform, device class, unit, state class, category, ZCL
  type, Tuya type) are integers (8.3).
- Expressions are byte strings (7.5).
- `map` transforms are flat arrays `[raw0, label0, raw1, label1, ...]`.

### 8.2 Key tables (v1)

Top level:

| Key | Field | Key | Field |
|---:|---|---:|---|
| 0 | zdef | 11 | endpoints |
| 1 | id | 12 | tuya |
| 2 | revision | 13 | configure |
| 3 | vendor | 14 | entities |
| 4 | model | 15 | gp |
| 5 | description | 16 | ignore |
| 6 | match | 17 | consts |
| 7 | white_label | 18 | source |
| 8 | requires | 19 | mfr |
| 9 | device | 20 | zdef_fragment |
| 10 | generic | 21 | templates |
| 22 | priority | 23 | self_description |
| 31 | x-wasm (reserved) | 64–127 | optional extensions (64 `x-partial`) |

Entity:

| Key | Field | Key | Field | Key | Field |
|---:|---|---:|---|---:|---|
| 0 | key | 13 | to | 26 | access |
| 1 | platform | 14 | mul | 27 | report |
| 2 | device_class | 15 | div | 28 | enabled |
| 3 | unit | 16 | offset | 29 | reset_after |
| 4 | state_class | 17 | precision | 30 | parts |
| 5 | category | 18 | invalid | 31 | event_types |
| 6 | name | 19 | map | 32 | triggers |
| 7 | ep | 20 | value_on | 33 | press |
| 8 | zcl | 21 | value_off | 34 | codec |
| 9 | tuya | 22 | min | 35 | present_if |
| 10 | struct | 23 | max | 36 | attrs |
| 11 | gp | 24 | step | 37 | hub |
| 12 | from | 25 | mode | | |

Entity key 38 is `invert` (cover direction, 5.1).

Sub-maps (bindings, match clauses, configure ops, device fields, codec
parameters, part names) share one table: `cluster 0, attr 1, type 2, mfr 3,
ep 4, id 5, attrs 6, clusters 7, value 8, codec 9, tag 10, cmd 11, report 12,
gp_cmd 13, when 14, event 15, write 16, any 17, prefix 18, manufacturer 19,
model 20, device_id 21, manufacturer_id 22, model_id 23, src_id 24, mask 25,
gp 26, bind 27, read 28, power 29, sleepy 30, battery 31,
voltage_to_percent 32, checkin_s 33, exclude 34, patch 35, disable 36,
only_endpoints 37, time_sync 38, magic_packet 39, query 40, security_min 41,
report_interval_s 42, vendor 43, tuya_schedule_day 44, day 45,
transitions 46, hvac_mode 47, hvac_action 48, target_temperature 49,
current_temperature 50, mfr_code 51, app_version 52, stack_version 53,
hw_version 54, sw_build 55, endpoints 56, profile 57, in 58, out 59, min 60,
max 61, payload 62, default 63`; 64–127 are extensions; then `group 128,
checkin 129, interval_qs 130, availability 131, timeout_s 132, ota 133,
ignore_commands 134, ieee 135, commands 136, server 137, client 138,
force_time_sync_s 139, bind_basic 140, gateway_status 141, send 142,
mcu_version 143, optimistic 144, dedup 145, interval_s 146, tuya 147,
zcl_tagged 148, tuya_schedule_week 149, variant 150, bcd_time 151,
hhmm 152, ascii 153, utf16be_hex 154, bitmap_flags 155, logical_type 156,
name 157, params 158, scale 159`, and the part names
`on_off 160, brightness 161, color_temp 162, color_xy 163, color_hs 164,
color_mode 165, effect 166, position 167, tilt 168, state 169, preset 170,
fan_mode 171, percentage 172, direction 173, oscillating 174,
target_temperature_low 175, target_temperature_high 176,
current_humidity 177, target_humidity 178, swing_mode 179, tone 180,
volume 181, duration 182, valve_position 183, running_state 184,
color_x 185, color_y 186, hue 187, saturation 188, command 189`. Keys
24–255 all take two bytes, so the gap costs nothing.

Named optional extensions (source key `x-<name>`, value any JSON, stored as
canonical CBOR with text keys): top level `64 x-partial` (the generator's
"partially translated" marker), match clause `64 x-priority` (zhc's
definition priority). Other keys 64–127 have no source form; the decoder
keeps them (top level, match clause, entity) so a decode/encode round trip
is lossless, and ignores them otherwise.

A decoder reads every sub-map with the one shared table, so a known key that
does not apply to that sub-map (e.g. `group` in a `zcl` binding) is ignored
by `decode` and rejected by the canonical check (`load_canonical`: the
re-encoded bytes differ).

A name matcher (`manufacturer`, `model`, `sw_build`) is a text string
(exact), an array (`any`; key 17 is not used in v1) or `{18: prefix}`.
A configure op, a `press` and a GP binding are one-entry maps keyed by
their kind. The authoritative tables for the hub and the compiler are
`crates/zdef/src/keys.rs` and `crates/zdef/src/enums.rs`; the CDDL for the
definitions repository (`schema/zdef-v1.cddl`) is to be generated from
them.

### 8.3 Enumerations

| Enum | Encoding |
|---|---|
| platform | 0 sensor, 1 binary_sensor, 2 number, 3 select, 4 switch, 5 button, 6 text, 7 light, 8 cover, 9 climate, 10 lock, 11 fan, 12 event, 13 siren, 14 valve, 15 update |
| device_class | index into one table of all HA device-class strings (union over platforms, ~110 entries), append-only |
| unit | index into one table of HA unit strings, append-only |
| state_class | 0 measurement, 1 total, 2 total_increasing, 3 measurement_angle |
| category | 1 config, 2 diagnostic |
| access | bitmask 1 read, 2 write, 4 report |
| ZCL type | the ZCL data type code (0x10 bool, 0x18 map8, 0x20 u8, … 0x29 i16, 0x30 enum8, 0x39 single, 0x41 octet string, 0x42 char string) |
| Tuya DP type | the Tuya code (0 raw, 1 bool, 2 value, 3 string, 4 enum, 5 bitmap) |
| power | 0 mains, 1 battery, 2 energy_harvesting (match clauses: 0, 1) |
| logical_type (match) | 0 coordinator, 1 router, 2 end_device |
| mode (number/text) | 0 auto, 1 box, 2 slider, 3 text, 4 password |
| self_description | 0 ignore, 1 under, 2 over |
| tuya.time_sync | 0 off, 1 1970, 2 2000 (source: `off`, `1970`/`2000` as integers or strings) |
| tuya.send | 0 dataRequest, 1 sendData |
| tuya.query | key 40: bitmask 1 configure, 2 announce; key 146 `interval_s` (source: a list of `configure`, `announce`, `{interval_s: N}`) |
| battery curve | 0 3V_2100, 1 3V_1500_2800, 2 CR2032 (or `{32: [min_mV, max_mV]}`) |
| codec | 0 zcl_tagged, 1 tuya_schedule_day, 2 tuya_schedule_week, 3 bcd_time, 4 hhmm, 5 ascii, 6 utf16be_hex, 7 bitmap_flags; a codec with parameters is `{code: {params}}` (6.6); `struct.codec` is the code |
| schedule variant | 0 multi_dp, 1 single_dp, 2 trv602z, 3 trv603wz |
| template name | 0 hue_light, 1 lumi, 2 tuya, 3 tuya_metering_zero_debounce (feature 20 + code) |
| `map` keys | integers; the source may also write `"true"`/`"false"` (1/0) |

Only `source` (free text) and names stay strings. Closed sets were text in
the draft of v1 and became integers before the v1 freeze.

Tables are append-only within a `zdef` major version; an index unknown to a
hub (newer table) makes the field "unknown" (the entity is still created,
without that device class or unit) and is reported in diagnostics.

### 8.4 Strings: standalone and catalog form

- **Standalone form** (user uploads, fetched definitions, self-descriptions,
  hashing): strings inline.
- **Tuya manufacturer names** of the form `_TZxxxx_` + 8 × `[0-9a-z]` (prefix
  from a fixed table: `_TZE200_`, `_TZE204_`, `_TZE284_`, `_TZ3000_`,
  `_TZ3210_`, `_TZE202_`, `_TYZB01_`, `_TZ3290_`, append-only) are always
  encoded as a 7-byte byte string: prefix index (u8) + the base-36 suffix as a
  48-bit big-endian integer. This is part of the canonical form. It cuts the
  ME167 definition by 171 bytes (19 packable names); together with the
  white-label indices below by 442 bytes (1 461 → 1 019 B in the prototype,
  1 024 B catalog form with `requires`, appendix 20).
- `white_label` manufacturer lists are indices into the definition's own
  `match` name list: the manufacturer names of all clauses in order (exact
  names and `any` lists). Packed names apply to every manufacturer matcher,
  exact or `any` (the prototype packed only `any` lists).
- **Catalog form** (inside a bank, 9.1): strings that occur in at least two
  definitions of the bank are replaced by a reference to entry *n* of the
  bank's string table, encoded like the shared-item references of "Packed
  CBOR" (draft-ietf-cbor-packed): simple value *n* for *n* < 16, tag 6 with
  *n* − 16 otherwise. The bank format version pins this encoding, whatever
  the draft's final numbering. Catalog form is derived mechanically from the
  standalone form and is never hashed.

### 8.5 Versioning

- `zdef` (key 0) is the schema **major** version. A hub supports a set of
  majors; a definition with an unsupported major is skipped. Additions that
  old hubs can safely ignore go into the optional extension range (keys
  64–127) without a major change.
- `requires` (key 8) lists **feature IDs** (small ints, append-only table,
  ascending in the CBOR): `1 expr-v1`, `2 tuya`, `3 gp`,
  `4 struct.zcl_tagged`, `5 codec.tuya_schedule_day`,
  `6 codec.tuya_schedule_week`, `7 codec.bcd_time`, `8 codec.hhmm`,
  `9 codec.ascii`, `10 codec.utf16be_hex`, `11 codec.bitmap_flags`,
  `20 template.hue_light`, `21 template.lumi`, `22 template.tuya`,
  `23 template.tuya_metering_zero_debounce` (vendor templates, C tier 17.4:
  `templates: [{name, params}]`, `params` free-form, key 21 at the top
  level), `60 wasm` (reserved). The compiler computes it from what the definition
  uses and merges names written in the source; the hub rejects a definition
  that uses a feature it does not list. A hub lacking any listed feature
  treats the definition as non-matching (3.3).
- `revision` orders versions of one `id`. The catalog index keeps only the
  newest revision per `id` that this hub can run (a catalog may carry an
  older revision for older hubs, selected by `requires`).
- The bytecode version is tied to `expr-v1`; a new opcode or built-in is a new
  feature ID (`expr-v2`).

### 8.6 Hashing

`content_hash = SHA-256(standalone canonical CBOR)`. It identifies a
definition everywhere: resolution records (3.4), the fetch protocol (9.3),
de-duplication of user uploads, golden tests. The first 8 bytes in hex are
the short form shown in the UI. Self-description schemas use the same hash
over their decompressed CBOR (12.3).

---

## 9. Catalog: banks, signatures, on-demand fetch, OTA, user definitions

### 9.1 Bank format

A bank is one file in `hubdata` (`defs/bank-a.zdb`, `defs/bank-b.zdb`;
LittleFS), at most 256 KiB on 4 MB boards (16).

```text
Header (64 B, little-endian):
   0 magic "ZDB1"            4 format u16 (1 or 2)     6 signature length u16
   8 bank generation u32    12 catalog date u32 (YYYYMMDD)
  16 catalog serial u16     18 blocks u16             20 definitions u16
  22 shared strings u16     24 index keys u32         28 string section length u32
  32 SHA-256 of the body (everything after the signature)
Signature:     COSE_Sign1 (RFC 9052), EdDSA/Ed25519 (alg -8), protected {1: -8},
               unprotected {4: kid}, detached payload (nil) = the 64-byte header;
               kid = first 8 bytes of SHA-256 of the public key
String table:  format 2 (written since 2026-11-21): u32 count, (count + 1) × u32
               offsets into the text area (the last = its length), the text
               area (UTF-8, concatenated); read in place from memory-mapped
               flash, no RAM. Format 1: u32 inflated length + raw deflate of a
               CBOR array of text strings, inflated into RAM (~32 KiB for the
               2 289-definition release bank). Catalog form, 8.4
Index:         n × 12 B, sorted by (key hash, block, slot):
                 u64 key hash (SipHash-2-4, key "zdef-index-v1" padded with NULs
                 to 16 bytes, over the canonical CBOR of the index key, below)
                 u16 block number, u8 slot in block, u8 flags (1 recheck,
                 2 fingerprint key, 4 white-label name)
Block table:   (blocks + 1) × u32 offsets into the block area (the last = its length)
Blocks:        raw deflate, ≤ 4 KiB uncompressed each, holding whole definitions
               in catalog form, each prefixed by its uncompressed length (u16)
```

Index keys (canonical CBOR arrays): `[0, manufacturer, model]` for exact
names (one per name of `any` lists), `[1, model]` for model-only and
manufacturer-prefix clauses, `[2, mfr_code or null, profile, device_id]` for
fingerprint-only clauses (from the clause's lowest endpoint that gives both
profile and device ID; a hash over the whole endpoint structure, as the
draft said, cannot be computed from a device whose Simple descriptors are a
superset), `[3, manufacturer_id, model_id]` for GP clauses with an unmasked
model ID, `[4, device_id]` for other GP clauses with a DeviceID (masked model
IDs, SrcID ranges), `[5, manufacturer_id]` for masked GP model IDs without a
DeviceID, `[6, manufacturer]` for manufacturer-only clauses (zhc fingerprints
with a `manufacturerName` and no model, common for Tuya; rechecked). A clause
that yields no key is rejected by the bank builder. The hub computes the same
keys from the fingerprint (trimmed names; the manufacturer alone; every
endpoint with and without its manufacturer code). The catalog form drops
`description` and replaces text items that are not map keys with shared
references where that is shorter; strings shared by at least two
definitions enter the table, the largest saving first.

- **Compression**: the builder emits the shorter of dynamic and static
  Huffman coding per block. Raw deflate decoded with the ESP32-S3 ROM `tinfl`
  (`esp_rom_miniz`, zero code size) into a 4 KiB PSRAM buffer with
  `TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF`, so no 32 KiB window is
  needed. One definition load = one block inflate (≤ 4 KiB, ~0.5 ms).
- **Lookup**: hash the device's `(manufacturer, model)` (and `(*, model)`,
  and the fingerprint key), binary-search the index (read in place from the
  memory-mapped bank), inflate the block, decode the definition, re-check
  the full clause (6.2), score (3.3).
- **RAM without PSRAM** (format 2, measured on the host with a counting
  allocator, `cargo test -p zdefc --test bank_heap -- --nocapture`,
  `ZDEF_BANK=bank-4m.zdb` for the release bank): the bank (index, string
  table, blocks) stays in flash; a lookup allocates the inflater state
  (10.5 KB with `miniz_oxide`; the ROM `tinfl` state is about the same),
  the 4 KiB block buffer and the decoded definition: 7.3 KB median, 33.7 KB
  for the largest of the 2 289 definitions (inflater excluded), all freed
  unless the definition is used. The same bank in format 1 holds 31.9 KB of
  inflated string table for as long as the catalog is open. Format 2 costs
  15 KiB of flash for that release (213.1 KiB instead of 197.8 KiB per bank,
  43 KiB below the 256 KiB 4 MB bank).
- **Atomic switch** (implemented, `hub_app::catalog::store`): the active
  bank is chosen by a 68-byte record in the catalog state area (16.1), two
  4 KiB slots written alternately, the valid one with the higher sequence
  number counts: `"ZCS1"`, u32 sequence, u8 active slot (0, 1, 0xFF none),
  u32 bank size, u32 lowest accepted bank generation, u32 manifest length,
  u64 lowest accepted manifest version, SHA-256 of the bank file, CRC-32C.
  A new bank is written into the inactive slot (sector by sector, one chunk
  in RAM), checked in place (SHA-256 against the manifest, COSE signature of
  the header against the trusted keys, body hash), then the record flips
  and the engine's catalog is swapped; every device re-resolves and the
  per-device changes are reported. The hub does not decode every
  definition at install (the generator validated them before signing; on
  the BR board a full decode took more than 30 s and ran the heap out):
  each lookup decodes and verifies the definitions it loads and skips one
  that fails. At boot the active bank is checked again (signature, body
  hash); a bank that fails is not used and the hub
  runs with "no catalog" (generic, user and fetched definitions still work)
  until the next update. A bank whose generation is below the last accepted
  one is refused (anti-rollback; `force` overrides for uploads).

### 9.2 Local definitions

Implemented (2026-11-21):

- user definitions (uploaded, 14.4) stay in the host store journal
  (`udef/<id>`), decoded in RAM (few, small);
- definitions fetched on demand (9.3) go to the local area of `hubdata`
  (`hub_app::catalog::local`): two halves written alternately, each a
  16-byte header (`"ZLD1"`, generation, CRC-32C) and 4-byte aligned records
  (u16 length, u8 key count, u8 kind, CRC-32C, the index-key hashes, the
  standalone CBOR). They are read in place from the mapping (no RAM until a
  fingerprint asks for them). A torn record (power lost while appending)
  ends the log and the next append compacts into the other half; the new
  header is written last, so a power cut during compaction keeps the old
  half;
- self-description schemas (12): still in the device registry.

Budget: 64 KiB on 4 MB (two 32 KiB halves; decided, D10; 16.1), 128 KiB on
8 MB. When a fetched definition does not fit, the definitions no paired
device resolves to are dropped while compacting; if it still does not fit,
it is refused (`no_space` in the `definition_fetched` report's `skipped`).
User definitions are never evicted.

### 9.3 On-demand fetch

When a device resolves to "generic only" and the hub has Internet access
(setting, default on, D21), the hub asks the definitions repository:

```text
{base} = https://github.com/faronov/zigbee-rs-definitions/releases/latest/download

GET {base}/manifest.cose          signed; catalog version, shard and bank hashes
GET {base}/shard-{hh}.cbor        everything for key-hash prefix hh (256 shards)
GET {base}/bank-4m.zdb            bank for 4 MB hubs (9.1), for catalog OTA
GET {base}/bank-8m.zdb            bank for 8 MB hubs
```

- **Hosting** (D9): every catalog release of `faronov/zigbee-rs-definitions`
  is a GitHub Release (tag `catalog-YYYY.MM.DD[.n]`). GitHub's
  `releases/latest/download/<asset>` redirect gives stable URLs; a hub can pin
  a tag instead. HTTPS with the ESP-IDF certificate bundle (the redirect goes
  to `objects.githubusercontent.com`).
- **Shards hold the definitions themselves**, not pointers: a release may
  carry at most 1 000 assets, so one asset per definition is impossible.
  `shard-{hh}.cbor` is a CBOR array of `[index key hashes, requires
  (array), standalone definition (byte string)]`, one record per definition
  that has index keys in prefix `hh` (the top byte of the 64-bit key hash),
  ordered by definition ID; the key hashes are a uint for one key and an
  ascending array for several (≈ 20–30 definitions, a few KiB deflated).
  Shards hold every definition, partial ones included. 256 shards + 2 banks
  + manifest + 2 reports = 261 assets.
- `manifest.cose`: COSE_Sign1, Ed25519 (alg −8), embedded payload: a CBOR
  map `{0: format 1, 1: catalog date, 2: serial, 3: minimum hub feature set,
  4: [{0: bank asset name, 1: SHA-256 of the published (signed) file,
  2: size, 3: definitions, 4: bank generation}], 5: 256 × (shard SHA-256 or
  null), 6: content digest, 7: source {text: text}}`. The content digest is
  the SHA-256 of the ascending content hashes of all shard definitions (equal
  digests = same definitions); `source` is informational (`zhc` version,
  repository commit). Keys 64–127 are ignored. Shards and banks
  are verified by these hashes and each definition by its content hash, so
  only the manifest is signed. The manifest's version must not go backwards
  (anti-rollback, stored in config).
- **Keys** (D9): the signing key is an Ed25519 private key in the GitHub
  Actions secrets of `faronov/zigbee-rs-definitions` (`ZDEF_SIGNING_KEY`,
  owned by the user; an unencrypted PKCS#8 PEM as `openssl genpkey -algorithm
  ed25519` writes it, `zdefc` also accepts the hex seed); the release
  workflow signs the bank headers and the manifest with `zdefc catalog sign`
  and checks the result with `zdefc catalog verify` against
  `keys/zdef-catalog-ed25519.pub.pem` (key ID `66f64ac1cbfc9e27`). The hub
  firmware carries that public key in slot 0 and one spare slot for a
  rotation key; a rotation ships the new public key in a hub release first,
  then switches the secret. Users can add their own keys (14.4) for private
  catalogs.
- The server learns only the shard (1/256 of the key space), not the exact
  model.
- A fetched definition goes to the local area (9.2), then resolution
  reruns. Fetches are rate-limited (one per device per hour).
- **Implementation** (2026-11-21, `hub_app::catalog::service`): **the hub
  downloads nothing** (no TLS stack in the firmware). The release is
  mirrored to GitHub Pages (`https://faronov.github.io/zigbee-rs-definitions/latest/`,
  which, unlike release assets, allows cross-origin reads), and clients
  with Internet access fetch the files and upload them in chunks through
  the API: the web UI, the Home Assistant integration (`zigbee_rs_hub.update_catalog`,
  daily check) and `hubctl catalog-push`. The hub streams each upload into
  flash (manifest and shards into the scratch area, banks into the
  inactive slot, the web tools into their area), one chunk in RAM, and
  checks it in place: the manifest by its signature and version, every
  other file by the SHA-256 the installed manifest lists (a bank also by its
  own signature and body hash). Uploads go in chunks of at most 4 KiB: a
  hub without PSRAM holds the frame, the parsed request and the decoded
  bytes at once, and the hub keeps the installed manifest without its 256
  shard hashes (read in place when needed). `catalog/status` tells clients what to
  fetch: `wants` (the bank for this hub's slot size if newer, the web tools
  if different) and `missing` (devices whose interview is done and that
  resolve to generic only, with the shards holding their index keys; also
  the event `definition_needed`). From an uploaded shard the hub keeps the
  records with those devices' keys that score against their fingerprints
  and need no feature it lacks, appends them to the local area and
  re-resolves the devices. Uploads survive a dropped connection
  (`catalog/status` `upload.written`).
- Without Internet anywhere: the UI offers "upload a definition" and
  "update the catalog from files" (the same upload API).

### 9.4 Catalog OTA

The catalog is versioned and released independently of the hub app:

1. A client (web UI, Home Assistant, `hubctl`) checks `manifest.cose` on
   the mirror (daily when the hub's `auto_update` setting is on), uploads
   it, and uploads the bank (`bank-4m.zdb` / `bank-8m.zdb`, the one built
   for the hub's slot size) when `catalog/status` `wants` it, and the web
   tools (`zdefc-web.zwb`).
2. It streams it into the inactive bank file, verifies signature and hash,
   decodes every definition, checks that every definition referenced by a
   paired device's resolution still exists or has a successor `id`.
3. It flips the active bank record (9.1).
4. It re-resolves devices whose referenced definition changed (3.4) and
   reports per device what changed.

A bank built for a newer hub (definitions it cannot run) is still usable:
those definitions are skipped by `requires`.

### 9.5 Precedence of sources (summary)

user definition > self-description (unless the catalog definition says
`self_description: ignore/under`) > catalog bank = fetched cache (higher
score wins; equal score: fetched, because it is newer) > generic.

---

## 10. Tuya EF00

The hub implements the Tuya protocol once; definitions only carry datapoint
tables and flags.

### 10.1 Frames

Cluster 0xEF00 (`manuSpecificTuya`), cluster-specific commands, no
manufacturer code. Payload (all multi-byte fields big-endian):

```text
u16 seq, then one or more DP records:
  u8 dp, u8 type, u16 len, len bytes data
type: 0 raw (n bytes), 1 bool (1), 2 value (4, signed i32), 3 string (n),
      4 enum (1), 5 bitmap (1, 2 or 4)
```

| Direction | Command | ID |
|---|---|---|
| hub → device | dataRequest (set DPs) | 0x00 |
| hub → device | dataQuery (ask for all DPs) | 0x03 |
| hub → device | sendData (alternative set, some devices) | 0x04 |
| hub → device | mcuVersionRequest | 0x10 |
| both | mcuSyncTime (device asks; hub answers with time) | 0x24 |
| both | mcuGatewayConnectionStatus | 0x25 |
| device → hub | dataResponse | 0x01 |
| device → hub | dataReport | 0x02 |
| device → hub | activeStatusReportAlt | 0x05 |
| device → hub | activeStatusReport | 0x06 |
| device → hub | dataReportAlt | 0x0A |
| device → hub | mcuVersionResponse | 0x11 |

(IDs as in zigbee-herdsman's `manuSpecificTuya` definition.)

### 10.2 Hub behaviour

- **Decode**: every received data command is split into DP records; each
  `(dp, type)` is dispatched to the bindings with `tuya: [dp, type]`. A type
  mismatch is counted; `value` is decoded as i32, `bitmap` as u8/u16/u32 by
  length, `enum` u8, `bool` 0/1, `raw`/`string` as bytes.
- **Encode**: a command for a bound entity builds one DP record (`value`:
  4-byte BE i32; `enum`: 1 byte; …) and sends dataRequest (or `sendData` with
  `tuya.send: sendData`), with a per-device sequence number. Several DPs set
  by one command (climate `set_temperature` + `set_hvac_mode`) go in one
  frame.
- **Default response**: dataRequest is sent with "disable default response";
  the device's dataResponse confirms the state (the entity updates only from
  the device, optimistic state off by default; `tuya.optimistic: true` for
  devices that never echo).
- **Duplicates**: Tuya devices often send the same report on several
  commands; identical `(seq, dp, data)` within 2 s is dropped
  (`tuya.dedup: false` to turn off).
- **Unknown DPs** are kept as diagnostic data in the device page ("DP 104
  value 37 at …"), which is how users write definitions for new devices.

### 10.3 Flags (`tuya:` block)

| Flag | Values | Behaviour (herdsman equivalent) |
|---|---|---|
| `time_sync` | `off`, `1970`, `2000` | answer mcuSyncTime (0x24) with `[u32 utc, u32 local]` seconds since that epoch (`tuyaBase timeStart`) |
| `force_time_sync_s` | int | send time at most this often on any message (`forceTimeUpdates`, 3600) |
| `magic_packet` | bool | at configure, read Basic 0x0004, 0x0000, 0x0001, 0x0005, 0x0007, 0xFFFE from the first endpoint (`configureMagicPacket`) |
| `query` | list of `configure`, `announce`; or `{interval_s}` | send dataQuery then (`queryOnConfigure`, `queryOnDeviceAnnounce`, `queryIntervalSeconds`) |
| `bind_basic` | bool | bind Basic at configure (`bindBasicOnConfigure`) |
| `gateway_status` | bool, default true | answer 0x25 with "connected" (1) |
| `send` | `dataRequest`, `sendData` | write command |
| `mcu_version` | bool | read MCU version at configure (diagnostic sensor) |
| `optimistic`, `dedup` | bool | 10.2 |

These cover `tuya.modernExtend.tuyaBase`, `tuyaMagicPacket` and the named
`tuya.configure*` helpers; the generator maps them directly (17.2).

### 10.4 Tuya on standard clusters

Many Tuya devices (`TS0001`, `TS011F`, `TS0201` …) are standard ZCL plus Tuya
quirks: manufacturer-specific attributes on standard clusters (e.g.
0x0006/0x8002 power-on behaviour, 0x0006/0x8001 indicator mode, 0xE001
cluster). These are ordinary `zcl` bindings; the magic packet flag is still
needed for some of them to report.

### 10.5 Example

`tuya-ts0601-trv-me167.yaml`: climate from DPs 2/3/4/5 with `map` and `div`,
settings DPs 7/36/39/47, a DP shared by two entities (35: `battery_low` with
`from: x == 1` and the raw `error`), and seven schedule DPs through the
`tuya_schedule_day` codec. 18 entities, 1 054 B CBOR (1 024 B catalog form),
691 B deflated.

---

## 11. Example: an Efekta-style settings device

`efekta-co2-smart-monitor.yaml` models zhc's `EFEKTA_CO2_Smart_Monitor`
(`devices/efekta.ts`): CO2, temperature and humidity from the generic tier,
plus 14 settings that Efekta firmware adds as extra attributes of the
standard measurement clusters, without a manufacturer code:

| Key | Platform | Binding | Notes |
|---|---|---|---|
| `reading_interval` | number s 15–300 | 0x040D/0x0201 u16 | |
| `light_indicator` | switch | 0x040D/0x0211 bool | |
| `light_indicator_level` | number % | 0x040D/0x0209 u8 | |
| `set_altitude` | number m 0–3000 | 0x040D/0x0205 u16 | |
| `temperature_offset` | number °C ±50 step 0.1 | 0x0402/0x0410 i16 `div: 10` | |
| `humidity_offset` | number % ±50 | 0x0405/0x0210 i16 | |
| `automatic_calibrations` | switch | 0x040D/0x0402 bool | |
| `forced_recalibration` | **button** | write 0x040D/0x0202 = true | herdsman exposes it as a switch; it is an action |
| `factory_reset_co2` | button (restart) | write 0x040D/0x0206 = true | idem |
| `manual_forced_recalibration` | number ppm 0–5000 (box) | 0x040D/0x0207 u16 | |
| `enable_gas`, `invert_logic_gas` | switch | 0x040D/0x0220, 0x0225 bool | |
| `high_gas`, `low_gas` | number ppm 400–5000 | 0x040D/0x0221, 0x0222 u16 | |

Plus `generic.patch` for the reporting of co2/temperature/humidity. Every
Efekta definition in zhc (76 of 76) is built from `m.numeric`, `m.binary`,
`m.enumLookup` and the generic measurement extends with literal arguments,
so the generator translates all of them without loss (17.3). Encoded size:
757 B CBOR for 14 settings (≈ 50 B per setting; the key strings dominate),
446 B deflated standalone, less inside a bank block.

---

## 12. Self-description for zigbee-rs devices

zigbee-rs devices publish the schema of their own entities, so the hub needs
no catalog entry for them, and a firmware update that adds a setting shows up
in the UI without a catalog release.

### 12.1 What is described

A **definition fragment** (`zdef_fragment: 1`) in the same CBOR schema as
definitions (8), restricted to: `vendor`, `model`, `mfr`, `device`,
`generic` (patch/exclude), `endpoints`, `entities` with `zcl` bindings
(`struct`/`tuya`/`gp` are not allowed), `configure` ops of kind
`report`/`read`/`checkin`, transforms including expressions (verified like
any definition), `triggers` with `cmd` and `press` with `cmd`/`write`. The
standard clusters need no entries: the generic tier covers them; the schema
adds what the generic tier cannot know (settings, diagnostics, custom
commands, overrides of generic choices). Example:
`zigbee-rs-selfdesc-thermometer.yaml` (6 entities, 393 B CBOR, 286 B
deflated, i.e. 6 chunks on air).

### 12.2 Device Schema cluster (ZRDS)

A manufacturer-specific server cluster on the device's first application
endpoint: cluster **0xFC6B**, manufacturer code **0x6D6B** (D8). All frames
carry the manufacturer code (manufacturer-specific bit set).

zigbee-rs cluster IDs under 0x6D6B:

| Cluster | ID | Purpose |
|---|---|---|
| Device Schema (ZRDS) | 0xFC6B | this section |
| zigbee-rs Settings | 0xFC6C | product settings generated by `#[derive(ZclSettings)]` (12.4) |
| reserved | 0xFC6D–0xFC6F | later zigbee-rs clusters |

Why these IDs:

- they are in the manufacturer-specific range 0xFC00–0xFFFF, where the
  manufacturer code, not the cluster ID, identifies the meaning;
- the low byte repeats the code's low byte (0x6**B**), which makes the IDs
  easy to recognise in captures and keeps the zigbee-rs block contiguous;
- none of 0xFC6B–0xFC6F is defined for any manufacturer in
  zigbee-herdsman's cluster table or zigbee-herdsman-converters' custom
  clusters (zhc 26.134.0), nor in zigpy and zha-device-handlers (GitHub code
  search, 2026-11-19). Stacks that decode manufacturer-specific clusters by ID
  alone (as zigbee-herdsman does for registered custom clusters) therefore
  never mistake these frames for another vendor's, even if the device is
  later paired to Zigbee2MQTT or ZHA.

| Attribute | ID | Type | Access | Meaning |
|---|---|---|---|---|
| SchemaFormat | 0x0000 | uint8 | R | bits 0–3 format version (1), bits 4–5 encoding (0 = CBOR, 1 = raw deflate of CBOR), bits 6–7 reserved |
| SchemaHash | 0x0001 | octet string (8) | R, reportable | first 8 bytes of SHA-256 of the **uncompressed** fragment CBOR |
| SchemaLength | 0x0002 | uint16 | R | length of the stored (possibly compressed) blob |
| ChunkSize | 0x0003 | uint8 | R | bytes per chunk attribute, 48 in format 1 |
| SchemaChunk[i] | 0x0100 + i, i < 256 | octet string (≤ ChunkSize) | R | chunk *i* of the blob; the last one may be shorter |

Why attributes and not a new command: a standard Read Attributes works through
every stack and sniffer, needs no command handler state on the device, and is
idempotent and retryable. Why 48 bytes: the Read Attributes Response must fit
in one unfragmented APS frame on the worst realistic path. Budget: 127 B PHY
frame − 11 B MAC (short addressing, PAN ID compression, FCS) − 8 B NWK header
− 18 B NWK security (14 B auxiliary header with the source IEEE + 4 B MIC) −
8 B APS header − 12 B source-route subframe (5 relays) − 9 B APS security
(5 B auxiliary header + 4 B MIC, if APS-encrypted) = 61 B of ZCL; minus 5 B
ZCL header (with manufacturer code) and 5 B attribute record header (ID,
status, type, length) = 51 B. 48 B leaves margin. Maximum blob: 256 × 48 =
12 KiB.

### 12.3 Hub procedure

1. Interview (3.1) sees cluster 0xFC6B in a Simple descriptor (and usually
   manufacturer code 0x6D6B in the Node descriptor; the cluster plus a valid
   SchemaFormat is what counts, because other DIY firmwares may use 0x6D6B too).
2. Read 0x0000–0x0003. Unknown format version → ignore the schema (use the
   generic and catalog layers only) and report it.
3. If a schema with that hash is in `defs/local/`, use it.
4. Otherwise read the chunks, one attribute per Read Attributes request,
   pipelined up to 4 outstanding on mains devices, 1 on sleepy devices (in
   the post-join awake window, else after a Poll Control check-in with fast
   poll requested). Typical 400–1 500 B → 9–32 reads, ~2–5 s on a router,
   ~10–30 s on a sleepy device.
5. Inflate if encoding = 1 (ROM `tinfl`, ≤ 12 KiB), verify the hash, decode,
   verify (8.1 rules, bytecode verifier, fragment restrictions), store under
   the hash, resolve (3.3, layer 3).
6. On every rejoin/announce and at configure, re-read SchemaHash only (one
   small read); the attribute is reportable, so firmware can also push a
   change. A new hash → steps 3–5 and re-resolution (3.4).

Failure handling: a missing or failed chunk is retried 3 times; a schema
that fails verification is discarded and the device stays on generic +
catalog layers, with the reason in diagnostics.

### 12.4 How zigbee-rs devices declare it (Rust sketch)

One source of truth: the settings struct generates both the ZCL cluster
(storage, read/write handling, persistence through the settings journal) and
the schema blob, at compile time.

```rust
// zigbee-zcl (sketch): derive generates the cluster impl and a const schema.
#[derive(ZclSettings)]
#[zcl(cluster = 0xFC6C, mfr = 0x6D6B)]
pub struct ThermoSettings {
    #[zcl(attr = 0x0000, number, unit = "s", min = 10, max = 3600, step = 10,
          category = "config", default = 300)]
    pub report_interval: u16,
    #[zcl(attr = 0x0001, number, device_class = "temperature", unit = "°C",
          div = 100, min = -5, max = 5, step = 0.1, category = "config")]
    pub temperature_offset: i16,
    #[zcl(attr = 0x0003, select, options(0 = "off", 1 = "on_report", 2 = "on_button"),
          category = "config")]
    pub led_mode: u8,
}

// Product composition:
static SCHEMA: zigbee_zcl::selfdesc::Schema = zigbee_zcl::selfdesc::schema! {
    vendor: "zigbee-rs", model: "zrs-thermo-1", mfr: 0x6D6B,
    settings: ThermoSettings,                          // entities from the derive
    extra: [
        sensor!(key = "tx_failures", cluster = 0xFC6C, attr = 0x0100, ty = u16,
                state_class = "total_increasing", category = "diagnostic", enabled = false),
        button!(key = "reset_settings", cmd(cluster = 0xFC6C, id = 0x00)),
    ],
    generic_patch: { temperature: report(30, 600, 10) },
};

let device = DeviceBuilder::new()
    .manufacturer("zigbee-rs").model("zrs-thermo-1")
    .endpoint(1, profile::HA, device_id::TEMPERATURE_SENSOR, |ep| ep
        .cluster_server(TEMPERATURE).cluster_server(HUMIDITY)
        .settings::<ThermoSettings>()                   // 0xFC6C
        .self_description(&SCHEMA))                     // 0xFC6B, serves the chunks
    .build();
```

- `schema!` runs at compile time (proc macro): it builds the fragment, encodes
  deterministic CBOR, deflates it if that is smaller, computes the SHA-256
  prefix, and emits `static` bytes in flash. The device needs no CBOR
  encoder, no hasher and no RAM for it; the cluster serves slices of a
  `&'static [u8]`.
- A build-time check runs the same verifier as the hub (shared crate
  `zdef`, `no_std`), so a device can never publish a schema the hub
  rejects.
- Cost on the device: the blob (typically 300–1 000 B flash) plus ~1 KB of
  code for the cluster.

### 12.5 Interaction with tiers a and b

- The schema is layer 3 (3.3): above the generic tier and above a catalog
  definition (D11); below user definitions and device overrides.
- Generic entities stay unless the schema excludes or patches them.
- A catalog definition for a zigbee-rs device is only needed to correct a
  firmware (`self_description: ignore` or `under`, matched on `sw_build` or
  `app_version`).
- The hub never writes the schema; it is device firmware data.

---

## 13. Green Power devices

### 13.1 Division of work with the NCP

The NCP runs the zigbee-rs GP combo as the network's only sink in
`NCP_SINK` mode (ZRHP 7.11): it commissions GPDs (also bidirectionally, as
TempMaster), verifies security and frame counters, de-duplicates frames from
several proxies, keeps the Sink Table (16 entries on the H2 NCP) and sends GP
Pairings. `gpsSecurityLevel` stays **0x00** (D14), so unsecured GPDs such as
FlowerX GP are accepted; anyone in range can spoof them (ZRHP 13). The hub
therefore marks every GPD commissioned at security level 0 (or below its
definition's `security_min`) with a persistent **"unsecured" warning**: a
badge in the UI device list, a `security_level` diagnostic sensor, and a
`warnings: ["gp_unsecured"]` field in the device record (14.2), which a later
HA integration can surface as a repair issue.

The hub:

- configures the sink at every NCP start: `GP_SINK_CONFIG{mode 1
  NCP_SINK, communication_mode 2 pre-commissioned groupcast, group 0x0B84,
  exit_mode, window_s, security_level 0x00 (user setting), radius}`;
- keeps the **GP device registry** (identity, commissioning data, resolution,
  entities) in the host store, and mirrors the Sink Table: at every start it
  reads `GP_SINK_TABLE_READ` and reconciles (entries only on the NCP → device
  "commissioned elsewhere", shown for adoption; entries only in the hub →
  pushed back with `GP_SINK_TABLE_PUT` flags 0 after an NCP replacement);
- turns `EVT_GP_FRAME` (only frames the sink accepted) into entity updates and
  events.

**Translation decision.** The NCP has no translation endpoint
(`SinkConfig.endpoint = None`): it does not turn GPD commands into ZCL
commands, and every accepted command reaches the hub as `EVT_GP_FRAME` with
`result` 1 (passed through). The NCP itself never acts on a GPD command:
GPD commands take effect through the hub's entities, events and **GP
bindings** (D23, 13.8). A binding is executed either by the target device
itself (a sink with a Translation Table, programmed by the hub through ZRHP
remote GP provisioning) or by the hub; the former also works while the hub
is down.

### 13.2 Identity and commissioning data

| Field | Source |
|---|---|
| GPD ID | `gpd_id` in every GP event: `app_id` 0 + `u32 src_id`, or `app_id` 2 + IEEE + endpoint (ZRHP 7.11) |
| DeviceID | `EVT_GP_COMMISSIONED.device_id`; also the commissioning payload |
| Commissioning payload | `EVT_GP_FRAME` with `command_id` 0xE0 and `result` 2 (commissioned) or 3 (Commissioning Reply sent): payload exactly as received (ZRHP 7.11) |
| Options | MAC sequence capability, RxOnCapability (bidirectional), fixed location, PAN ID / key requests |
| Extended options | security level capability, key type, key present/encrypted, counter present |
| Application information (when Options bit 2) | Manufacturer ID (u16), Model ID (u16), GPD command list, server/client cluster lists, switch information (contacts, switch type, contact status), "Application Description follows" |
| Security | `R_GP_SINK` record in `EVT_GP_COMMISSIONED` (level, key type; key bytes only if export is unlocked) |

The hub decodes the commissioning payload with the same strict decoder as
zigbee-rs (`zigbee_gp::commissioning::CommissioningPayload::decode`, shared
crate) and stores it. For a bidirectional commissioning the payload comes
with `result` 3 before `EVT_GP_COMMISSIONED`; the hub keeps the last
commissioning payload per `gpd_id` for 30 s and binds it to the following
`EVT_GP_COMMISSIONED`. A GPD commissioned before the hub knew it (imported
Sink Table) has no application information until it re-commissions; it
resolves on DeviceID and SrcID only.

GPD DeviceIDs (as in zigpy `zigpy/zgp/types.py` `DeviceID`, checked
2026-11-19; the zigbee-rs repositories carry no GP specification text):

| DeviceID | Name | DeviceID | Name |
|---|---|---|---|
| 0x00 | Simple generic 1-state switch | 0x10 | Color dimmer switch |
| 0x01 | Simple generic 2-state switch | 0x11 | Light sensor |
| 0x02 | On/Off switch | 0x12 | Occupancy sensor |
| 0x03 | Level control switch | 0x20 | Door lock controller |
| 0x04 | Simple sensor | 0x30 | Temperature sensor |
| 0x05 | Advanced generic 1-state switch | 0x31 | Pressure sensor |
| 0x06 | Advanced generic 2-state switch | 0x32 | Flow sensor |
| 0x07 | Generic switch | 0x33 | Indoor environment sensor |
| | | 0xFE | Manufacturer-specific / undefined |

Note: 0x31 is the *pressure* sensor; a GP temperature + humidity sensor uses
0x33 (or 0x30 for temperature only). 0x50/0x51 are not DeviceIDs; they are
the GPD *commands* Lock door / Unlock door. Manufacturer-specific devices use
DeviceID 0xFE (or a standard DeviceID) plus Manufacturer ID / Model ID and
the manufacturer-defined commands 0xB0–0xBF.

### 13.3 Matching

```yaml
match:
  - gp: {device_id: 0x07, src_id: [0x01500000, 0x015FFFFF]}          # EnOcean PTM 216Z
  - gp: {device_id: 0x02, src_id: [0x00400000, 0x004FFFFF]}          # Hue Tap
  - gp: {manufacturer_id: 0x6D6B, model_id: 0xF300, device_id: 0x33} # zigbee-rs MG22 sensor
  - gp: {device_id: 0x33, manufacturer_id: 0x6D6B, model_id: {value: 0xF100, mask: 0xFF00}}  # FlowerX GP family
```

Fields (all optional, AND-ed): `device_id`, `manufacturer_id`, `model_id`
(int or `{value, mask}`), `src_id` (`[lo, hi]` inclusive), `ieee` (prefix),
`commands` (must all be in the GPD command list), `clusters` (`{server: [...],
client: [...]}` subsets). Scoring: manufacturer + model (masked or not) = 600,
SrcID range or IEEE prefix + DeviceID = 600, DeviceID + command subset = 300,
DeviceID or SrcID/IEEE alone = 200, anything else = 100; GP clauses get no
+10 extras. Index keys: `[3, manufacturer_id, model_id]` (unmasked model),
`[4, device_id]` and `[5, manufacturer_id]` (masked model without DeviceID),
rechecked after the lookup (9.1).

Known identities:

| Device | Match | Notes |
|---|---|---|
| Philips Hue Tap | DeviceID 0x02, SrcID 0x00400000–0x004FFFFF | commands 0x22 (button 1), 0x10, 0x11, 0x12 (buttons 2–4); zha-quirks `philips/hue_tap.py` |
| Friends of Hue switches (EnOcean PTM 215Z based) | DeviceID 0x02, SrcID 0x01700000–0x017FFFFF and 0x01A40000–0x01A4FFFF | Recall/Store scene commands per contact pair; zha-quirks `enocean/ptm215z.py` |
| EnOcean PTM 216Z | DeviceID 0x07, SrcID 0x01500000–0x015FFFFF | 0x69/0x6A contact bit vector; `gp-enocean-ptm216z.yaml` |
| DIYRuZ FlowerX GP | DeviceID 0x33, 0x6D6B / 0xF100 \| bits (mask 0xFF00) | level 0, 0xA2; soil moisture, temperature, illuminance, battery, voltage; bit 0 BME280 → humidity, pressure; bits 0+1 → soil temperature 0x0402/0x5000; `gp-diyruz-flowerx.yaml` |
| EfektaLab BGT1 GP | DeviceID 0x30, 0x6D6B / 0xF200 \| bits (mask 0xFF00) | level 3, 0xA2; temperature, battery, voltage; bit 1 DS18B20 probe; `gp-efektalab-bgt1.yaml` |
| zigbee-rs `efr32mg22-gpd-sensor` | DeviceID 0x33, 0x6D6B / 0xF300 | level 3, 0xA2; temperature, humidity, battery, voltage; `gp-zigbee-rs-sensor.yaml` |
| zigbee-rs `efr32mg22-gpd-switch` | DeviceID 0x02, 0x6D6B / 0xF400 | level 3; Off/On/Toggle (0x20–0x22) → event entity; `gp-zigbee-rs-switch.yaml` |

GP model-ID allocation under Manufacturer ID 0x6D6B (D16). The ID space is
shared with the DIY firmwares that already use 0x6D6B; new IDs are allocated
here (and mirrored in zigbee-rs' GP doc) before a firmware ships:

| Model ID | Owner / device | Low byte |
|---|---|---|
| 0xF100–0xF1FF | DIYRuZ FlowerX GP | sensor bits: bit 0 BME280, bit 1 DS18B20 |
| 0xF200–0xF2FF | EfektaLab BGT1 GP | bit 1 DS18B20 probe |
| 0xF300 | zigbee-rs GP sensor (`efr32mg22-gpd-sensor`) | — |
| 0xF400 | zigbee-rs GP switch (`efr32mg22-gpd-switch`) | — |
| 0xF500–0xFEFF | free; zigbee-rs allocates new GPD models from here (one 0xFn00 block per family when low-byte feature bits are needed) | |
| 0x0000–0xF0FF, 0xFF00–0xFFFF | unused; avoid (0xFFFF reads as "unknown" in some tools) | |

### 13.4 Generic GP tier

Without a definition every commissioned GPD gets entities from its DeviceID,
command list and the frames it sends:

| GPD commands | Entities |
|---|---|
| 0x20 Off, 0x21 On, 0x22 Toggle, 0x23 Release | `action` event: `off`, `on`, `toggle`, `release` |
| 0x10–0x17 Recall scene 0–7, 0x18–0x1F Store scene 0–7 | `action`: `recall_scene_<n>`, `store_scene_<n>` |
| 0x30–0x38 Level move/step/stop (with on/off) | `action`: `brightness_move_up`/`_down`, `brightness_step_up`/`_down`, `brightness_stop` (attributes rate, step size) |
| 0x40–0x4B color move/step | `action`: `hue_move_up` …, `color_move`, `color_step` |
| 0x50 Lock door, 0x51 Unlock door | `action`: `lock`, `unlock` |
| 0x60–0x68 Press/Release/Short press n of m | `action`: `press_<n>_of_<m>`, `release_<n>_of_<m>`, `short_press_<n>_of_<m>` |
| 0x69 / 0x6A Press / Release 8-bit vector | one event entity per contact announced in the switch information (`button_1` … `button_<n>`, n ≤ 8): `press` when its bit sets, `release` when it clears; without switch information one `action` with the raw bitmap |
| 0xA0 Attribute reporting, 0xA2 Multi-cluster reporting | sensor/binary entities created on the first report of each `(cluster, attribute)`, using the **tier-a rules** (5.1) for that cluster/attribute (temperature, humidity, pressure, illuminance, occupancy, battery, …); clusters announced in the commissioning cluster list are created up front |
| 0xA1 / 0xA3 manufacturer-specific reports | no entity without a definition; values shown as diagnostic data |
| 0xA8 Compact attribute reporting | decoded only if an Application Description (0xE4) was received; otherwise diagnostic data (later) |
| 0xB0–0xBF manufacturer-defined | no entity without a definition; payload shown as diagnostic data |
| any | `last_seen`, `rssi`/`lqi` (from `EVT_GP_FRAME`), `security_level`, `frame_counter` diagnostics |

The `action` event entity's `event_types` come from the GPD command list in
the commissioning payload (or the DeviceID's typical commands if absent;
types appear as they are first received when nothing is known).

### 13.5 Declarative GP definitions

A GP definition uses the same entity model with GP bindings:

- `gp: {report: {cluster, attr, type, mfr?}}` binds an attribute of 0xA0–0xA3
  reports (`mfr` selects 0xA1/0xA3 with that manufacturer code);
- `gp: {cmd: 0xNN}` binds a GPD command; `payload` is its payload. For
  0xB0–0xBF the payload starts with the Manufacturer ID (u16 LE) as zigpy
  reads it (`GPManufacturerDefinedPayload`), so vendor data starts at offset 2
  (snippet below);
- event `triggers` use `gp_cmd: 0xNN` (with `when`/`event`/`attrs`);
- `gp:` device block: `report_interval_s` (availability), `security_min`
  (warn when a GPD commissioned below it), `ignore_commands`;
- `device.power: energy_harvesting` → no availability tracking (4.6);
  `device.battery` → `battery_percent()` for vendor voltage fields;
- `present_if` with `gpd.model_id` bits for families that encode features in
  the model ID.

Examples:

- `gp-enocean-ptm216z.yaml`: four contact event entities from the 0x69/0x6A
  bit vector;
- `gp-diyruz-flowerx.yaml`: masked model ID, `present_if` on model bits
  (humidity and pressure only with a BME280, soil temperature 0x0402/0x5000
  only with BME280 + DS18B20), the illuminance formula as an expression;
- `gp-efektalab-bgt1.yaml`: masked model ID, level 3, the "refresh on every
  report" behaviour (13.6);
- `gp-zigbee-rs-sensor.yaml`, `gp-zigbee-rs-switch.yaml`: the zigbee-rs GPDs.

Manufacturer-specific frames (no example device uses them yet; illustrative):

```yaml
- key: battery_voltage           # 0xB0: [mfr u16][battery mV u16 LE][flags u8]
  platform: sensor
  device_class: voltage
  unit: mV
  gp: {cmd: 0xB0}
  from: u16le(payload, 2)
- key: sensor_fault
  platform: binary_sensor
  device_class: problem
  gp: {cmd: 0xB0}
  from: bit(payload[4], 0)
- key: dew_point                 # 0xA3 with manufacturer code 0x6D6B
  platform: sensor
  device_class: temperature
  unit: °C
  gp: {report: {mfr: 0x6D6B, cluster: 0xFC6C, attr: 0x0001, type: i16}}
  div: 100
```

### 13.6 Battery and energy harvesting semantics

- GPDs are never polled and never asked for anything (unidirectional), so
  availability is passive only: 4.6.
- A GP sensor with `report_interval_s` is unavailable after 3 missed
  intervals (25 h without a known interval, D19; both configurable per
  definition, 4.6); its last values stay (HA shows them as unavailable).
- **Every accepted report refreshes** `last_seen` and publishes an
  `entity_state` for each entity in it, even when the value is unchanged
  (D16, the BGT1 GP requirement; 4.5). The NCP already drops duplicates of
  the same GPDF (same frame counter, several proxies), so a refresh means a
  new report, never a repeat.
- Switches have no availability; `last_seen` shows when they were last used.
- The GPD frame counter is shown as a diagnostic; a counter that resets
  (re-flash, new key) appears as commissioning failures (`Stale`) in
  `EVT_GP_COMMISSIONING_FAILED`/DIAG counters; the UI suggests
  re-commissioning.

### 13.7 Commissioning UX (WebSocket)

| API | ZRHP | Notes |
|---|---|---|
| `gp/commissioning/open {duration_s ≤ 600, involve_proxies: true, exit_on_first_pairing: false}` | `GP_SINK_COMMISSIONING{action 1, flags, window_s}` | also offered from the "permit join" dialog, as zigpy does (`NWK_PERMIT_JOIN` + GP window) |
| `gp/commissioning/close` | `GP_SINK_COMMISSIONING{action 0}` | |
| `gp/commissioning/status` (API 1.2) | hub state | `{open, state, remaining_s, involve_proxies, exit_on_first_pairing, exit_reason, pending: [{gpd, command_id, result, since, time}], failed: [{gpd, reason, reason_code, time}]}`; `remaining_s` is `null` for a window without a deadline, 0 when closed; `pending` holds at most 8 GPDs (dropped 60 s after their last commissioning frame, on `gp/commissioned` or `gp/failed`), `failed` the last 8 failures, newest first; times in s like `last_seen`; not persisted |
| event `gp/commissioning` | `EVT_GP_COMMISSIONING`, and the hub's own open/close/expiry | `state` (`open`/`closed`), `remaining_s`, `exit_reason` (`none`, `explicit`, `first_pairing`, `timeout`); API 1.2 adds `open`, `involve_proxies`, `exit_on_first_pairing` (the body of `gp/commissioning/status` without the lists). The hub keeps its own deadline, as for permit join (14.2): an NCP image need not report the window, so the hub reports opening and closing through the API, its expiry and the exit after the first pairing itself |
| event `gp/pending` | `EVT_GP_FRAME` with `command_id` 0xE0/0xE3 (`result` 3 or 5) | a GPD is commissioning (bidirectional steps in progress): `{gpd, command_id, result, time}` (`time` since 1.2) |
| event `gp/commissioned` | `EVT_GP_COMMISSIONED` + the stored payload + the resolution | device appears with its entities |
| event `gp/failed` | `EVT_GP_COMMISSIONING_FAILED{reason}` | `{gpd, reason, reason_code, time}`: reason text from `SinkDrop` (SecurityPolicy, TableFull, NoTempMaster, …); `reason_code` and `time` since 1.2 |
| `gp/devices/list` | hub registry + `GP_SINK_TABLE_READ` | device records plus `security_level`, `frame_counter`, `sink` (the `R_GP_SINK` record) and, since 1.2, `device_id` (GPD DeviceID) and `commands` (GPD commands from the commissioning payload and seen since, sorted); "on NCP but unknown to the hub" is not listed yet (14.5) |
| `gp/devices/remove {device, send_remove: true}` | `GP_SINK_REMOVE{gpd_id, flags bit0}` | `send_remove` sends GP Pairing RemoveGPD to proxies |
| `gp/config {security_level?, communication_mode?}` | `GP_SINK_CONFIG` | admin only; stays 0x00 (D14); a level > 0 would reject unsecured GPDs such as FlowerX GP. Since 1.2 an absent field keeps its current value (before: 0 and 2), values above 3 are `invalid_value`, and the result is the `gp/config/get` record |
| `gp/config/get` (API 1.2) | hub state | `{security_level, communication_mode, group, source: "default"\|"api", warning}`: the configuration last written through the API since the hub started, else the defaults (0x00, mode 2, group 0x0B84); `warning` is set for a level > 0 |
| event `gp/config` (API 1.2) | `GP_SINK_CONFIG` written | the `gp/config/get` record |
| `gp/bindings/list {gpd?, target?}` | hub registry | binding records with executor, state and per-target verification (13.8) |
| `gp/bindings/create {gpd, trigger, target, action, executor: auto\|device\|hub}` | on-device: `GP_PAIRING_CONFIGURATION`, `GP_TRANSLATION_UPDATE`, `GP_TRANSLATION_REQUEST` (7.11); hub-side: none | admin only; returns the binding with state `pending`, then `active` or `failed{zcl_status}`; `unsupported` when `device` is forced on a target without gpsFunctionality bits 11 and 18 |
| `gp/bindings/update {binding, executor?, action?, trigger?}` | as create | changing the executor removes the old one first (13.8) |
| `gp/bindings/delete {binding}` | on-device: `GP_TRANSLATION_UPDATE{remove}`, last one: `GP_PAIRING_CONFIGURATION{remove pairing}` | state `removing` until the target confirmed |
| `gp/bindings/verify {binding?}` | `GP_TRANSLATION_REQUEST`, `GP_REMOTE_SINK_TABLE_REQUEST` | re-reads on-device targets, repairs drift |
| `gp/targets/probe {device}` | ZCL Read Attributes of 0x0021 attrs 0x0000/0x0006 on EP 242, `GP_REMOTE_SINK_TABLE_REQUEST` | `{sink: bool, translation_table: bool, security_levels, max_sink_entries, functionality, probed}` when known, else `{probing: true}` and the event `gp/target` follows |
| event `gp/target` | the probe above | `{device, capabilities}` (the probe result) |
| event `gp/binding` | the steps above | the binding record (`{id, gpd, trigger, target, action, choice, executor, state, targets, …}`, `zcl_status?`); `state: "removed"` once deleted. `targets[].last_verified` is in ms, unlike the other times |
| device field `warnings: ["gp_unsecured"]` | `EVT_GP_COMMISSIONED` record security level | shown as a badge; set for level 0 or below `security_min` (13.1) |

### 13.8 GP bindings (D23)

zigpy has no way to bind a GP switch to a light: ZHA only exposes GPD
commands as events. The hub binds them.

**Binding record.** A binding maps one trigger to one action on one
target:

- **trigger**: the GPD (`gpd_id`) and a GPD command (`command_id`), for
  example Toggle 0x22, or Press 1 of 1 0x69 with a contact bitmask match
  for generic switches; definitions name the commands per button (13.5),
  so the UI offers "button 2, press";
- **target**: a device endpoint (`device`, `endpoint`) or a group
  (`group`);
- **action**: profile (0x0104), cluster, ZCL command (client → server) and
  a payload that is fixed (≤ 8 bytes), **derived** (the GP default
  derivation, for example Step mode and step size from the GPD's Step
  command) or **copied** from the GPD payload; the same three choices as
  the GP Translation Table (ZRHP 7.11 `translation`);
- **executor**: `device` (on-device) or `hub`, chosen automatically and
  overridable.

**(a) On-device bindings.** The target executes the binding itself; it
works while the hub, the NCP or the host link is down, and costs no extra
radio frame per press.

- *Capability detection* when a binding is created and at every
  interview: read the GP cluster server attributes of endpoint 242
  (gpsFunctionality 0x0006, gpsMaxSinkTableEntries 0x0000) with ZCL Read
  Attributes; on-device needs gpsFunctionality bit 11 (CT-based
  commissioning: GP Pairing Configuration) and bit 18 (Translation
  Table), and the security level of the GPD (13.1) among the levels the
  target accepts (bits 13/15/16). A `GP_REMOTE_SINK_TABLE_REQUEST`
  answered with SUCCESS or NOT_FOUND confirms a working sink. A Read
  Attributes answered UNSUPPORTED_ATTRIBUTE, or a proxy-only device
  (gppFunctionality 0x0016 only), means hub-side. zigbee-rs combo lights
  with `green-power-translation-table` qualify (GP doc §3.8).
- *Programming* (ZRHP 7.11, remote GP provisioning), all unicast to the
  target, each step confirmed by `EVT_GP_REMOTE_STATUS` SUCCESS with the
  request's `zcl_tsn` (timeout 3 s after `EVT_APS_DATA_CONFIRM`, 2
  retries):
  1. `GP_PAIRING_CONFIGURATION{dst, actions: extend | send GP Pairing,
     gpd_id, device_id, communication_mode, flags, security, key,
     frame_counter, paired: 0x00 (none)}`. The GPD data comes from the
     hub's GP registry (13.2): the key and the current frame counter of the
     NCP's Sink Table. `paired` 0x00 keeps the target from adding its own
     default translations; the binding's translation is the only one.
     Communication mode: the one the NCP sink uses for the GPD (13.1:
     pre-commissioned groupcast to 0x0B84, `communication_mode` 2 with
     that group; derived groupcast for GPDs commissioned that way). The
     target's sink then joins the same group on endpoint 242, so the
     proxies' existing groupcast reaches it and no new proxy pairing is
     needed; "send GP Pairing" is set so proxies that miss the pairing
     learn it.
  2. `GP_TRANSLATION_UPDATE{dst, action: replace, gpd_id, translations:
     [{gpd_command, endpoint, profile, cluster, zcl_command, payload}]}`;
     all bindings of one GPD on one target travel together (at most 7 per
     update, 16 per target on a zigbee-rs combo).
  3. Verify with `GP_TRANSLATION_REQUEST` (and
     `GP_REMOTE_SINK_TABLE_REQUEST` by GPD ID): the binding becomes
     `active` only when the target's table holds it.
- *Groups*: an on-device group binding programs every member that
  qualifies (one Pairing Configuration + Translation Update each, the
  translation's endpoint being the member's endpoint in the group). If any
  member does not qualify, the whole group binding is hub-side (one
  groupcast) instead of mixing executors.
- *Removal*: `GP_TRANSLATION_UPDATE{remove}` for the binding's
  translations; when it was the GPD's last binding on that target,
  `GP_PAIRING_CONFIGURATION{remove pairing | send GP Pairing}` with the
  same communication mode and groups. Removing the GPD from the hub
  (13.7) removes its on-device bindings first. An unreachable target leaves
  the binding in `removing` and is retried at its next announcement.
- *Drift*: at NCP start, daily, and when a target announces itself
  (`Device_annce`, a rejoin or a factory reset), the hub reads the
  target's Translation Table and reprograms missing or different entries
  (`drift` state while it does). The GPD's frame counter is re-sent with
  every reprogramming, so a reset target never accepts replays.

**(b) Hub-side virtual bindings.** The hub executes the binding on
`EVT_GP_FRAME` (`result` 1): it sends the ZCL command with
`APS_DATA_REQUEST` (unicast with APS ACK to a device endpoint, groupcast to
a group, source endpoint 1, Default Response disabled for groups). Any
Zigbee light or group qualifies, including lamps that are only GP proxies
or not GP-capable at all.

- *Latency budget* (press to the target's radio, 95th percentile,
  single-hop target): GP tunnelling at the proxy (gpp tunnelling delay
  plus the Notification, ≤ 100 ms; a GPDF heard directly by the NCP: 0),
  NCP sink processing and the host link (≤ 15 ms), hub binding lookup and
  encode (≤ 5 ms), NCP APS queue and transmission (≤ 30 ms): **≤ 150 ms**,
  verified in hardware tests (18.6). A binding over a multi-hop route adds
  the route's delay.
- Frames the NCP drops (duplicates from several proxies, stale counters)
  never trigger a binding: `EVT_GP_FRAME` is emitted only for accepted
  frames (ZRHP 7.11).
- No effect while the hub is down; the UI shows hub-side bindings as
  "needs the hub".

**Choice of executor.** `auto` (default): on-device when every target
qualifies, else hub-side. The user can force `hub` (for example to keep
the logic visible) or `device` (rejected with `unsupported` when a target
does not qualify). Changing the executor removes the binding from the old
executor before the new one is active, so a press never runs twice.

**Interaction with entities and Home Assistant automations.**

- GP event entities always fire, bound or not (4.7); a binding never
  swallows an event. The event carries `bindings: [ids]` and
  `executor: device|hub` so HA automations can add a condition.
- The hub cannot see HA automations. When a binding is created for a
  trigger, the UI warns that HA automations on the same event entity will
  run as well, and the HA integration (15) exposes the `bindings` attribute
  on the event entity. A user who wants HA to own a button simply does not
  bind it.
- On-device bindings report `result` 0 only if the NCP itself translated a
  frame, which it never does (13.1); a target's translation is not visible
  to the hub except through the light's own attribute reports.

**Persistence.** Bindings live in the host store journal in `hubdata`
(16.1): `gp_binding {id, gpd_id, trigger{command_id, contact_mask?,
contact_value?}, target{device_ieee, endpoint | group}, action{profile,
cluster, command, payload_mode, payload}, executor, state (pending |
active | drift | removing | failed), targets[{device_ieee, last_verified,
error}]}`; at most 64 bindings, about 96 bytes each (6 KiB of the
128 KiB host store). A restore of a hub backup reprograms on-device
bindings as drift.

**Status.** The zigbee-rs side exists and is host-tested only: ZRHP
remote GP provisioning on the NCP and the combo's Pairing Configuration
and Translation Table (zigbee-rs `gp-translation`, GP doc §3.8, ZRHP
7.11). The hub side is specification only.

---

## 14. WebSocket API surface

The device, entity and definition parts of the hub API, and (14.6) network
and hub maintenance; transport and firmware updates are specified elsewhere.

### 14.1 Envelope

JSON text frames, modelled on Home Assistant's WebSocket API so that HA-side
code feels familiar:

```jsonc
{"type": "auth", "token": "…"}                                  // first frame
{"type": "auth_ok", "hub": "zrs-hub-7cdfa1", "api_version": "1.0",
 "zdef_versions": [1], "features": ["expr-v1", "tuya", "gp", "struct.zcl_tagged"]}

{"id": 17, "type": "devices/list"}                               // request
{"id": 17, "type": "result", "success": true, "result": [ … ]}   // response
{"id": 17, "type": "result", "success": false,
 "error": {"code": "not_found", "message": "no device zb:00…"}}

{"id": 18, "type": "subscribe", "topics": ["devices", "entities", "events", "gp", "network"]}
{"id": 18, "type": "event", "seq": 1042, "event": {"topic": "entity_state", …}}
```

- `id` is chosen by the client, unique per connection.
- `api_version` is `major.minor`; minor versions only add fields and message
  types. Clients ignore unknown fields. 1.1 added `network/info`, the
  `network` topic and the device record's `nwk`; 1.2 added the maintenance
  requests of 14.6, `maintenance` in `hub/info`, the `network_updated`
  and `topology_progress` events, `gp/commissioning/status`,
  `gp/config/get`, the GP fields of 14.5, `network/active_scan` and
  `tx_power_requested` in `network/info`.
- Subscriptions first deliver a **snapshot** (all devices/entities/states with
  the current `seq`), then deltas with increasing `seq`, so a client never
  misses a change between "list" and "subscribe".
- Errors: `unauthorized`, `not_found`, `invalid_format`, `invalid_value`,
  `unsupported`, `busy`, `timeout`, `device_unavailable`, `no_space`,
  `verification_failed`, plus the ZCL/ZRHP status for device commands
  (`{"code": "zcl_status", "zcl_status": "UNSUPPORTED_ATTRIBUTE"}`).

### 14.2 Devices and network

| Type | Parameters | Result |
|---|---|---|
| `hub/info` | — | `name`, `api_version`, `features`, `seq`, `devices`, `entities`; once the NCP session is up also `coordinator_ieee` (16 hex), `ncp_board`, `ncp_product` (the HA config entry's unique ID) |
| `network/info` | — | `state` (`up`, `formed_down`, `none`), `session`, `ownership` (`owned`, `no_network`, `lost`, `mismatch`, `observed`, `unknown`), `permit_join_s` (seconds left), `pan_id`, `ext_pan_id` (hex), `channel`, `nwk_update_id`, `tx_power`, `ncp`: {`fw`, `protocol`, `chip`, `ieee`, `flash_kib`, `board`, `product`, `baud`, `flow_control`}, `time` (the hub's clock in s, the time base of `last_seen` and state timestamps; clients use it for relative times while the hub's clock is not yet synchronised) (fields the host core does not know are absent; `unsupported` without a host core) |
| `network/permit_join` | `duration_s` (0–254), `target` (device id or all), `include_gp` (bool, opens the GP window too, 13.7) | — |
| `devices/list` | — | device records (4.1) |
| `devices/get` | `id` | device record + endpoints + resolution summary |
| `devices/update` | `id`, `name`, `disabled` | device record |
| `devices/remove` | `id`, `force` (skip Mgmt_Leave) | — (ZDO Mgmt_Leave, then `NWK_REMOVE_DEVICE`; GP: 13.7) |
| `devices/reinterview` | `id` | progress events `interview_progress` |
| `devices/reconfigure` | `id` | per-step results (binds, reporting, reads) |
| `devices/diagnostics` | `id` | fingerprint, layers and their hashes, configure results, unknown attributes and Tuya DPs seen, expression error counters, last 50 frames (hex), GP commissioning payload |
| `devices/pin_definition` | `id`, `definition` (id or null) | new resolution |

Events (topic `devices`): `device_added`, `device_updated` (any registry field,
availability, `last_seen` at most once per minute), `device_removed`,
`interview_progress`, `resolution_changed` (layers, added/removed/orphaned entities; removed ones also get `entity_removed`).

Events (topic `network`, API 1.1): `permit_join` (`remaining_s`, when joining
opens, is re-opened, closes or expires — whoever changed it: the API, the
console, the NCP — not every second; the hub derives it from its own
deadline, as an NCP image need not report `EVT_PERMIT_JOIN_STATE`) and `network_state` (`state`: `down`, `forming`, `up`,
`leaving`); the snapshot carries `network` (the `network/info` result).

### 14.3 Entities

| Type | Parameters | Result |
|---|---|---|
| `entities/list` | `device` (optional) | entity records (4.2) |
| `entities/states` | `uids` (optional) | states (4.5) |
| `entities/command` | `uid`, `command` (HA service name), `params` | `{"status": "ok"}` after the device confirmed (write/command response, Tuya dataResponse), `"queued"` for sleepy devices |
| `entities/refresh` | `uid` | reads the bound attribute(s) now (mains devices) |
| `entities/update` | `uid`, `name`, `enabled`, `value` for `hub` bindings | entity record |
| `entities/delete` | `uid` | only orphaned entities (3.4) |

Events (topic `entities`): `entity_added`, `entity_updated`,
`entity_removed`, `entity_state` (4.5). Topic `events`: `entity_event`
`{uid, event_type, attributes, time}` for event entities.

### 14.4 Definitions and catalog

The hub stores, accepts and returns **CBOR only** (D15). YAML (or JSON,
JSONC, JSON5 — D22) is compiled by `zdefc` on the client: the CLI, or its
WebAssembly build inside the web UI.
The UI decompiles CBOR back to YAML for editing (lossless except comments,
which the UI keeps with the user's own copy). The hub never parses YAML,
which keeps a YAML parser and the compiler out of the hub app.
The WebAssembly build (`crates/zdefc-wasm`, 638 KB, 231 KB gzipped) exports
`compile(source, format)` → CBOR bytes (throws the diagnostics as JSON),
`check(source, format)` → `{ok, errors: [{path, line?, col?, message}],
size, content_hash, requires}`, `decompile(cbor, format)`,
`content_hash(cbor)`, `expr(source)` and `schema()`; formats are `yaml`,
`json`, `jsonc`, `json5`.

| Type | Parameters | Result |
|---|---|---|
| `definitions/validate` | `cbor` (base64) | `{ok, errors: [{path, line?, message}], warnings, requires, content_hash, size, matches: [device ids]}` — nothing stored |
| `definitions/upload` | same + `replace` (id), optional `source` (≤ 8 KiB, kept for editing) and `source_format` | stored in `defs/local/`, user source; devices it matches re-resolve; returns their resolution diffs |
| `definitions/delete` | `id` | user definitions only; affected devices re-resolve |
| `definitions/list` | `source` (catalog, fetched, user, self_description), `device` (only those matching it) | `{id, revision, vendor, model, source, content_hash, size, requires}` |
| `definitions/get` | `id` or `content_hash`, `format` (`cbor` base64, or `json`: the decoded CBOR with symbolic keys) | the definition |
| `definitions/preview` | `device`, `cbor` | the resolution the device *would* get: entities added/changed/removed, configure diff; nothing applied |
| `definitions/scaffold` | `device` | the material for a starter definition (JSON): identity, a match clause, every attribute and Tuya DP seen with its type and last value, the generic entities; the UI renders it as YAML |
| `definitions/fetch` | `device` | `{device, shards}`: the shards (numbers) a client fetches from the mirror and uploads (target `shard`) for this device (9.3) |
| `catalog/status` | — | `installed` (generation, date, serial, definitions, signing kid, size), `active_slot`, `manifest` (version, date, serial, zhc), `wants` {`bank`, `web`: {name, size, sha256} or null}, `missing` [{`device`, `shards`}], `upload` {`target`, `written`}, `web_tools`, `local` (fetched definitions, bytes used/size), `slot_size`, `config`, `warnings`, `engine` |
| `catalog/upload_chunk` | `target` (`manifest`, `bank`, `web`, `shard`), `offset`, `data` (base64 of ≤ 4 KiB: without PSRAM the hub holds the frame, the parsed request and the decoded bytes at once); at offset 0 optionally `size`, `sha256` | `{written}`; offset 0 starts a new upload (and drops an unfinished one of the same target; another target's upload that advanced in the last 30 s makes it fail with `busy`) |
| `catalog/commit` | `target`; `shard` (0–255) for shards, `device` (optional) ; `force` (bank anti-rollback) | manifest: `{version, catalog_date, catalog_serial, wants}`; bank: `{sha256, generation, definitions, devices: [{device, from, to, added, removed, orphaned}]}`; web: `{size, files}`; shard: `{definitions, skipped, devices}`. `verification_failed` for a wrong signature, hash, version or decode |
| `catalog/abort` | — | drops an upload |
| `catalog/config` | `base_url` (the mirror), `auto_update`, `auto_fetch` (each optional) | the settings clients follow (stored in the journal, `cfg/catalog`) |

Topic `catalog` (subscribe): `catalog_manifest`, `catalog_updated`,
`web_tools_updated`, `definition_fetched` (the commit results) and
`definition_needed` {`device`, `shards`}. `definitions/list` takes
`query` (substring of id, vendor or model), `offset` and `limit` (≤ 500);
with any of them the result is `{total, offset, items}`, without them the
first 200 summaries as before. With `device` it lists the catalog
candidates the index keys reach for that device (a bank is never listed
whole into RAM). `definitions/get` finds a bank definition by `id` (a scan
of the blocks reading only the identity fields).
| `catalog/keys` | `list` / `add {public_key, name}` / `remove {kid}` | admin: trusted signing keys besides the built-in ones |

### 14.5 Green Power

See 13.7 (`gp/commissioning/open|close`, `gp/devices/list|remove`,
`gp/config`, `gp/bindings/list|create|update|delete|verify`,
`gp/targets/probe`, events `gp/commissioning`, `gp/pending`,
`gp/commissioned`, `gp/failed`, `gp/binding`) and 13.8 for bindings. GP
devices also appear in `devices/*` and `entities/*` like Zigbee devices;
binding management is admin only.

API 1.2 added, for the web UI's Green Power tab (all additive):

- `gp/commissioning/status`: the window, the pending GPDs and the recent
  failures, which were only events before, so a page reload loses nothing;
  the `gp` snapshot carries the same object as `gp_commissioning` (next to
  `bindings`);
- `gp/config/get` and the event `gp/config`: the sink configuration was
  write-only; `gp/config` keeps absent fields and validates its values;
- the hub's own commissioning deadline: `gp/commissioning` is reported for
  API open/close, expiry and the exit after the first pairing even when the
  NCP image does not send `EVT_GP_COMMISSIONING`;
- fields: `time`/`reason_code` on `gp/pending` and `gp/failed`,
  `open`/`involve_proxies`/`exit_on_first_pairing` on `gp/commissioning`,
  `device_id`/`commands` in `gp/devices/list` (the binding trigger picker);
- documented what existed but was missing here: the event `gp/target`, the
  probe's `{probing: true}`, the `gp/binding` body and `state: "removed"`,
  and the parameter names `device`/`binding`.

Known gaps, left open:

- `gp/devices/list` does not read the NCP's Sink Table
  (`GP_SINK_TABLE_READ`): a GPD the NCP knows but the hub does not appears
  only with its first frame (13.2);
- the sink configuration, the pending GPDs and the failures are not
  persisted: after a hub restart `gp/config/get` reports the defaults
  (`source: "default"`) even if the NCP kept another configuration, which
  the hub cannot read back (ZRHP has no `GP_SINK_CONFIG` read);
- `EVT_GP_COMMISSIONING.involve_proxies` is not passed to the engine, so a
  window opened elsewhere (console, NCP) reports the flags of the last API
  request;
- bindings: the target of a binding cannot be changed with
  `gp/bindings/update` (delete and create), group members are not looked
  up by the hub (an on-device group binding needs `members` from the
  client; the UI creates group bindings without them, i.e. hub-side), and
  the per-button command names of GP definitions (13.5) are not exposed, so
  the UI offers GPD command IDs;
- `targets[].last_verified` of a binding is in ms while every other API
  time is in s (kept for compatibility);
- no admin role exists yet: "admin only" operations are open to every
  token.

### 14.6 Network and hub maintenance (API 1.2)

Authenticated sessions only, like every request. Requests that talk to the
NCP run as jobs off the application thread, in two lanes with one job each:
**heavy** (radio or network-changing work: scans, map, channel change,
backup, restore, reset) and **light** (`network/diagnostics`,
`network/set_tx_power`). A second job in a busy lane fails with `busy`
(`"<job> (job N) is running"`). Jobs run on one long-lived worker
thread; a job that starts while another runs needs a thread of its own and
is refused with `busy` while the platform reports less free heap than its
stack plus 16 KiB, or no block for the stack (`"low memory (N KB free)"`,
S3 without PSRAM); a light request does not wait for a scan,
and the rest of the API (devices, entities, `network/info`, permit join, …)
is served meanwhile. `hub/info` gains
`maintenance`: the requests this hub and its NCP support (`diagnostics`,
`topology`, `change_channel`, `reset` always; `energy_scan`, `tx_power`,
`backup`, `restore` by NCP capability bits; `wifi`, `reboot`, `mode`,
`logs`, `token_rotate` by platform). An NCP image that lacks a command
answers it `UNSUPPORTED_COMMAND`, reported as `unsupported`. Further error
codes: `failed` (anything else), `device_unavailable` (NCP link lost),
`no_space` (the result does not fit the hub's free memory; large results are
written as text with fallible allocation, never as a JSON tree).

| Type | Parameters | Result |
|---|---|---|
| `network/energy_scan` | `channels` (list, default 11–26), `duration_exp` (0–8, default 3: ≈ 0.14 s per channel), `passes` (1–10, default 1; `count` is the old name) | **background job** (below). Result: `passes` (done), `passes_requested`, `cancelled`, `channels`: [{`channel`, `energy` (average over the passes, 0–255), `energy_max` (peak), `max_rssi_dbm` (peak), `wifi_overlap`, `score`}], `current`, `wifi_channel` (the hub's own Wi-Fi channel, when known), `recommended`, `duration_ms`; a partial result after every pass but the last. `score` (lower is better) = 4 × energy, +1000 under the hub's 20 MHz Wi-Fi channel (Wi-Fi 1 → 11–14, 6 → 16–19, 11 → 21–24), +120 next to it, +160 for 26, +30 off the Light Link primaries 11/15/20/25 |
| `network/active_scan` | `channels` (list, default 11–26), `duration_exp` (0–6, default 3) | **background job**. Result: `networks`: [{`channel`, `pan_id`, `ext_pan_id` (big-endian hex), `src` (the beacon's sender), `lqi`, `rssi`, `permit_join`, `stack_profile`, `update_id`, `beacons` (how many were heard), `own` (this hub's network)}] — one entry per network and channel, from its best beacon (every router beacons; ZRHP `NWK_ACTIVE_SCAN` 0x010A); `duration_ms`. `unsupported` on NCP images without it |
| `network/change_channel` | `channel` (11–26) | `channel`, `nwk_update_id` once the NCP tuned (ZRHP `NWK_CHANGE_CHANNEL`: `Mgmt_NWK_Update_req` broadcast, switch ≈ 9 s later); the store follows. Sleepy devices find the network by their rejoin scan |
| `network/set_tx_power` | `dbm` (antenna-referred) | `requested_dbm`, `applied_dbm` (the NCP clamps and rounds down to its radio's steps: ESP32-H2 −24…+20, ESP32-C6 −15…+20 dBm, 3 dB); persisted in the host store and re-applied after every NCP restart. `network/info` reports `tx_power` (applied, absent while the NCP reports 0x7F, unknown) and `tx_power_requested` |
| `network/diagnostics` | `reset` (bool), `groups` (`DIAG` group mask, default all) | `ncp` (`SYS_GET_STATUS`: `uptime_s`, `free_aps_slots`, `pta_coex`, `key_export_locked`, `permit_join`, …), `counters` [{`id`, `group`, `name`, `value`}] (`DIAG_GET_COUNTERS`), `frame_counters`, `host_link` (the hub's link counters), `pta` {`capable`, `enabled`}, `tx_power_set`, `unsupported` (commands the NCP lacks), `errors` |
| `network/topology` | `refresh` (default true: a **background job**; false answers at once with the last map, null before the first scan or when it did not fit the 8 KB kept for results), `routes` (default true) | `nodes` [{`nwk`, `ieee`, `type` (`coordinator`/`router`/`end_device`/`unknown`), `device`, `name`, `scanned`, `error`}], `links` [{`source`, `target` (NWK), `lqi`, `relationship`, `depth`}] as reported by `source`, `routes` [{`source`, `dst`, `next_hop`, `status`}], `notes`, `time`, `duration_ms`. The coordinator's rows come from `DIAG_GET_NEIGHBORS`/`ROUTES`/`CHILDREN` (ZRHP has no ZDO loopback), the routers' from Mgmt_Lqi_req/Mgmt_Rtg_req over the air, breadth first (48 routers, 4 s per answer, 150 s at most); cancelling stops before the next router |
| `network/jobs` | — | `running` [{`job`, `kind`, `progress`, `pass`, `passes`, `elapsed_ms`, `eta_ms`, `cancelling`}] (every job, request/response ones included), `results` [{`job`, `kind`}]: the kept results |
| `network/job/result` | `job` or `kind` (or neither: the newest) | `{job, kind, partial: false, result}` of a kept result: the last one of each kind, 8 KB for all kinds together (the oldest go first); `not_found` otherwise |
| `network/job/cancel` | `job` | `cancelling`: an energy scan stops after the current pass (its result is then marked `cancelled`), the map before the next router; an active scan runs to its end (one NCP command). `not_found` for a job that is not running |
| `network/backup` | `keys` (default true) | the backup file, Open ZigBee Coordinator Backup v1 (`zigpy/open-coordinator-backup`; ZRHP 9.1/9.2): the NCP's export (`BKP_EXPORT_*`) plus the hub's devices without keys, `metadata.internal.zigbee_rs`, `stack_specific.zigbee_rs` with the durable counter limits. With keys it is kept as the last backup in the host store; without keys (or when the NCP's key export is locked) it is marked `metadata.internal.incomplete` and cannot be restored. Keys need a non-empty API token |
| `network/backup/last` | — | `backup`: the last complete backup (taken by `network/backup`, `network/reset` or `network/restore`), or null |
| `network/restore` | `backup` (the OCB object; its `devices` may be left out or partial), then any number of requests with `devices` (a batch of `devices[]` entries), each with `more` (bool) while more follow; the first request also takes `allow_ieee_change` (default true), `force`, `skip_backup`. A request with `more` is answered `staged_devices`; the last one starts the restore. Every request is a small JSON value, so a large backup never has to be in the hub's RAM at once (the web UI sends 8 devices per request) | `pan_id`, `channel`, `coordinator_ieee`, `devices`, `nwk_fc_start`, `aps_fc_start`, `link_keys_on_ncp`, `link_keys_host_backed`, `children`, `notes` [{`index`, `note`}], `previous_network_backed_up`. ZRHP `BKP_RESTORE_*` with margins 10 000 / 10 000 (APS 2²⁰ when the file has no TCLK counter, Z2M); the restored network is adopted (store, counters as floors) and its devices imported. An NCP that runs a network needs `force`; it is backed up first unless `skip_backup` |
| `network/reset` | `confirm` = `"RESET"`, `channel` or `channels` (default 11–26), `pan_id`, `ext_pan_id` (big-endian hex), `tx_power`, `skip_backup` | `network` {`pan_id`, `ext_pan_id`, `channel`}, `backup` (the automatic backup of the previous network, or null). Backup first (a failed backup changes nothing unless `skip_backup`), `NWK_LEAVE_RESET` (IEEE override and host endpoints kept), then forming with hub-chosen key and the given parameters. Also forms a network on an empty NCP. Devices stay in the registry and must join again |
| `hub/system` | — | platform facts (`version`, `uptime_s`, `heap` {`free`, `min_free`, `largest_block`}, `mode`, `wifi` {`ssid`, `state`, `ip`, `rssi`, `channel`}, `reset_reason`), `app` (devices, sessions, registry and APS counters), `store` (journal), `ncp` (firmware, resumes, restarts, last error, slots), `time` |
| `hub/logs` | `lines` (default 100, at most 500) | `lines`: the hub's recent log lines (a small RAM ring; keys are never logged) |
| `hub/wifi/set` | `ssid` (1–32 bytes), `password` (empty or 8–63) | `stored`, `reconnecting`: kept in NVS, the station reconnects with them (the connection to the hub drops) |
| `hub/token/rotate` | — | `token`: the new API token (persisted); sessions already authenticated stay, new ones need it |
| `hub/reboot` | — | `rebooting` |
| `hub/mode/set` | `mode` (`hub`, `bridge`) | `mode`, `rebooting`: persisted boot mode, then a reboot |

Events (topic `network`, API 1.2): `network_updated` {`reason`:
`channel_changed` (with `channel`, `nwk_update_id`), `tx_power`, `restored`,
`formed`} — clients read `network/info` again; `job_progress`, `job_done`,
`job_failed` (below).

**Background jobs.** `network/energy_scan`, `network/active_scan` and
`network/topology` (with `refresh`) answer at once with `{job, kind,
passes}` and run on their own thread. Then:

- `job_progress` events (topic `network`) once a second and after every
  pass: `{job, kind, progress (0–1), pass (passes done), passes,
  elapsed_ms, eta_ms, cancelling}`. Within a pass the progress is estimated
  from the radio time (channels × 15.36 ms × (2^`duration_exp` + 1), × 1.5
  for the NCP interleaving the scan with the stack), from the measured time
  of the previous pass, or, for the map, from the routers asked vs queued.
  When the NCP sends the optional `EVT_SCAN_PROGRESS` (proposed ZRHP 0x018B,
  NETWORK event: `u16 req_tid, u8 scan (0 energy, 1 active), u8 channel,
  u8 channels_done, u8 channels_total, u8 networks_found`, after each
  channel) the pass fraction is `channels_done / channels_total`; images
  without it are estimated by time;
- a `job_result` frame `{"type": "job_result", job, kind, partial, result}`
  (or `error` {`code`, `message`} instead of `result`) to the session that
  started the job, subscribed or not: `partial: true` after each energy-scan
  pass, `false` at the end;
- `job_done` {`job`, `kind`, `cancelled`} or `job_failed` {`job`, `kind`,
  `error`} to every `network` subscriber; other clients fetch the result
  with `network/job/result`.

A client that connects while a job runs finds it in `network/jobs` and
follows its `job_progress` events; the job does not depend on the session
that started it. The application thread never waits for the NCP here: it
only reads the job board once a second.

## 15. Boundary to a Home Assistant integration

A later HA custom integration (far future, not part of this work) should be a
thin bridge: no Zigbee logic, no device knowledge, only mapping. The API above
is designed so that it can be:

| HA concept | Comes from | Integration does |
|---|---|---|
| Config entry | hub address, token; discovery via mDNS `_zigbee-rs-hub._tcp` (to be specified with the network part) | connect, `auth`, `subscribe` |
| Hub device | `auth_ok` + `hub/info` | one device, parent of all others (`via_device`) |
| Device registry | device records (4.1) | `identifiers`, `connections`, manufacturer, model, `model_id`, versions, name |
| Entities | entity records (4.2, 4.3): `uid` → `unique_id`, platform, device class, unit, state class, entity category, `enabled_default`, `translation_key`/`name`, capabilities | instantiate one generic entity class per platform; no per-device code |
| State | `entity_state` events | `async_write_ha_state` |
| Availability | device `available` | entity `available` |
| Commands | HA service calls | `entities/command` with the same service and parameter names |
| Config entities | `category: config` entities (number, select, switch, button, text) | ordinary entities with `EntityCategory.CONFIG` |
| Events / device triggers | `event` entities + `event_types` | event entities; device triggers generated from `event_types` |
| Orphaned entities | `orphaned: true` | mark unavailable; HA's own "remove entity" UI deletes them via `entities/delete` |
| Diagnostics | `devices/diagnostics` | HA diagnostics download |
| Repairs | `resolution_changed` with orphaned entities, catalog update failures | optional repair issues |

Requirements this places on the hub API (all covered above): stable `uid`s;
HA's own strings for device classes, units, state classes and categories;
complete capabilities in the entity record (HA needs `options`,
`event_types`, color modes, hvac modes … at entity creation); snapshot + `seq`
on subscribe; HA service and parameter names for commands; a minor-versioned
API.

## 16. Storage and RAM budgets

### 16.1 Flash: 4 MB S3 (development layout, D24)

4 MB boards are development samples: one 2 944 KiB app slot, no OTA,
`hubdata` 1 MiB. `hubdata` holds raw areas (no file system), defined by
`hub_core::layout::HubdataMap` (implemented 2026-11-21; every area is whole
4 KiB sectors, the journal stays where earlier builds put it):

| Area | Offset | Size | Content |
|---|---:|---:|---|
| Host store journal | 0x000000 | 128 KiB | ZRHP host tables, device registry, user definitions (`udef/`), catalog settings (`cfg/catalog`), GP registry and bindings (13.8) |
| Catalog state | 0x020000 | 8 KiB | active-bank record, two 4 KiB slots (9.1) |
| Manifest | 0x022000 | 24 KiB | the installed, verified `manifest.cose` (9.3; 9 KB today) |
| Converter bank A | 0x028000 | 256 KiB | catalog bank (9.1) |
| Converter bank B | 0x068000 | 256 KiB | the other bank (staging, previous) |
| Local definitions | 0x0A8000 | 64 KiB | fetched definitions, two 32 KiB halves (9.2) |
| Scratch | 0x0B8000 | 32 KiB | one downloaded shard or manifest, checked in place (largest shard 19.8 KiB) |
| Web tools | 0x0C0000 | 256 KiB | the definition compiler for the web UI (reserved) |

The map fills the 1 MiB exactly.

### 16.2 Flash: 8 MB S3, the product (`hubdata` 1 920 KiB), approved (D20, D24)

Two 3 MiB app slots with A/B OTA. Same order as 16.1; the journal keeps
its 128 KiB (a larger journal would move every later area of a board that
already holds data):

| Area | Offset | Size |
|---|---:|---:|
| Host store journal | 0x000000 | 128 KiB |
| Catalog state, manifest | 0x020000 | 8 + 24 KiB |
| Converter banks A, B | 0x028000, 0x0B8000 | 2 × 576 KiB |
| Local definitions | 0x148000 | 128 KiB |
| Scratch | 0x168000 | 32 KiB |
| Web tools | 0x170000 | 256 KiB |
| Free | 0x1B0000 | 192 KiB |

**16 MB (Dongle-M class, later):** `HubdataMap::for_size` gives every
`hubdata` of at least 1 920 KiB the 8 MB map; a larger partition only adds
free space until a 16 MB map is specified (larger banks for vendor
templates, 17.4).

**Catalog on 4 MB vs 8 MB.** The release's `bank-4m.zdb` (format 2,
213.1 KiB, 2 289 definitions) fits the 256 KiB slot with 43 KiB to spare;
`bank-8m.zdb` is the same today and may grow to 576 KiB (vendor templates).
A device whose definition is not in the bank (partial translations,
definitions larger than a 4 KiB block, a 4 MB bank that ran out of room)
gets it on demand from the shards (9.3) into the local area (64 KiB on
4 MB, 128 KiB on 8 MB; the largest definition is 8.4 KB).

### 16.3 What fits in a bank

From the measurement in 17.3 (catalog-scale proxy encoding of zhc's
definitions, 4 KiB deflate blocks) and the examples (appendix 20):

| Content | Definitions | Index keys | Definitions, deflated | String table | Index (12 B/key) | Total |
|---|---:|---:|---:|---:|---:|---:|
| Fully declarative zhc (tiers A+B) | 2 569 | 4 084 | 209 KiB | 26 KiB | 47 KiB | **284 KiB** |
| **Curated 4 MB catalog** (D12) | **2 000** | ~3 200 | ~163 KiB | ~26 KiB | ~37 KiB | **~226 KiB** |
| A+B+C (with vendor templates) | 3 788 | 6 088 | 267 KiB | 26 KiB | 71 KiB | **365 KiB** |
| Everything incl. partial D | 4 664 | 7 571 | 457 KiB | 26 KiB | 88 KiB | **572 KiB** |

- **4 MB:** about **2 000** curated definitions per 256 KiB bank (~30 KiB
  spare), which meets the 1 600–2 000 target. All A+B definitions (284 KiB)
  do not quite fit; the less popular ones are fetched on demand (D12).
- **8 MB:** a 576 KiB bank holds all definitions the generator can translate
  with vendor templates (A+B+C, 365 KiB) with ~58 % headroom. Everything
  including partial D-tier translations (572 KiB) would only just fit, and
  partial definitions are not banked anyway (D12).
- Per definition: the measured average for zhc A+B definitions is 216 B of
  CBOR (A mean 169 B, median 75 B; B mean 361 B, median 254 B) and **~83 B
  deflated in a bank block**. Our own richer examples (appendix 20) are 123–
  1 024 B in catalog form and 228 B deflated on average inside one block; the
  simple ones (plain ZCL override, the GP switch and sensors, Philips motion
  with two manufacturer attributes) are 123–278 B, inside the 150–300 B target.

### 16.4 RAM (PSRAM unless noted)

| Item | Size |
|---|---:|
| Bank index (2 000 defs, ~3 200 keys × 12 B) | ~37 KiB in flash (memory-mapped, no RAM) |
| String table | format 2: in flash, no RAM; format 1: 31.9 KB inflated (release bank, 1 524 strings) |
| Inflate buffer for one block + one standalone definition | 8 KiB |
| Per device: entity table (~48 B/entity: binding, transform refs, flags, last state), bytecode and constants (~100–300 B), dispatch entries (16 B/binding) | ~0.5–1.5 KiB |
| 200 devices | ~100–300 KiB |
| Expression interpreter | 16-slot stack, < 1 KiB of internal SRAM, no heap |

Definitions are loaded lazily: at boot only resolution records are read; a
device's definition is inflated, verified and compiled into its tables on its
first frame or API access, then the CBOR is freed.

### 16.5 App code (estimate)

The app slot is 3 MiB on 8 MB boards and 2 944 KiB on the 4 MB dev layout
(D24); the measured application build (engine, ZRHP host core, Wi-Fi,
WebSocket API: 2.1 MiB) is in the README ("Stage 2"). The estimate below
covers the definition engine only.

CBOR decoder ~15 KB, expression VM + verifier ~12 KB, generic ZCL rules and
codecs ~40–60 KB, Tuya ~8 KB, GP decoding (shared `zigbee-gp` codecs) ~10 KB,
named codecs ~10 KB, Ed25519 verify ~15 KB; SHA-256 and inflate come from
mbedTLS/ROM. Roughly 110–130 KB in total, before vendor templates (17.4).
Measured for the implemented part (2026-11-20, `tools/zdef-size`,
xtensa-esp32s3, `opt-level = "s"`, LTO, hub configuration with the SHA
accelerator and ROM `tinfl` behind `zdef`'s traits): decoder + validator +
verifier 84 KB (was 98), VM 26 KB (37), matching 10 KB (19), resolution
43 KB (54), bank reader with Ed25519 101 KB (114); all together 158 KB (200),
still above the estimate; details and next steps in `zdef.md`.

## 17. herdsman generator and measured coverage

### 17.1 Method (how the numbers were measured)

zhc 26.130.0 from npm, loaded in Node 22 with instrumentation by
`tools/herdsman-coverage/measure.cjs` (how to run it:
`tools/herdsman-coverage/README.md`; one run takes ~10 s):

1. Every exported function of `lib/modernExtend` (`m.*`) and of the vendor
   libraries (`tuya.modernExtend.*`, `philips.m.*`, `lumi.lumiModernExtend.*`,
   `ikea.*`, …) is wrapped before the device files load. The wrapper tags the
   returned extend's `fromZigbee`, `toZigbee`, `configure` and `onEvent` items
   with the function name, records the call arguments, and marks the call
   "has function argument" if any argument (or a field of one) is a function
   (`fzConvert`, a `scale` function, an inline value converter).
2. Every exported converter object of `converters/fromZigbee`,
   `converters/toZigbee`, `lib/tuya` (`fz`, `tz`), `lib/legacy` and the vendor
   libraries is indexed by identity ("named converter").
   `tuya.valueConverter.*` objects and the results of
   `tuya.valueConverterBasic.*` and parametrised `tuya.valueConverter.*`
   functions are tagged likewise.
3. All 409 device files are required; every definition (`DefinitionWithExtend`)
   is classified by its items: from a generic extend (`m.*`), a Tuya extend or
   named Tuya converter, a vendor extend or named vendor converter, a named
   standard converter, or **custom** (an inline object or function).
4. Inline `configure` functions are executed against a **recording mock
   device** (endpoints created on demand; `bind`, `configureReporting`,
   `read`, `write`, `command` recorded; `utils.sleep` stubbed; 300 ms limit).
   If one completes it counts as recordable (6.8), else custom.
5. Tiers: **A** generic declarative (only `m.*` without function arguments,
   named standard converters, recordable configure), **B** Tuya declarative
   (A plus Tuya extends, named Tuya converters, DP tables whose converters are
   named or built by `valueConverterBasic`/parametrised named functions),
   **C** vendor template (uses vendor-library extends or converters, nothing
   custom), **D** custom JS (anything inline).

Limits: identity tagging cannot see inside named converters (a named standard
converter counts as translatable only if the hub implements it, see the
top-N figures); the mock run does not detect `configure` code that branches on
device fields it did not set; `exposes` given as a function (106 definitions)
are dynamic but were not counted against A/B on their own.

### 17.2 Generator design (`zhc-gen`)

A Node tool in the definitions repository, pinned to a zhc version:

```text
load zhc (instrumented as in 17.1)
for each definition:
  identity:   zigbeeModel / fingerprint -> match clauses; whiteLabel -> white_label
  extends:    m.<fn>(args)          -> translator table (48 m.* functions used)
              tuya.modernExtend.*   -> tuya flags + entities (dpNumeric, dpBinary, dpEnumLookup, …)
              vendor extends        -> template reference (C tier, 17.4) or "untranslated"
  converters: named fz/tz           -> generic-tier behaviour (no data) or a named codec
  DP tables:  [dp, key, converter]  -> tuya binding + transform:
              raw -> none, divideByN -> div, onOff/trueFalse*/lockUnlock -> bool map,
              valueConverterBasic.lookup -> map, scale/multiplyBy -> mul/div,
              thermostatScheduleDay* -> codec tuya_schedule_day, …
  exposes:    expose type/unit/category/min/max/values -> entity platform, unit, limits
              (z2m expose -> HA platform mapping as in Z2M's HA discovery)
  configure:  recorded ops -> configure: [...]
  meta:       battery.voltageToPercentage -> device.battery; tuyaSendCommand -> tuya.send
emit YAML + coverage record {definition, tier, untranslated items with source file:line (from the .js.map)}
compile YAML with zdefc; run golden tests (18.1); build banks
```

- Entity keys reuse zhc's expose `property` names (`temperature`,
  `local_temperature_calibration`, `child_lock`, …): users moving from
  Zigbee2MQTT recognise them, and HA unique IDs stay meaningful.
- The z2m expose → HA mapping follows Zigbee2MQTT's own HA discovery rules
  (binary → binary_sensor or switch by access, numeric → sensor or number,
  enum → sensor or select, composite climate/light/cover/lock/fan → the HA
  platform), so generated entities behave like Z2M's in HA.
- Partial translation: a C/D definition whose untranslated items only affect
  some exposes is emitted with those entities dropped and `x-partial: true`
  (extension key), and listed in the coverage report. Partial definitions are
  never put into a bank (D12); they are only served on demand.
- **Curation of the 4 MB bank** (D12): fully translated definitions only
  (tiers A and B, and C once its vendor template exists), ordered by
  popularity until the bank is full (~2 000, 16.3); definitions that the
  generic tier already handles identically (most of the 1 282 "naming only"
  ones) rank last, because without them the device still works, only its
  display name is generic.
- **Popularity** (D18, `tools/herdsman-coverage/popularity.py`): zhc has no
  public usage statistics, so popularity is the number of distinct GitHub
  issues and pull requests in `Koenkk/zigbee2mqtt`,
  `Koenkk/zigbee-herdsman-converters`, `zigpy/zha-device-handlers` and
  `zigpy/zha` whose title or body (first 20 000 characters) mentions one of
  the definition's identifiers as a whole word, case-insensitively.
  Identifiers: the model, every `zigbeeModel` string, every fingerprint model
  ID and manufacturer name; only those with ≥ 4 characters and a digit, `.`
  or `_` (zhc has `zigbeeModel` strings such as "Switch" or "motion"), and
  only those that belong to exactly one definition (TS0601 and other shared
  IDs cannot tell definitions apart, so a Tuya device is counted through its
  `_TZE…` names). Issue texts are cached as JSONL and refreshed
  incrementally; the release workflow of the definitions repository reruns
  it for every catalog. Bias to keep in mind: devices with many problems get
  many issues; the score ranks what people own and talk about, which is the
  point, but it is not a market share.
  First run (2026-11-19, ~13 min): 46 758 issues and pull requests (Z2M
  26 313, zhc 13 830, zha-device-handlers 5 594, zha 1 021); 4 266 of the
  4 664 definitions are mentioned at least once, 2 340 of the 2 569 fully
  translated ones. Top of the list: Xiaomi WSDCGQ01LM (208), Tuya
  TS011F_plug_1 (202), Aqara WSDCGQ11LM (189), RTCGQ11LM (183); the two Tuya
  examples of this spec rank 12 (TS0601 T/H sensor, 134) and 14 (ME167 TRV,
  127). Among the fully translated definitions the 1 600th has 2 mentions and
  the 2 000th has 1, so the bank's tail is ordered by small numbers and the
  tie-break (tier A before B, then fewer bytes) matters. Noise: coordinator
  and adapter definitions (SMLIGHT SLZB-06M, 1 083; "Silabs series 2
  router", 186) top the raw list because Z2M's issue template asks for the
  adapter; the catalog builder excludes definitions of coordinator/NCP
  hardware from the ranking.
- **Coverage report** (Markdown + JSON per generator run): per vendor file and
  overall counts per tier, the list of untranslated items ranked by how many
  definitions they block (this ranks the next named codec or vendor template to
  implement), and per definition the reasons.
- **License**: zhc is MIT. Generated definitions record their origin
  (`x-source: zhc@26.130.0 devices/efekta.ts`), and the definitions repository
  carries zhc's MIT notice.

### 17.3 Measured results (zhc 26.130.0)

4 664 definitions (+1 270 white labels; 4 808 `zigbeeModel` strings and 2 763
fingerprint entries):

| Tier | Definitions | Share |
|---|---:|---:|
| A generic declarative | 1 944 | 41.7 % |
| B Tuya declarative | 625 | 13.4 % |
| **A + B: fully declarative** | **2 569** | **55.1 %** |
| C vendor template needed | 1 219 | 26.1 % |
| D custom JavaScript | 876 | 18.8 % |

Details:

- **Needs nothing but naming in tier a**: 1 282 definitions (27.5 %) use only
  generic extends with standard semantics (`m.light`, `m.onOff`,
  `m.temperature`, `m.battery`, `m.iasZoneAlarm`, …) and no recorded configure.
- **Recordable configure**: 1 144 of 1 215 inline `configure` functions
  (94.2 %) completed against the mock device using only bind/report/read/
  write/command.
- **Standard named converters**: 217 distinct are used. A+B definitions fully
  covered when the hub implements the most-used N: top 50 → 2 062 (44.2 % of
  all), top 100 → 2 345 (50.3 %), top 150 → 2 502 (53.6 %).
- **Tuya**: 527 definitions have DP tables; **418 (79.3 %) are fully
  declarative**. Of the 561 `TS0601` definitions, 380 (67.7 %) are B, 77 C,
  103 D. The 5 899 DP-converter uses involve 115 distinct converters; the
  top 5 (`raw`, `onOff`, `lookup`, `divideBy10`, `divideBy100`) cover 74.4 %
  of uses, the top 20 91.1 %, the top 40 95.9 %. A DP table has 12 DPs on
  average (median 9).
- **Efekta**: 76 of 76 definitions are A.
- **C tier**: 624 distinct vendor items; the top 10 alone unblock 644 of the
  1 219 C definitions (53 %), the top 40 unblock 856 (70 %). `philips.m.light`
  (583 definitions) dominates: one "Hue light" template (Hue effects, power-on
  behaviour, gradient flag as parameters) makes 595 of the 608 Philips
  definitions declarative. Next: Lumi cluster helpers and OTA (~130 each),
  Ledvance light (105), IKEA (89 + 66).
- **D tier** reasons (a definition can have several): inline `fromZigbee` 554,
  inline `toZigbee` 495, custom Tuya value converter 98, `m.*` with function
  arguments 86, `onEvent` 50, non-replayable configure 40. 292 D definitions
  have ≤ 800 characters of custom JS in total (candidates for hand-written
  YAML with expressions).
- **Size proxy** (`tools/herdsman-coverage/proxy_size.py`): the
  per-definition generator input (identity strings, extend calls with
  arguments, named converter references, exposes, DP tables), encoded as CBOR
  with integer keys and a shared string table (3 949 strings, 26 KiB
  deflated): A mean 169 B, B mean 361 B; A+B 541 KiB raw, 209 KiB with 4 KiB
  block deflate (×2.6); all tiers 1 159 KiB raw, 457 KiB deflated. Used for
  16.3.

**Expected coverage** of the hub with the generator: ~55 % of zhc definitions
fully declarative from day one (A+B, given the top ~100 standard converters
as generic behaviour and ~40 Tuya converters/codecs), rising to ~70–75 % with
the ten most valuable vendor templates, plus the generic tier for any
standard device zhc does not list at all.

### 17.4 Vendor templates (C tier)

A template is hub code (like a named codec) parametrised by definition data
(`templates: [{name: hue_light, params: {gradient: true}}]`, top-level key
21, reserved in v1). Each costs app flash. Order (D13):

1. **Hue light** (`philips.m.light`, 583 definitions; parameters: gradient,
   effects, power-on behaviour on the Hue cluster 0xFC03/0xFC01);
2. **Lumi** (manufacturer cluster 0xFCC0 helpers, `lumi_specific` struct
   decoding beyond `zcl_tagged`, Lumi OTA and power-outage helpers; ~130
   definitions each);
3. **Tuya specifics**: the remaining schedule codecs
   (`tuya_schedule_week` variants, `thermostatScheduleDayMultiDP*`), time-sync
   variants and the other Tuya named converters the coverage report ranks
   highest.

After that, the coverage report's ranking decides (Ledvance light 105,
IKEA 89 + 66 are next).

### 17.5 Generator results (herdsman-gen, zhc 26.130.0)

The generator is implemented in `faronov/zigbee-rs-definitions` (`generator/`,
TypeScript), next to the catalog CI, as 17.2 proposed. It records the zhc
calls like `measure.cjs`, translates extends, named converters, Tuya DP tables
and replayed `configure` functions, and counts a definition as fully
translated only if **every zhc expose** is claimed by a translated item
(stricter than 17.3, which classified by item kind). Results on 26.130.0
(2026-11-20), from its `coverage/COVERAGE.md`:

| Tier (achieved) | Definitions | Share |
|---|---:|---:|
| A generic declarative | 1 861 | 39.9 % |
| B Tuya declarative | 421 | 9.0 % |
| C with a vendor template (`hue_light` 582, `lumi`, `tuya` families) | 917 | 19.7 % |
| **Fully translated** | **3 199** | **68.6 %** |
| Partial (served on demand only) | 1 465 | 31.4 % |

Catalog built from it with the Python compiler (Rust `zdefc` pending): with
no vendor template in the hub yet, the 4 MB bank holds all 2 284 eligible
A+B and hand-written definitions in 231.6 KiB (index 41.8 KiB, string table
8.2 KiB, blocks 181.2 KiB); with all templates (projection) the 4 MB bank
holds the 2 344 most popular of 3 199 (cut at 2 issue mentions) in 255.9 KiB
and the 8 MB bank all 3 199 in 320.2 KiB. 256 shards total 3.0 MiB (largest
24.6 KiB). Formats fixed there: `docs/catalog-format.md`. The origin of a
generated definition is the v1 key `source` (`zhc devices/<file>.ts`, without
the zhc version and line, so a zhc bump does not change an unchanged
definition's revision or hash); version and line are in the YAML header
comment. Popularity excludes the 13 adapter definitions (`is_adapter` in
`popularity.py`).

## 18. Testing strategy

### 18.1 Golden tests from herdsman

For every generated definition the generator also emits test vectors:

- **Decode**: synthetic frames for every bound attribute / DP / command
  (boundary values: min, max, zero, invalid, negative) plus captured frames
  (18.2); run through zhc's `fromZigbee` converters in Node → expected
  `{property: value}`; run through the hub core (`hub-core` host build, the
  same Rust as on the S3) → entity states; compare after the key mapping.
- **Encode**: for every writable expose, values across its range; zhc's
  `convertSet` against the recording mock (17.1) → expected ZCL writes /
  EF00 frames; the hub's command path → frames; compare byte for byte.
- A definition only enters a bank if its vectors pass; failures appear in the
  coverage report as "translated but divergent".

### 18.2 Frame replay

- Corpus `tests/frames/<definition id>/*.jsonl`: one ZRHP indication per line
  (`EVT_APS_DATA_INDICATION` fields or `EVT_GP_FRAME` fields, hex payload) and
  the expected entity states/events after it.
- Sources: the hub's own capture mode (`devices/diagnostics` keeps the last 50
  frames; a debug setting logs all frames of one device), sniffer captures
  converted from pcap (802.15.4 + Zigbee decryption with the network key), and
  the user's existing devices.
- Replayed in host tests against the full device manager with a mock ZRHP
  link; also used as regression tests for interview and resolution (a captured
  interview reproduces the same resolution).

### 18.3 Format and language

- `zdefc` and the hub share one `zdef` crate (`no_std`): CBOR schema,
  verifier, interpreter. Round trip YAML → CBOR → YAML → CBOR is byte
  identical; hashes are stable across platforms (CI on x86_64 and on the
  Xtensa target through the host-tested crate).
- Expression property tests: random well-typed programs from the grammar,
  compiled, verified, executed against a reference interpreter (Python) with
  random inputs; results equal.
- Fuzzing (`cargo fuzz`): the CBOR decoder, the definition verifier and the
  bytecode verifier/interpreter with arbitrary bytes. Invariants: no panic, no
  allocation above the limits, the step bound holds.
- Tuya EF00 codec vectors from zhc's `LIST_TUYA_DATAPOINT_VALUES` encoding;
  GP decoders reuse zigbee-rs' zigpy golden vectors.

### 18.4 Self-description and GP

- zigbee-rs side: host tests reading every chunk through the ZCL dispatcher;
  the build-time check that the hub verifier accepts the schema; a hash change
  after a settings change.
- Hub side: chunk loss and retry, a wrong hash, an oversize schema, a format
  version from the future.
- GP: `EVT_GP_*` sequences for unidirectional and bidirectional commissioning
  (result 3 then 2), decommissioning, Sink Table reconciliation after an NCP
  swap, the generic tier on the PTM 216Z and zigbee-rs MG22 switch/sensor
  frames; FlowerX GP with every combination of model bits (entity sets);
  BGT1 GP identical reports producing a state refresh each time; the
  `gp_unsecured` warning for level-0 commissioning.
- GP bindings (13.8): capability probe outcomes (zigbee-rs combo, a
  proxy-only lamp, UNSUPPORTED_ATTRIBUTE); the on-device programming
  sequence against recorded `EVT_GP_REMOTE_*` answers, including a
  FAILURE status, a timeout and a target factory reset (drift repair);
  hub-side execution with duplicate frames from two proxies (one command);
  executor change without a double execution; removal of a GPD removing
  its on-device bindings first.

### 18.5 Budgets in CI

The bank builder fails if a bank exceeds its partition size, a definition
exceeds 4 KiB, or an expression exceeds the verifier limits; it prints the
size table (definitions, strings, index) per run. The hub CI checks the app
against its slots: 3 MiB (8 MB production) and 2 944 KiB (4 MB dev layout,
D24; README).

### 18.6 Hardware

On the Thread BR board with the H2 NCP: one device per class (plug with
metering, bulb with color, Aqara sensor, Tuya TRV, Philips remote, IKEA
remote, IAS contact sensor, a zigbee-rs self-describing device, EnOcean or
Hue GP switch, zigbee-rs MG22 GPD sensor), interview to entities to commands,
catalog OTA with a forced failure and rollback. GP bindings: an MG22 GP
switch bound on-device to a zigbee-rs combo light (press with the hub
stopped) and hub-side to a non-GP bulb, with the press-to-light latency
measured against the 150 ms budget (13.8).

## 19. Open questions

None. The questions of the drafts were decided on 2026-11-19 (D8–D21,
section 1). New questions will come from implementation and are tracked as
issues of `faronov/zigbee-rs-hub` and `faronov/zigbee-rs-definitions`.

## 20. Appendix: measured sizes of the examples

Measured with `zdefc measure definitions/examples` (Rust compiler,
2026-11-20; the first draft used the Python prototype, which produced the
same bytes without `requires`, see `zdef.md`). "CBOR" is the standalone form
with the description and the computed `requires`; "catalog form" is without
the description (and without shared-string references); "deflate" is the
catalog form deflated alone. Inside one bank block, the 13 examples take
2 971 B deflated together (228 B each on average); a signed bank of the 12
non-fragment examples is 3 898 B.

| File | Entities | YAML (no comments) | CBOR | Catalog form | Deflate | Longest expression |
|---|---:|---:|---:|---:|---:|---:|
| `innr-sp120.yaml` (plain ZCL, overrides only) | 0 | 466 | 156 | 123 | 114 | — |
| `gp-zigbee-rs-switch.yaml` | 1 | 559 | 181 | 181 | 160 | 2 |
| `philips-sml001-motion.yaml` (2 mfr attributes) | 2 | 724 | 257 | 238 | 211 | — |
| `gp-efektalab-bgt1.yaml` | 3 | 1 002 | 277 | 219 | 187 | — |
| `gp-zigbee-rs-sensor.yaml` | 4 | 1 361 | 320 | 278 | 224 | — |
| `zigbee-rs-selfdesc-thermometer.yaml` | 6 | 1 435 | 393 | 393 | 286 | — |
| `gp-enocean-ptm216z.yaml` | 4 | 1 637 | 480 | 437 | 229 | 15 |
| `gp-diyruz-flowerx.yaml` (3 conditional entities) | 8 | 2 313 | 529 | 494 | 329 | 25 |
| `tuya-ts0601-th-sensor.yaml` (24 mfr names) | 4 | 1 627 | 557 | 523 | 393 | — |
| `philips-rwl022-dimmer.yaml` (18 event types) | 1 | 1 218 | 570 | 545 | 351 | 16 |
| `aqara-rtcgq14lm-motion-p1.yaml` | 9 | 2 586 | 702 | 684 | 455 | 18 |
| `efekta-co2-smart-monitor.yaml` (14 settings) | 14 | 2 578 | 808 | 757 | 446 | — |
| `tuya-ts0601-trv-me167.yaml` (20 mfr names, 7 white labels) | 18 | 3 686 | 1 054 | 1 024 | 691 | 4 |

Observations:

- Simple devices land at 120–320 B, as targeted; settings-heavy and Tuya
  definitions grow with entities (~35–50 B per entity, mostly the key string)
  and manufacturer-name lists (7 B per packed Tuya name).
- Bank blocks compress well because keys, units and binding patterns repeat;
  the catalog-scale average (~83 B per definition, 17.3) is far below the
  examples' average because most real definitions are small (median A
  definition: 75 B of CBOR).
