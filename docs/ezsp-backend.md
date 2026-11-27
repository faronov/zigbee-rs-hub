# EZSP coordinator backend

`hub_ezsp::coord` (feature `coord`, on by default) is the hub's coordinator
backend for Silicon Labs EmberZNet NCPs: `hub-coord`'s `Coordinator` and
`NetworkAdmin` over the ASH + EZSP host of `hub-ezsp`. Design:
`docs/coordinator-boundary.md` §2, §3, §5.

**Status: host-tested** against the simulated NCP of `hub_ezsp::sim`, and
**running on one SONOFF Dongle Max** (stock EmberZNet 7.4.5 NCP; "On the
Dongle Max" below). The hub app runs on it (hub-app feature `ezsp`,
`Backend::ezsp`, PR H1; "In the hub app" below), on the desktop with
`hubctl --backend ezsp --sim`, and in the SONOFF Dongle Max image
`zrh-donglem` (PR H2, `docs/dongle-max.md`). Backup and restore ("Backup
and restore"): a backup export and a restore onto the same network ran on
the lab Dongle Max (hub 0.3.0; the joined sensor kept working), and so did
a Zigbee2MQTT-format import of its own network (main 21354727); a restore
or import onto another radio or IEEE is host-tested only.

## Target

SONOFF Dongle Max: a classic ESP32 host and an EFR32MG24 running SONOFF's
stock NCP firmware "1.0.0". The stock ESP32 firmware embeds the EZSP host of
Gecko SDK 4.4.5, i.e. EmberZNet 7.4.x and **EZSP protocol 13**, so 13 is the
version that must work first. The UART runs at 115 200 Bd without RTS/CTS;
the NCP image contains XON/XOFF strings, so software flow control is
assumed possible (see "Flow control").

### Verified on hardware

The probe image on a Dongle Max (stock NCP, the `hub-ezsp` host core, not
yet this backend) confirmed:

- ASH version 2; the RSTACK answering RST carries reset code 0x0B
  (software).
- EZSP 13 after two version exchanges (the host asks for 18, the NCP
  answers 13, the host confirms 13).
- Stack version 7.4.5.0 (`VERSION_INFO`: EmberZNet 7.4.5, build 0, type
  170 = 0xAA, GA).
- `getEui64` (B0E8E8FFFE5D4D03), manufacturing tokens `STRING` "SONOFF" and
  `BOARD_NAME` "Dongle-M".
- `networkInit` on the fresh NCP answers `NOT_JOINED`.
- 115 200 Bd without flow control: no retransmission, NAK or CRC error over
  the probe's exchanges.
- Gecko Bootloader v2.04.02 (for the firmware update follow-up).

### On the Dongle Max (the backend in `zrh-donglem`)

- **Session start: 2.6 s** from the hub's start to `Session::Up` with the
  network up (reset 2.2 s, version 10 ms, configuration 0.2 s, policies and
  endpoints 30 ms, `networkInit` 20 ms, the rest 80 ms; `hub` on the
  console). It took ~52 s first: the gateway's UART read asked ESP-IDF's
  `uart_read_bytes` for 256 bytes, which returns only when they have come
  or the line has been quiet for the whole timeout, so every short frame
  (ACK, response) waited up to 1 s (`firmware/gateway/src/uart.rs` now
  returns what is buffered once the first byte is in).
- **RST and RSTACK**: an idle NCP answers RST after 1.14 s (it reboots). An
  RST in the first ~0.2 s after the MG24's nRESET is lost; the NCP then
  announces its own boot (RSTACK with a hardware reset code) and the host
  sends RST again at once instead of after the 3.2 s RSTACK timeout
  (`Ash::reset`; 4.3 s → 2.2 s).
- **Configuration values of SONOFF's image** (`mg24 ezsp config` in a
  bring-up boot): neighbors 26, APS unicasts 32, bindings 32, address
  table 8, multicast table 8, routes 16, discovery 8, children 32, source
  routes 200, key table 12, trust center address cache 0, broadcasts 15,
  GP proxy 5 / sink 0, indirect transmission 3000 ms, stack profile 0. The
  NCP's memory takes no more: multicast 16 and address table 16 are refused
  with `ERROR_OUT_OF_MEMORY` (address table 12 fits), the trust center
  address cache stays 0. The hub's groups are limited to the 8 multicast
  entries ("Capabilities").
- **`incomingMessageHandler` with a trailing byte**: for a device that
  joined in the NCP's current session the NCP appends one byte (0x02 every
  time; not documented, the SDK host reads the fields and ignores it). The
  backend refused such frames as malformed, so every frame of a freshly
  joined device was lost and its interview ended `partial` with nothing
  learned, while ZDO requests went out and were acknowledged. Bytes after
  the known fields of a callback are now ignored and counted
  (`callbacks_trailing`; `Callback::decode_with_trailing`); a regression
  test feeds the captured frames.
- 115 200 Bd without flow control: no retransmission, NAK, CRC error or
  UART overrun over a session start, joins and interviews.

## Versions

