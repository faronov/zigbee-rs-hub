# Coordinator backend boundary (plan phase 4)

**Status: design, not implemented.** Implements step 2 of
`docs/coordinator-profiles.md` §5 against §3–§4. Baseline `main` 1feaac8;
open PRs #54, #55, #56 considered. Nothing here was built or measured.

Invariants for every step: ZRHP message IDs, payloads, `CallOpts`/timeouts,
request ordering and `hubdata` bytes are unchanged; resume, `NWK_INIT`
floors and replay (`hub-ncp/src/lifecycle.rs:455-660`) are untouched; each PR
compiles, passes CI and preserves behaviour unless it lists a user-visible
change.

## 1. Current coupling inventory

| File:line | Symbol / use | Category | Needs from the backend |
|---|---|---|---|
| hub-app `transport.rs:18-22, 49-57, 139-148` | `Client::start(APS_DATA_REQUEST)`, `Ticket`, `Confirm::aps(60 s)`; the TID rendezvous blocks on the driver thread (`driver.rs:450`) | transport | non-blocking submit |
| `transport.rs:81-135`, polled in the 1 s tick (`app.rs:1069`) | `try_wait` → `ApsCounters`, `ApsFailure{zb_status = confirm[3], legacy_codes}` | transport/diag | terminal outcome per request |
| `transport.rs:154-180` | GP `call_raw` (TSN = reply byte 1), `group_membership`, `remove_device`, `permit_join_on`: **blocking on the app thread**, refused unless `Ready` (73-78) | transport | ticketed control requests |
| `transport.rs:193-245` | `network/info` from `HubStatus`, `Ownership` strings, baud/flow, `stored_tx_power` | lifecycle/diag | status snapshot and backend facts |
| `convert.rs:6-13, 79-303` (all 17 uses) | ZRHP events ⇄ engine types; ZRHP encoders | transport | neutral events (moves into the adapter) |
| `app.rs:24-25, 310-325` | `Hub`, `Message`, `SessionState`; forwarder `subscribe()` → **unbounded** input | threading | event sink |
| `app.rs:552, 630, 675-680, 1087` | `with_store` KV: catalog config, automations | storage | `KvStore` |
| `app.rs:729`; `jobs.rs:26, 158` | raw `EVT_SCAN_PROGRESS` payload | admin | `ScanProgress` event |
| `app.rs:1033-1060` | `client().state()`; `peer_cookie` gates register, maintenance, `groups_resync`, refresh | lifecycle | session epoch |
| `app.rs:1100-1118` | session info, `store_state().endpoints`, `add_endpoint` (**blocking on the app thread**) | lifecycle | endpoints as backend config |
| `app.rs:1122-1158` | `ncp_devices()` (TBL_READ/DIAG calls, **blocking on the app thread**) | lifecycle | inventory request |
| `app.rs:1163-1169` | `permit_join_remaining()` | lifecycle | snapshot |
| `registry.rs:25, 76-117, 166-246`; `laststate.rs:19, 50-63, 95-144` | `with_store`; sweeps **delete** non-live `dev/`, `grp/`, `bnd/` (registry.rs:185-192, 213-221) | storage | `KvStore` |
| `admin.rs:18-23, 166-179` | ZRHP `Status` → API codes (`failed` is the default) | errors | normalized errors |
| `admin.rs:513-549, 600-607, 682-689, 774-787` | `backup_export`, session fw/chip/build_id; KV `grp/`, `bkp/last`, `cfg/tz`, `cfg/ntp` | backup/storage | export, `KvStore` |
| `admin.rs:570-581` | `ncp_has(caps::…)` → `hub/info.maintenance` | capabilities | `Capabilities` |
| `admin.rs:644-672` | journal stats, resumes, restarts, slots | diag | status and store stats |
| `admin.rs:907-1238` | energy/active scan (`ScanResult` 969), channel, TX power, backup, restore, `leave_reset` + `form`, all on job threads | admin | blocking admin API |
| `admin.rs:1297-1365` | DIAG counters, SYS_GET_STATUS flags, frame counters, link counters, PTA | diag | neutral diagnostics |
| `topology.rs:15-19, 147-201, 301-352` | `send_aps` + private `subscribe()`; DIAG neighbours/children/routes | topology | ZDO exchange, tables |
| `ocb.rs:18-23, 113-277, 325-466` | OCB ⇄ ZRHP `Record` streams | backup | backup sink / restore encoder |
| `catalog/store.rs:31`, `catalog/local.rs:27` | `zigbee_zrhp::crc::crc32c` | storage | own CRC |
| `tests.rs`, `tests_groups.rs`, `catalog/tests_e2e.rs` | `SimNcp`, `Store`, ZRHP ids | tests | adapter via dev-dependency |
| hub-devices `transport.rs:123-140, 161-164` | `Status(u8)` shown as "ZRHP status"; the default `group_remove` returns 0x05 = ZRHP **InvalidState**, not UnsupportedCommand (0x04) | errors | `CoordError` |
| hub-devices `hub.rs:665-672, 2091-2120` | refusal → `busy`; failures matched by `p.req == f.req` (ambiguous for identical requests in flight) | transport | `RequestId` |
| hub-devices `configure.rs:592`, `zgroups.rs:1298-1306`, `gp/mod.rs:665-838`, `gp/bindings.rs:592, 970`, `api/mod.rs:503-505`, `hub.rs:2574` | synchronous results (group result, GP TSN key, permit join) | transport | async completion |
| hub-ncp `netops.rs:38, 204-210` | backend writes `cfg/tx_power` in the app's KV namespace | storage | backend-scoped keys |
| hub-ncp `store/journal.rs:26, 98` | one-shot and streaming CRC from zigbee-zrhp | storage | hub-store |
| hub-core `Cargo.toml:12`, `lib.rs:13` | unconditional `zigbee-zrhp`; the re-export has no workspace users | build | feature gate |
| firmware `main.rs:40-44, 338-374, 382, 445, 404-450` | `Store`, `Hub::start`, `set_coex` from Wi-Fi callbacks, `App::start_with(hub)` | wiring | backend construction |
| PR #55 `ota.rs` | `with_store` ×4 (`ota/cfg`, `ota/d/`), `capacity::MAX_ASDU` in the tick; hub-ota → hub-ncp only for `store::Flash` | storage/caps | `KvStore`, `max_asdu`, hub-store |

