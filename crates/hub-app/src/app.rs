//! The hub application: one thread owns the device engine, the API sessions
//! and the registry; inputs arrive over a channel.
//!
//! ```text
//!  hub-ncp events ─┐
//!  API connections ┼─> app thread: engine (hub-devices) ─> NcpTransport ─> hub-ncp
//!  1 s tick ───────┘        │  ApiServer (sessions, subscriptions)
//!                           └─ Registry ─> host store (hubdata)
//! ```

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hub_devices::api::{ApiExtension, ApiServer, SessionId};
use hub_devices::bankcat::StoreCatalog;
use hub_devices::device::Millis;
use hub_devices::hub::{Hub as Engine, HubConfig};
use hub_devices::resolve::{Catalog, MemoryCatalog};
use hub_devices::zdef_adapter::ZdefSchemaDecoder;
use hub_ncp::store::EndpointRecord;
use hub_ncp::{Hub as Ncp, Message, SessionState};
use log::{info, warn};

use crate::admin::{self, Admin, JobOut};
use crate::automations::{self, Automations, Clock, World};
use crate::catalog::data::SharedFlash;
use crate::catalog::service::{CONFIG_KEY, CatalogService, load_config};
use crate::catalog::store::CatalogStore;
use crate::catalog::web::SharedWeb;
use crate::convert::{EngineInput, engine_input};
use crate::platform::Platform;
use crate::registry::Registry;
use crate::transport::{ApsCounters, NcpTransport};

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
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            hub_name: "zigbee-rs-hub".into(),
            api_token: String::new(),
            tz_offset_s: 0,
            endpoint: EndpointRecord {
                ep: 1,
                profile: 0x0104,
                device_id: 0x0005,
                version: 0,
                // Basic, Identify, Time, OTA server.
                in_clusters: vec![0x0000, 0x0003, 0x000A, 0x0019],
                // Clients the hub drives: On/Off, Level, Scenes, Groups,
                // Color, IAS Zone (CIE), measurement reads.
                out_clusters: vec![0x0000, 0x0003, 0x0004, 0x0005, 0x0006, 0x0008, 0x0300, 0x0500, 0x0402, 0x0405],
            },
            builtin_catalog: true,
            stack_size: 96 * 1024,
            import_ncp_devices: true,
            platform: None,
            admin_stack: 16 * 1024,
        }
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
            Ok(d) => cat.insert(d),
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
    Ncp(Message),
    Connect(SessionId, Box<dyn SessionSink>),
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
}

fn wall_ms() -> Millis {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as Millis)
}

impl App {
    /// Start the application on a running host core.
    pub fn start(ncp: Ncp, cfg: AppConfig) -> App {
        Self::start_with(ncp, cfg, None)
    }

    /// [`App::start`] with the catalog store in `hubdata`.
    pub fn start_with(ncp: Ncp, cfg: AppConfig, catalog: Option<CatalogSetup>) -> App {
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(AppStatus::default()));
        let stopped = Arc::new(AtomicBool::new(false));
        let events = ncp.subscribe();
        let fwd = tx.clone();
        thread::Builder::new()
            .name("hub-app-ncp".into())
            .stack_size(4096)
            .spawn(move || {
                for m in events {
                    if fwd.send(Input::Ncp(m)).is_err() {
                        break;
                    }
                }
            })
            .expect("hub-app forwarder");
        let st = status.clone();
        let stop = stopped.clone();
        let jobs = tx.clone();
        
        let web = SharedWeb::default();
        let web2 = web.clone();
        thread::Builder::new()
            .name("hub-app".into())
            .stack_size(cfg.stack_size)
            .spawn(move || Runner::new(ncp, cfg, st, jobs, catalog, web2).run(rx, stop))
            .expect("hub-app thread");
        App { tx, status, stopped, next_session: Arc::new(AtomicU32::new(1)), web }
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
    ncp: Ncp,
    cfg: AppConfig,
    engine: Engine<NcpTransport>,
    api: ApiServer,
    sinks: BTreeMap<SessionId, Box<dyn SessionSink>>,
    registry: Registry,
    last: crate::laststate::LastValues,
    status: Arc<Mutex<AppStatus>>,
    wall0: Millis,
    t0: Instant,
    registered: bool,
    /// NCP cookie of the session the last import ran in.
    imported_in: Option<u32>,
    import_retry: Option<Instant>,
    imported: usize,
    /// Unknown states of restored mains devices have been read.
    refreshed: bool,
    /// Permit-join seconds left at the last look.
    permit_left: u32,
    /// Maintenance requests and their jobs.
    admin: Admin,
    /// Job results come back on the app's own channel.
    jobs: Sender<Input>,
    /// NCP cookie `hub/info` `maintenance` was computed for.
    maintenance_for: Option<u32>,
    /// The catalog store and its downloads.
    catalog: Option<CatalogService>,
    /// Local automations.
    automations: Automations,
    /// Last request ID of the automations' internal session.
    automation_requests: u64,
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
    engine: &'a mut Engine<NcpTransport>,
    api: &'a mut ApiServer,
    now: Millis,
    next_id: &'a mut u64,
}

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

    fn request(&mut self, req: &serde_json::Value) -> Result<serde_json::Value, hub_devices::hub::ApiError> {
        // A deferred result (a device command): sent.
        self.call(req.clone()).1.unwrap_or(Ok(serde_json::Value::Null))
    }
}

