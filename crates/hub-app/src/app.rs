//! The hub application: one thread owns the device engine, the API sessions
//! and the registry; inputs arrive over a channel. It never waits for the
//! NCP: requests go to the coordinator backend, whose events come back
//! through a bounded queue and a doorbell input.
//!
//! ```text
//!  coordinator events ─┐ (EventQueue, Input::Coord)
//!  API connections ────┼─> app thread: engine (hub-devices) ─> CoordTransport ─> Coordinator (zrhp-pump, ezsp-worker)
//!  1 s tick ───────────┘        │  ApiServer (sessions, subscriptions)
//!                               └─ Registry ─> Records (host store in hubdata)
//! ```
//!
//! The backend comes as a [`Backend`] ([`App::start_backend`]);
//! [`App::start`] and [`App::start_with`] (feature `zrhp`) start the app on
//! a running ZRHP host core.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hub_coord::{
    BackendConfig, CoordEvent, EVENT_SLOTS, EndpointConfig, EventQueue, NcpInfo, NetChange, NetworkAdmin, RadioControl,
    Reply, Request, RequestId, SessionEvent, SessionState as CoordSession, Terminal,
};
use hub_devices::api::{ApiExtension, ApiServer, Frame, SessionId};
use hub_devices::bankcat::StoreCatalog;
use hub_devices::device::Millis;
use hub_devices::hub::{Hub as Engine, HubConfig};
use hub_devices::resolve::{Catalog, MemoryCatalog};
use hub_devices::transport::Outcome;
use hub_devices::zdef_adapter::ZdefSchemaDecoder;
use hub_store::{EndpointRecord, KvStore, kv};
use log::{info, warn};

use crate::admin::{self, Admin, JobOut};
use crate::backend::{Backend, Records};
use crate::automations::{self, Automations, Clock, World};
use crate::catalog::data::SharedFlash;
use crate::catalog::service::{CONFIG_KEY, CatalogService, load_config};
use crate::catalog::store::CatalogStore;
use crate::catalog::web::SharedWeb;
use crate::platform::Platform;
use crate::registry::Registry;
use crate::transport::{ApsCounters, CoordTransport};

#[path = "ownership.rs"]
mod ownership;

/// Where the API frames of one connection go. `false` = the connection is
/// gone (the session is dropped).
pub trait SessionSink: Send {
    /// Send one text frame.
    fn send(&mut self, text: &str) -> bool;

    /// Send one frame the caller no longer needs (large results): a sink
    /// that queues frames takes the buffer instead of copying it.
    fn send_owned(&mut self, text: String) -> bool {
        self.send(&text)
    }

    /// Whether the connection still exists. Checked every second, so a
    /// connection that went away silently (no close notification, no frame
    /// to fail on) does not keep its session.
    fn alive(&self) -> bool {
        true
    }

    /// Probe the connection (a WebSocket ping), every
    /// [`KEEPALIVE_S`] seconds; `false` ends the session. A peer that
    /// half-closed its socket is only found by sending to it.
    fn keepalive(&mut self) -> bool {
        true
    }

    /// Sinks that send one message as several WebSocket fragments: a
    /// subscription snapshot then goes piece by piece, each made once the
    /// sink's backlog allows it. `None`: whole messages only.
    fn pieces(&self) -> Option<Pieces> {
        None
    }

    /// Bytes queued and not sent yet (sinks with [`pieces`](Self::pieces)).
    fn backlog(&self) -> usize {
        0
    }

    /// One piece of a message: `first` opens it, `last` ends it.
    fn send_piece(&mut self, text: String, first: bool, last: bool) -> bool {
        let _ = (text, first, last);
        false
    }

    /// The app dropped the session (its client fell behind): close the
    /// connection too, so the client notices and connects again.
    fn close(&mut self) {}
}

/// How a sink takes a message in pieces ([`SessionSink::pieces`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pieces {
    /// Size of one piece.
    pub size: usize,
    /// The next piece is made while the sink's backlog is at most this.
    pub backlog: usize,
    /// The sink's byte budget: frames held until the snapshot is complete
    /// (they cannot go between its pieces) stay below it, else the session
    /// is dropped and its client subscribes again; a held frame goes once
    /// it fits next to the sink's backlog.
    pub held: usize,
}

/// Frames of a session behind a snapshot that goes piece by piece.
#[derive(Default)]
struct Outbox {
    items: std::collections::VecDeque<Queued>,
    held: usize,
    /// The front snapshot's first piece has been sent.
    started: bool,
}

enum Queued {
    Frame(String),
    Snapshot(hub_devices::api::SnapshotStream),
}

/// Requests served by a service outside the engine (the MQTT bridge's
/// `mqtt/*` settings), on the app thread. Must not block.
pub trait RequestHook: Send {
    /// Request types this hook serves start with this (e.g. `mqtt/`).
    fn prefix(&self) -> &'static str;
    /// A `hub/info` `maintenance` feature this hook adds.
    fn feature(&self) -> Option<&'static str> {
        None
    }
    /// Handles request `ty`; `req` is the whole frame.
    fn request(&mut self, ty: &str, req: &serde_json::Value) -> Result<serde_json::Value, hub_devices::hub::ApiError>;
}

/// Request prefixes of optional services and why a build without their
/// hook lacks them: such requests answer `unsupported`, not
/// `invalid_format`.
const OPTIONAL_SERVICES: &[(&str, &str)] = &[
    ("mqtt/", "the MQTT bridge is not enabled in this firmware build"),
    #[cfg(not(feature = "ota"))]
    ("ota/", "Zigbee device OTA is not in this firmware build (8 MB images only)"),
];

/// The `unsupported` result of a request for an optional service this
/// build does not have.
fn optional_missing(text: &str) -> Option<String> {
    let (prefix, why) = OPTIONAL_SERVICES.iter().find(|(p, _)| text.contains(p))?;
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    v["type"].as_str().filter(|t| t.starts_with(prefix))?;
    let id = v["id"].as_u64()?;
    Some(hub_devices::api::ServerMessage::err(id, hub_devices::hub::ApiError::new("unsupported", *why)).to_text())
}

/// Period of [`SessionSink::keepalive`].
pub const KEEPALIVE_S: u64 = 30;

impl SessionSink for Sender<String> {
    fn send(&mut self, text: &str) -> bool {
        Sender::send(self, text.to_string()).is_ok()
    }
}

/// Application settings.
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// Hub name in `auth_required`/`auth_ok`.
    pub hub_name: String,
    /// API token.
    pub api_token: String,
    /// Local time offset for Tuya time sync, s.
    pub tz_offset_s: i64,
    /// The hub's endpoint, registered with the NCP once and replayed on
    /// every resume.
    pub endpoint: EndpointRecord,
    /// Load the built-in example catalog.
    pub builtin_catalog: bool,
    /// Stack of the app thread (the engine's handlers are deep: an
    /// unoptimised build needs 64–96 KiB).
    pub stack_size: usize,
    /// Import devices the NCP knows but the registry does not (joined
    /// before the hub adopted the network) in every new NCP session, and
    /// interview them.
    pub import_ncp_devices: bool,
    /// Hub settings, system facts and the log tail (`hub/*` maintenance
    /// requests); `None` answers them `unsupported`.
    pub platform: Option<Arc<dyn Platform>>,
    /// Stack of the maintenance job thread (network map, backup, …),
    /// started per job.
    pub admin_stack: usize,
    /// The OTA image store's region (the inactive app slot on the hub; none:
    /// streamed images only).
    #[cfg(feature = "ota-store")]
    pub ota_flash: crate::ota_store::OtaFlash,
    /// OTA images streamed at once (`Resources::ota_streams` on the hub).
    #[cfg(feature = "ota")]
    pub ota_streams: usize,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            hub_name: "zigbee-rs-hub".into(),
            api_token: String::new(),
            tz_offset_s: 0,
            endpoint: hub_endpoint(cfg!(feature = "ota")),
            builtin_catalog: true,
            stack_size: 96 * 1024,
            import_ncp_devices: true,
            platform: None,
            admin_stack: 16 * 1024,
            #[cfg(feature = "ota-store")]
            ota_flash: Default::default(),
            #[cfg(feature = "ota")]
            ota_streams: hub_ota::stream::MAX_STREAMS,
        }
    }
}

