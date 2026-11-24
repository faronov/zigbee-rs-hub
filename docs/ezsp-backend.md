# EZSP coordinator backend

`hub_ezsp::coord` (feature `coord`, on by default) is the hub's coordinator
backend for Silicon Labs EmberZNet NCPs: `hub-coord`'s `Coordinator` and
`NetworkAdmin` over the ASH + EZSP host of `hub-ezsp`. Design:
`docs/coordinator-boundary.md` §2, §3, §5.

**Status: host-tested only**, against the simulated NCP of `hub_ezsp::sim`.
The hub app runs on it (hub-app feature `ezsp`, `Backend::ezsp`, PR H1;
"In the hub app" below), on the desktop with `hubctl --backend ezsp --sim`.
Not in a firmware yet (the Dongle-M: PR H2), and not run against hardware
yet (HIL on the SONOFF Dongle Max is a follow-up).

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

1. ASH reset (Cancel + RST, RSTACK) and the version exchange.
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

Set before `networkInit`; a value the NCP refuses (no RAM) is logged, and
the capabilities report what it took.

| Value | Set to | Why |
|---|---|---|
| `STACK_PROFILE` | 2 | ZigBee PRO (default, explicit) |
| `SECURITY_LEVEL` | 5 | NWK encryption + MIC (default, explicit) |
| `SUPPORTED_NETWORKS` | 1 | RAM for the tables |
| `APPLICATION_ZDO_FLAGS` | 0x01 | supported ZDO requests (device announcements) reach the host; the NCP still answers them |
| `APS_UNICAST_MESSAGE_COUNT` | 32 | acknowledged unicasts in flight (default 10) |
| `BROADCAST_TABLE_SIZE` | 15 | spec value |
| `NEIGHBOR_TABLE_SIZE` | 16 | default |
| `MAX_END_DEVICE_CHILDREN` | 32 | direct children of the coordinator |
| `SOURCE_ROUTE_TABLE_SIZE` | 200 | the NCP keeps the source routes (high-RAM concentrator); the host keeps none |
| `ADDRESS_TABLE_SIZE` | 16 | EUI64 ↔ short address entries |
| `TRUST_CENTER_ADDRESS_CACHE_SIZE` | 4 | near-simultaneous joins (default 2) |
| `MULTICAST_TABLE_SIZE` | 16 | hub group memberships |
| `INDIRECT_TRANSMISSION_TIMEOUT` | 7680 ms | spec default for sleepy children |
| `END_DEVICE_POLL_TIMEOUT` | 8 (2⁸ min) | end devices that negotiate no timeout |
| `MAX_HOPS` | 30 | default |
| `TRANSIENT_KEY_TIMEOUT_S` | 300 s | longer than the longest permit-join window (254 s) |

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
app"). Control requests are barriers.
`RemoveDevice` only forgets the address: the engine's ZDO Mgmt_Leave_req
stays the primary removal path, and with hashed link keys the trust center
keeps no key for the device. An EZSP `removeDevice` to the parent, as an
explicit force removal of unreachable or sleepy children, is a follow-up.

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
sizes from the configuration values, group membership, energy/active scan,
channel change and TX power, diagnostics. No fragmentation, Green Power,
coexistence, backup or firmware update yet.

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

1. Start: this backend's tag makes the records `unverified` (loaded,
   read-only); another backend's tag makes them `foreign` (hidden). An
   untagged store is never taken silently: a ZRHP image's stored network
   gets the tag ZRHP's migration would write (`{"backend":"zrhp",…}`, so
   `foreign`); network-scoped records without a stored network stay
   `unverified` and hidden (closed, no device traffic) until the operator
   adopts or clears them (or forms or restores a network over them); an
   empty store is `untagged` until the first network.
2. The session's `Ownership(Observed(identity))` (before `Session::Up`):
   matching the tag (EPID; the key ID too when both are known, else the
   EPID alone, design §8.5), or no tag yet → `owned`, the tag written or
   its informative fields (PAN ID, channel, key sequence, coordinator)
   rewritten; a key ID already in the tag stays when the NCP does not
   export its key now. Another network → `foreign`, nothing written.