/// Opens the catalog store in `hubdata` and its service.
#[inline(never)]
fn open_catalog(ncp: &Ncp, setup: CatalogSetup, web: SharedWeb) -> Option<(StoreCatalog, CatalogService)> {
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
        let cfg = load_config(ncp.with_store(|s| s.kv_get(CONFIG_KEY).map(<[u8]>::to_vec)));
    let cat = StoreCatalog { bank, fetched: store.local.catalog(), ..Default::default() };
    Some((cat, CatalogService::new(setup.flash, store, cfg, web)))
}

/// Opened, closed (or expired), or re-opened since the last look; the
/// countdown itself is not an event.
fn permit_changed(before: u32, now: u32) -> bool {
    (before == 0) != (now == 0) || now > before
}

impl Runner {
    fn new(
        ncp: Ncp,
        cfg: AppConfig,
        status: Arc<Mutex<AppStatus>>,
        jobs: Sender<Input>,
                setup: Option<CatalogSetup>,
        web: SharedWeb,
    ) -> Self {
        let builtin = if cfg.builtin_catalog { builtin_catalog() } else { MemoryCatalog::default() };
        let (catalog, service): (Box<dyn Catalog>, Option<CatalogService>) =
            match setup.map(|s| open_catalog(&ncp, s, web)) {
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
        let mut engine = Engine::new(NcpTransport::new(ncp.clone()), hub_cfg, catalog);
        engine.set_schema_decoder(Box::new(ZdefSchemaDecoder));
        let api = ApiServer::new(cfg.hub_name.clone(), cfg.api_token.clone());
        let mut admin = Admin::new(cfg.platform.clone(), cfg.hub_name.clone());
        admin.job_stack = cfg.admin_stack;
        admin.start_worker();
        admin.load_clock(&ncp);
        let mut r = Self {
            ncp,
            cfg,
            engine,
            api,
            sinks: BTreeMap::new(),
            registry: Registry::default(),
            last: Default::default(),
            status,
            wall0: wall_ms(),
            t0: Instant::now(),
            registered: false,
            imported_in: None,
            import_retry: None,
            imported: 0,
            refreshed: false,
            permit_left: 0,
            admin,
            jobs,
            maintenance_for: None,
            catalog: service,
            automations: Automations::default(),
            automation_requests: 0,
        };
        let now = r.now();
        let mut registry = std::mem::take(&mut r.registry);
        registry.load(&r.ncp, &mut r.engine, now);
        r.registry = registry;
        // The clock first: kept times convert only once the time is known.
        r.admin.clock_tick(&mut r.engine.cfg, now);
        let mut last = std::mem::take(&mut r.last);
        last.load(&r.ncp, &mut r.engine, now);
        r.last = last;
        let records: Vec<(String, Vec<u8>)> = r
            .ncp
            .with_store(|s| s.kv_prefix(automations::PREFIX).map(|(k, v)| (k.to_string(), v.to_vec())).collect());
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
        // Restored devices produce added events; nobody is subscribed yet,
        // and their snapshots are what the store holds.
        let _ = r.engine.drain_events();
        r.automations.prime(&AppWorld { engine: &mut r.engine, api: &mut r.api, now, next_id: &mut 0 });
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
        let mut w =
            AppWorld { engine: &mut self.engine, api: &mut self.api, now: clk.now_ms, next_id: &mut self.automation_requests };
        f(&mut self.automations, &mut w, &clk)
    }

    /// Changed rules to the store, automation events to subscribers.
    fn pump_automations(&mut self) {
        let dirty = std::mem::take(&mut self.automations.dirty);
        if !dirty.is_empty() {
            let a = &self.automations;
            self.ncp.with_store(|s| {
                for id in &dirty {
                    let k = format!("{}{id}", automations::PREFIX);
                    let r = match a.rule(id) {
                        Some(rule) => s.kv_put(&k, rule.src.as_bytes()),
                        None => s.kv_delete(&k),
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
            let wait = due.map_or(next_tick, |d| d.min(next_tick)).saturating_duration_since(Instant::now());
            match rx.recv_timeout(wait) {
                Ok(Input::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(input) => self.handle(input),
                Err(RecvTimeoutError::Timeout) => {}
            }
            if Instant::now() >= next_tick {
                next_tick += Duration::from_secs(1);
                self.tick();
            }
            self.after_input();
        }
        let now = self.now();
        self.registry.flush(&self.ncp, &self.engine, now, true);
        self.last.flush(&self.ncp, &self.engine, now, true);
        stopped.store(true, Ordering::SeqCst);
    }

    fn handle(&mut self, input: Input) {
        let now = self.now();
        match input {
            Input::Ncp(m) if m.msg_id == crate::jobs::EVT_SCAN_PROGRESS => self.admin.ncp_progress(&m.payload),
            Input::Ncp(m) => match engine_input(&m) {
                Some(EngineInput::Indication(i)) => {
                    if let Some(ieee) = i.src_ieee {
                        self.engine.transport.nwk.insert(ieee, i.src_nwk);
                    }
                    self.engine.on_aps_indication(i, now);
                }
                Some(EngineInput::Joined { ieee, nwk }) => {
                    self.engine.transport.nwk.insert(ieee, nwk);
                    self.engine.on_device_joined(ieee, nwk, now);
                }
                Some(EngineInput::Gp(e)) => self.engine.on_gp_event(e, now),
                // The host core's remaining time decides (see `observe_permit`).
                Some(EngineInput::Network(body)) if body["topic"] == "permit_join" => {}
                Some(EngineInput::Network(body)) => self.engine.network_event(body),
                None => {}
            },
            Input::Connect(sid, mut sink) => {
                let hello = self.api.connect_as(sid);
                if sink.send(&hello) {
                    self.sinks.insert(sid, sink);
                } else {
                    self.api.disconnect(sid);
                }
            }
            Input::Text(sid, text) => {
                if self.api.is_authed(sid)
                    && let Some(head) = admin::head(&text)
                {
                    if matches!(head.ty.as_str(), "hub/reboot" | "hub/mode/set") {
                        self.registry.flush(&self.ncp, &self.engine, now, true);
                        self.last.flush(&self.ncp, &self.engine, now, true);
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
                        self.with_automations(|a, w, clk| a.request(kind, &v, w, clk))
                    }
                {
                    let m = match r {
                        Ok(v) => hub_devices::api::ServerMessage::ok(id, v),
                        Err(e) => hub_devices::api::ServerMessage::err(id, e),
                    };
                    self.send(sid, &m.to_text());
                    self.pump_automations();
                    return;
                }
                let ext = self.catalog.as_mut().map(|c| c as &mut dyn ApiExtension<NcpTransport>);
                for frame in self.api.handle_text_ext(&mut self.engine, sid, &text, now, ext) {
                    self.send(sid, &frame);
                }
                if text.contains("\"definitions/upload\"") || text.contains("\"definitions/delete\"") {
                    self.registry.definitions_changed(now);
                }
            }
            Input::Disconnect(sid) => {
                self.sinks.remove(&sid);
                self.api.disconnect(sid);
            }
            Input::Admin(out) => match out {
                JobOut::Reply(sid, frame) => {
                    if let Some(sink) = self.sinks.get_mut(&sid)
                        && !sink.send_owned(frame)
                    {
                        self.sinks.remove(&sid);
                        self.api.disconnect(sid);
                    }
                }
                JobOut::Event(body) => self.engine.network_event(body),
                JobOut::Reimport => {
                    self.imported_in = None;
                    self.import_retry = None;
                }
            },
            Input::Shutdown => {}
        }
    }

    /// A maintenance request (`admin`), with what it may need from here.
    fn admin_request(&mut self, sid: SessionId, head: &admin::Head, now: Millis) -> Vec<String> {
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
        let ctx = admin::Ctx {
            ncp: &self.ncp,
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
        if let Some(sink) = self.sinks.get_mut(&sid)
            && !sink.send(frame)
        {
            self.sinks.remove(&sid);
            self.api.disconnect(sid);
        }
    }

    fn tick(&mut self) {
        let now = self.now();
        let probe = self.t0.elapsed().as_secs().is_multiple_of(KEEPALIVE_S);
        let dead: Vec<SessionId> = self
            .sinks
            .iter_mut()
            .filter_map(|(sid, s)| (!s.alive() || (probe && !s.keepalive())).then_some(*sid))
            .collect();
        for sid in dead {
            self.sinks.remove(&sid);
            self.api.disconnect(sid);
        }
        if !self.registered && self.ncp.client().state() == SessionState::Ready {
            self.register();
            if self.registered {
                self.with_automations(|a, w, clk| a.started(w, clk));
            }
        }
        let cookie = self.ncp.client().session().map(|s| s.link.peer_cookie);
        if self.ncp.client().state() == SessionState::Ready && self.maintenance_for != cookie {
            self.maintenance_for = cookie;
            self.api.set_info("maintenance", self.admin.features(&self.ncp).into());
        }
        if self.registered && self.cfg.import_ncp_devices {
            self.import_ncp_devices(now);
        }
        // The registry keeps devices, not live states.
        if self.registered && !self.refreshed && self.ncp.client().state() == SessionState::Ready {
            self.refreshed = true;
            let n = self.engine.refresh_unknown(now);
            if n > 0 {
                info!("app: reading {n} unknown states of restored devices");
            }
        }
        self.admin.clock_tick(&mut self.engine.cfg, now);
        // Background maintenance jobs report progress once a second.
        for ev in self.admin.progress_events() {
            self.engine.network_event(ev);
        }
        // Confirms first: the tick fails undeliverable requests at once.
        self.engine.transport.poll();
        self.engine.tick(now);
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
            if let Err(e) = self.ncp.with_store(|s| s.kv_put(CONFIG_KEY, v.as_bytes())) {
                warn!("catalog: saving the settings: {e}");
            }
        }
        for e in evs {
            for (sid, frame) in self.api.publish("catalog", e) {
                self.send(sid, &frame);
            }
        }
    }

    /// Hub endpoint and coordinator IEEE, once per process (the endpoint is
    /// replayed by the host core on every resume).
    fn register(&mut self) {
        if let Some(info) = self.ncp.client().session().and_then(|s| s.info.clone()) {
            self.engine.cfg.coordinator_ieee = u64::from_le_bytes(info.ieee_active);
            self.api.set_info("coordinator_ieee", format!("{:016x}", self.engine.cfg.coordinator_ieee).into());
            self.api.set_info("ncp_board", info.board.clone().into());
            self.api.set_info("ncp_product", info.product.clone().into());
        }
        let known = self.ncp.store_state().endpoints.get(&self.cfg.endpoint.ep) == Some(&self.cfg.endpoint);
        let result = if known { Ok(()) } else { self.ncp.add_endpoint(self.cfg.endpoint.clone()) };
        match result {
            Ok(()) => {
                self.registered = true;
                self.engine.cfg.hub_endpoint = self.cfg.endpoint.ep;
                info!("app: hub endpoint {} registered, coordinator {:016X}", self.cfg.endpoint.ep, self.engine.cfg.coordinator_ieee);
            }
            Err(e) => warn!("app: NWK_ADD_ENDPOINT: {e}"),
        }
    }

    /// Once per NCP session: devices the NCP knows and the engine does not
    /// are added as if they had just joined (interview, then configure). A
    /// sleepy device answers when it next polls.
    fn import_ncp_devices(&mut self, now: Millis) {
        let Some(cookie) = self.ncp.client().session().map(|s| s.link.peer_cookie) else { return };
        if self.ncp.client().state() != SessionState::Ready
            || self.imported_in == Some(cookie)
            || self.import_retry.is_some_and(|t| Instant::now() < t)
        {
            return;
        }
        let inv = match self.ncp.ncp_devices() {
            Ok(inv) => inv,
            Err(e) => {
                warn!("app: NCP device inventory: {e}");
                self.import_retry = Some(Instant::now() + Duration::from_secs(30));
                return;
            }
        };
        self.imported_in = Some(cookie);
        let mut added = 0;
        for d in inv.devices.iter().filter(|d| d.nwk < 0xFFF8) {
            let ieee = u64::from_le_bytes(d.ieee);
            if self.engine.devices().any(|x| x.ieee() == Some(ieee)) {
                continue;
            }
            self.engine.transport.nwk.insert(ieee, d.nwk);
            self.engine.on_device_joined(ieee, d.nwk, now);
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
        let left = self.ncp.permit_join_remaining();
        if permit_changed(self.permit_left, left) {
            self.engine.network_event(serde_json::json!({"topic": "permit_join", "remaining_s": left}));
        }
        self.permit_left = left;
    }

    fn after_input(&mut self) {
        self.observe_permit();
        let now = self.now();
        if self.catalog.as_ref().is_some_and(|c| c.config_dirty || !c.events.is_empty()) {
            self.pump_catalog();
        }
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
                self.send(sid, &frame);
            }
        }
        self.pump_automations();
        self.registry.flush(&self.ncp, &self.engine, now, false);
        self.last.flush(&self.ncp, &self.engine, now, false);
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
