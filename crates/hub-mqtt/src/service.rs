//! The bridge at run time: one thread owns the [`Bridge`], a hub API
//! session (trusted, from [`App::connect_trusted`]) and an MQTT client
//! behind [`Transport`] (esp-mqtt on the S3, [`crate::tcp`] on a desktop).
//!
//! - Inputs arrive on one bounded channel: API frames, client events,
//!   settings. A full channel drops the API session (its sink reports the
//!   connection gone) and the bridge opens a new one, whose snapshot
//!   replaces the model: memory stays bounded and nothing is half-applied.
//! - The client is (re)connected with exponential backoff (1 s … 60 s) once
//!   the first snapshot arrived, with the LWT `bridge/state` = `offline`.
//! - `mqtt/config/get`, `mqtt/config/set` and `mqtt/status` are served on
//!   the app thread by [`MqttHook`]; settings changes restart the thread.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::obj::Obj;
use hub_app::{App, RequestHook, SessionSink};
use hub_devices::hub::ApiError;
use log::{info, warn};
use serde_json::Value as J;

use crate::bridge::{Bridge, Limits, Msg, Stats};
use crate::config::{MqttConfig, SettingsStore};

/// What the client reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Connected (CONNACK accepted).
    Connected,
    /// The connection ended or failed.
    Disconnected(String),
    /// A message on a subscribed topic.
    Message(String, Vec<u8>),
}

/// Event delivery from the client's own thread or task.
pub type EventFn = Arc<dyn Fn(Event) + Send + Sync>;

/// Connection parameters.
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    /// Broker host.
    pub host: String,
    /// Broker port.
    pub port: u16,
    /// Client ID.
    pub client_id: String,
    /// User name.
    pub username: Option<String>,
    /// Password.
    pub password: Option<String>,
    /// Keepalive, s.
    pub keepalive_s: u16,
    /// Last will.
    pub will: Msg,
}

/// An MQTT client.
pub trait Transport: Send {
    /// Starts connecting; [`Event::Connected`] or [`Event::Disconnected`]
    /// follows through `events`. The client does not reconnect by itself.
    fn connect(&mut self, opts: &ConnectOptions, events: EventFn) -> Result<(), String>;
    /// Closes the connection (no will: a clean DISCONNECT).
    fn disconnect(&mut self);
    /// Publishes (may block for the network timeout).
    fn publish(&mut self, m: &Msg) -> Result<(), String>;
    /// Subscribes (QoS 0).
    fn subscribe(&mut self, filters: &[String]) -> Result<(), String>;
}

/// Makes a client for each connection.
pub type TransportFactory = Arc<dyn Fn() -> Box<dyn Transport> + Send + Sync>;

/// Platform facts for the service.
#[derive(Clone)]
pub struct ServiceConfig {
    /// Stack of the bridge thread.
    pub stack_size: usize,
    /// Queue and `bridge/devices` limits.
    pub limits: Limits,
    /// Seed for the default client ID before the coordinator IEEE is known.
    pub client_suffix: String,
    /// The least free stack of the calling thread so far, bytes (the
    /// platform's high-water mark; `mqtt/status` reports it).
    pub stack_free: Option<fn() -> usize>,
    /// Free heap, bytes: the bridge waits (state `low_memory`, checked
    /// every 30 s) while it is below `min_free` (boards without PSRAM).
    pub heap_free: Option<fn() -> usize>,
    /// See `heap_free`.
    pub min_free: usize,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            stack_size: 24 * 1024,
            limits: Limits::default(),
            client_suffix: "000000".into(),
            stack_free: None,
            heap_free: None,
            min_free: 0,
        }
    }
}