## 2. Proposed boundary

### 2.1 Crates

```text
hub-store   Flash, Journal, Store (moved unchanged), crc32c (one-shot + streaming), KvStore    no zigbee-zrhp
hub-coord   boundary types + traits; ApsRequest/ApsIndication/GP types moved
            from hub-devices (re-exported at old paths); `testkit` feature
hub-devices → hub-coord
hub-ncp     → hub-store, hub-coord, zigbee-zrhp; new `coord` module = the ZRHP adapter
hub-app     → hub-store, hub-coord, hub-devices; feature `zrhp` → hub-ncp
hub-core    `zigbee-zrhp` behind feature `zrhp`
firmware    features zrhp | ezsp | znp, exactly one
```

The adapter lives in hub-ncp. ZRHP knowledge stays in one crate, and
hub-app tests use it through the existing dev-dependency
`hub-ncp = {features = ["sim"]}` even under `--no-default-features`.

### 2.2 Requests, outcomes, events

```rust
pub struct RequestId(pub u32);              // allocated by the adapter, no rendezvous
pub enum Request {
    Aps(ApsRequest), Gp(GpRequest),
    Group { endpoint: u8, group: u16, add: bool },
    RemoveDevice { ieee: u64, nwk: Option<u16> },
    PermitJoin { duration_s: u8, target: Option<u16> },
    DeviceInventory,
}
pub enum Progress { Accepted { zcl_tsn: Option<u8> } }   // optional, at most once
pub enum Terminal {                       // exactly one per RequestId
    Delivered,                // acknowledged unicast confirmed
    Sent,                     // accepted, no positive delivery (broadcast, group, no ACK)
    NotDelivered(CoordError), // confirm failure  → engine retries or fails now
    Refused(CoordError),      // the NCP refused  → engine retries or fails now
    Unknown(CoordError),      // timeout, reset, local error → engine keeps its response timeout
    Cancelled,                // cancelled before it reached the NCP
    Done(Reply),              // control requests: Reply::{None, Inventory(Box<_>)}
}
pub enum CoordEvent {                     // large variants boxed: bounded slots are preallocated
    Session(Box<SessionEvent>),           // Up{epoch, info, caps: Arc<Capabilities>} | Down | Incompatible
    Ownership(Box<NetworkOwnership>), NetworkState(NetState), PermitJoin { remaining_s: u32 },
    Indication(ApsIndication), DeviceJoined { ieee: u64, nwk: u16 }, Gp(Box<GpEvent>),
    Progress { id: RequestId, p: Progress }, Done { id: RequestId, t: Terminal },
    ScanProgress { scan: u8, done: u8, total: u8 }, Dropped { events: u32 },
}
pub trait Coordinator: Send {
    /// Never waits on I/O or a worker; refusals are immediate.
    fn submit(&mut self, req: Request, deadline: Option<Duration>) -> Result<RequestId, CoordError>;
    fn cancel(&mut self, id: RequestId);
    fn status(&self) -> CoordStatus;      // snapshot behind a short lock, no I/O
}
```