/// The hub endpoint (1, HA profile). Server clusters: Basic, Identify, Time
/// and, with the OTA server (`ota`: the 8 MB images), OTA Upgrade (0x0019),
/// so devices without it find no OTA server. A stored endpoint that
/// differs (an image with the other feature set ran before) is registered
/// again at start (`NWK_ADD_ENDPOINT` replaces it on the NCP) and stored.
pub fn hub_endpoint(ota: bool) -> EndpointRecord {
    let mut in_clusters = vec![0x0000, 0x0003, 0x000A];
    if ota {
        in_clusters.push(0x0019);
    }
    EndpointRecord {
        ep: 1,
        profile: 0x0104,
        device_id: 0x0005,
        version: 0,
        in_clusters,
        // Clients the hub drives: On/Off, Level, Scenes, Groups, Color, IAS
        // Zone (CIE), measurement reads.
        out_clusters: vec![0x0000, 0x0003, 0x0004, 0x0005, 0x0006, 0x0008, 0x0300, 0x0500, 0x0402, 0x0405],
    }
}

/// The definition examples of the repository (`definitions/examples`, as
/// compiled by zdefc), until the catalog banks exist.
pub fn builtin_catalog() -> MemoryCatalog {
    macro_rules! golden {
        ($($n:literal),*) => {
            [$((($n), &include_bytes!(concat!("../../zdefc/tests/golden/", $n, ".cbor"))[..])),*]
        };
    }
    let mut cat = MemoryCatalog::default();
    for (name, cbor) in golden!(
        "aqara-rtcgq14lm-motion-p1",
        "efekta-co2-smart-monitor",
        "gp-diyruz-flowerx",
        "gp-efektalab-bgt1",
        "gp-enocean-ptm216z",
        "gp-zigbee-rs-sensor",
        "gp-zigbee-rs-switch",
        "innr-sp120",
        "philips-rwl022-dimmer",
        "philips-sml001-motion",
        "tuya-ts0601-th-sensor",
        "tuya-ts0601-trv-me167"
    ) {
        match hub_devices::zdef_adapter::load_cbor(cbor, hub_devices::model::LayerSource::Catalog) {
            Ok(d) => {
                cat.insert(d);
            }
            Err(e) => warn!("builtin catalog: {name}: {e:?}"),
        }
    }
    cat
}

/// Application status.
#[derive(Debug, Clone, Default)]
pub struct AppStatus {
    /// Devices in the engine.
    pub devices: usize,
    /// API sessions.
    pub sessions: usize,
    /// Devices restored from the store at start.
    pub restored: usize,
    /// Registry writes and failures.
    pub registry_writes: u64,
    /// Kept last values: `(restored, writes, bytes, errors)`.
    pub last_values: (usize, u64, u64, u64),
    /// Registry write failures.
    pub registry_errors: u64,
    /// APS outcome counters.
    pub aps: ApsCounters,
    /// The hub endpoint is registered with the NCP.
    pub endpoint_registered: bool,
    /// Devices imported from the NCP's tables (see
    /// [`AppConfig::import_ncp_devices`]).
    pub imported: usize,
}

/// The definitions catalog in `hubdata` (device-support 9). The hub never
/// downloads: clients upload the signed release files.
pub struct CatalogSetup {
    /// `hubdata`.
    pub flash: SharedFlash,
    /// Signing keys trusted besides the built-in ones (raw Ed25519 public
    /// keys: private catalogs, tests).
    pub extra_keys: Vec<[u8; 32]>,
}

enum Input {
    /// Coordinator events are queued ([`EventQueue`]).
    Coord,
    Connect(SessionId, Box<dyn SessionSink>),
    ConnectTrusted(SessionId, Box<dyn SessionSink>),
    Hook(Box<dyn RequestHook>),
    Text(SessionId, String),
    Disconnect(SessionId),
    Admin(JobOut),
    Shutdown,
}

/// Handle to the running application. Cheap to clone.
#[derive(Clone)]
pub struct App {
    tx: Sender<Input>,
    status: Arc<Mutex<AppStatus>>,
    stopped: Arc<AtomicBool>,
    // 32-bit: the S3 has no 64-bit atomics.
    next_session: Arc<AtomicU32>,
    web: SharedWeb,
    radio: Option<Arc<dyn RadioControl>>,
}

fn endpoint_config(e: &EndpointRecord) -> EndpointConfig {
    EndpointConfig {
        ep: e.ep,
        profile: e.profile,
        device_id: e.device_id,
        version: e.version,
        in_clusters: e.in_clusters.clone(),
        out_clusters: e.out_clusters.clone(),
    }
}

fn wall_ms() -> Millis {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as Millis)
}

impl App {
    /// Start the application on a running ZRHP host core.
    #[cfg(feature = "zrhp")]
    pub fn start(ncp: hub_ncp::Hub, cfg: AppConfig) -> App {
        Self::start_with(ncp, cfg, None)
    }

    /// [`App::start`] with the catalog store in `hubdata`:
    /// [`App::start_backend`] with [`Backend::zrhp`].
    #[cfg(feature = "zrhp")]
    pub fn start_with(ncp: hub_ncp::Hub, cfg: AppConfig, catalog: Option<CatalogSetup>) -> App {
        Self::start_backend(Backend::zrhp(ncp), cfg, catalog)
    }