3. Device traffic (APS, groups, permit join, removal, the device
   inventory) is refused until step 2 matched in the current session; a
   session loss, `NoNetwork` or `Unknown` stop it again until the next
   match. The flag is shared with this backend (`EzspOptions::traffic`):
   the worker clears it at the end and at the start of every session and
   ends queued requests `Refused(LinkDown)` while it is clear, so nothing
   queued before a reset reaches a network not decided yet. The hub
   endpoint's group replay, the import of the NCP's devices and the state
   refresh wait for it.
4. `NoNetwork`: `network/info.ownership` `no_network`; `network/form`
   forms a network (only then, and only over `untagged` or `owned` records;
   no `confirm`), or `network/reset` ("RESET"). While the operator
   replaces the network (`network/form`, `network/reset`,
   `network/restore`), reports decide nothing; just before `formNetwork`
   the records are tagged with the extended PAN ID it forms (picked by the
   hub unless the request names one; key ID unknown), so an interrupted
   job (a timeout, a reboot) still leaves them the new network's;
   afterwards the tag gets the full identity.
5. `foreign`: `network/info.ownership` `mismatch` and
   `network/info.registry` `{"state":"foreign","backend","why"}`;
   `network/registry/adopt` (the records become the NCP network's) or
   `network/registry/clear` (deleted, the NCP's devices imported again),
   both with `"confirm": true`.

The operator's TX power is kept as `ezsp/tx_power` (backend-scoped) and
applied in every session (`EzspOptions::tx_power`). A device leave
(`DeviceLeft`) keeps the device: unavailable with `unavailable_reason:
"left"` until its next frame or rejoin; removal is `devices/remove`.

## Diagnostics

The EmberZNet counters (`readCounters`, or `readAndClearCounters` with
reset) by name, the NWK frame counter, and host counters (ASH frames,
retransmissions, XOFF, EZSP commands and callbacks, NCP overflows, sessions,
devices left, route records, GP frames ignored).

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

## Follow-ups

1. **Network backup and restore (OCB)**, in product scope: export with the
   security manager (`exportKey` for the network key and the hashed TC link
   key root → `BackupItem::HashedTclkSeed`, `exportLinkKeyByIndex`,
   `getNetworkKeyInfo` frame counter, child table), `RestoreEncoder` and
   restore (`setInitialSecurityState` with `NO_FRAME_COUNTER_RESET`,
   `importLinkKey`, NWK/APS frame counter values, `formNetwork`); key
   export on EZSP 13 goes through the same `exportKey` frame the identity
   uses now.
2. **Green Power via EZSP**, in product scope: GP endpoint 242,
   `gpepIncomingMessageHandler` → `CoordEvent::Gp` (the callback already
   reaches the worker's GP path), sink table (`gpSinkTable*`,
   `gpSinkCommission`), proxy table, `dGpSend` / `dGpSentHandler` for GP
   requests and remote provisioning (`Accepted.zcl_tsn` chosen by the
   host).
3. NCP firmware update: `launchStandaloneBootloader` + XMODEM GBL
   (`FwUpdateMethod::GeckoXmodem`).
4. Force removal: `removeDevice(parent short address, parent EUI64, target
   EUI64)` (APS Remove Device to the parent) for unreachable or sleepy
   children, as an explicit fallback behind the engine's Mgmt_Leave_req.
5. PR H2: the Dongle-M firmware on this backend (the UART transport on the
   ESP32, the hub app, the `files`-partition `hubdata`). Done in H1: the
   hub-app `ezsp` feature, the `NcpNv` ownership decision, the TX power
   kept across restarts.
6. HIL on the Dongle Max: session start on the stock "1.0.0" image, XON/XOFF
   on the 115 200 Bd line, key export, permit join with real devices,
   channel change, callback bursts and the capacities the NCP accepts.

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

Numeric facts only (IDs, values, field order); no code was taken from the
SDK or from GPL hosts.