| SDK | EmberZNet | EZSP | |
|---|---|---|---|
| Gecko SDK 4.3.x | 7.3 | 12 | refused (`TooOld`) |
| Gecko SDK 4.4.0–4.4.6, 4.5.0 | 7.4.0–7.5.1 | **13** | Dongle Max |
| Simplicity SDK 2024.6.0–.2 | 8.0.0–8.0.2 | 14 | |
| Simplicity SDK 2024.6.3, 2024.12.x | 8.0.3, 8.1.x | 16 | |
| Simplicity SDK 2025.6.0–.1 | 8.2.0–8.2.1 | 17 | |
| Simplicity SDK 2025.6.2–.3 | 8.2.2–8.2.3 | 18 | |

15 (no public SDK tag) and versions above 18 are refused unless
`VersionPolicy::accept_unverified`. The host asks for 18 in the first
(legacy-format) `version` command and confirms the NCP's own version with a
second one.

Layouts that change between 13 and 14+ (all others are identical in 13–18),
from `command-functions.h` / `ezsp-frame-utilities.{c,h}` of Gecko SDK 4.4.6
and Simplicity SDK 2024.6.0 / 2025.6.3:

| Frame | EZSP 13 | EZSP 14–18 |
|---|---|---|
| statuses | `EzspStatus` / `EmberStatus` (u8) | `sl_status_t` (u32) |
| `startScan`, `exportKey`, `importTransientKey`, `getNetworkKeyInfo` | `sl_status_t` already | `sl_status_t` |
| message tag | u8 | u16 |
| `sendBroadcast`, `sendMulticast` | no alias fields | alias + NWK sequence (+ `broadcastAddr`) |
| `messageSentHandler` | status after the tag | status first |
| `incomingMessageHandler` | LQI, RSSI, sender, indexes | `sl_zigbee_rx_packet_info_t` (adds the sender EUI64) |
| `leaveNetwork` | no parameter | options byte |
| `exportKey` response | key, status | status, key, context |
| `importTransientKey` | EUI64, key, flags | EUI64, key |
| `lookupNodeIdByEui64` response | node ID | status, node ID |
| `readCounters` | 41 values | 41 (14, 16), 42 (18: `CSL_RX_SCHEDULE_FAILED`) |
| `exportLinkKeyByIndex` response | EUI64, key, metadata, status | status, context (holds the EUI64), key, metadata |
| `getCurrentSecurityState`, `setMfgToken`, `get`/`setTokenData`, `setChildData`, `clearKeyTable` | `EmberStatus` (u8) | `sl_status_t` |
| `importLinkKey` | `sl_status_t` already | `sl_status_t` |

## Threading

One `ezsp-worker` thread owns the host and the UART; `submit` only checks
and queues (design §3). Events go to the application's `EventQueue` with
`try_push` (added to hub-coord for this backend): when the application is
behind, the worker keeps servicing ASH (ACKs, retransmissions) but stops
reading callbacks, so the host's event queue fills and ASH not-ready (nRdy,
UG101 §5) makes the NCP hold its callbacks. Blocking on the full queue, as
the ZRHP pump does, would stop the ACKs and let the NCP declare the link
failed. The worker waits on the UART in slices of `EzspOptions::poll`
(10 ms), which bounds how late a submitted request is noticed.

## Session start

Every session begins with an NCP reset, because EmberZNet keeps the EZSP
state (configuration, endpoints, policies, multicast table) in RAM:

1. ASH reset (Cancel + RST, RSTACK; RST again at once when the NCP
   announces its own boot after it) and the version exchange.
2. `getValue(VERSION_INFO)` for the firmware string (build, major, minor,
   patch, special, type: `ezspGetVersionStruct`, Gecko SDK 4.4.6 `ezsp.c`).
3. Configuration values (below), read back for the capabilities.
4. Policies (below).
5. `addEndpoint` for each hub endpoint (`BackendConfig`); only possible
   before `networkInit`.
6. `networkInit`: the NCP resumes the network from its NVM, or answers
   `NOT_JOINED`; NETWORK_UP is awaited (`network_up_timeout`, 10 s).
7. EUI64 and the manufacturing tokens `BOARD_NAME` / `STRING`.
8. The multicast table, written again.
9. With the network up: network parameters, network key info and key ID,
   `maximumPayloadLength`, the extended security bitmask, the concentrator,
   the operator's TX power.
10. `Ownership`, `Session::Up` (new epoch), `NetworkState(Up)`; then the
    callbacks that came meanwhile.

The first session starts at once, and so does the next one after a session
is lost; a failed start is retried after 1, 2, 4 … 30 s. An NCP with an
unsupported EZSP version is reported `Session::Incompatible` (queued
requests end `Unknown(LinkDown)`, new APS requests are refused) and tried
every 30 s.

## Configuration values

Set before `networkInit`, in this order (`DEFAULT_CONFIG`). The tables come
last, by priority, and are **capacities**: the hub asks for at least so
many and never lowers a larger value of the NCP's own; a value the NCP's
memory does not hold (`ERROR_OUT_OF_MEMORY`) is fitted: the largest of at
most 8 values between the NCP's own and the asked one it accepts, tried
from the top (a refusal costs nothing, only the accepted value takes
memory), else the NCP's own. The outcome is logged once at info and kept:
after an NCP reset the next session sets it with one command. The other
values are set as they are (a refusal is logged). The values are read back
for the capabilities; `hub` and `network/diagnostics` list what did not
fit.