/// `mqtt/status`.
#[derive(Debug, Clone, Default)]
pub struct Status {
    /// `disabled`, `low_memory`, `waiting_for_hub`, `connecting`,
    /// `connected`, `retrying`.
    pub state: &'static str,
    /// The last connection error.
    pub last_error: Option<String>,
    /// Successful connects since start.
    pub connects: u32,
    /// Seconds until the next attempt (`retrying`).
    pub retry_in_s: Option<u64>,
    /// Counters.
    pub stats: Stats,
    /// Devices bridged.
    pub devices: usize,
    /// Bytes queued.
    pub queued_bytes: usize,
    /// API sessions opened again after an overflow.
    pub api_resyncs: u32,
    /// The bridge thread's least free stack so far, bytes.
    pub stack_min_free: Option<usize>,
}

impl Status {
    fn to_json(&self) -> J {
        Obj::new()
            .with("state", self.state)
            .with("last_error", self.last_error.clone())
            .with("connects", self.connects)
            .with("retry_in_s", self.retry_in_s)
            .with("published", self.stats.published)
            .with("received", self.stats.received)
            .with("dropped", self.stats.dropped)
            .with("errors", self.stats.errors)
            .with("devices", self.devices)
            .with("queued_bytes", self.queued_bytes)
            .with("api_resyncs", self.api_resyncs)
            .with("stack_min_free", self.stack_min_free)
            .with("tls", false)
            .j()
    }
}

enum In {
    Api(u32, String),
    Mqtt(u32, Event),
    Stop,
}

struct Sink {
    tx: SyncSender<In>,
    epoch: u32,
    lost: Arc<AtomicBool>,
}

impl SessionSink for Sink {
    fn send(&mut self, text: &str) -> bool {
        self.send_owned(text.to_string())
    }
    fn send_owned(&mut self, text: String) -> bool {
        match self.tx.try_send(In::Api(self.epoch, text)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.lost.store(true, Ordering::SeqCst);
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
    fn alive(&self) -> bool {
        !self.lost.load(Ordering::SeqCst)
    }
}

struct Shared {
    cfg: Mutex<MqttConfig>,
    status: Mutex<Status>,
    store: Arc<dyn SettingsStore>,
    ctl: Mutex<Option<(SyncSender<In>, thread::JoinHandle<()>)>>,
    app: App,
    transport: TransportFactory,
    svc: ServiceConfig,
}

/// The running service (cheap to clone).
#[derive(Clone)]
pub struct MqttService {
    shared: Arc<Shared>,
}

/// Inputs queued at most (API frames, client events).
const CHANNEL: usize = 64;

impl MqttService {
    /// Loads the settings, starts the bridge if enabled and registers the
    /// `mqtt/*` requests with the app.
    pub fn start(
        app: &App,
        store: Arc<dyn SettingsStore>,
        transport: TransportFactory,
        svc: ServiceConfig,
    ) -> Self {
        let cfg = MqttConfig::from_store(store.load().as_deref());
        let shared = Arc::new(Shared {
            cfg: Mutex::new(cfg.clone()),
            status: Mutex::new(Status {
                state: "disabled",
                ..Default::default()
            }),
            store,
            ctl: Mutex::new(None),
            app: app.clone(),
            transport,
            svc,
        });
        let s = Self { shared };
        s.apply(cfg);
        app.add_hook(Box::new(MqttHook { svc: s.clone() }));
        s
    }

    /// The settings.
    pub fn config(&self) -> MqttConfig {
        self.shared.cfg.lock().unwrap().clone()
    }

    /// The status.
    pub fn status(&self) -> Status {
        self.shared.status.lock().unwrap().clone()
    }

    /// Validates, stores and applies settings fields.
    pub fn set(&self, fields: &serde_json::Map<String, J>) -> Result<MqttConfig, String> {
        let mut c = self.config();
        c.apply(fields, true)?;
        self.shared.store.save(&c.to_store())?;
        *self.shared.cfg.lock().unwrap() = c.clone();
        self.apply(c.clone());
        Ok(c)
    }

    fn apply(&self, cfg: MqttConfig) {
        let mut ctl = self.shared.ctl.lock().unwrap();
        // Any change restarts the bridge: topics, discovery and the
        // connection all depend on the settings.
        if let Some((tx, h)) = ctl.take() {
            let _ = tx.send(In::Stop);
            let _ = h.join();
        }
        if !cfg.enabled {
            self.shared.status.lock().unwrap().state = "disabled";
            return;
        }
        let (tx, rx) = sync_channel(CHANNEL);
        let sh = self.shared.clone();
        let tx2 = tx.clone();
        let spawned = thread::Builder::new()
            .name("hub-mqtt".into())
            .stack_size(self.shared.svc.stack_size)
            .spawn(move || run(sh, cfg, tx2, rx));
        match spawned {
            Ok(h) => *ctl = Some((tx, h)),
            Err(e) => {
                warn!("mqtt: bridge thread: {e}");
                let mut st = self.shared.status.lock().unwrap();
                st.state = "disabled";
                st.last_error = Some(format!("thread: {e}"));
            }
        }
    }

    /// Stops the bridge (publishes `offline` first).
    pub fn stop(&self) {
        if let Some((tx, h)) = self.shared.ctl.lock().unwrap().take() {
            let _ = tx.send(In::Stop);
            let _ = h.join();
        }
    }
}

/// `mqtt/*` on the app thread.
struct MqttHook {
    svc: MqttService,
}

impl RequestHook for MqttHook {
    fn prefix(&self) -> &'static str {
        "mqtt/"
    }
    fn feature(&self) -> Option<&'static str> {
        Some("mqtt")
    }
    fn request(&mut self, ty: &str, req: &J) -> Result<J, ApiError> {
        match ty {
            "mqtt/config/get" => Ok(self.svc.config().to_api()),
            "mqtt/config/set" => {
                let fields = req
                    .get("config")
                    .or(Some(req))
                    .and_then(J::as_object)
                    .cloned()
                    .unwrap_or_default();
                let fields: serde_json::Map<String, J> = fields
                    .into_iter()
                    .filter(|(k, _)| k != "id" && k != "type")
                    .collect();
                self.svc
                    .set(&fields)
                    .map(|c| c.to_api())
                    .map_err(|e| ApiError::new("invalid_value", e))
            }
            "mqtt/status" => Ok(self.svc.status().to_json()),
            other => Err(ApiError::new(
                "not_found",
                format!("unknown request {other}"),
            )),
        }
    }
}