    /// Start the application on a coordinator backend (its coordinator is
    /// started here, on the app's event queue, with the hub endpoint of
    /// `cfg`), with the catalog store in `hubdata` when `catalog` is given.
    pub fn start_backend(backend: Backend, cfg: AppConfig, catalog: Option<CatalogSetup>) -> App {
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(AppStatus::default()));
        let stopped = Arc::new(AtomicBool::new(false));
        let bell = tx.clone();
        let events = Arc::new(EventQueue::new(
            EVENT_SLOTS,
            Box::new(move || {
                let _ = bell.send(Input::Coord);
            }),
        ));
        let endpoints = BackendConfig { endpoints: vec![endpoint_config(&cfg.endpoint)] };
        let (coord, net) = (backend.coordinator)(endpoints, events.clone());
        let records = backend.kv;
        let st = status.clone();
        let stop = stopped.clone();
        let jobs = tx.clone();
        let web = SharedWeb::default();
        let web2 = web.clone();
        thread::Builder::new()
            .name("hub-app".into())
            .stack_size(cfg.stack_size)
            .spawn(move || Runner::new(records, coord, net, events, cfg, st, jobs, catalog, web2).run(rx, stop))
            .expect("hub-app thread");
        App { tx, status, stopped, next_session: Arc::new(AtomicU32::new(1)), web, radio: backend.radio }
    }

    /// The backend's Wi-Fi coexistence control (the firmware's Wi-Fi
    /// callbacks); `None` when the NCP has none.
    pub fn radio(&self) -> Option<&Arc<dyn RadioControl>> {
        self.radio.as_ref()
    }

    /// A new API connection; the `auth_required` frame goes to `sink`.
    ///
    /// Never waits for the app thread: the caller may be the transport's
    /// own thread (esp_http_server's WebSocket handler), which the app
    /// thread itself waits for when it sends through a blocking sink.
    pub fn connect(&self, sink: Box<dyn SessionSink>) -> Option<SessionId> {
        let sid = SessionId::from(self.next_session.fetch_add(1, Ordering::SeqCst));
        self.tx.send(Input::Connect(sid, sink)).ok()?;
        Some(sid)
    }

    /// An in-process API session that is authenticated already (with the
    /// current token, so a rotation does not lock it out): `auth_required`
    /// and `auth_ok` go to `sink`. For services inside the hub (the MQTT
    /// bridge); never exposed to the network.
    pub fn connect_trusted(&self, sink: Box<dyn SessionSink>) -> Option<SessionId> {
        let sid = SessionId::from(self.next_session.fetch_add(1, Ordering::SeqCst));
        self.tx.send(Input::ConnectTrusted(sid, sink)).ok()?;
        Some(sid)
    }

    /// Serve the requests of `hook` (types starting with its prefix).
    pub fn add_hook(&self, hook: Box<dyn RequestHook>) {
        let _ = self.tx.send(Input::Hook(hook));
    }

    /// A text frame from a connection.
    pub fn text(&self, s: SessionId, text: String) {
        let _ = self.tx.send(Input::Text(s, text));
    }

    /// A closed connection.
    pub fn disconnect(&self, s: SessionId) {
        let _ = self.tx.send(Input::Disconnect(s));
    }

    /// The web tools (`/zdefc/<file>`, served by the HTTP server).
    pub fn web(&self) -> &SharedWeb {
        &self.web
    }

    /// Status snapshot.
    pub fn status(&self) -> AppStatus {
        self.status.lock().unwrap().clone()
    }

    /// Wait until `pred` holds.
    pub fn wait(&self, timeout: Duration, pred: impl Fn(&AppStatus) -> bool) -> bool {
        let until = Instant::now() + timeout;
        while Instant::now() < until {
            if pred(&self.status()) {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Persist and stop.
    pub fn shutdown(&self) {
        let _ = self.tx.send(Input::Shutdown);
        let until = Instant::now() + Duration::from_secs(5);
        while !self.stopped.load(Ordering::SeqCst) && Instant::now() < until {
            thread::sleep(Duration::from_millis(5));
        }
    }
}

struct Runner {
    /// The hub's records and whose they are.
    records: Arc<dyn Records>,
    cfg: AppConfig,
    engine: Engine<CoordTransport>,
    /// The coordinator's events.
    events: Arc<EventQueue>,
    api: ApiServer,
    sinks: BTreeMap<SessionId, Box<dyn SessionSink>>,
    /// Sessions whose snapshot is being sent piece by piece.
    outboxes: BTreeMap<SessionId, Outbox>,
    registry: Registry,
    last: crate::laststate::LastValues,
    status: Arc<Mutex<AppStatus>>,
    wall0: Millis,
    t0: Instant,
    registered: bool,
    /// NCP cookie of the session the last import ran in.
    imported_in: Option<u32>,
    import_retry: Option<Instant>,
    /// The device inventory in flight: request, NCP cookie, generation.
    import: Option<(RequestId, u32, u32)>,
    /// Bumped when the NCP's devices must be imported again (an inventory
    /// in flight then does not count).
    import_gen: u32,
    imported: usize,
    /// Unknown states of restored mains devices have been read.
    refreshed: bool,
    /// Permit-join seconds left at the last look.
    permit_left: u32,
    /// Maintenance requests and their jobs.
    admin: Admin,
    /// Network administration for the maintenance jobs.
    net: Arc<dyn NetworkAdmin>,
    /// Job results come back on the app's own channel.
    jobs: Sender<Input>,
    /// NCP cookie `hub/info` `maintenance` was computed for.
    maintenance_for: Option<u32>,
    /// NCP cookie the hub endpoint's group memberships were replayed in.
    groups_for: Option<u32>,
    /// The catalog store and its downloads.
    catalog: Option<CatalogService>,
    /// Local automations.
    automations: Automations,
    /// Last request ID of the automations' internal session.
    automation_requests: u64,
    /// `automations/run` answers that wait for their run's request action:
    /// session, request ID, run.
    run_answers: Vec<(SessionId, u64, u32)>,
    /// Results of request actions whose run waits for control requests.
    request_results: RequestResults,
    /// Requests served outside the engine (`mqtt/*`).
    hooks: Vec<Box<dyn RequestHook>>,
    /// The OTA Upgrade server.
    #[cfg(feature = "ota")]
    ota: crate::ota::Ota,
}

/// The session automations send their `request` actions through.
const AUTOMATIONS_SESSION: SessionId = 1 << 40;

/// The wall clock once it is set (SNTP): anything before 2024 is a clock
/// that still counts from its power-on.
fn wall_s() -> Option<i64> {
    let s = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    (s >= 1_704_067_200).then_some(s)
}

/// What the automations act on: the engine, and the API for their
/// commands and `request` actions (group commands, scenes) through an
/// internal session whose deferred results come back as frames.
struct AppWorld<'a> {
    engine: &'a mut Engine<CoordTransport>,
    api: &'a mut ApiServer,
    now: Millis,
    next_id: &'a mut u64,
    /// Results of request actions whose run waits for control requests.
    results: &'a mut RequestResults,
}

type ApiResult = Result<serde_json::Value, hub_devices::hub::ApiError>;
type RequestResults = Vec<(u64, ApiResult)>;

/// A result frame of the internal session.
fn reply(frame: &str) -> Option<(u64, Result<serde_json::Value, hub_devices::hub::ApiError>)> {
    let r: serde_json::Value = serde_json::from_str(frame).ok()?;
    let id = r["id"].as_u64().filter(|_| r["type"] == "result")?;
    Some((
        id,
        match r["success"].as_bool() {
            Some(true) => Ok(r["result"].clone()),
            _ => Err(hub_devices::hub::ApiError::new(
                r["error"]["code"].as_str().unwrap_or("failed"),
                r["error"]["message"].as_str().unwrap_or(""),
            )),
        },
    ))
}

impl AppWorld<'_> {
    /// Sends `req` on the internal session: its ID, and the result unless
    /// it follows later.
    fn call(&mut self, mut req: serde_json::Value) -> (u64, Option<Result<serde_json::Value, hub_devices::hub::ApiError>>) {
        *self.next_id += 1;
        let id = *self.next_id;
        req["id"] = id.into();
        let frames = self.api.handle_text(self.engine, AUTOMATIONS_SESSION, &req.to_string(), self.now);
        settle(self.engine, self.now);
        if frames.is_empty() {
            return (id, self.api.take_result(self.engine, AUTOMATIONS_SESSION, id));
        }
        (id, frames.first().and_then(|f| reply(f)).map(|(_, r)| r))
    }
}