| Value | Set to | Why | Dongle Max (SONOFF's image → taken) |
|---|---|---|---|
| `SUPPORTED_NETWORKS` | 1 | RAM for the tables | 1 |
| `STACK_PROFILE` | 2 | ZigBee PRO (explicit) | 0 → 2 |
| `SECURITY_LEVEL` | 5 | NWK encryption + MIC (default, explicit) | 5 |
| `APPLICATION_ZDO_FLAGS` | 0x01 | supported ZDO requests (device announcements) reach the host; the NCP still answers them | 0 → 0x01 |
| `INDIRECT_TRANSMISSION_TIMEOUT` | 7680 ms | spec default for sleepy children | 3000 → 7680 |
| `END_DEVICE_POLL_TIMEOUT` | 8 (2⁸ min) | end devices that negotiate no timeout | 8 |
| `MAX_HOPS` | 30 | default | 30 |
| `TRANSIENT_KEY_TIMEOUT_S` | 300 s | longer than the longest permit-join window (254 s) | 300 |
| `MULTICAST_TABLE_SIZE` | ≥ 16 | hub group memberships (`network/info` `capacity.hub_groups`) | 8 (16 does not fit) |
| `ADDRESS_TABLE_SIZE` | ≥ 16 | EUI64 ↔ short address entries | 8 → 12 |
| `TRUST_CENTER_ADDRESS_CACHE_SIZE` | ≥ 4 | near-simultaneous joins (UG100 default 2) | 0 (nothing fits) |
| `MAX_END_DEVICE_CHILDREN` | ≥ 32 | direct children of the coordinator | 32 |
| `APS_UNICAST_MESSAGE_COUNT` | ≥ 32 | acknowledged unicasts in flight (UG100 default 10) | 32 |
| `NEIGHBOR_TABLE_SIZE` | ≥ 16 | default | 26 (kept) |
| `BROADCAST_TABLE_SIZE` | ≥ 15 | spec value | 15 |
| `SOURCE_ROUTE_TABLE_SIZE` | ≥ 200 | the NCP keeps the source routes (high-RAM concentrator); the host keeps none; last: the largest table takes what is left | 200 |

`PACKET_BUFFER_COUNT` is not set: EmberZNet 7.4 sizes packet buffers at
build time ("The NCP no longer supports configuration of packet buffer count
at runtime", Gecko SDK 4.4.6 `ezsp-enum.h`).

## Policies

| Policy | Decision | Why |
|---|---|---|
| `TRUST_CENTER_POLICY` | `ALLOW_UNSECURED_REJOINS` (0x02); `ALLOW_JOINS \| ALLOW_UNSECURED_REJOINS` (0x03) while joining is permitted | new joins only while the network is open; devices that missed a key update may rejoin through the trust center |
| `TC_KEY_REQUEST_POLICY` | `ALLOW_TC_KEY_REQUESTS_AND_SEND_CURRENT_KEY` (0x51) | Zigbee 3.0 devices request a TC link key after joining; with hashed link keys the "current key" is unique per device |
| `APP_KEY_REQUEST_POLICY` | `DENY_APP_KEY_REQUESTS` (0x60) | the hub brokers no application link keys |
| `MESSAGE_CONTENTS_IN_CALLBACK_POLICY` | `MESSAGE_TAG_ONLY_IN_CALLBACK` (0x40) | less UART traffic; outcomes go by tag |

The trust center choices follow Silabs' `network-creator-security` plugin
(Gecko SDK 4.4.6). Both lists are `EzspOptions` fields.

## Security when forming

`NetworkAdmin::form` sets the initial security state of a Zigbee 3.0
centralized network the way `network-creator-security` does:
`TRUST_CENTER_GLOBAL_LINK_KEY | HAVE_PRECONFIGURED_KEY | HAVE_NETWORK_KEY |
NO_FRAME_COUNTER_RESET | REQUIRE_ENCRYPTED_KEY |
TRUST_CENTER_USES_HASHED_LINK_KEY` (0x1B84), a random root key (each
device's TC link key is the root key hashed with its EUI64), the network key
from `FormParams` or random, and the extended security bitmask
`JOINER_GLOBAL_LINK_KEY | NWK_LEAVE_REQUEST_NOT_ALLOWED`. The trust center
ignoring NWK leave requests is not kept across NCP resets, so it is set
again on every NETWORK_UP. Random keys, PAN ID and EPID are the host's
random bytes (SipHash keyed by the OS) XOR the NCP's `getRandomNumber`. The
channel is the quietest of the mask (energy scan) unless the mask has one
channel.

Permit join: `importTransientKey(FF…FF, "ZigBeeAlliance09")` (joining
devices use the well-known key), the open trust center policy,
`permitJoining(n)` and a ZDO Mgmt_Permit_Joining_req broadcast to 0xFFFC
(or the request to one router only). At 0 or when the window ends: the
closed policy and `clearTransientLinkKeys`. A request that fails half way
is undone the same way (and `permitJoining(0)`), so nothing stays open that
the hub does not report; a new request replaces the window of the previous
one. 255 ("for ever", not allowed from R21) is clamped to 254 s.

## Requests

See the `hub_ezsp::coord` module docs. In short: tag ↔ `RequestId`;
`Accepted` on the send response; `messageSentHandler` decides `Delivered`
(acknowledged unicast) / `Sent` / `NotDelivered(NoAck)`; buffer and slot
exhaustion is `Refused(Busy)`; no `messageSentHandler` within the deadline
(60 s default; checked only after the callbacks received before it) is
`Unknown(Timeout)`; an NCP reset ends the requests on the
NCP with `Unknown(DeliveryUnknown)` after `Session::Down` and keeps the
queued ones for the next session (with `EzspOptions::traffic`, as the
hub app sets it: ends them `Refused(LinkDown)` instead, see "In the hub
app"). Without a session, control requests are refused `LinkDown`; APS
requests, and with `EzspOptions::traffic_hold` control requests too, are
queued. Control requests are barriers.
`RemoveDevice` (reachable or `force`) forgets the address; a router in
the neighbor table gets `removeDevice(its short address, its EUI64, its
EUI64)` (an APS Remove-Device encrypted with its link key, asking it to
leave), any other device's key table entry (an imported or install-code
key) is erased (`findKeyTableEntry` + `eraseKeyTableEntry`; with hashed
link keys there is usually none). The router keeps its entry: the
Remove-Device is encrypted with it. The engine's ZDO Mgmt_Leave_req stays
the primary path for reachable devices. A child entry stays until the
child leaves or ages out: EZSP 13 has no `removeChild` (EZSP ≥ 14), and
dropping a sleepy child before it polled would lose the Mgmt_Leave_req
queued for it (it would rejoin); the app does not import a removed device
again (`hub_app::removed`). Statuses of these frames are logged, not
errors. Same layouts in EZSP 13–18 (Gecko SDK 4.4.6, zigbee-herdsman's
`ezsp.ts`); not run on a Dongle Max yet.

## Flow control

- RTS/CTS: none on the Dongle Max. ASH nRdy covers callbacks only.
- XON/XOFF (UG101 §4): a raw 0x11/0x13 from the NCP is never frame data.
  XOFF holds the host's transmitter (ACK, NAK, DATA, RST) until XON, or for
  at most `AshConfig::xoff_timeout_ms` (1 s; UG101 gives no value) so that
  a lost XON cannot stall the link; the ACK timer does not run meanwhile.
  XON/XOFF between an Escape Byte and the byte it escapes leave the escape
  pending. The host never sends XON/XOFF (the NCP ignores them) and escapes
  0x11/0x13 in its own frames. A frame already written to the UART is not
  recalled: the pause applies between frames. If the ESP32 UART driver
  handles software flow control itself, the ASH layer only sees what passes
  through.

## Capabilities

`backend: Ezsp`, `ownership: NcpNv`, `max_in_flight` =
`APS_UNICAST_MESSAGE_COUNT`, `max_asdu` = `maximumPayloadLength`, table
sizes from the configuration values the NCP took, group membership,
energy/active scan, channel change and TX power, diagnostics, backup (with
keys) and restore. No fragmentation, Green Power, coexistence or firmware
update yet.

The application shows the capacities in `network/info` (`capacity`:
`in_flight`, `asdu`, `hub_groups`, `address_table`, `children`,
`neighbors`, `routes`, `source_routes`, `bindings`, `link_keys`) and limits
the hub endpoint's groups to the multicast table: `groups/create` or
`groups/update` with the hub as a member is refused `no_space` once it is
a member of that many groups (groups without the hub are not limited).

## Identity and ownership

The NCP owns its network (`NcpNv`, design §2.5): the backend reports the
NCP's identity and never decides ownership. It reports
`Ownership(Observed(identity))`, never `Owned`: the EPID, PAN ID, channel,
key sequence, coordinator IEEE and the key ID (SHA-256 of the network key,
§2.5) when the security manager exports the key, `None` otherwise (Secure
Vault), in which case the EPID alone decides (§8.5). It reports `NoNetwork`
when `networkInit` answers `NOT_JOINED`, and `Unknown` before a session.

hub-app decides: it compares the observed identity with the `hub/owner`
tag (§2.7) and turns it into owned or foreign ("In the hub app").

## In the hub app

`hub_app::Backend::ezsp(transport, clock, opts, store)` starts this
backend when the app starts (`App::start_backend`): the coordinator and
`EzspAdmin` on the app's event queue, with the hub endpoint, and the hub's
records in `store` (`hub_app::NcpNvRecords`: the hub's own `hubdata`
journal, not the NCP's). `hubctl --backend ezsp --sim [--sim-devices]
[--store FILE] serve` runs the whole app on the simulated NCP (EZSP 13,
EmberZNet 7.4.5, a network formed; the devices in its neighbor table).

**Ownership flow** (design §2.5, §2.7; `NcpNvRecords`):

1. Start: this backend's tag of a formed network makes the records
   `unverified` (loaded, read-only); its tag naming no formed network
   (`epid: null`, or a network being formed: channel 0) makes them
   `owned`; another backend's tag makes them `foreign` (hidden). An
   untagged store is never taken silently: a ZRHP image's stored network
   gets the tag ZRHP's migration would write (`{"backend":"zrhp",…}`, so
   `foreign`); network-scoped records without a stored network stay
   `unverified` and hidden (closed, no device traffic) until the operator
   adopts or clears them (or forms or restores a network over them); an
   empty store is `untagged` until the first network, and the first
   network-scoped record written meanwhile (a rule, a group) tags it
   `{"backend":"ezsp","epid":null}`, so a restart knows the records are
   the hub's.
2. The session's `Ownership(Observed(identity))` (before `Session::Up`):
   matching the tag (EPID; the key ID too when both are known, else the
   EPID alone, design §8.5), or no tag yet → `owned`, the tag written or
   its informative fields (PAN ID, channel, key sequence, coordinator)
   rewritten; a key ID already in the tag stays when the NCP does not
   export its key now. Another network → `foreign`, nothing written.
   Only reports of the current session count: each with its session's
   epoch, and only while the backend's status still shows it (not after
   an NCP restart in between).
3. Device traffic (APS, groups, permit join, removal, the device
   inventory) is refused until step 2 matched in the current session; a
   session loss, `NoNetwork` or `Unknown` stop it again until the next
   match. The flag is shared with this backend (`EzspOptions::traffic`)
   and names the session: `(epoch << 1) | open`. The worker stores
   `epoch << 1` when a session starts (before its ownership report) and 0
   when it ends, and ends queued requests `Refused(LinkDown)` while it is
   not open for its session; the records open it with a compare-and-swap
   for the epoch whose report matched. So nothing queued before a reset,
   and no late report of an earlier session, reaches a network not
   decided yet. The hub
   endpoint's group replay, the import of the NCP's devices and the state
   refresh wait for it.
   Right after the app starts or a session ends, while the next session's
   ownership report may still verify the records (untagged, owned, or
   this backend's tag; not foreign, of unknown origin, or being
   replaced), the engine's requests wait instead of being refused, for at
   most `TRAFFIC_HOLD` (3 s): Home Assistant reconnecting to a restarted
   hub sends `network/permit_join` at once, as it may on ZRHP, whose pump
   queues requests until its session is up. The worker keeps requests
   queued meanwhile, control requests included
   (`EzspOptions::traffic_hold`, set by `Backend::ezsp`), and sends them
   only once the records open the flag for that session; those queued
   when a session ends still end at once. When the report decides against
   them (another network, `NoNetwork`) or none comes within the hold,
   `CoordTransport` cancels them and they end `Refused(LinkDown)` (the
   API's `busy`) within about a second; new ones are refused at once
   again.
4. `NoNetwork`: `network/info.ownership` `no_network`; `network/form`
   forms a network (only then, and only over the hub's records of no
   formed network: no tag, or its tag with `epid: null` or of a network
   being formed; no `confirm`), or `network/reset` ("RESET"), which also
   replaces a network the NCP lost. While the operator
   replaces the network (`network/form`, `network/reset`,
   `network/restore`), reports decide nothing; just before `formNetwork`
   the records are tagged with the extended PAN ID it forms (picked by the
   hub unless the request names one, the backup's for a restore; key ID
   unknown), so an interrupted job (a timeout, a reboot) still leaves them
   the new network's; afterwards the tag gets the full identity.
   `network/restore` needs `force` over a network the NCP stores and
   `confirm` over records of another network than the backup's
   (`Records::may_restore`: another tag, records of unknown origin; they
   are re-tagged); the backend's `restore_check` runs before anything
   changes, then the running network is backed up and the backup kept
   before the network is left (`bkp/replaced/<ext_pan_id>`, the last 3
   networks, hub-scoped, so also over closed records;
   `network/backup/replaced` with an API token, and in the error frame of
   a restore or reset that fails after the leave), the stored network
   left, the records tagged and the backup restored. A backup without the
   network key is refused (`backup_incomplete`) unless `skip_backup`; only
   the NCP running the backup's own network (extended PAN ID and key)
   already kept as the last backup (a retry after a restore that failed
   late) is not kept again as a replaced network. Every session's `Up` sets the
   engine's coordinator IEEE (binds, the IAS CIE address, `hub/info`): a
   restore may change it.
5. `foreign`: `network/info.ownership` `mismatch` and
   `network/info.registry` `{"state":"foreign","backend","why"}`;
   `network/registry/adopt` (the records become the NCP network's) or
   `network/registry/clear` (deleted, the NCP's devices imported again),
   both with `"confirm": true`.

The operator's TX power is kept as `ezsp/tx_power` (backend-scoped) and
applied in every session (`EzspOptions::tx_power`). A device leave
(`DeviceLeft`) keeps the device: unavailable with `unavailable_reason:
"left"` until its next frame or rejoin; removal is `devices/remove`.

## Backup and restore

`NetworkAdmin::backup_export`, `restore_encoder`, `restore_check` and
`backup_restore`; hub-app writes and reads the Open Coordinator Backup
(OCB, `crates/hub-app/src/ocb.rs`) in the form bellows (zigpy/ZHA) and
zigbee-herdsman's ember adapter (Z2M) use, so a network moves between Z2M,
ZHA and the hub (with the devices' records too: `network/import`,
[`docs/migration.md`](migration.md), restores through the same path).
Frames and structs: Gecko SDK 4.4.6 (EZSP 13) and
Simplicity SDK 2024.6.0, 2024.12.0, 2025.6.0, 2025.6.3 (EZSP 14–18,
identical there), table "Versions" above.

**Export** (admin job, the network up):

| OCB | EZSP |
|---|---|
| `coordinator_ieee` | `getEui64` |
| `pan_id`, `extended_pan_id`, `channel`, `channel_mask`, `nwk_update_id`, `nwk_manager`, `tx_power` | `getNetworkParameters` (the TX power the operator set, else the NCP's) |
| `security_level` | `getConfigurationValue(SECURITY_LEVEL)` |
| `network_key` {`key`, `sequence_number`, `frame_counter`} | `exportKey` (network key context) and `getNetworkKeyInfo` |
| `stack_specific.ezsp.hashed_tclk` | `exportKey` (TC link key context): the root key the trust center hashes per device, when `getCurrentSecurityState` reports `TRUST_CENTER_USES_HASHED_LINK_KEY` |
| `metadata.internal.network.tc_link_key` {`key`, `frame_counter`}, `tc_address` | `ZigBeeAlliance09` (hashed link keys, as bellows writes it; else the exported key), `getValue(APS_FRAME_COUNTER)`, the coordinator. Without a readable APS counter the section is left out: a restore then adds 2²⁰ |
| `metadata.internal.ezspVersion` | the negotiated EZSP version (Z2M's ember adapter loads no EmberZNet backup without it and `stack_specific.ezsp`) |
| `devices[]` with `is_child` | the child table (`getChildData` up to `MAX_END_DEVICE_CHILDREN`) |
| `devices[].link_key` {`key`, `tx_counter`, `rx_counter`} | the key table (`exportLinkKeyByIndex` up to `KEY_TABLE_SIZE`; usually empty with hashed link keys); a device's short address from the joins seen or `lookupNodeIdByEui64` |

Without `keys`, or when the security manager does not export the network
key, the backup has no keys and no key table and is marked incomplete (it
cannot be restored).

**Restore** (bellows' order; `NetworkAdmin::restore_check` runs the
checks of step 0 alone, without changing anything):

0. Checks: the backup's coordinator, network (security level 5, channel
   11–26) and network key; both frame counters plus their margins below
   2³²; the coordinator IEEE (below). A refusal leaves the NCP as it is.
   The counters start from the NCP's own NWK and APS frame counters
   (`getNetworkKeyInfo`, `getValue`) where they are higher than the
   backup's, whatever network they belong to (a higher counter is always
   safe; an older backup of the running network would otherwise go back
   below what its devices have seen): of the network the NCP stores, up
   or not, or of the one it left just before (hub-app leaves first; the
   backend reads them before every leave), note `floor_raised`. A stored
   network whose counter the NCP does not report (or reports as 0 while
   it is not up: UNVERIFIED whether EmberZNet has it before
   `networkInit`) gets a margin of at least 2²⁰ over the backup's (note
   `counters_unknown`; the margin used for a backup without an APS
   counter, ~150 days of a busy coordinator's frames).
1. hub-app tags the records with the backup's extended PAN ID (after
   leaving the network the NCP stores, as for `network/reset`), then the
   backend leaves any network left and clears the key table
   (`clearKeyTable`).
2. The coordinator IEEE, when it differs and `allow_ieee_change`: the
   rewritable restored-EUI64 stack token (`setTokenData`
   `CREATOR_STACK_RESTORED_EUI64` 0xE12A, then the NVM3 key 0x1E12A, as
   bellows tries them) if the NCP has the token interface; else the custom
   EUI-64 manufacturing token (`setMfgToken(MFG_CUSTOM_EUI_64)`, 0x0C).
   That token "is set only if currently unset" (UG100 successor §5): it
   can be written **once, never again**, so it is written only with
   `RestoreOptions::ieee_write_once` (API `confirm_ieee_write_once`;
   bellows' `i_understand_i_can_update_eui64_only_once_and_i_still_want_to_do_it`),
   and a restore is refused when it already holds another IEEE (keep the
   NCP's IEEE with `allow_ieee_change: false`: note `ieee_kept`). The NCP
   is reset (a new session) and must report the backup's IEEE.
3. `setValue(NWK_FRAME_COUNTER)` and `setValue(APS_FRAME_COUNTER)` (0x23,
   0x24, u32; without a network) to the counters of step 0 plus the margins
   (hub-app: 10 000 each, zigpy's default `counter_increment`; APS 2²⁰
   without a TCLK counter), rounded up to a multiple of 4096. EmberZNet
   keeps the counter in its NVM truncated to 0x1000: `setValue` stores it
   truncated, `formNetwork` does not advance it, and each initialization
   (`networkInit`) resumes from the stored value and stores it plus 0x1000
   for the next one (Silicon Labs, host-NCP to RCP migration notes; lab
   unit, 7.4.5: NWK 96 058 set, 94 208 after the restore's reset). On the
   boundary the stored value is the start itself (the APS counter is
   assumed to behave alike: 7.4.5 does not report it). Refused → the
   restore fails (forming with counters from 0 would make the devices drop
   the coordinator's frames).
4. `setInitialSecurityState`: as forming (0x1B84: global TC link key,
   preconfigured and network key, `NO_FRAME_COUNTER_RESET`, encrypted key
   required, hashed link keys) with the backup's network key and sequence
   and the hashed TCLK root (a random one for another stack's backup: note
   `tclk_root_generated`; the devices' own link keys go to the key table);
   the extended security bitmask as forming.
5. `importLinkKey` for the devices' link keys, up to `KEY_TABLE_SIZE`
   (12 on the Dongle Max's stock NCP, `network/info` `capacity.link_keys`;
   the rest: note `ignored`). Their frame counters cannot be set.
6. `formNetwork` with the backup's PAN ID, EPID, channel, mask, NWK
   manager and update ID (TX power: the operator's, the backup's, else
   8 dBm; a backup's power the NCP refuses, `PHY_INVALID_POWER` /
   `SL_STATUS_TRANSMIT_INVALID_POWER`, forms again at 8 dBm, as bellows);
   NETWORK_UP awaited.
7. The NWK update ID: bellows found that `formNetwork` ignores it; it is
   written into the network management stack token
   (`CREATOR_STACK_NETWORK_MANAGEMENT` 0xE125 / 0x1E125, `updateId` at
   offset 6 of `tokTypeStackNetworkManagement`) when the NCP has the token
   interface, else note `update_id_not_restored` when the NCP formed with
   another.
8. `setChildData` for the backup's children with a short address, up to
   `MAX_END_DEVICE_CHILDREN` (sleepy unless the capability says receiver
   on; the rest: note `child_demoted`). bellows found that it leaves the
   NVM child table entry's flags 0x00 where a working coordinator has
   0x80: with the token interface the entry (`NVM3KEY_STACK_CHILD_TABLE`
   0x10580, then `CREATOR_STACK_CHILD_TABLE` 0xFF0D, at the child's index;
   11 bytes, `tokTypeStackChildTable`: EUI64, node ID, flags) is read back
   and, when it is exactly this child, written with 0x80.
9. An NCP reset (a new session, `networkInit`): the restored network must
   come back with the backup's EPID, PAN ID and key ID. This first
   initialization resumes at the start of step 3 again, below the counters
   the network used since `formNetwork` (its concentrator's MTORR, link
   status, ZDO answers, steps 7 and 8), which devices may have heard.
10. Another NCP reset: the second initialization resumes 0x1000 above
    the start, past every counter used before; the network must come back
    again. The counters it resumed from are read back: the report's
    `nwk_fc_start` and `aps_fc_start` (those of step 3 where the NCP does
    not report them; a warning if the NWK counter is not above the one
    read just before step 9).

The NCP resets of steps 2, 9 and 10 end the session and start a new one from
the worker's loop (`Session::Down`, `Up` with a new epoch); the restore
waits and goes on there, so the session start does not run on top of the
job's stack (the worker has 16 KB). hub-app's records decide nothing while
the network is replaced (`Records::replacing`) and take the restored
network afterwards (the tag rewritten with its identity, traffic opened
for the new session).

Keys never reach a log or an error text: responses that carry one
(`exportKey`, `exportLinkKeyByIndex`) are not formatted
(`HostError::UnexpectedResponse` shows their frame ID only), the restore
data's `Debug` leaves the keys out, and a test checks the log lines of an
export and restore for the keys it used.

## Diagnostics

The EmberZNet counters (`readCounters`, or `readAndClearCounters` with
reset) by name, the NWK frame counter, and host counters: ASH frames,
retransmissions, ACKs, NAKs, CRC and other bad frames, discarded and stale
RSTACKs, XOFF; EZSP commands, callbacks, callbacks with trailing bytes, bad
frames and unexpected responses; NCP overflows, sessions, failed starts,
devices left, route records, GP frames ignored; the UART driver's overruns
and line errors (`Transport::line_errors`); and the last session start,
phase by phase (`start_ms`, `start_reset_ms`, `start_version_ms`,
`start_config_ms`, `start_setup_ms`, `start_network_ms`, `start_rest_ms`).
The same, with the configuration values that did not fit, is in
`EzspOptions::link` (`LinkReport`, refreshed about once a second) for a
firmware console: `hub` on the Dongle Max.

## Not verified, known gaps

- Beyond the session start above, everything behavioural is checked
  against the simulator only: callback
  ordering and timing, error statuses of a real NCP (table index past the
  end, key not exportable, no network), whether NETWORK_OPENED/CLOSED
  follow `permitJoining`, CHANNEL_CHANGED after a channel change of the
  coordinator itself.
- `sendMulticast.broadcastAddr` (EZSP ≥ 14) is 0xFFFD; undocumented.
- The XOFF timeout (1 s) and the 10 ms UART slice are choices.
- Leaves: `trustCenterJoinHandler` DEVICE_LEFT becomes
  `CoordEvent::DeviceLeft` with `rejoin: None` (the trust center's update
  does not say); the engine keeps the device, unavailable with the reason
  `left` until it is heard again. The ZRHP adapter does not emit it yet.
- `TxOptions::extended_timeout` is not applied (EmberZNet sets it per
  destination with `setExtendedTimeout`).
- Beacons of an active scan carry no sender address (0xFFFF).
- Child table entries have no time since the last poll (0).
- Backup and restore (an export and a same-network restore ran on the
  lab unit, not checked frame by frame; the rest from the simulator):
  whether the stock "1.0.0" image has
  the token interface (`getTokenData`) and which key it takes (creator or
  NVM3 key); `getValue(APS_FRAME_COUNTER)` does not answer on the lab
  unit's 7.4.5 (backups have no TCLK section, restores add 2²⁰); the statuses for a key table index without an
  entry and for `setMfgToken` over a written token; whether
  `setValue(NWK_FRAME_COUNTER)` is refused with a network up; the
  precedence of the restored-EUI64 token over the custom EUI-64; that
  `formNetwork` ignores `nwkUpdateId` and that `setChildData` leaves the
  child table flags 0x00 (bellows' findings, modelled); the child table
  entry's layout (bellows'); the status of a TX power `formNetwork`
  refuses; and that `setChildData` children are kept across the reset.
- Z2M interop: whatever byte order zigbee-herdsman's ember adapter writes
  `extended_pan_id` and `coordinator_ieee` in, the hub reads Z2M files
  exactly as zigpy (and so ZHA's backup upload) does: big-endian, no
  detection or swap (zigpy's Z2M handling is `internal.date`, unpadded
  NWK addresses, the optional `is_child` and the default TCLK; its own
  test reads a zigbee-herdsman file so). A Z2M file moves to the hub as it
  moves to ZHA. Not checked with a real Z2M ember file.

## Follow-ups

1. Done here: network backup and restore (OCB), "Backup and restore"
   above; its hardware checks are part of 6.
2. **Green Power via EZSP**, in product scope: GP endpoint 242,
   `gpepIncomingMessageHandler` → `CoordEvent::Gp` (the callback already
   reaches the worker's GP path), sink table (`gpSinkTable*`,
   `gpSinkCommission`), proxy table, `dGpSend` / `dGpSentHandler` for GP
   requests and remote provisioning (`Accepted.zcl_tsn` chosen by the
   host).
3. NCP firmware update: `launchStandaloneBootloader` + XMODEM GBL
   (`FwUpdateMethod::GeckoXmodem`).
4. Force removal: done for routers of the neighbor table
   (`removeDevice` to the router itself) and key table entries; an APS
   Remove-Device to the parent of another router's sleepy child (the
   parent from `trustCenterJoinHandler`) and `removeChild` on EZSP ≥ 14
   for an unreachable child of the coordinator are still open.
5. Done in H2: the Dongle-M firmware on this backend (`firmware/gateway`:
   the UART transport on the ESP32, the hub app, its records in the
   claimed `files` region; `docs/dongle-max.md`). Done in H1: the hub-app
   `ezsp` feature, the `NcpNv` ownership decision, the TX power kept
   across restarts.
6. HIL on the Dongle Max, still open: XON/XOFF on the 115 200 Bd line, key
   export, channel change, callback bursts, more devices; backup and
   restore (the open points of "Not verified", a Z2M ↔ hub move with real
   devices, the one-time IEEE token on a spare dongle) (done: session
   start, capacities, permit join, join and interview of a sleepy end
   device).

## Sources

- UG101 (ASH) §2–§7 and the UG100 successor "Simplicity SDK EZSP Reference
  Guide" §2–§10 on docs.silabs.com (9.1.2); the AN706 successor for flow
  control and reset.
- Gecko SDK 4.4.6 (EZSP 13): `ezsp-enum.h`, `command-functions.h`,
  `ezsp-frame-utilities.{c,h}`, `ember-types.h`, `error-def.h`,
  `sl_status.h`, `ezsp.c`, `zigbee-security-manager-types.h`,
  `zigbee-device-stack.h` (`emberChannelChangeRequest`), the
  `network-creator-security` and `concentrator` plugins.
- Simplicity SDK 2024.6.0, 2024.12.0, 2025.6.0, 2025.6.3 (EZSP 14–18): the
  same generated files and `sl_zigbee_types.h`.
- EmberZNet 7.4.5 API reference, `token-stack.h` (docs.silabs.com): the
  creator codes and NVM3 keys of `STACK_RESTORED_EUI64` and
  `STACK_NETWORK_MANAGEMENT`, `tokTypeStackNetworkManagement`.
- Behaviour, not code: bellows (`zigbee/application.py`
  `write_network_info`, `ezsp/__init__.py` `write_custom_eui64`,
  `ezsp/v13`), zigpy (`backups.py`: `counter_increment` 10 000, the OCB
  conversion), zigbee-herdsman (`adapter/ember/adapter/emberAdapter.ts`,
  `utils/backup.ts`); GitHub `dev`/`master`, November 2026.

Numeric facts only (IDs, values, field order); no code was taken from the
SDK or from GPL hosts.
