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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hub_devices::api::{ApiServer, SessionId};
use hub_devices::device::Millis;
use hub_devices::hub::{Hub as Engine, HubConfig};
use hub_devices::resolve::{Catalog, MemoryCatalog};
use hub_devices::zdef_adapter::ZdefSchemaDecoder;
use hub_ncp::store::EndpointRecord;
use hub_ncp::{Hub as Ncp, Message, SessionState};
use log::{info, warn};

use crate::convert::{EngineInput, engine_input};
use crate::registry::Registry;
use crate::transport::{ApsCounters, NcpTransport};

/// Where the API frames of one connection go. `false` = the connection is
/// gone (the session is dropped).
pub trait SessionSink: Send {
    /// Send one text frame.
    fn send(&mut self, text: &str) -> bool;
}

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

enum Input {
    Ncp(Message),
    Connect(Box<dyn SessionSink>, SyncSender<SessionId>),
    Text(SessionId, String),
    Disconnect(SessionId),
    Shutdown,
}

/// Handle to the running application. Cheap to clone.
#[derive(Clone)]
pub struct App {
    tx: Sender<Input>,
    status: Arc<Mutex<AppStatus>>,
    stopped: Arc<AtomicBool>,
}

fn wall_ms() -> Millis {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as Millis)
}

impl App {
    /// Start the application on a running host core.
    pub fn start(ncp: Ncp, cfg: AppConfig) -> App {
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
        thread::Builder::new()
            .name("hub-app".into())
            .stack_size(cfg.stack_size)
            .spawn(move || Runner::new(ncp, cfg, st).run(rx, stop))
            .expect("hub-app thread");
        App { tx, status, stopped }
    }

    /// A new API connection; the `auth_required` frame goes to `sink`.
    pub fn connect(&self, sink: Box<dyn SessionSink>) -> Option<SessionId> {
        let (rtx, rrx) = mpsc::sync_channel(1);
        self.tx.send(Input::Connect(sink, rtx)).ok()?;
        rrx.recv().ok()
    }

    /// A text frame from a connection.
    pub fn text(&self, s: SessionId, text: String) {
        let _ = self.tx.send(Input::Text(s, text));
    }

    /// A closed connection.
    pub fn disconnect(&self, s: SessionId) {
        let _ = self.tx.send(Input::Disconnect(s));
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
    status: Arc<Mutex<AppStatus>>,
    wall0: Millis,
    t0: Instant,
    registered: bool,
    /// NCP cookie of the session the last import ran in.
    imported_in: Option<u32>,
    import_retry: Option<Instant>,
    imported: usize,
}

impl Runner {
    fn new(ncp: Ncp, cfg: AppConfig, status: Arc<Mutex<AppStatus>>) -> Self {
        let catalog: Box<dyn Catalog> =
            Box::new(if cfg.builtin_catalog { builtin_catalog() } else { MemoryCatalog::default() });
        let hub_cfg = HubConfig { tz_offset_s: cfg.tz_offset_s, ..HubConfig::default() };
        let mut engine = Engine::new(NcpTransport::new(ncp.clone()), hub_cfg, catalog);
        engine.set_schema_decoder(Box::new(ZdefSchemaDecoder));
        let api = ApiServer::new(cfg.hub_name.clone(), cfg.api_token.clone());
        let mut r = Self {
            ncp,
            cfg,
            engine,
            api,
            sinks: BTreeMap::new(),
            registry: Registry::default(),
            status,
            wall0: wall_ms(),
            t0: Instant::now(),
            registered: false,
            imported_in: None,
            import_retry: None,
            imported: 0,
        };
        let now = r.now();
        let mut registry = std::mem::take(&mut r.registry);
        registry.load(&r.ncp, &mut r.engine, now);
        r.registry = registry;
        let known: Vec<(u64, u16)> = r.engine.devices().filter_map(|d| Some((d.ieee()?, d.nwk()?))).collect();
        r.engine.transport.nwk.extend(known);
        // Restored devices produce added events; nobody is subscribed yet,
        // and their snapshots are what the store holds.
        let _ = r.engine.drain_events();
        r
    }

    fn now(&self) -> Millis {
        self.wall0 + self.t0.elapsed().as_millis() as Millis
    }

    fn run(mut self, rx: Receiver<Input>, stopped: Arc<AtomicBool>) {
        let mut next_tick = Instant::now();
        loop {
            let wait = next_tick.saturating_duration_since(Instant::now());
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
        stopped.store(true, Ordering::SeqCst);
    }

    fn handle(&mut self, input: Input) {
        let now = self.now();
        match input {
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
                None => {}
            },
            Input::Connect(mut sink, reply) => {
                let (sid, hello) = self.api.connect();
                if sink.send(&hello) {
                    self.sinks.insert(sid, sink);
                } else {
                    self.api.disconnect(sid);
                }
                let _ = reply.send(sid);
            }
            Input::Text(sid, text) => {
                for frame in self.api.handle_text(&mut self.engine, sid, &text, now) {
                    self.send(sid, &frame);
                }
            }
            Input::Disconnect(sid) => {
                self.sinks.remove(&sid);
                self.api.disconnect(sid);
            }
            Input::Shutdown => {}
        }
    }

    fn send(&mut self, sid: SessionId, frame: &str) {
        if let Some(sink) = self.sinks.get_mut(&sid)
            && !sink.send(frame)
        {
            self.sinks.remove(&sid);
            self.api.disconnect(sid);
        }
    }

    fn tick(&mut self) {
        let now = self.now();
        if !self.registered && self.ncp.client().state() == SessionState::Ready {
            self.register();
        }
        if self.registered && self.cfg.import_ncp_devices {
            self.import_ncp_devices(now);
        }
        self.engine.tick(now);
        self.engine.transport.poll();
    }

    /// Hub endpoint and coordinator IEEE, once per process (the endpoint is
    /// replayed by the host core on every resume).
    fn register(&mut self) {
        if let Some(info) = self.ncp.client().session().and_then(|s| s.info.clone()) {
            self.engine.cfg.coordinator_ieee = u64::from_le_bytes(info.ieee_active);
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

    fn after_input(&mut self) {
        let now = self.now();
        let events = self.engine.drain_events();
        self.registry.note(&events, now);
        for (sid, frame) in self.api.pump_events(&mut self.engine, events) {
            self.send(sid, &frame);
        }
        self.registry.flush(&self.ncp, &self.engine, now, false);
        let mut st = self.status.lock().unwrap();
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