impl World for AppWorld<'_> {
    fn state(&self, uid: &str) -> Option<serde_json::Value> {
        self.engine.state_of(uid)
    }

    fn available(&self, device: &str) -> Option<bool> {
        self.engine.device(device).map(|d| d.available)
    }

    fn command(
        &mut self,
        uid: &str,
        command: &str,
        params: &serde_json::Value,
    ) -> Result<(u64, &'static str), hub_devices::hub::ApiError> {
        let req = serde_json::json!({"type": "entities/command", "uid": uid, "command": command, "params": params});
        match self.call(req) {
            (id, None) => Ok((id, "pending")),
            (id, Some(Ok(r))) => Ok((id, if r["status"] == "queued" { "queued" } else { "ok" })),
            (_, Some(Err(e))) => Err(e),
        }
    }

    fn request(&mut self, req: &serde_json::Value) -> Result<(u64, Option<serde_json::Value>), hub_devices::hub::ApiError> {
        let before = self.engine.transport.controls_submitted();
        let (id, r) = self.call(req.clone());
        // What the request's control requests decide (its deferred result,
        // the rest of a group deletion) is done before the rule's next
        // action: the run waits for them.
        let submitted = self.engine.transport.controls_submitted() != before;
        if self.engine.transport.controls_open() && (submitted || r.is_none()) {
            if let Some(r) = r {
                self.results.push((id, r));
            }
            return Ok((id, None));
        }
        match r {
            Some(r) => r.map(|v| (id, Some(v))),
            // A deferred result (a device command): sent.
            None => Ok((id, Some(serde_json::Value::Null))),
        }
    }
}

/// Hands the outcomes of control requests the app has to the engine.
fn settle(engine: &mut Engine<CoordTransport>, now: Millis) {
    while let Some((id, o)) = engine.transport.take_control() {
        engine.on_outcome(id, o, now);
    }
}

/// Opens the catalog store in `hubdata` and its service.
#[inline(never)]
fn open_catalog(kv: &dyn KvStore, setup: CatalogSetup, web: SharedWeb) -> Option<(StoreCatalog, CatalogService)> {
    let size = setup.flash.lock().unwrap().size();
    let Some(map) = hub_core::layout::HubdataMap::for_size(size) else {
        warn!("catalog: hubdata of {size} B has no catalog areas");
        return None;
    };
    let opened = {
        let mut f = setup.flash.lock().unwrap();
        let mut keys = crate::catalog::trusted_keys();
        keys.extend(setup.extra_keys.iter().map(|k| zdef::sign::TrustedKey::new(*k)));
        CatalogStore::open(&mut **f, map, keys)
    };
    let (store, bank) = match opened {
        Ok(x) => x,
        Err(e) => {
            warn!("catalog: store: {e}");
            return None;
        }
    };
    for w in &store.warnings {
        warn!("catalog: {w}");
    }
    if let Some(c) = &store.active {
        info!("catalog: bank generation {} ({} definitions, key {})", c.generation, c.definitions, c.kid);
    }
    let cfg = load_config(kv::with(kv, |s| s.get(CONFIG_KEY).map(<[u8]>::to_vec)));
    let cat = StoreCatalog { bank, fetched: store.local.catalog(), ..Default::default() };
    Some((cat, CatalogService::new(setup.flash, store, cfg, web)))
}

/// Opened, closed (or expired), or re-opened since the last look; the
/// countdown itself is not an event.
fn permit_changed(before: u32, now: u32) -> bool {
    (before == 0) != (now == 0) || now > before
}

impl Runner {
    #[allow(clippy::too_many_arguments)]
    fn new(
        records: Arc<dyn Records>,
        coord: Box<dyn hub_coord::Coordinator>,
        net: Arc<dyn NetworkAdmin>,
        events: Arc<EventQueue>,
        cfg: AppConfig,
        status: Arc<Mutex<AppStatus>>,
        jobs: Sender<Input>,
        setup: Option<CatalogSetup>,
        web: SharedWeb,
    ) -> Self {
        let builtin = if cfg.builtin_catalog { builtin_catalog() } else { MemoryCatalog::default() };
        let (catalog, service): (Box<dyn Catalog>, Option<CatalogService>) =
            match setup.map(|s| open_catalog(&*records, s, web)) {
                Some(Some((mut c, svc))) => {
                    // The examples stand in until a bank is installed.
                    if c.bank.is_none() {
                        c.builtin = builtin;
                    }
                    (Box::new(c), Some(svc))
                }
                _ => (Box::new(builtin), None),
            };
        let hub_cfg = HubConfig { tz_offset_s: cfg.tz_offset_s, ..HubConfig::default() };
        let mut transport = CoordTransport::new(coord, net.clone());
        transport.records = Some(records.clone());
        let mut engine = Engine::new(transport, hub_cfg, catalog);
        engine.set_schema_decoder(Box::new(ZdefSchemaDecoder));
        // Without the OTA server, OTA cluster commands take the engine's
        // generic path (a requested Default Response, no OTA answer).
        #[cfg(feature = "ota")]
        engine.app_clusters.push(hub_ota::server::CLUSTER);
        #[cfg(feature = "ota")]
        let ota = crate::ota::Ota::new(&cfg);
        #[allow(unused_mut)]
        let mut api = ApiServer::new(cfg.hub_name.clone(), cfg.api_token.clone());
        #[cfg(feature = "ota")]
        api.set_info("ota", ota.info());
        let mut admin = Admin::new(cfg.platform.clone(), cfg.hub_name.clone());
        admin.job_stack = cfg.admin_stack;
        admin.start_worker();
        admin.load_clock(&*records);
        let mut r = Self {
            records,
            cfg,
            engine,
            events,
            api,
            sinks: BTreeMap::new(),
            outboxes: BTreeMap::new(),
            registry: Registry::default(),
            last: Default::default(),
            status,
            wall0: wall_ms(),
            t0: Instant::now(),
            registered: false,
            imported_in: None,
            import_retry: None,
            import: None,
            import_gen: 0,
            imported: 0,
            refreshed: false,
            permit_left: 0,
            admin,
            net,
            jobs,
            maintenance_for: None,
            groups_for: None,
            catalog: service,
            automations: Automations::default(),
            automation_requests: 0,
            run_answers: Vec::new(),
            request_results: Vec::new(),
            hooks: Vec::new(),
            #[cfg(feature = "ota")]
            ota,
        };
        let now = r.now();
        let mut registry = std::mem::take(&mut r.registry);
        registry.load(&*r.records, &mut r.engine, now);
        r.registry = registry;
        // The clock first: kept times convert only once the time is known.
        r.admin.clock_tick(&mut r.engine.cfg, now);
        let mut last = std::mem::take(&mut r.last);
        last.load(&*r.records, &mut r.engine, now);
        r.last = last;
        let records: Vec<(String, Vec<u8>)> =
            kv::with(&*r.records, |s| s.prefix(automations::PREFIX).map(|(k, v)| (k.to_string(), v.to_vec())).collect());
        let n = r.automations.load(records);
        if n > 0 {
            info!("automations: {n} rules restored");
        }
        let _ = r.api.connect_as(AUTOMATIONS_SESSION);
        let auth = serde_json::json!({"type": "auth", "token": r.cfg.api_token}).to_string();
        let _ = r.api.handle_text(&mut r.engine, AUTOMATIONS_SESSION, &auth, now);
        r.api.set_info(
            "automations",
            serde_json::json!({"format": automations::rule::FORMAT, "max_rules": automations::rule::MAX_RULES}),
        );
        let known: Vec<(u64, u16)> = r.engine.devices().filter_map(|d| Some((d.ieee()?, d.nwk()?))).collect();
        r.engine.transport.nwk.extend(known);
        #[cfg(feature = "ota")]
        r.ota.load(&*r.records, &r.engine, now);
        // Restored devices produce added events; nobody is subscribed yet,
        // and their snapshots are what the store holds.
        let _ = r.engine.drain_events();
        r.automations.prime(&AppWorld {
            engine: &mut r.engine,
            api: &mut r.api,
            now,
            next_id: &mut 0,
            results: &mut Vec::new(),
        });
        r
    }

