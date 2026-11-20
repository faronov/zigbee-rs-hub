# hub-devices

Device and entity engine of zigbee-rs-hub, as specified in
[`docs/device-support.md`](../../docs/device-support.md): interview →
fingerprint → resolution through the definition layers → entities with
stable uids → state updates, commands, configure, availability and events,
for Zigbee devices (generic ZCL tier, definitions, Tuya EF00,
self-description) and Green Power devices (generic GP tier, GP definitions,
GP bindings). `std`, sans-IO, host-tested without hardware.

## Modules

| Module | Contents | Spec |
|---|---|---|
| `zcl` | ZCL data types (all widths, strings, struct/array), frames, foundation commands, status names, cluster IDs | — |
| `zdo` | Node/Active EP/Simple descriptors, Bind_req, Device_annce, Mgmt_Leave | 3.1 |
| `fingerprint` | Fingerprint, canonical CBOR, 16-byte hash (identical to `zdef::Fingerprint::hash16`) | 3.2 |
| `model` | Engine-side definition model: entities, bindings, parts, transforms, triggers, configure ops, Tuya/GP flags | 6 |
| `expr` | Expression boundary: values, context, `Expression` trait | 7 |
| `zdef_adapter` | `zdef::load` + conversion into `model`; bytecode run on the `zdef` VM; scoring through `zdef::matching`; self-description decoder | 8, 12.3 |
| `generic` | Tier a: server-cluster rules, reporting defaults, client clusters → `action` events, device-level diagnostics | 5 |
| `resolve` | Layer merge (generic < catalog < self-description < user < overrides), knobs, white labels, `present_if`, default endpoints | 3.3, 6.3 |
| `values`, `codec` | Transform pipeline (decode and inverse), named codecs (`zcl_tagged`, `tuya_schedule_day`, …) | 6.4, 6.6 |
| `hub` | Device manager and entity runtime: interview, materialization (uids, orphans), table-driven dispatch, events, availability, sleepy queue, self-description chunks, tickets | 3, 4.5–4.7 |
| `configure` | Binds, reporting, initial reads, IAS enrollment, Poll Control, definition ops, Tuya flags; idempotent with a configure hash | 5.3, 6.8, 10.3 |
| `commands` | HA service names → ZCL commands / attribute writes / Tuya DPs / hub options, per platform | 4.3, 4.5 |
| `tuya`, `tuya_rt` | EF00 frames and DPs; dispatch, de-dup, time sync, gateway status, MCU version, queries | 10 |
| `gp` | GP events, commissioning, generic GP tier, GP reports 0xA0–0xA3, GP definitions, GP bindings (on-device and hub-side) | 13 |
| `api` | WebSocket JSON message model and a transport-free `ApiServer` (auth, subscriptions with snapshot + `seq`, handlers) | 14 |
| `transport` | `ZigbeeTransport` trait (the boundary to the ZRHP host core) and `MockTransport` | 2 |

## Boundary to the ZRHP host core (`hub-ncp`, branch `phase1`)

The engine never blocks. Outgoing traffic goes through `ZigbeeTransport`;
everything incoming is a method call with the current time.

| Engine | `hub-ncp` (phase1, 5af686b) |
|---|---|
| `aps_send(ApsRequest)` | `APS_DATA_REQUEST` through `Client::start` (non-blocking); `Hub::send_aps` blocks on the confirm, so an adapter must run it off the engine thread or use `start` |
| `gp_request(GpRequest) -> Option<zcl_tsn>` | `Client::call(&GpPairingConfiguration / GpTranslationUpdate / GpTranslationRequest / GpRemoteSinkTableRequest / GpRemoteProxyTableRequest / GpSinkCommissioning / GpSinkRemove / GpSinkConfig)`, `zcl_tsn` from `GpRemoteRsp` (0x0A08–0x0A0D are in the pinned `zigbee-zrhp`) |
| `group_add(ep, group)` | `NWK_GROUP_MEMBERSHIP` (no `hub-ncp` helper yet) |
| `remove_device(ieee)` | `NWK_REMOVE_DEVICE` (no helper yet) |
| `permit_join(duration, target)` | `Hub::permit_join(secs)` (whole network; per-router targets are not in phase1) |
| `Hub::on_aps_indication` | `EVT_APS_DATA_INDICATION` from `Hub::subscribe()` |
| `Hub::on_device_joined` | `EVT_DEVICE_JOINED` with an admitted decision |
| `Hub::on_gp_event` | `EVT_GP_FRAME`, `EVT_GP_COMMISSIONING`, `EVT_GP_COMMISSIONED`, `EVT_GP_COMMISSIONING_FAILED`, `EVT_GP_DECOMMISSIONED`, `EVT_GP_REMOTE_*` (owned mirrors in `gp::types`, field names 1:1 with `zigbee_zrhp::msg::gp`) |
| `Hub::tick(now)` | a 1 s timer |