struct Runner {
    sh: Arc<Shared>,
    cfg: MqttConfig,
    tx: SyncSender<In>,
    bridge: Bridge,
    client: Option<Box<dyn Transport>>,
    connected: bool,
    connecting_since: Option<Instant>,
    mqtt_gen: u32,
    /// Connection events the channel had no room for (1 connected, 2 lost).
    conn_flag: Arc<AtomicU8>,
    api_gen: u32,
    sid: Option<u64>,
    lost: Arc<AtomicBool>,
    backoff: Duration,
    next_attempt: Instant,
    t0: Instant,
    connects: u32,
    resyncs: u32,
    last_error: Option<String>,
}

const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

fn run(sh: Arc<Shared>, cfg: MqttConfig, tx: SyncSender<In>, rx: Receiver<In>) {
    let bridge = Bridge::new(cfg.clone(), sh.svc.limits);
    let mut r = Runner {
        sh,
        cfg,
        tx,
        bridge,
        client: None,
        connected: false,
        connecting_since: None,
        mqtt_gen: 0,
        conn_flag: Arc::new(AtomicU8::new(0)),
        api_gen: 0,
        sid: None,
        lost: Arc::new(AtomicBool::new(false)),
        backoff: BACKOFF_MIN,
        next_attempt: Instant::now(),
        t0: Instant::now(),
        connects: 0,
        resyncs: 0,
        last_error: None,
    };
    info!(
        "mqtt: bridge to {}:{} (base topic {}, discovery {})",
        r.cfg.host, r.cfg.port, r.cfg.base_topic, r.cfg.discovery
    );
    // Not on a heap the rest of the hub needs.
    if let Some(free) = r.sh.svc.heap_free {
        loop {
            let f = free();
            if f >= r.sh.svc.min_free {
                break;
            }
            {
                let mut st = r.sh.status.lock().unwrap();
                st.state = "low_memory";
                st.last_error = Some(format!(
                    "{} KB free heap, {} KB needed",
                    f / 1024,
                    r.sh.svc.min_free / 1024
                ));
            }
            match rx.recv_timeout(Duration::from_secs(30)) {
                Ok(In::Stop) | Err(RecvTimeoutError::Disconnected) => {
                    r.sh.status.lock().unwrap().state = "disabled";
                    return;
                }
                _ => {}
            }
        }
    }
    r.open_api();
    let mut busy = false;
    loop {
        let wait = if busy {
            Duration::ZERO
        } else {
            Duration::from_millis(250)
        };
        match rx.recv_timeout(wait) {
            Ok(In::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(i) => r.input(i),
            Err(RecvTimeoutError::Timeout) => {}
        }
        // Everything already queued before more output.
        while let Ok(i) = rx.try_recv() {
            if matches!(i, In::Stop) {
                r.shutdown();
                return;
            }
            r.input(i);
        }
        busy = r.step();
    }
    r.shutdown();
}

impl Runner {
    fn open_api(&mut self) {
        if let Some(sid) = self.sid.take() {
            self.sh.app.disconnect(sid);
        }
        self.api_gen = self.api_gen.wrapping_add(1);
        self.lost = Arc::new(AtomicBool::new(false));
        self.bridge.api_reset();
        let sink = Sink {
            tx: self.tx.clone(),
            epoch: self.api_gen,
            lost: self.lost.clone(),
        };
        self.sid = self.sh.app.connect_trusted(Box::new(sink));
    }

    fn input(&mut self, i: In) {
        match i {
            In::Api(g, text) if g == self.api_gen => self.bridge.on_api(&text),
            In::Api(..) => {}
            In::Mqtt(g, ev) if g == self.mqtt_gen => match ev {
                Event::Connected => {
                    self.connecting_since = None;
                    let subs = self.bridge.subscriptions();
                    let r = self
                        .client
                        .as_mut()
                        .map_or(Err("no client".into()), |c| c.subscribe(&subs));
                    match r {
                        Ok(()) => {
                            self.connected = true;
                            self.connects += 1;
                            self.backoff = BACKOFF_MIN;
                            self.last_error = None;
                            self.bridge.on_connected();
                            info!("mqtt: connected to {}:{}", self.cfg.host, self.cfg.port);
                        }
                        Err(e) => self.failed(format!("subscribe: {e}")),
                    }
                }
                Event::Disconnected(e) => self.failed(e),
                Event::Message(t, p) => self.bridge.on_message(&t, &p),
            },
            In::Mqtt(..) => {}
            In::Stop => {}
        }
    }

    fn failed(&mut self, e: String) {
        if self.connected {
            warn!("mqtt: connection lost: {e}");
        } else {
            warn!("mqtt: {}:{}: {e}", self.cfg.host, self.cfg.port);
        }
        self.last_error = Some(e);
        self.drop_client();
        self.next_attempt = Instant::now() + self.backoff;
        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
    }

    fn drop_client(&mut self) {
        if let Some(mut c) = self.client.take() {
            c.disconnect();
        }
        self.mqtt_gen = self.mqtt_gen.wrapping_add(1);
        self.connected = false;
        self.connecting_since = None;
        self.bridge.on_disconnected();
    }

    fn client_id(&self) -> String {
        if !self.cfg.client_id.is_empty() {
            return self.cfg.client_id.clone();
        }
        let tail = match self.bridge.coordinator() {
            Some(c) if c.len() >= 6 => c[c.len() - 6..].to_string(),
            _ => self.sh.svc.client_suffix.clone(),
        };
        format!("zigbee-rs-hub-{tail}")
    }

    fn connect(&mut self) {
        let mut c = (self.sh.transport)();
        self.mqtt_gen = self.mqtt_gen.wrapping_add(1);
        let epoch = self.mqtt_gen;
        let tx = self.tx.clone();
        let flag = Arc::new(AtomicU8::new(0));
        self.conn_flag = flag.clone();
        // Never blocks the client's task (esp-mqtt's, while this thread may
        // be waiting for it to stop): messages are dropped when the bridge
        // is behind, connection events fall back to a flag.
        let events: EventFn = Arc::new(move |ev| {
            let fallback = match &ev {
                Event::Connected => 1,
                Event::Disconnected(_) => 2,
                Event::Message(..) => 0,
            };
            if tx.try_send(In::Mqtt(epoch, ev)).is_err() && fallback != 0 {
                flag.store(fallback, Ordering::SeqCst);
            }
        });
        let opts = ConnectOptions {
            host: self.cfg.host.clone(),
            port: self.cfg.port,
            client_id: self.client_id(),
            username: (!self.cfg.username.is_empty()).then(|| self.cfg.username.clone()),
            password: (!self.cfg.password.is_empty()).then(|| self.cfg.password.clone()),
            keepalive_s: self.cfg.keepalive_s,
            will: self.bridge.will(),
        };
        match c.connect(&opts, events) {
            Ok(()) => {
                self.client = Some(c);
                self.connecting_since = Some(Instant::now());
            }
            Err(e) => self.failed(e),
        }
    }

    /// One round of work; `true` when more output is waiting.
    fn step(&mut self) -> bool {
        match self.conn_flag.swap(0, Ordering::SeqCst) {
            1 => self.input(In::Mqtt(self.mqtt_gen, Event::Connected)),
            2 => self.input(In::Mqtt(
                self.mqtt_gen,
                Event::Disconnected("connection lost".into()),
            )),
            _ => {}
        }
        if self.lost.load(Ordering::SeqCst) {
            warn!("mqtt: the bridge fell behind the hub API; resynchronising");
            self.resyncs += 1;
            self.open_api();
        }
        for req in self.bridge.take_api() {
            if let Some(sid) = self.sid {
                self.sh.app.text(sid, req);
            }
        }
        self.bridge.tick(self.t0.elapsed().as_millis() as u64);
        let now = Instant::now();
        if self.client.is_none() && self.bridge.ready() && now >= self.next_attempt {
            self.connect();
        }
        if !self.connected
            && self
                .connecting_since
                .is_some_and(|t| t.elapsed() > CONNECT_TIMEOUT)
        {
            self.failed("connect timed out".into());
        }
        let mut busy = false;
        if self.connected {
            for n in 0.. {
                if n == 32 {
                    busy = true;
                    break;
                }
                let Some(m) = self.bridge.next_msg() else {
                    break;
                };
                let Some(c) = self.client.as_mut() else { break };
                match c.publish(&m) {
                    Ok(()) => self.bridge.stats.published += 1,
                    Err(e) => {
                        self.bridge.unsend(m);
                        self.failed(format!("publish: {e}"));
                        break;
                    }
                }
            }
        }
        let mut st = self.sh.status.lock().unwrap();
        st.state = match (self.connected, self.client.is_some(), self.bridge.ready()) {
            (true, _, _) => "connected",
            (_, true, _) => "connecting",
            (_, _, false) => "waiting_for_hub",
            _ => "retrying",
        };
        st.retry_in_s = (st.state == "retrying")
            .then(|| self.next_attempt.saturating_duration_since(now).as_secs());
        st.last_error = self.last_error.clone();
        st.connects = self.connects;
        st.stats = self.bridge.stats.clone();
        st.devices = self.bridge.devices();
        st.queued_bytes = self.bridge.queued_bytes();
        st.api_resyncs = self.resyncs;
        st.stack_min_free = self.sh.svc.stack_free.map(|f| f());
        busy
    }

    fn shutdown(&mut self) {
        if self.connected
            && let Some(c) = self.client.as_mut()
        {
            let _ = c.publish(&self.bridge.offline());
        }
        if let Some(mut c) = self.client.take() {
            c.disconnect();
        }
        if let Some(sid) = self.sid.take() {
            self.sh.app.disconnect(sid);
        }
        self.sh.status.lock().unwrap().state = "disabled";
        info!("mqtt: bridge stopped");
    }
}