    fn clock(&self) -> Clock {
        Clock {
            mono_ms: self.t0.elapsed().as_millis() as u64,
            now_ms: self.now(),
            wall_s: wall_s(),
            // The hub's time zone (hub/time/set), DST included; the admin's
            // clock tick keeps it current.
            tz_offset_s: self.engine.cfg.tz_offset_s,
        }
    }

    /// Runs `f` on the automations with the engine as their world.
    fn with_automations<R>(&mut self, f: impl FnOnce(&mut Automations, &mut AppWorld, &Clock) -> R) -> R {
        let clk = self.clock();
        let mut w = AppWorld {
            engine: &mut self.engine,
            api: &mut self.api,
            now: clk.now_ms,
            next_id: &mut self.automation_requests,
            results: &mut self.request_results,
        };
        f(&mut self.automations, &mut w, &clk)
    }

    /// Changed rules to the store, automation events to subscribers.
    fn pump_automations(&mut self) {
        let dirty = std::mem::take(&mut self.automations.dirty);
        if !dirty.is_empty() {
            let a = &self.automations;
            kv::with(&*self.records, |s| {
                for id in &dirty {
                    let k = format!("{}{id}", automations::PREFIX);
                    let r = match a.rule(id) {
                        Some(rule) => s.put(&k, rule.src.as_bytes()),
                        None => s.delete(&k),
                    };
                    if let Err(e) = r {
                        warn!("automations: {k}: {e}");
                    }
                }
            });
        }
        for e in std::mem::take(&mut self.automations.events) {
            for (sid, frame) in self.api.publish("automations", e) {
                self.send(sid, &frame);
            }
        }
    }

    fn now(&self) -> Millis {
        self.wall0 + self.t0.elapsed().as_millis() as Millis
    }