**Contract** (tested by `hub-coord::testkit`):
- A terminal never precedes an indication that the NCP sent before it.
- `Progress` precedes `Terminal`.
- `Session::Down` ends every in-flight request with `Unknown(DeliveryUnknown)`.
- Control requests are **barriers**: nothing submitted later reaches the NCP
  before they complete. This is today's order, which the blocking app thread
  produces.

`deadline: None` means the backend default; for ZRHP these are exactly
today's `CallOpts`.

**Endpoints** are backend configuration given at construction
(`BackendConfig { endpoints }`), not a request:
- EZSP accepts `addEndpoint` only before `networkInit`.
- ZNP needs `AF_REGISTER` after every reset.
- ZRHP keeps today's rule: add the endpoint if the store lacks it, then
  replay it on every resume.

**Coexistence** is a firmware-owned, thread-safe `RadioControl::set_coex`.
It is called from Wi-Fi callbacks (`main.rs:382, 445`, #54), not from the
app thread.

### 2.3 Errors

```rust
pub enum CoordError {
    Unsupported, Busy, LinkDown, NoAck(Code), Timeout, DeliveryUnknown,
    Backend { code: Code, kind: Kind },
}
pub struct Code { pub backend: BackendKind, pub value: u16, pub text: Option<&'static str> }
pub enum Kind { NoRoute, TableFull, TooLarge, Security, InvalidParameter, InvalidState,
                Denied, NotFound, Other }
```

API codes stay **per context** and are pinned by JSON goldens:
- **Engine:** refusals stay `busy` and delivery failures `not_delivered`
  (`hub.rs:665-672, 2860-2869`).
- **Admin:** keeps the `admin.rs:166-179` table, including `failed`.

`zb_status_text` and the ≤0.1.1 legacy numbering move into the ZRHP adapter
as `Code.text`, so messages stay byte-identical. The only visible text
change is the old "ZRHP status 0x.." display. The default `group_remove`
becomes `Unsupported`.

### 2.4 Capabilities

One `Capabilities` value per session. ZRHP renegotiates after every NCP
restart or update, so values can change.

```rust
pub struct Capabilities {
    pub backend: BackendKind, pub ownership: OwnershipModel,
    pub max_in_flight: u16, pub max_asdu: u16, pub fragmentation: bool,
    pub group_membership: bool,
    pub tables: Tables,          // children, address_map, source_routes, multicast,
                                 // neighbors, routes, bindings, link_keys: Option<u16>
    pub gp: GpSupport,           // sink, proxy, sink_table
    pub coex: bool, pub radio: RadioOps,      // energy_scan, active_scan, channel, tx_power
    pub backup: BackupSupport,                // export, export_keys, restore
    pub fw_update: Option<FwUpdateMethod>,    // ZrhpAb | GeckoXmodem | TiRomBsl
    pub diagnostics: bool,
}
```

### 2.5 Network identity and ownership

```rust
pub enum OwnershipModel { HostStore /* ZRHP */, NcpNv /* EZSP, ZNP */ }
pub struct NetworkIdentity {
    pub ext_pan_id: [u8; 8],
    pub key_id: Option<[u8; 8]>,  // SHA-256("zigbee-rs-hub/key-id/1" ‖ key)[..8]
    pub pan_id: u16, pub channel: u8, pub key_seq: u8, pub coordinator_ieee: [u8; 8], // informative
}
pub enum NetworkOwnership { Unknown, NoNetwork, Owned(NetworkIdentity),
    Lost(Option<NetworkIdentity>), Mismatch { found: Option<NetworkIdentity>, why: String },
    Observed(NetworkIdentity) }     // as_str(): today's network/info strings
```

**Matching:** EPID must be equal, and key_id must be equal when both are
known; otherwise EPID alone decides. PAN ID (conflict resolution), channel
and key_seq are informative and are rewritten when they change.

**Authority:**
- `HostStore`: the backend decides (the ZRHP lifecycle, unchanged).
- `NcpNv`: hub-app compares the NCP's identity with the hubdata tag (§2.7).

`docs/coordinator-profiles.md:111` must drop channel and PAN ID from the
identity list.

### 2.6 Administration (job threads only; defaults `Err(Unsupported)`)

```rust
pub trait NetworkAdmin: Send + Sync {        // debug_assert!: never on the app thread
    fn form(&self, p: FormParams) -> Result<NetworkParams, CoordError>;
    fn leave_reset(&self) -> Result<(), CoordError>;
    fn energy_scan(&self, mask: u32, exp: u8) -> Result<Vec<EnergyChannel>, CoordError>;
    fn active_scan(&self, mask: u32, exp: u8) -> Result<Vec<Beacon>, CoordError>;
    fn change_channel(&self, ch: u8) -> Result<ChannelChange, CoordError>;
    fn set_tx_power(&self, dbm: i8) -> Result<i8, CoordError>;
    fn tx_power_setting(&self) -> Option<i8>;
    fn coordinator_tables(&self, routes: bool) -> TablesRead;          // per-table Result
    fn zdo_request(&self, nwk: u16, cluster: u16, args: &[u8], t: Duration) -> Result<Vec<u8>, CoordError>;
    fn diagnostics(&self, groups: u8, reset: bool) -> Diagnostics;    // neutral struct; hub-app renders the JSON
    fn backup_export(&self, keys: bool, sink: &mut dyn BackupSink) -> Result<(), CoordError>;
    fn restore_encoder(&self) -> Result<Box<dyn RestoreEncoder>, CoordError>;
    fn backup_restore(&self, blob: RestoreBlob, o: RestoreOptions) -> Result<RestoreReport, CoordError>;
    fn firmware(&self) -> Option<&dyn NcpFirmware>;  // slots, preflight(manifest), update, confirm, pending phase
}
```

The OCB JSON stays in hub-app's `ocb.rs`. ZRHP's `Record` ⇄
`BackupSink`/`RestoreEncoder` conversion moves into the adapter. The
encoder appends the same TLVs into the same `Vec<u8>`, so RAM use and
restore bytes are identical.

### 2.7 Hub-owned storage and hubdata tagging

```rust
pub trait Kv {
    fn get(&self, k: &str) -> Option<&[u8]>;
    fn prefix<'a>(&'a self, p: &'a str) -> Box<dyn Iterator<Item = (&'a str, &'a [u8])> + 'a>;
    fn put(&mut self, k: &str, v: &[u8]) -> io::Result<()>;
    fn delete(&mut self, k: &str) -> io::Result<()>;
}
pub trait KvStore: Send + Sync {   // one lock per call, zero-copy, as with_store today
    fn with(&self, f: &mut dyn FnMut(&mut dyn Kv));
    fn stats(&self) -> StoreStats;
}
```

`impl Kv for Store`; `impl KvStore for Mutex<Store>` (EZSP/ZNP);
`impl KvStore for hub_ncp::Hub` via `with_store` (ZRHP: same journal, same
lock, same status update). One journal stays shared, so persistence is
unchanged.

**Tag.** New KV key `hub/owner` (unused prefix):
`{"v":1,"backend":"zrhp","epid":…,"key_id":…|null,"pan_id":…,"channel":…,"key_seq":…,"coordinator":…}`.

Keys are classified by exact key:

| Class | Keys |
|---|---|
| network-scoped | `dev/`, `st/`, `grp/`, `bnd/`, `auto/` (rules name device IDs), `ota/d/`, `bkp/last` |
| hub-scoped | `udef/`, `usrc/`, `cfg/tz`, `cfg/ntp`, `cfg/catalog`, `ota/cfg` |
| backend-scoped | `cfg/tx_power`, kept as is; future keys go under `zrhp/`, `ezsp/`, `znp/` |

**Foreign gate.** It is enforced in the KV layer by a prefix guard that makes
network-scoped keys read-only. Simply skipping the load would let the next
registry sweep delete every `dev/` key (registry.rs:213-221).
- `tag.backend` differs from the image's backend: network-scoped records
  are neither loaded nor writable. `network/info.registry = {"state":"foreign",…}`.
- `HostStore`: identity is checked at start against the ZRHP store's
  `NetworkRecord`, before `registry.load` (`app.rs:621`).
- `NcpNv`: the registry loads read-only ("unverified"), and `CoordTransport`
  refuses device traffic until `Session::Up` reports a matching identity.
- Operator actions `network/registry/adopt` and `network/registry/clear`
  (both require `confirm`) rewrite the tag or purge network-scoped keys and
  engine state. `network/reset` and `network/restore` are already explicit
  and rewrite the tag.

**Tag upkeep.** For ZRHP the adapter mirrors the tag from the stored
`NetworkRecord` on every `Owned`: after auto-adopt (`lifecycle.rs:535`),
form, restore, and channel or key changes (`lifecycle.rs:562`).

**Migration** runs on the first boot of the tagging firmware, in the ZRHP
adapter:
- A `NetworkRecord` exists: derive the tag from it. key_id is `None` when
  the stored key is all-zero (adopted without KEY_EXPORT, `lifecycle.rs:542`).
- No `NetworkRecord` but network-scoped keys exist: write `{zrhp, identity null}`
  and fill it at the first `Owned`.
- Store empty: no tag until a network is formed or adopted.

Only one KV record is written. Older ZRHP firmware ignores `hub/owner` and
compaction preserves it, so a ZRHP→older-ZRHP downgrade is safe. Older
firmware would **not** honour an EZSP tag (documented limit).

## 3. Threading model

**Today.** The app thread blocks for up to 10 s (default `CallOpts`) at
`transport.rs:157, 163, 168, 174, 179` and `app.rs:1108, 1130`. It also
waits on the driver TID rendezvous for every APS send, and on the store
mutex while the lifecycle thread commits to flash (`lifecycle.rs:393-407`).
The input channel (`app.rs:310`), the subscriber fan-out
(`lifecycle.rs:368, 439`) and the driver's `pending` map are unbounded.

**Proposed.**

```text
API/WS/MQTT ─► Input (unchanged) ─► app thread ─ submit() ─► adapter command path
backend worker ─► bounded event queue (64 slots, boxed variants)
               └─► doorbell Input::Coord (AtomicBool; at most one outstanding)
admin job threads ─► NetworkAdmin (blocking allowed)
```

1. The app thread never waits on I/O or on a worker:
   - `submit` is non-blocking;
   - `status()` is a snapshot;
   - `KvStore` keeps today's short local-flash lock.
2. On `Input::Coord` the app clears the flag, drains up to 64 events, and
   re-rings itself if events remain.
3. Workers may block: on serial I/O, on the NCP's command/response
   discipline, and on a full event queue. The app never waits for a worker,
   so there is no deadlock cycle.
4. A full event queue stops the worker from reading its link. Backpressure
   then reaches the NCP through ZRHP credits or UART flow control. ZRHP's
   lifecycle fan-out stays unbounded in phase 4 and gets a high-water
   counter in diagnostics.
5. **Command path bound.** Today's ZRHP `aps_send` never refuses.
   `refresh_unknown` sends one read per entity of every restored device
   (`commands.rs:905-916`), so a small bound would lose reads. The bound is
   an explicit cap (proposal: 2048 queued requests, plus a high-water
   counter); exceeding it returns `Busy`.
6. **In-flight capacity.** Each backend enforces `max_in_flight` for its
   own NCP.
   - ZRHP keeps today's enforcement (driver credits plus NCP `BUSY`), which
     keeps the wire unchanged.
   - EZSP and ZNP hold excess requests in the worker FIFO.

## 4. ZRHP mapping (`hub_ncp::coord`)

- **`zrhp-pump` thread** (about 6 KB stack). It replaces the 4 KB forwarder
  (`app.rs:313-325`) and is fed through one unbounded channel: commands from
  the app, plus subscription messages from a new callback subscriber
  `Hub::subscribe_into` (no extra thread).
  - **Commands:** `Client::start` with the moved `convert.rs` encoders
    (verbatim, same `Confirm::aps(60 s)`).
  - **Control requests** call today's blocking `Hub` methods **inline**:
    `call_raw` for GP, `group_membership`, `remove_device`,
    `permit_join_on`, the `add_endpoint` rule, `ncp_devices`. Later commands
    wait in a hold buffer, and events wait in the channel. This is exactly
    what the blocked app thread does today, minus the stalled API.
  - **Ordering:** the driver completes a ticket in `match_confirm` before it
    emits the event (`driver.rs:1156-1157`), but the event reaches the pump
    only after lifecycle processing. So APS terminals are emitted at the
    position of their `EVT_APS_DATA_CONFIRM` in the subscription stream,
    correlated by `req_tid`. Reply-only tickets are drained before each later
    message; the driver completes replies before it reads later frames.
    Deadlines are checked on `recv_timeout(100 ms)`.
- **Readiness checks unchanged.** Control requests return `LinkDown` at
  once unless `Ready` (`transport.rs:73-78`); APS is refused only when
  `Stopped` or `Incompatible` (140-142).
- **Terminal mapping and counters.** `CoordTransport` (hub-app, neutral)
  counts per a pinned table that covers every branch of `transport.rs:89-133`:

  | Today's branch | Terminal | Counters |
  |---|---|---|
  | confirm status 0 | `Delivered` | accepted, delivered |
  | confirm status ≠ 0 | `NotDelivered` (with `zb_status`) | accepted, not_delivered |
  | Success, no confirm body | `Sent` | accepted |
  | NCP Busy reply | `Refused(Busy)` | busy |
  | other NCP status | `Refused(Backend)` | failed |
  | driver timeout | `Unknown(Timeout)` | accepted, unconfirmed |
  | reset, reject, codec, closed, driver-local `TooLarge` (`driver.rs:1374`) | `Unknown` | failed, not reported to the engine |

  ZRHP emits no APS `Accepted`: the driver holds the reply until the
  confirm (`driver.rs:1183-1188`). GP remote requests emit
  `Accepted{zcl_tsn}`.
- **Engine timing.** In phase 4 the engine receives APS terminals at its
  1 s tick (buffered), as `poll` at `app.rs:1069` does today.
- **Status and epoch.** `status()` is the same `hub.status()` +
  `client().session()` calls. The epoch is `peer_cookie`, so the app's tick
  gates and `observe_permit` are unchanged.
- **Capabilities** come from `Negotiated`:
  - capacities `MAX_APS_INFLIGHT`, `MAX_ASDU` (absent → 82, as `DEFAULT_ASDU`
    in #55), `MAX_CHILDREN`, `ADDRESS_MAP_CACHE`, `SOURCE_ROUTE_CACHE`,
    `GROUP_TABLE`, `BINDING_TABLE`, `NEIGHBOR_TABLE`, `ROUTE_TABLE`,
    `LINK_KEY_CACHE`, `GP_SINK_TABLE`;
  - caps `GROUP_MEMBERSHIP`, `GREEN_POWER_SINK/PROXY`, `PTA_COEX`,
    `ENERGY_SCAN`, `TX_POWER`, `BACKUP_RESTORE`, `KEY_EXPORT`,
    `NCP_FW_UPDATE`, `APS_FRAGMENTATION`;
  - `HostStore`, `ZrhpAb`.
- **Proof of no wire or persistence change:**
  1. encoders and decoders are moved, not rewritten (review the `git mv` diff);
  2. the same `Hub`/`Client` calls with the same `CallOpts`;
  3. barriers reproduce today's serialization;
  4. a golden sim trace (new `SimState.trace` of every request's msg_id and
     payload; deterministic test RNG) is compared as a multiset, per
     destination order, and barrier positions;
  5. a golden byte image of the journal;
  6. golden JSON for `network/info`, `hub/system`, diagnostics, OCB export,
     and restore stream bytes.
- **Sim and tests.** `hub_ncp::sim` gains only the trace field. hub-app,
  hub-mqtt and hubctl tests build the backend through `hub_ncp::coord` on
  the existing `rig()`.

## 5. EZSP and ZNP mapping (validates the abstraction)

The names are assumed: they come from UG100 (EZSP) and UG101 (ASH), and
TI's ZNP/MT interface specification. Pin them per firmware version.

| Boundary | EZSP/ASH (EmberZNet) | ZNP/MT (Z-Stack) |
|---|---|---|
| APS submit / `Accepted` | `sendUnicast`/`sendMulticast`/`sendBroadcast`, `EmberApsFrame`, `messageTag` ↔ RequestId | `AF_DATA_REQUEST(_EXT)`, `TransID` ↔ RequestId; SRSP |
| Terminal | `messageSentHandler(tag, status)`: success = delivered only for ACKed unicast, else `Sent` | `AF_DATA_CONFIRM(endpoint, TransID, status)`; none if the SRSP failed |
| Busy / in-flight | `EMBER_MAX_MESSAGE_LIMIT_REACHED`; `EZSP_CONFIG_APS_UNICAST_MESSAGE_COUNT`, broadcast table separately | no capacity report: fixed per pinned firmware |
| Max ASDU | `maximumPayloadLength` | AF limit; `_EXT` for larger |
| Indications, joins | `incomingMessageHandler`, `trustCenterJoinHandler`, `childJoinHandler` | `AF_INCOMING_MSG`, `ZDO_TC_DEV_IND`, `ZDO_END_DEVICE_ANNCE_IND` |
| Session epoch | ASH RSTACK / `stackStatusHandler` | `SYS_RESET_IND`, `ZDO_STATE_CHANGE_IND` |
| Endpoints | `addEndpoint` before `networkInit` | `AF_REGISTER` after reset (duplicate = success) |
| Groups | `setMulticastTableEntry` (RAM: replay after reset) | `ZDO_EXT_ADD_GROUP` (NV: duplicates on resync) |
| Ownership | `NcpNv` (NVM3 tokens; `networkInit`) | `NcpNv` (`ZCD_NV_*`) |
| Backup | `getNetworkParameters`, version-dependent key export, key/child tables | NV reads of NIB, keys, TCLK table |
| Firmware update | `launchStandaloneBootloader` + XMODEM GBL | TI ROM BSL (reset + backdoor lines) |

**Conclusions:**
- Both protocols are serialized command/response with asynchronous
  callbacks, so one worker per backend fits the ticket model. The tag or
  `TransID` map lives in the worker; status codes and tag width are
  version-specific.
- NV authority means the hub tag decides ownership.
- On EZSP and ZNP, GP remote provisioning is ZCL built by the host, so
  `Accepted.zcl_tsn` may be chosen by the host.
- Barrier ordering is part of the contract for every backend.

## 6. Cargo features and size

```toml
# hub-app
hub-ncp = { workspace = true, optional = true }   # zigbee-zrhp dependency removed
[features]
default = ["zrhp"]        # plus "ota-store" (implies "ota") from #55
zrhp = ["dep:hub-ncp"]
```

- **hub-app API.** `App::start_backend(Backend{coordinator, admin, kv, radio}, cfg, catalog)`
  is new. `App::start_with(ncp, …)` stays under `zrhp` as a wrapper, so
  firmware (`main.rs:435`) and hubctl (`main.rs:577`) need no change until
  PR H.
- **Workspace consumers.** #55 makes the workspace dependency
  `hub-app = {default-features = false}`. hub-mqtt's normal dependency must
  **not** enable `zrhp`; only its dev-dependency does.
- **hub-core** re-exports zigbee-zrhp behind `zrhp`.
- **Firmware** gates hub-ncp behind `zrhp` (default) and uses
  `compile_error!` unless exactly one backend is selected; `--all-features`
  is unsupported for firmware.
- **CI additions:**
  - `cargo test -p hub-app --no-default-features --features zrhp`
    (the 4 MB image combination);
  - `cargo build -p hub-app --no-default-features`;
  - `cargo tree -e normal` on hub-app, hub-mqtt and hub-ota with no backend:
    no `hub-ncp` or `zigbee-zrhp`.
- **Size estimates (assumed; measure with `tools/build-hub-firmware.sh --layout 4mb --locked`
  before and after):**
  - ZRHP image: +2–6 KB flash (adapter glue, outcome events, dynamic
    dispatch) and +2–4 KB internal RAM (the pump stack replaces the
    forwarder). Proposed gate: at most 8 KB flash growth (headroom today is
    259 KB).
  - EZSP/ZNP images: lose hub-ncp and zigbee-zrhp (~109 KB per
    `coordinator-profiles.md:233-236`) minus the hub-store journal they
    keep (a few KB).

## 7. Migration plan (small PRs, in order)

Merge order: **#54 → #56 → #55**, then this plan.
- #54 and #56 change firmware `net::start` closures and must be reconciled
  with each other. #56 also touches hub-core and hub-web.
- #55 changes traffic and `hub-app/app.rs`, so the baselines are recorded
  after it.

| PR | Change | Acceptance |
|---|---|---|
| **A** Baselines (test-only) | `SimState.trace`; scripted sim scenario (resume, endpoint, permit join, join, interview, group add/remove, GP commissioning and remote pairing, remove, topology, backup, reset/form, restore); golden journal bytes; catalog area bytes; API JSON goldens | green on `main` + #55 |
| **B** hub-store | `git mv hub-ncp/src/store`; own crc32c, one-shot and streaming (RFC 3720 vectors, cross-checked against zigbee-zrhp in dev-tests); `hub_ncp::store` re-export; catalog CRC switched | journal and catalog goldens byte-identical; no zigbee-zrhp in `cargo tree -p hub-store` |
| **C** hub-coord | types and traits; ApsRequest/GP types moved with re-exports | all dependents compile unchanged |
| **F** Ticketed engine transport | hub-devices: `RequestId` returns, `on_outcome`, `CoordError`; GP waits keyed by RequestId then TSN; group/configure/permit results deferred through the existing deferred-result path (`api/mod.rs:1115-1124`, same JSON); `MockTransport` manual and auto completion; `NcpTransport` still blocking inside | hub-devices and hub-mqtt tests (adapted only where they asserted synchronous effects, listed); a test where the event precedes `Accepted`; wire golden identical |
| **D** KvStore | registry, laststate, automations, catalog config, admin clock/backup-last, #55 `ota.rs` | golden journal identical; recorded KV call sequence identical |
| **E** `ZrhpAdmin` | admin, topology and ocb use `NetworkAdmin`; record codec moved | OCB and restore bytes identical; diagnostics/topology JSON identical; `Status`→API table test |
| **G** ZRHP Coordinator | pump, `CoordTransport`, `RadioControl`, endpoint config | wire golden identical; **sim `stall` of GP_SINK_CONFIG: the app still answers API requests** (today it blocks for 10 s); counter-table test |
| **H** Features | hub-app `zrhp`, `start_backend`; hub-core gate; hub-ota → hub-store; OTA `max_asdu` from caps; firmware, hubctl, hub-mqtt wiring | `--no-default-features` builds; cargo-tree check; 4 MB size gate |
| **I** Tagging | `hub/owner`, migration, KV guard, adopt/clear | untagged golden journal → only `hub/owner` added; foreign start + shutdown leaves KV bytes identical; identity matrix (PAN change, key rotation, key unknown) |
| **J** Surfacing | caps in `hub/info` and `network/info.capabilities` (additive); additive delivery outcome in command results; testkit conformance (Mock + ZRHP sim); docs | goldens updated only additively |

C and B can land in parallel with #55. F conflicts with #55's `hub.rs` and
`api/mod.rs` changes, so it goes after #55. D precedes E because both edit
`admin.rs` `with_store` calls.

## 8. Risks and decisions needed

1. **Error taxonomy extension:** adopt `Backend{code, kind}` with a `kind`
   hint so the existing API codes (`invalid_value`, `unauthorized`,
   `not_found`, `failed`) survive?
2. **Command cap:** use 2048 queued requests, or pace `refresh_unknown` in
   the engine (a behaviour change)?
3. **Barriers:** keep strict barriers (today's order, more latency) for all
   backends, or relax them later per request type?
4. **Delivery failures:** keep them at the 1 s tick, or switch to immediate
   handling later (changes retry timing)?
5. **Identity rule:** EPID plus key_id when both are known. Accept EPID-only
   when the key is not exportable (ZRHP without KEY_EXPORT, some EZSP
   builds)?
6. **Foreign registry:** confirm the API names (`network/registry/adopt`,
   `network/registry/clear`) and that `auto/` is network-scoped.
7. **Backend KV keys:** keep `cfg/tx_power` in place (moving it is a
   persistence change)?
8. **ZRHP host records in EZSP/ZNP images:** hub-store keeps the ZRHP
   host-record codec in those images (a few KB, enables foreign detection),
   or gate it behind a feature?
9. **RAM:** approve the internal-RAM budget for the pump stack and the
   preallocated event slots; measure the high-water mark on hardware.
10. **EZSP/ZNP facts:** all capacities and commands in §5 stay *assumed*
    until the supported firmware versions are pinned.