The hub endpoint (1, HA profile) must be registered with `add_endpoint`.
Wanted later: an `on_aps_confirm` input (faster failures; spec 13.8 times
remote GP requests from the APS confirm, the engine uses 5 s from the send).

## zdef: what is used, and the API requests

Used: `zdef::load` (canonical decode, validation, verifier, `requires`),
the model, `expr::Vm` with a `Host`, `matching::score_clause` through
`Definition::match_score`, `ContentHash`, `Fingerprint::hash16` (equal to
the engine's fingerprint hash, property-tested), the zdefc golden CBOR of
`definitions/examples` (the scenarios load exactly those bytes).

Not used, and why:

1. **`zdef::resolve` for the merge — decided: the engine keeps its own
   merge.** The generic tier needs behaviour that a `zdef::Entity` cannot
   carry and the hub cannot compile at run time (zdef has no compiler):
   attribute-derived scaling (Metering/Electrical multiplier and divisor
   attributes, unless a layer fixes `mul`/`div`), cluster-command write paths
   (On/Off, Move to Level with On/Off, Move to Color Temperature, Go To Lift
   Percentage, Lock/Unlock), mireds ↔ kelvin, IAS zone bits. Making
   `Entity` expressive enough would put engine semantics into the v1 wire
   format. Instead the definition layers are pinned to the reference: every
   vendor scenario checks that the `(endpoint, key)` set a catalog definition
   contributes equals what `zdef::resolve` yields for the same definition
   and fingerprint (`scenarios::assert_zdef_resolve_agrees`).
2. **`expr::Host` lifetimes.** `state()`/`opt()` return `Value<'a>` borrowed
   for the evaluation; a host with owned state needs an arena (the adapter
   uses `typed-arena`). Still open (an owned/`Cow` return or an arena hook).

Closed in `zdef-size` (2026-11-20): `ConfigureOp::Cmd` has an optional
`payload` expression (evaluated when the configure plan is built, with
options and state); part keys `color_x`, `color_y`, `hue`, `saturation`,
`command`; the GP score follows the spec (600, no extras); `GpFingerprint`
carries `switch_info` and `security_level`; `codec.tuya_schedule_week` is
implemented (variants `multi_dp`, `single_dp`, `trv602z`, `trv603wz`, byte
vectors checked against zigbee-herdsman-converters) and the engine loads
with all of `HubFeatures::v1()`; `fp.manufacturer`/`fp.model` in
`present_if`.

## Decisions worth knowing

- **ZCL codec is local.** zigbee-rs `zigbee-zcl` was evaluated and not
  reused: device-side (server) code with 32 B string and 128 B payload
  `heapless` buffers, no `int24`/`uint40`/`int48` or composite types, no
  client-side builders or response parsers. `zigbee-gp` is reused (git
  dependency, the workspace's zigbee-rs revision): the commissioning payload decoder and the
  GP default translation (`Derived` payloads of hub-side bindings).
- **API parameter names.** The envelope's `id` is the request ID, so the
  spec's `id` parameters are named `device`, `definition` and `binding`.
- **uids** are `<device>:<endpoint>:<key>`; device-level diagnostics
  (`linkquality`, `rssi`, `last_seen`) use endpoint 0, GP devices 242.
- **Composite states**: the primary part is the HA state (`on_off`,
  `hvac_mode`, `state`, cover position → open/closed); other parts are
  attributes under their part names; lights expose HA's `brightness`,
  `color_temp_kelvin`, `xy_color`, `hs_color`, `color_mode`.
- **Hub-side GP bindings** de-duplicate (frame counter, command) for 2 s in
  addition to the NCP; contact-mask triggers (0x69/0x6A) are hub-side only
  (a Translation Table entry cannot express a contact mask); a group
  binding needs its members to run on-device, otherwise it is one groupcast.
- The Tuya magic packet is one Read of six Basic attributes (the herdsman
  frame), the only read above the five-attribute rule.

## Tests

`cargo test -p hub-devices`: unit tests per module, golden scenarios in
`src/scenarios/` (plain ZCL sensor and colour light, Efekta settings,
Aqara P1, Philips RWL022 and SML001, Innr SP 120, a generic remote,
zigbee-rs self-description over ZRDS chunks, Tuya ME167 TRV and T/H
sensor, FlowerX GP variants, BGT1 GP, zigbee-rs GP sensor/switch, EnOcean
PTM 216Z, generic GP tier, GP bindings on-device and hub-side), WebSocket
API flows (`src/api/tests.rs`) and property tests (`src/scenarios/props.rs`).
A device simulator (`src/testutil/sim.rs`) answers ZDO/ZCL/EF00. The YAML
loader and the reference expression interpreter in `src/testutil/` are
test-only; a cross-check test asserts that they and the zdefc CBOR + zdef VM
path produce identical states.