    fn run(mut self, rx: Receiver<Input>, stopped: Arc<AtomicBool>) {
        let mut next_tick = Instant::now();
        loop {
            // An automation's delay may end before the next tick.
            let due = self.automations.next_wake().map(|ms| self.t0 + Duration::from_millis(ms));
            let mut wait = due.map_or(next_tick, |d| d.min(next_tick)).saturating_duration_since(Instant::now());
            // A paced OTA block response may be due before both.
            #[cfg(feature = "ota")]
            if let Some(due) = self.ota.next_due() {
                wait = wait.min(Duration::from_millis(due.saturating_sub(self.now())));
            }
            // A snapshot waits for its sink to send the previous pieces.
            if !self.outboxes.is_empty() {
                wait = wait.min(Duration::from_millis(10));
            }
            match rx.recv_timeout(wait) {
                Ok(Input::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                // Each event is an input of its own, as the NCP's messages
                // always were.
                Ok(Input::Coord) => {
                    let events = self.events.clone();
                    events.drain(EVENT_SLOTS, |ev| {
                        self.coord_event(ev);
                        self.settled();
                        self.after(&mut next_tick);
                    });
                    continue;
                }
                Ok(input) => {
                    self.handle(input);
                    self.settled();
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.after(&mut next_tick);
        }
        let now = self.now();
        self.registry.flush(&*self.records, &self.engine, now, true);
        self.last.flush(&*self.records, &self.engine, now, true);
        // The coordinator (its pump) stops with the engine.
        drop(self);
        stopped.store(true, Ordering::SeqCst);
    }

    /// The tick when it is due, then what follows every input.
    fn after(&mut self, next_tick: &mut Instant) {
        if Instant::now() >= *next_tick {
            *next_tick += Duration::from_secs(1);
            self.tick();
        }
        self.after_input();
    }

    /// One event of the coordinator.
    fn coord_event(&mut self, ev: CoordEvent) {
        let now = self.now();
        match ev {
            CoordEvent::Indication(i) => {
                if let Some(ieee) = i.src_ieee {
                    self.engine.transport.nwk.insert(ieee, i.src_nwk);
                }
                self.engine.on_aps_indication(i, now);
            }
            CoordEvent::DeviceJoined { ieee, nwk } => {
                self.engine.transport.nwk.insert(ieee, nwk);
                self.engine.on_device_joined(ieee, nwk, now);
            }
            // Logged: a device stays until it is removed (`devices/remove`),
            // as with ZRHP, which reports no leaves.
            CoordEvent::DeviceLeft { ieee, nwk, rejoin } => {
                info!("app: device {ieee:016x} (0x{nwk:04X}) left, rejoin {rejoin:?}")
            }
            CoordEvent::Gp(e) => self.engine.on_gp_event(*e, now),
            CoordEvent::NetworkState(c) => {
                let state = match c {
                    NetChange::Down => "down",
                    NetChange::Forming => "forming",
                    NetChange::Up => "up",
                    NetChange::Leaving => "leaving",
                };
                self.engine.network_event(serde_json::json!({"topic": "network_state", "state": state}));
            }
            // The host core's remaining time decides (see `observe_permit`).
            CoordEvent::PermitJoin { .. } => {}
            CoordEvent::ScanProgress { done, total, .. } => self.admin.scan_progress(done, total),
            CoordEvent::Progress { id, p } => self.outcome(id, p.into(), now),
            CoordEvent::Done { id, t } => self.outcome(id, t.into(), now),
            CoordEvent::Session(s) => match *s {
                SessionEvent::Up { info, .. } => self.register(&info),
                // The NCP's network is unknown until the next session's
                // ownership report.
                SessionEvent::Down | SessionEvent::Incompatible => self.records.observe(None),
            },
            CoordEvent::Ownership(o) => self.records.observe(Some(&o)),
            CoordEvent::Dropped { events } => warn!("app: {events} coordinator events lost"),
        }
    }

    /// The outcome of a request: the engine's (control outcomes are
    /// settled at once, APS ones in the tick) or the app's own.
    fn outcome(&mut self, id: RequestId, o: Outcome, now: Millis) {
        if let Some(o) = self.engine.transport.outcome(id, o) {
            self.own_outcome(id, o, now);
        }
    }

    /// [`settle`], then the automation runs waiting for what it decided.
    fn settled(&mut self) {
        let now = self.now();
        settle(&mut self.engine, now);
        self.resume_requests();
    }

    /// Continues the automation runs that wait for their request action
    /// once no control request is open: with the request's result (known
    /// at once, deferred, or none for a device command, as before); then
    /// answers the `automations/run` requests that waited for them.
    fn resume_requests(&mut self) {
        loop {
            let keys = self.automations.waiting_requests();
            self.request_results.retain(|(k, _)| keys.contains(k));
            for key in &keys {
                // Taken before the session's frames are sent.
                if let Some(r) = self.api.take_result(&mut self.engine, AUTOMATIONS_SESSION, *key) {
                    self.request_results.push((*key, r));
                }
            }
            if keys.is_empty() || self.engine.transport.controls_open() {
                break;
            }
            for key in keys {
                let i = self.request_results.iter().position(|(k, _)| *k == key);
                let r = i.map_or(Ok(serde_json::Value::Null), |i| self.request_results.swap_remove(i).1);
                self.with_automations(|a, w, clk| a.request_result(key, r, w, clk));
            }
        }
        if self.run_answers.is_empty() {
            return;
        }
        let (ready, waiting): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.run_answers).into_iter().partition(|(_, _, run)| !self.automations.pending(*run));
        self.run_answers = waiting;
        for (sid, id, run) in ready {
            let v = self.automations.run_result(run);
            self.send(sid, &hub_devices::api::ServerMessage::ok(id, v).to_text());
        }
    }

    fn handle(&mut self, input: Input) {
        let now = self.now();
        match input {
            Input::Coord => {}
            Input::Connect(sid, mut sink) => {
                let hello = self.api.connect_as(sid);
                if sink.send(&hello) {
                    self.sinks.insert(sid, sink);
                } else {
                    self.api.disconnect(sid);
                }
            }
            Input::ConnectTrusted(sid, mut sink) => {
                let hello = self.api.connect_as(sid);
                let auth = serde_json::json!({"type": "auth", "token": self.api.token()}).to_string();
                let frames = self.api.handle_text(&mut self.engine, sid, &auth, now);
                if sink.send(&hello) && frames.iter().all(|f| sink.send(f)) {
                    self.sinks.insert(sid, sink);
                } else {
                    self.api.disconnect(sid);
                }
            }
            Input::Hook(h) => {
                self.hooks.push(h);
                if self.maintenance_for.is_some() {
                    self.set_maintenance();
                }
            }
            Input::Text(sid, text) => {
                #[cfg(feature = "ota")]
                if self.api.is_authed(sid)
                    && let Some(frame) = self.ota_request(sid, &text, now)
                {
                    drop(text);
                    self.send_owned(sid, frame);
                    return;
                }
                if self.api.is_authed(sid)
                    && let Some(frame) = self.hook_request(&text)
                {
                    self.send(sid, &frame);
                    return;
                }
                if self.api.is_authed(sid)
                    && let Some(head) = admin::head(&text)
                {
                    if matches!(head.ty.as_str(), "hub/reboot" | "hub/mode/set") {
                        self.registry.flush(&*self.records, &self.engine, now, true);
                        self.last.flush(&*self.records, &self.engine, now, true);
                    }
                    for frame in self.admin_request(sid, &head, now) {
                        self.send(sid, &frame);
                    }
                    return;
                }
                if self.api.is_authed(sid)
                    && text.contains("\"automations/")
                    && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
                    && let (Some(id), Some(kind)) = (v["id"].as_u64(), v["type"].as_str())
                    && let Some(r) = {
                        self.automations.free_heap = self.cfg.platform.as_ref().and_then(|p| p.data_heap_free());
                        match self.automations_refused(kind) {
                            Some(e) => Some(Err(e)),
                            None => self.with_automations(|a, w, clk| a.request(kind, &v, w, clk)),
                        }
                    }
                {
                    // A run waiting for its request action (or deferred behind
                    // one) answers once it goes on, as it did when the result
                    // was known in place.
                    if kind == "automations/run"
                        && let Ok(v) = &r
                        && let Some(run) = v["run"].as_u64().map(|r| r as u32).filter(|r| self.automations.pending(*r))
                    {
                        self.run_answers.push((sid, id, run));
                        self.pump_automations();
                        return;
                    }
                    let m = match r {
                        Ok(v) => hub_devices::api::ServerMessage::ok(id, v),
                        Err(e) => hub_devices::api::ServerMessage::err(id, e),
                    };
                    self.send(sid, &m.to_text());
                    self.pump_automations();
                    return;
                }
                let ext = self.catalog.as_mut().map(|c| c as &mut dyn ApiExtension<CoordTransport>);
                for frame in self.api.handle_text_frames(&mut self.engine, sid, &text, now, ext) {
                    self.deliver(sid, frame, now);
                }
                if text.contains("\"definitions/upload\"") || text.contains("\"definitions/delete\"") {
                    self.registry.definitions_changed(now);
                }
            }
            Input::Disconnect(sid) => self.drop_session(sid),
            Input::Admin(out) => match out {
                JobOut::Reply(sid, frame) => self.send_owned(sid, frame),
                JobOut::Event(body) => self.engine.network_event(body),
                JobOut::Groups(list) => {
                    self.engine.zg_replace_groups(&list, now);
                    info!("app: {} groups from the restored backup", self.engine.zg.groups.len());
                }
                JobOut::Reimport => {
                    self.imported_in = None;
                    self.import_retry = None;
                    self.import_gen = self.import_gen.wrapping_add(1);
                }
            },
            Input::Shutdown => {}
        }
    }

    /// An `ota/*` request (`ota/block_data` read from the text): its
    /// result frame, `None` for other requests.
    #[cfg(feature = "ota")]
    fn ota_request(&mut self, sid: SessionId, text: &str, now: Millis) -> Option<String> {
        if let Some(frame) = self.ota.block_data_text(sid, text) {
            return Some(frame);
        }
        let (id, ty, v) = crate::ota::head(text)?;
        // A job and a device's policy are network records (see `ota`).
        if matches!(ty.as_str(), "ota/start" | "ota/devices/policy")
            && let Some(e) = self.registry_read_only()
        {
            return Some(hub_devices::api::ServerMessage::err(id, e).to_text());
        }
        Some(match self.ota.request(&mut self.engine, &*self.records, sid, &ty, &v, now) {
            Ok(r) => r.frame(id),
            Err(e) => hub_devices::api::ServerMessage::err(id, e).to_text(),
        })
    }

    /// The OTA server: client commands, paced responses, persistence and
    /// the streams' block requests.
    #[cfg(feature = "ota")]
    fn pump_ota(&mut self, now: Millis) {
        self.ota.frames(&mut self.engine, now);
        self.ota.poll(&mut self.engine, &*self.records, now);
        // Streams of sessions that are gone wait for a client.
        let sinks = &self.sinks;
        self.ota.prune(|sid| sinks.contains_key(&sid), now);
        for (sid, frame) in self.ota.outbox() {
            self.send_owned(sid, frame);
        }
    }

    /// `hub/info` `maintenance`: the admin features and the hooks'.
    fn set_maintenance(&mut self) {
        let caps = self.engine.transport.status().caps;
        let mut f = self.admin.features(caps.as_deref());
        f.extend(self.hooks.iter().filter_map(|h| h.feature()));
        self.api.set_info("maintenance", f.into());
    }

    /// A request of a [`RequestHook`]: the result frame, `None` when no
    /// hook serves it.
    fn hook_request(&mut self, text: &str) -> Option<String> {
        let Some(h) = self.hooks.iter_mut().find(|h| text.contains(h.prefix())) else {
            return optional_missing(text);
        };
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let ty = v["type"].as_str().filter(|t| t.starts_with(h.prefix()))?;
        let id = v["id"].as_u64()?;
        Some(
            match h.request(ty, &v) {
                Ok(r) => hub_devices::api::ServerMessage::ok(id, r),
                Err(e) => hub_devices::api::ServerMessage::err(id, e),
            }
            .to_text(),
        )
    }

    /// A maintenance request (`admin`), with what it may need from here.
    fn admin_request(&mut self, sid: SessionId, head: &admin::Head, now: Millis) -> Vec<String> {
        if head.ty.starts_with("network/registry/") {
            return vec![self.registry_request(head, now)];
        }
        let engine = &self.engine;
        let known = || {
            engine
                .devices()
                .filter_map(|d| {
                    Some(crate::topology::Known {
                        ieee: d.ieee()?,
                        nwk: d.nwk(),
                        id: d.id.clone(),
                        name: d.user_name.clone().or_else(|| d.model()),
                        router: d.fp.logical_type == Some(hub_devices::zdo::LogicalType::Router),
                    })
                })
                .collect()
        };
        let status = self.status.clone();
        let sessions = self.sinks.len();
        let app_facts = || {
            let a = status.lock().unwrap().clone();
            serde_json::json!({
                "devices": a.devices,
                "sessions": sessions,
                "restored": a.restored,
                "imported": a.imported,
                "registry_writes": a.registry_writes,
                "registry_errors": a.registry_errors,
                // Kept last values: [states restored, records written, bytes, errors].
                "last_values": [a.last_values.0, a.last_values.1, a.last_values.2, a.last_values.3],
                "aps": {"accepted": a.aps.accepted, "busy": a.aps.busy, "failed": a.aps.failed,
                    "delivered": a.aps.delivered, "not_delivered": a.aps.not_delivered, "unconfirmed": a.aps.unconfirmed},
            })
        };
        let jobs = Mutex::new(self.jobs.clone());
        let st = self.engine.transport.status();
        let ctx = admin::Ctx {
            records: &self.records,
            status: &st,
            net: &self.net,
            api: &mut self.api,
            known: &known,
            app_facts: &app_facts,
            out: Arc::new(move |o| {
                let _ = jobs.lock().unwrap().send(Input::Admin(o));
            }),
            now_s: hub_devices::device::ts(now),
        };
        self.admin.handle(ctx, sid, head)
    }

    fn send(&mut self, sid: SessionId, frame: &str) {
        if sid == AUTOMATIONS_SESSION {
            // A command result that followed later.
            if let Some((id, r)) = reply(frame) {
                self.automations.command_result(id, r.map(|_| ()));
            }
            return;
        }
        if self.outboxes.contains_key(&sid) {
            self.hold(sid, frame.to_string());
        } else if let Some(sink) = self.sinks.get_mut(&sid)
            && !sink.send(frame)
        {
            self.drop_session(sid);
        }
    }

    /// [`send`](Self::send) of a frame the caller no longer needs.
    fn send_owned(&mut self, sid: SessionId, frame: String) {
        if sid == AUTOMATIONS_SESSION {
            return self.send(sid, &frame);
        }
        if self.outboxes.contains_key(&sid) {
            self.hold(sid, frame);
        } else if let Some(sink) = self.sinks.get_mut(&sid)
            && !sink.send_owned(frame)
        {
            self.drop_session(sid);
        }
    }

    fn deliver(&mut self, sid: SessionId, frame: Frame, now: Millis) {
        match frame {
            Frame::Text(t) => self.send_owned(sid, t),
            Frame::Snapshot(snap) => {
                if self.sinks.get(&sid).is_some_and(|s| s.pieces().is_some()) {
                    self.outboxes.entry(sid).or_default().items.push_back(Queued::Snapshot(snap));
                    self.pump_outboxes(now);
                } else {
                    let text = snap.render(&mut self.engine, now);
                    self.send_owned(sid, text);
                }
            }
        }
    }

    /// A frame behind a snapshot still being sent.
    fn hold(&mut self, sid: SessionId, frame: String) {
        let limit = self.sinks.get(&sid).and_then(|s| s.pieces()).map_or(0, |p| p.held);
        let Some(out) = self.outboxes.get_mut(&sid) else { return };
        out.held += frame.len();
        if out.held > limit {
            warn!("api: session {sid}: {} B held behind its snapshot, dropping it", out.held);
            self.drop_session(sid);
            return;
        }
        out.items.push_back(Queued::Frame(frame));
    }

    /// Sends what the sinks of snapshots in progress can take now.
    fn pump_outboxes(&mut self, now: Millis) {
        let sids: Vec<SessionId> = self.outboxes.keys().copied().collect();
        for sid in sids {
            let (Some(sink), Some(out)) = (self.sinks.get_mut(&sid), self.outboxes.get_mut(&sid)) else {
                self.drop_session(sid);
                continue;
            };
            let Some(p) = sink.pieces() else {
                self.drop_session(sid);
                continue;
            };
            let mut ok = true;
            while ok {
                match out.items.front_mut() {
                    None => break,
                    Some(Queued::Frame(f)) => {
                        let backlog = sink.backlog();
                        if backlog > 0 && (backlog > p.backlog || backlog + f.len() > p.held) {
                            break;
                        }
                        let Some(Queued::Frame(f)) = out.items.pop_front() else { unreachable!() };
                        out.held -= f.len();
                        ok = sink.send_owned(f);
                    }
                    Some(Queued::Snapshot(snap)) => {
                        if sink.backlog() > p.backlog {
                            break;
                        }
                        match snap.next_piece(&mut self.engine, now, p.size) {
                            Some((piece, last)) => {
                                ok = sink.send_piece(piece, !out.started, last);
                                out.started = !last;
                                if last {
                                    out.items.pop_front();
                                }
                            }
                            None => {
                                out.items.pop_front();
                                out.started = false;
                            }
                        }
                    }
                }
            }
            if !ok {
                self.drop_session(sid);
            } else if out.items.is_empty() {
                self.outboxes.remove(&sid);
            }
        }
    }

    fn drop_session(&mut self, sid: SessionId) {
        if let Some(mut sink) = self.sinks.remove(&sid) {
            sink.close();
        }
        self.outboxes.remove(&sid);
        self.api.disconnect(sid);
    }

    fn tick(&mut self) {
        let now = self.now();
        self.check_registry(now);
        let probe = self.t0.elapsed().as_secs().is_multiple_of(KEEPALIVE_S);
        let dead: Vec<SessionId> = self
            .sinks
            .iter_mut()
            .filter_map(|(sid, s)| (!s.alive() || (probe && !s.keepalive())).then_some(*sid))
            .collect();
        for sid in dead {
            self.drop_session(sid);
        }
        let st = self.engine.transport.status();
        let ready = st.session == CoordSession::Ready;
        // Device traffic waits until the NCP's network is the records'
        // (`NcpNv` backends; always for ZRHP).
        let verified = self.records.verified();
        let cookie = st.epoch;
        if ready && self.maintenance_for != cookie {
            self.maintenance_for = cookie;
            #[cfg(feature = "ota")]
            self.ota.set_asdu(st.caps.as_ref().map(|c| u32::from(c.max_asdu)));
            self.set_maintenance();
        }
        // Group memberships of the hub endpoint, once per NCP session.
        if self.registered && ready && verified && self.groups_for != cookie {
            self.groups_for = cookie;
            let n = self.engine.groups_resync(now);
            if n > 0 {
                info!("app: hub endpoint joined {n} groups");
            }
        }
        if self.registered && self.cfg.import_ncp_devices && ready && verified {
            self.import_ncp_devices(cookie);
        }
        self.refresh(ready && verified, now);
        self.admin.clock_tick(&mut self.engine.cfg, now);
        // Background maintenance jobs report progress once a second.
        for ev in self.admin.progress_events() {
            self.engine.network_event(ev);
        }
        // Confirms first: the tick fails undeliverable requests at once.
        for (id, t) in self.engine.transport.take_done() {
            self.engine.on_outcome(id, t.into(), now);
        }
        self.engine.tick(now);
        self.settled();
        self.with_automations(|a, w, clk| a.tick(w, clk));
        if let Some(c) = self.catalog.as_mut()
            && self.registered
        {
            c.tick(&self.engine);
        }
    }

    /// Catalog events to subscribers, changed catalog settings to the store.
    #[inline(never)]
    fn pump_catalog(&mut self) {
        let Some(c) = self.catalog.as_mut() else { return };
        let evs = std::mem::take(&mut c.events);
        if c.config_dirty {
            c.config_dirty = false;
            let v = c.cfg.to_json().to_string();
            if let Err(e) = kv::with(&*self.records, |s| s.put(CONFIG_KEY, v.as_bytes())) {
                warn!("catalog: saving the settings: {e}");
            }
        }
        for e in evs {
            for (sid, frame) in self.api.publish("catalog", e) {
                self.send(sid, &frame);
            }
        }
    }

    /// Coordinator IEEE, once per process, when the first session is up
    /// (the backend has registered the hub endpoint by then).
    fn register(&mut self, info: &NcpInfo) {
        if self.registered {
            return;
        }
        if let Some(ieee) = info.ieee {
            self.engine.cfg.coordinator_ieee = u64::from_le_bytes(ieee);
            self.api.set_info("coordinator_ieee", format!("{:016x}", self.engine.cfg.coordinator_ieee).into());
            self.api.set_info("ncp_board", info.board.clone().into());
            self.api.set_info("ncp_product", info.product.clone().into());
        }
        self.registered = true;
        self.engine.cfg.hub_endpoint = self.cfg.endpoint.ep;
        info!("app: hub endpoint {} registered, coordinator {:016X}", self.cfg.endpoint.ep, self.engine.cfg.coordinator_ieee);
        self.with_automations(|a, w, clk| a.started(w, clk));
    }

    /// Once per NCP session: the NCP's devices are read
    /// ([`Self::own_outcome`] imports them).
    fn import_ncp_devices(&mut self, cookie: Option<u32>) {
        let Some(cookie) = cookie else { return };
        if self.imported_in == Some(cookie) || self.import.is_some() || self.import_retry.is_some_and(|t| Instant::now() < t) {
            return;
        }
        match self.engine.transport.submit_own(Request::DeviceInventory) {
            Ok(id) => self.import = Some((id, cookie, self.import_gen)),
            Err(e) => {
                warn!("app: NCP device inventory: {e}");
                self.import_retry = Some(Instant::now() + Duration::from_secs(30));
            }
        }
    }

    /// The outcome of the app's own request (the device inventory): devices
    /// the NCP knows and the engine does not are added as if they had just
    /// joined (interview, then configure). A sleepy device answers when it
    /// next polls.
    fn own_outcome(&mut self, id: RequestId, o: Outcome, now: Millis) {
        let Some((_, cookie, generation)) = self.import.filter(|(r, ..)| *r == id) else { return };
        let Outcome::Done(t) = o else { return };
        self.import = None;
        match t {
            Terminal::Done(Reply::Inventory(inv)) => self.import_devices(&inv, cookie, generation, now),
            t => {
                match t {
                    Terminal::Refused(e) | Terminal::NotDelivered(e) | Terminal::Unknown(e) => {
                        warn!("app: NCP device inventory: {e}")
                    }
                    _ => warn!("app: NCP device inventory: no devices in the answer"),
                }
                self.import_retry = Some(Instant::now() + Duration::from_secs(30));
            }
        }
        let ready = self.engine.transport.status().session == CoordSession::Ready && self.records.verified();
        self.refresh(ready, now);
    }

    /// The unknown states of restored devices are read once (the registry
    /// keeps devices, not live states), after the import of the NCP's
    /// devices, whose interviews go first, as before.
    fn refresh(&mut self, ready: bool, now: Millis) {
        if self.registered && !self.refreshed && ready && self.import.is_none() {
            self.refreshed = true;
            let n = self.engine.refresh_unknown(now);
            if n > 0 {
                info!("app: reading {n} unknown states of restored devices");
            }
        }
    }

    fn import_devices(&mut self, inv: &hub_coord::Inventory, cookie: u32, generation: u32, now: Millis) {
        if generation == self.import_gen {
            self.imported_in = Some(cookie);
        }
        let mut added = 0;
        for d in &inv.devices {
            let Some(nwk) = d.nwk.filter(|n| *n < 0xFFF8) else { continue };
            if self.engine.devices().any(|x| x.ieee() == Some(d.ieee)) {
                continue;
            }
            self.engine.transport.nwk.insert(d.ieee, nwk);
            self.engine.on_device_joined(d.ieee, nwk, now);
            added += 1;
        }
        self.imported += added;
        if added > 0 || !inv.unsupported.is_empty() {
            info!(
                "app: imported {added} of {} NCP devices (not answered by the NCP: {:04X?})",
                inv.devices.len(),
                inv.unsupported
            );
        }
    }

    /// `permit_join` events whoever opened or closed joining (API, console,
    /// an NCP event) and on expiry: NCP images need not send
    /// `EVT_PERMIT_JOIN_STATE` (the zigbee-rs NCP does not), so the event
    /// follows the host core's remaining time.
    fn observe_permit(&mut self) {
        let left = self.engine.transport.permit_join_s();
        if permit_changed(self.permit_left, left) {
            self.engine.network_event(serde_json::json!({"topic": "permit_join", "remaining_s": left}));
        }
        self.permit_left = left;
    }

    fn after_input(&mut self) {
        let now = self.now();
        self.settled();
        self.observe_permit();
        if !self.outboxes.is_empty() {
            self.pump_outboxes(now);
        }
        if self.catalog.as_ref().is_some_and(|c| c.config_dirty || !c.events.is_empty()) {
            self.pump_catalog();
        }
        #[cfg(feature = "ota")]
        self.pump_ota(now);
        self.with_automations(|a, w, clk| a.resume(w, clk));
        // Automations act on the events; what their commands change follows
        // in the next round (a few rounds here, the rest at the next input
        // or tick).
        for _ in 0..4 {
            let events = self.engine.drain_events();
            if events.is_empty() {
                break;
            }
            self.registry.note(&events, now);
            self.last.note(&events, &self.engine);
            self.with_automations(|a, w, clk| a.observe(&events, w, clk));
            for (sid, frame) in self.api.pump_events(&mut self.engine, events) {
                self.send_owned(sid, frame);
            }
        }
        self.pump_automations();
        self.registry.flush(&*self.records, &self.engine, now, false);
        self.last.flush(&*self.records, &self.engine, now, false);
        let mut st = self.status.lock().unwrap();
        st.last_values = (self.last.restored, self.last.writes, self.last.bytes, self.last.errors);
        st.devices = self.engine.devices().count();
        st.sessions = self.sinks.len();
        st.restored = self.registry.restored;
        st.registry_writes = self.registry.writes;
        st.registry_errors = self.registry.errors;
        st.aps = self.engine.transport.counters;
        st.endpoint_registered = self.registered;
        st.imported = self.imported;
    }
}
