//! The bridge: hub API frames and MQTT messages in, MQTT publications and
//! hub API requests out. No I/O and no threads ([`crate::service`] runs it),
//! so the topic and payload behaviour is tested on the host.
//!
//! Outgoing messages are produced lazily: device states are marked dirty
//! and rendered when the client can take them, bulky publications
//! (discovery configs, `bridge/devices`) are jobs rendered one device at a
//! time. The few queued messages (responses, events, actions) are bounded
//! in bytes; a full queue drops the newest non-essential message, so a slow
//! broker never grows the heap.

use std::collections::VecDeque;

use crate::obj::Obj;
use log::{info, warn};
use serde_json::{Map, Value as J};

use crate::config::MqttConfig;
use crate::discovery;
use crate::model::{Device, Entity, Model, State, array_items, object_text};
use crate::z2m::{self, Action, Kind, Layout, Names};

/// One publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Msg {
    /// Topic.
    pub topic: String,
    /// Payload.
    pub payload: Vec<u8>,
    /// QoS 0 or 1.
    pub qos: u8,
    /// Retained.
    pub retain: bool,
}

impl Msg {
    fn json(topic: String, v: &J, qos: u8, retain: bool) -> Self {
        Self {
            topic,
            payload: v.to_string().into_bytes(),
            qos,
            retain,
        }
    }

    /// An empty retained message (clears a retained topic).
    pub fn clear(topic: String) -> Self {
        Self {
            topic,
            payload: Vec::new(),
            qos: 1,
            retain: true,
        }
    }

    fn size(&self) -> usize {
        self.topic.len() + self.payload.len() + 32
    }
}

/// Counters for `mqtt/status`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    /// Messages handed to the client.
    pub published: u64,
    /// Messages dropped (queue full, too large).
    pub dropped: u64,
    /// Messages received.
    pub received: u64,
    /// `/set` and `/get` payloads that failed (unknown device or property,
    /// a command error).
    pub errors: u64,
}

#[derive(Debug)]
enum Job {
    Availability(String),
    State(String),
    Discovery(String),
    BridgeDiscovery,
}

#[derive(Debug)]
enum Pending {
    HubInfo,
    NetworkInfo,
    Subscribe,
    Command {
        friendly: String,
    },
    Response {
        req: String,
        data: J,
        transaction: Option<J>,
    },
}

/// Limits.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Bytes of queued messages.
    pub queue_bytes: usize,
    /// Largest `bridge/devices` with exposes (beyond it the exposes are
    /// left out).
    pub devices_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            queue_bytes: 16 * 1024,
            devices_bytes: 48 * 1024,
        }
    }
}

/// The bridge state.
pub struct Bridge {
    cfg: MqttConfig,
    limits: Limits,
    m: Model,
    names: Names,
    // Hub API.
    next_id: u64,
    api_out: Vec<String>,
    pending: Vec<(u64, Pending)>,
    ready: bool,
    version: String,
    coordinator: Option<String>,
    network: Map<String, J>,
    permit_left: u32,
    permit_end_ms: Option<u64>,
    last_joined: Option<String>,
    // MQTT.
    connected: bool,
    born: bool,
    queue: VecDeque<Msg>,
    queued: usize,
    jobs: VecDeque<Job>,
    /// Devices whose state is to be published, oldest first.
    dirty: Vec<String>,
    info_dirty: bool,
    devices_due: Option<u64>,
    /// Discovery config topics published per device.
    configs: Vec<(String, Vec<String>)>,
    now_ms: u64,
    next_network_poll: u64,
    /// Counters.
    pub stats: Stats,
}

/// `bridge/devices` is debounced by this much after a change.
const DEVICES_DEBOUNCE_MS: u64 = 1000;

impl Bridge {
    /// A bridge with `cfg`.
    pub fn new(cfg: MqttConfig, limits: Limits) -> Self {
        Self {
            cfg,
            limits,
            m: Model::default(),
            names: Names::default(),
            next_id: 1,
            api_out: Vec::new(),
            pending: Vec::new(),
            ready: false,
            version: String::new(),
            coordinator: None,
            network: Map::new(),
            permit_left: 0,
            permit_end_ms: None,
            last_joined: None,
            connected: false,
            born: false,
            queue: VecDeque::new(),
            queued: 0,
            jobs: VecDeque::new(),
            dirty: Vec::new(),
            info_dirty: false,
            devices_due: None,
            configs: Vec::new(),
            now_ms: 0,
            next_network_poll: 0,
            stats: Stats::default(),
        }
    }

    /// The settings.
    pub fn config(&self) -> &MqttConfig {
        &self.cfg
    }

    /// The model (tests, status).
    pub fn model(&self) -> &Model {
        &self.m
    }

    /// The friendly name of a device.
    pub fn friendly(&self, id: &str) -> Option<&str> {
        self.names.get(id)
    }

    /// Devices known.
    pub fn devices(&self) -> usize {
        self.m.devices.len()
    }

    /// Bytes queued.
    pub fn queued_bytes(&self) -> usize {
        self.queued
    }

    /// The coordinator IEEE (16 hex digits) once `hub/info` told it.
    pub fn coordinator(&self) -> Option<&str> {
        self.coordinator.as_deref()
    }

    /// The snapshot arrived (states are being published).
    pub fn ready(&self) -> bool {
        self.ready
    }

    // -- topics ------------------------------------------------------------

    fn t(&self, rest: &str) -> String {
        format!("{}/{rest}", self.cfg.base_topic)
    }

    /// The LWT: `<base>/bridge/state` `{"state":"offline"}`, retained, QoS 1.
    pub fn will(&self) -> Msg {
        Msg::json(
            self.t("bridge/state"),
            &Obj::new().with("state", "offline").j(),
            1,
            true,
        )
    }

    /// The `offline` message a clean stop publishes (same as the will).
    pub fn offline(&self) -> Msg {
        self.will()
    }

    /// Topic filters to subscribe after every connect.
    pub fn subscriptions(&self) -> Vec<String> {
        let b = &self.cfg.base_topic;
        let mut v: Vec<String> = [
            "+/set",
            "+/set/+",
            "+/get",
            "+/get/+",
            "+/+/set",
            "+/+/set/+",
            "+/+/get",
            "+/+/get/+",
            "bridge/request/#",
        ]
        .iter()
        .map(|s| format!("{b}/{s}"))
        .collect();
        if self.cfg.discovery {
            v.push(format!("{}/status", self.cfg.discovery_prefix));
        }
        v
    }

    // -- hub API -------------------------------------------------------------

    /// Requests for the hub API (taken by the service).
    pub fn take_api(&mut self) -> Vec<String> {
        std::mem::take(&mut self.api_out)
    }

    fn request(&mut self, mut body: J, p: Pending) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        body["id"] = id.into();
        self.api_out.push(body.to_string());
        self.pending.push((id, p));
        id
    }

    /// A new API session (start, or after the old one was lost): the model
    /// is rebuilt from the next snapshot.
    pub fn api_reset(&mut self) {
        self.ready = false;
        self.pending.clear();
        self.api_out.clear();
    }

    /// One frame from the hub API session.
    pub fn on_api(&mut self, text: &str) {
        if text.contains("\"event\":{\"topic\":\"snapshot\"") {
            self.on_snapshot(text);
            return;
        }
        let Ok(v) = serde_json::from_str::<J>(text) else {
            return;
        };
        match v["type"].as_str() {
            Some("auth_ok") => {
                if let Some(h) = v["hub"].as_str() {
                    self.version = hub_version(h);
                }
                self.request(Obj::new().with("type", "hub/info").j(), Pending::HubInfo);
                self.request(
                    Obj::new()
                        .with("type", "subscribe")
                        .with(
                            "topics",
                            J::from(vec!["devices", "entities", "events", "network"]),
                        )
                        .j(),
                    Pending::Subscribe,
                );
            }
            Some("auth_invalid") => warn!("mqtt: the hub API refused the bridge's session"),
            Some("result") => {
                let at = v["id"]
                    .as_u64()
                    .and_then(|id| self.pending.iter().position(|(i, _)| *i == id));
                if let Some(p) = at.map(|i| self.pending.swap_remove(i).1) {
                    self.on_result(p, &v);
                }
            }
            Some("event") => self.on_event(&v["event"]),
            _ => {}
        }
    }

    fn on_snapshot(&mut self, text: &str) {
        let mut m = Model::default();
        for t in array_items(text, "devices").unwrap_or_default() {
            if let Some(d) = serde_json::from_str::<J>(t)
                .ok()
                .as_ref()
                .and_then(Device::from_record)
            {
                m.devices.insert(d.id.clone(), Box::new(d));
            }
        }
        for t in array_items(text, "entities").unwrap_or_default() {
            if let Some(e) = serde_json::from_str::<J>(t)
                .ok()
                .as_ref()
                .and_then(Entity::from_record)
            {
                m.entities.insert(e.uid.clone(), Box::new(e));
            }
        }
        for t in array_items(text, "states").unwrap_or_default() {
            if let Some((uid, st, _)) = serde_json::from_str::<J>(t)
                .ok()
                .as_ref()
                .and_then(State::from_msg)
                && let Some(e) = m.entities.get_mut(&uid)
            {
                e.st = Some(st);
            }
        }
        if let Some(n) =
            object_text(text, "network").and_then(|t| serde_json::from_str::<J>(t).ok())
        {
            self.set_network(&n);
        }
        // Devices gone while the session was down.
        let gone: Vec<String> = self
            .m
            .devices
            .keys()
            .filter(|id| !m.devices.contains_key(*id))
            .cloned()
            .collect();
        for id in gone {
            self.device_removed(&id);
        }
        self.m = m;
        self.names.assign(&self.m.devices);
        self.ready = true;
        info!(
            "mqtt: {} devices, {} entities from the hub",
            self.m.devices.len(),
            self.m.entities.len()
        );
        // Everything is published again (the birth does it for a new
        // connection).
        if self.born {
            self.republish();
        }
    }

    fn republish(&mut self) {
        self.info_dirty = true;
        self.devices_due = Some(0);
        if self.cfg.discovery {
            self.jobs.push_back(Job::BridgeDiscovery);
        }
        let ids: Vec<String> = self.m.devices.keys().cloned().collect();
        for id in ids {
            self.jobs.push_back(Job::Availability(id.clone()));
            self.jobs.push_back(Job::State(id.clone()));
            if self.cfg.discovery {
                self.jobs.push_back(Job::Discovery(id));
            }
        }
    }

    fn set_network(&mut self, n: &J) {
        if let Some(o) = n.as_object() {
            for (k, v) in o {
                self.network.insert(k.clone(), v.clone());
            }
        }
        if let Some(s) = n["permit_join_s"].as_u64() {
            self.set_permit(s as u32);
        }
        self.info_dirty = true;
    }

    fn set_permit(&mut self, left: u32) {
        self.permit_left = left;
        self.permit_end_ms = (left > 0).then(|| wall_ms() + u64::from(left) * 1000);
        self.info_dirty = true;
    }

    fn on_result(&mut self, p: Pending, v: &J) {
        let ok = v["success"].as_bool() == Some(true);
        let err = || {
            v["error"]["message"]
                .as_str()
                .or(v["error"]["code"].as_str())
                .unwrap_or("failed")
                .to_string()
        };
        match p {
            Pending::HubInfo if ok => {
                let r = &v["result"];
                if let Some(c) = r["coordinator_ieee"].as_str() {
                    let new = Some(c.to_string()) != self.coordinator;
                    self.coordinator = Some(c.to_string());
                    if new {
                        self.devices_due.get_or_insert(self.now_ms);
                        if self.cfg.discovery && self.born {
                            self.jobs.push_back(Job::BridgeDiscovery);
                        }
                    }
                }
                if self.version.is_empty()
                    && let Some(n) = r["name"].as_str()
                {
                    self.version = hub_version(n);
                }
                self.info_dirty = true;
            }
            Pending::NetworkInfo if ok => self.set_network(&v["result"]),
            Pending::HubInfo | Pending::NetworkInfo | Pending::Subscribe => {}
            Pending::Command { friendly } => {
                if !ok {
                    let e = err();
                    self.stats.errors += 1;
                    warn!("mqtt: {friendly}: {e}");
                    self.log(
                        "error",
                        format!("Publish 'set' to '{friendly}' failed: '{e}'"),
                    );
                }
            }
            Pending::Response {
                req,
                data,
                transaction,
            } => {
                let body = if ok {
                    Obj::new().with("data", data).with("status", "ok").j()
                } else {
                    Obj::new()
                        .with("data", J::Object(Map::new()))
                        .with("status", "error")
                        .with("error", err())
                        .j()
                };
                self.respond(&req, body, transaction);
            }
        }
    }

    fn respond(&mut self, req: &str, mut body: J, transaction: Option<J>) {
        if let Some(t) = transaction {
            body["transaction"] = t;
        }
        let m = Msg::json(self.t(&format!("bridge/response/{req}")), &body, 0, false);
        self.enqueue(m, true);
    }

    fn log(&mut self, level: &str, message: String) {
        let m = Msg::json(
            self.t("bridge/logging"),
            &Obj::new().with("level", level).with("message", message).j(),
            0,
            false,
        );
        self.enqueue(m, false);
    }

    fn bridge_event(&mut self, ty: &str, data: J) {
        let m = Msg::json(
            self.t("bridge/event"),
            &Obj::new().with("type", ty).with("data", data).j(),
            0,
            false,
        );
        self.enqueue(m, true);
    }

    fn renamed(&mut self) {
        for (id, old, new) in self.names.assign(&self.m.devices) {
            if let Some(o) = &old {
                // Zigbee2MQTT clears the retained topics of the old name.
                if self.born {
                    self.enqueue(Msg::clear(self.t(o)), true);
                    self.enqueue(Msg::clear(self.t(&format!("{o}/availability"))), true);
                }
            }
            if new.is_some() && self.born && self.m.devices.contains_key(&id) {
                self.jobs.push_back(Job::Availability(id.clone()));
                self.mark_dirty(id.clone());
                if self.cfg.discovery {
                    self.jobs.push_back(Job::Discovery(id.clone()));
                }
            }
            self.devices_dirty();
        }
    }

    fn mark_dirty(&mut self, id: String) {
        if !self.dirty.contains(&id) {
            self.dirty.push(id);
        }
    }

    fn devices_dirty(&mut self) {
        self.devices_due
            .get_or_insert(self.now_ms + DEVICES_DEBOUNCE_MS);
    }

    fn discovery_job(&mut self, id: &str) {
        if self.cfg.discovery
            && self.born
            && !self
                .jobs
                .iter()
                .any(|j| matches!(j, Job::Discovery(x) if x == id))
        {
            self.jobs.push_back(Job::Discovery(id.to_string()));
        }
    }

    fn device_removed(&mut self, id: &str) {
        let Some(friendly) = self.names.get(id).map(str::to_string) else {
            self.m.remove_device(id);
            return;
        };
        let ieee = self.m.devices.get(id).map(|d| d.ieee_address());
        if self.born {
            let i = self.configs.iter().position(|(d, _)| d == id);
            for t in i.map(|i| self.configs.swap_remove(i).1).unwrap_or_default() {
                self.enqueue(Msg::clear(t), true);
            }
            self.enqueue(Msg::clear(self.t(&friendly)), true);
            self.enqueue(
                Msg::clear(self.t(&format!("{friendly}/availability"))),
                true,
            );
            self.bridge_event(
                "device_leave",
                Obj::new()
                    .with("friendly_name", friendly)
                    .with("ieee_address", ieee)
                    .j(),
            );
        }
        self.m.remove_device(id);
        self.dirty.retain(|d| d != id);
        self.renamed();
        self.devices_dirty();
    }

    fn on_event(&mut self, ev: &J) {
        let topic = ev["topic"].as_str().unwrap_or("");
        match topic {
            "device_added" | "device_updated" => {
                let Some(d) = Device::from_record(&ev["device"]) else {
                    return;
                };
                let id = d.id.clone();
                let old = self.m.devices.insert(id.clone(), Box::new(d.clone()));
                if self.names.get(&id).is_none() || old.as_ref().is_none_or(|o| o.name != d.name) {
                    self.renamed();
                }
                let friendly = self.names.get(&id).unwrap_or_default().to_string();
                let ieee = d.ieee_address();
                match &old {
                    None => {
                        self.last_joined = Some(id.clone());
                        if self.born {
                            self.bridge_event(
                                "device_joined",
                                Obj::new()
                                    .with("friendly_name", friendly.clone())
                                    .with("ieee_address", ieee.clone())
                                    .j(),
                            );
                            if d.interview == "in_progress" {
                                self.bridge_event(
                                    "device_interview",
                                    Obj::new()
                                        .with("friendly_name", friendly)
                                        .with("ieee_address", ieee)
                                        .with("status", "started")
                                        .j(),
                                );
                            }
                        }
                        self.discovery_job(&id);
                        self.devices_dirty();
                    }
                    Some(o) => {
                        if o.interview == "in_progress" && d.interview != "in_progress" && self.born
                        {
                            let l = Layout::of(&self.m, &d);
                            let def = z2m::definition(&l);
                            self.bridge_event(
                                "device_interview",
                                Obj::new()
                                    .with("friendly_name", friendly)
                                    .with("ieee_address", ieee)
                                    .with("status", "successful")
                                    .with("supported", d.has_definition)
                                    .with("definition", def)
                                    .j(),
                            );
                        }
                        if o.available != d.available && self.born {
                            self.jobs.push_back(Job::Availability(id.clone()));
                        }
                        let same = Device {
                            available: o.available,
                            lqi: o.lqi,
                            ..d.clone()
                        } == **o;
                        if !same {
                            self.devices_dirty();
                            self.discovery_job(&id);
                        }
                    }
                }
            }
            "device_removed" => {
                if let Some(id) = ev["id"].as_str() {
                    self.device_removed(id);
                }
            }
            "resolution_changed" => {
                if let Some(id) = ev["device"].as_str() {
                    self.devices_dirty();
                    self.discovery_job(id);
                }
            }
            "entity_added" | "entity_updated" => {
                let Some(mut e) = Entity::from_record(&ev["entity"]) else {
                    return;
                };
                let id = e.device().to_string();
                let old = self.m.entities.remove(&e.uid);
                e.st = old.as_ref().and_then(|o| o.st.clone());
                let changed = old.as_deref() != Some(&e);
                self.m.entities.insert(e.uid.clone(), Box::new(e));
                if changed {
                    self.devices_dirty();
                    self.discovery_job(&id);
                }
            }
            "entity_removed" => {
                if let Some(uid) = ev["uid"].as_str()
                    && let Some(e) = self.m.entities.remove(uid)
                {
                    self.devices_dirty();
                    let dev = e.device().to_string();
                    self.discovery_job(&dev);
                    self.mark_dirty(dev);
                }
            }
            "entity_state" => {
                let Some((uid, st, _)) = State::from_msg(ev) else {
                    return;
                };
                let Some(e) = self.m.entities.get_mut(&uid) else {
                    return;
                };
                e.st = Some(st);
                let dev = e.device().to_string();
                self.mark_dirty(dev);
            }
            "entity_event" => {
                let (Some(uid), Some(ty)) = (ev["uid"].as_str(), ev["event_type"].as_str()) else {
                    return;
                };
                self.on_action(uid, ty);
            }
            "permit_join" => {
                if let Some(s) = ev["remaining_s"].as_u64() {
                    self.set_permit(s as u32);
                }
            }
            "network_state" | "network_updated" => {
                self.request(
                    Obj::new().with("type", "network/info").j(),
                    Pending::NetworkInfo,
                );
                if self.coordinator.is_none() {
                    self.request(Obj::new().with("type", "hub/info").j(), Pending::HubInfo);
                }
            }
            _ => {}
        }
    }

    fn on_action(&mut self, uid: &str, ty: &str) {
        let Some(e) = self.m.entities.get(uid) else {
            return;
        };
        let Some(d) = self.m.devices.get(e.device()) else {
            return;
        };
        if !self.born || d.disabled {
            return;
        }
        let Some(friendly) = self.names.get(&d.id).map(str::to_string) else {
            return;
        };
        let l = Layout::of(&self.m, d);
        let Some(f) = l
            .feats
            .iter()
            .find(|f| f.e.uid == uid && f.kind == Kind::Action)
        else {
            return;
        };
        let action = z2m::action_value(f, ty);
        let mut st = z2m::device_state(&l);
        st.insert("action".into(), action.clone().into());
        let topic = self.t(&friendly);
        let qos = self.cfg.qos;
        self.enqueue(Msg::json(topic.clone(), &J::Object(st), qos, false), true);
        self.enqueue(
            Msg {
                topic: format!("{topic}/action"),
                payload: action.into_bytes(),
                qos,
                retain: false,
            },
            true,
        );
    }

    // -- MQTT ------------------------------------------------------------------

    /// The client connected (and subscribed): everything is published again.
    pub fn on_connected(&mut self) {
        self.connected = true;
        self.born = false;
        self.queue.clear();
        self.queued = 0;
        self.jobs.clear();
    }

    /// The client lost the broker.
    pub fn on_disconnected(&mut self) {
        self.connected = false;
        self.born = false;
    }

    fn birth(&mut self) {
        self.born = true;
        self.queue.clear();
        self.queued = 0;
        self.jobs.clear();
        self.dirty.clear();
        self.configs.clear();
        self.enqueue(
            Msg::json(
                self.t("bridge/state"),
                &Obj::new().with("state", "online").j(),
                1,
                true,
            ),
            true,
        );
        self.enqueue(
            Msg {
                topic: self.t("bridge/groups"),
                payload: b"[]".to_vec(),
                qos: 0,
                retain: true,
            },
            true,
        );
        self.republish();
    }

    /// Adds a message to the bounded queue; `essential` ones (responses,
    /// events, clears) go in even when it is full, others are dropped.
    fn enqueue(&mut self, m: Msg, essential: bool) {
        let n = m.size();
        if !essential && self.queued + n > self.limits.queue_bytes {
            self.stats.dropped += 1;
            return;
        }
        // Even essential messages cannot grow it without bound.
        while self.queued + n > self.limits.queue_bytes * 2 {
            let Some(old) = self.queue.pop_front() else {
                break;
            };
            self.queued -= old.size();
            self.stats.dropped += 1;
        }
        self.queued += n;
        self.queue.push_back(m);
    }

    /// One message from the broker.
    pub fn on_message(&mut self, topic: &str, payload: &[u8]) {
        self.stats.received += 1;
        if self.cfg.discovery && topic == format!("{}/status", self.cfg.discovery_prefix) {
            if payload == b"online" && self.born {
                // Home Assistant restarted: configs and states again.
                self.jobs.push_back(Job::BridgeDiscovery);
                let ids: Vec<String> = self.m.devices.keys().cloned().collect();
                for id in ids {
                    self.jobs.push_back(Job::Discovery(id.clone()));
                    self.jobs.push_back(Job::State(id));
                }
            }
            return;
        }
        let Some(rest) = topic
            .strip_prefix(&self.cfg.base_topic)
            .and_then(|r| r.strip_prefix('/'))
        else {
            return;
        };
        if let Some(req) = rest.strip_prefix("bridge/request/") {
            self.on_request(req, payload);
            return;
        }
        let segs: Vec<&str> = rest.split('/').collect();
        let n = segs.len();
        let (op, name_end, prop) = match () {
            _ if n >= 2 && matches!(segs[n - 1], "set" | "get") => (segs[n - 1], n - 1, None),
            _ if n >= 3 && matches!(segs[n - 2], "set" | "get") => {
                (segs[n - 2], n - 2, Some(segs[n - 1]))
            }
            _ => return,
        };
        let name = segs[..name_end].join("/");
        let (id, ep) = match self.names.lookup(&name) {
            Some(id) => (id.to_string(), None),
            None => match name.rsplit_once('/').and_then(|(d, ep)| {
                self.names
                    .lookup(d)
                    .map(|id| (id.to_string(), ep.to_string()))
            }) {
                Some((id, ep)) => (id, Some(ep)),
                None => {
                    self.stats.errors += 1;
                    self.log("error", format!("Entity '{name}' is unknown"));
                    return;
                }
            },
        };
        let text = String::from_utf8_lossy(payload);
        let value =
            |s: &str| serde_json::from_str::<J>(s).unwrap_or_else(|_| J::String(s.to_string()));
        let body: Map<String, J> = match prop {
            Some(p) => {
                let mut m = Map::new();
                m.insert(p.to_string(), value(text.trim()));
                m
            }
            None => match value(text.trim()) {
                J::Object(o) => o,
                J::String(s)
                    if op == "set"
                        && ["ON", "OFF", "TOGGLE"]
                            .iter()
                            .any(|x| x.eq_ignore_ascii_case(&s)) =>
                {
                    let mut m = Map::new();
                    m.insert("state".into(), s.to_uppercase().into());
                    m
                }
                _ if op == "get" && text.trim().is_empty() => {
                    let mut m = Map::new();
                    m.insert("state".into(), "".into());
                    m
                }
                _ => {
                    self.stats.errors += 1;
                    self.log("error", format!("Invalid JSON '{text}', skipping..."));
                    return;
                }
            },
        };
        let Some(d) = self.m.devices.get(&id) else {
            return;
        };
        let friendly = self.names.get(&id).unwrap_or(&name).to_string();
        let l = Layout::of(&self.m, d);
        let (acts, errs) = if op == "set" {
            z2m::set_actions(&l, &body, ep.as_deref())
        } else {
            z2m::get_actions(&l, &body, ep.as_deref())
        };
        for e in errs {
            self.stats.errors += 1;
            warn!("mqtt: {friendly}/{op}: {e}");
            self.log("error", format!("'{friendly}' {op}: {e}"));
        }
        for a in acts {
            let body = match a {
                Action::Command {
                    uid,
                    command,
                    params,
                } => Obj::new()
                    .with("type", "entities/command")
                    .with("uid", uid)
                    .with("command", command)
                    .with("params", params)
                    .j(),
                Action::Refresh(uid) => Obj::new()
                    .with("type", "entities/refresh")
                    .with("uid", uid)
                    .j(),
            };
            self.request(
                body,
                Pending::Command {
                    friendly: friendly.clone(),
                },
            );
        }
    }

    fn device_of(&self, name: &str) -> Option<String> {
        self.names.lookup(name).map(str::to_string)
    }

    fn on_request(&mut self, req: &str, payload: &[u8]) {
        let text = String::from_utf8_lossy(payload);
        let v: J = serde_json::from_str(text.trim())
            .unwrap_or_else(|_| J::String(text.trim().to_string()));
        let transaction = v.get("transaction").cloned();
        let fail = |b: &mut Self, msg: String| {
            b.respond(
                req,
                Obj::new()
                    .with("data", J::Object(Map::new()))
                    .with("status", "error")
                    .with("error", msg)
                    .j(),
                transaction.clone(),
            );
        };
        match req {
            "permit_join" => {
                let (time, value) = match &v {
                    J::Bool(b) => (if *b { 254 } else { 0 }, Some(*b)),
                    J::String(s) if s == "true" || s == "false" => {
                        (if s == "true" { 254 } else { 0 }, Some(s == "true"))
                    }
                    J::Object(o) => match (
                        o.get("time").and_then(J::as_u64),
                        o.get("value").and_then(J::as_bool),
                    ) {
                        (_, Some(false)) => (0, Some(false)),
                        (Some(t), val) => (t.min(254), val),
                        (None, Some(true)) => (254, Some(true)),
                        (None, _) => return fail(self, "permit_join: 'time' (0–254 s)".into()),
                    },
                    J::Number(n) => (n.as_u64().unwrap_or(0).min(254), None),
                    _ => return fail(self, "permit_join: 'time' (0–254 s)".into()),
                };
                let mut body = Obj::new()
                    .with("type", "network/permit_join")
                    .with("duration_s", time)
                    .j();
                let mut data = Obj::new().with("time", time).j();
                if let Some(val) = value {
                    data["value"] = val.into();
                }
                if let Some(dev) = v.get("device").and_then(J::as_str) {
                    match self.device_of(dev) {
                        Some(id) => body["target"] = id.into(),
                        None => return fail(self, format!("Device '{dev}' does not exist")),
                    }
                    data["device"] = dev.into();
                }
                self.request(
                    body,
                    Pending::Response {
                        req: req.into(),
                        data,
                        transaction,
                    },
                );
            }
            "device/rename" => {
                let to = v.get("to").and_then(J::as_str).unwrap_or("").to_string();
                let from_id = if v.get("last").and_then(J::as_bool) == Some(true) {
                    self.last_joined.clone()
                } else {
                    v.get("from")
                        .and_then(J::as_str)
                        .and_then(|f| self.device_of(f))
                };
                let Some(id) = from_id else {
                    return fail(
                        self,
                        format!(
                            "Device '{}' does not exist",
                            v.get("from").and_then(J::as_str).unwrap_or("")
                        ),
                    );
                };
                if let Err(e) = z2m::check_name(&to) {
                    return fail(self, e);
                }
                if self.names.lookup(&to).is_some_and(|other| other != id) {
                    return fail(self, format!("friendly_name '{to}' is already in use"));
                }
                let from = self.names.get(&id).unwrap_or_default().to_string();
                let data = Obj::new()
                    .with("from", from)
                    .with("to", to.clone())
                    .with(
                        "homeassistant_rename",
                        v.get("homeassistant_rename")
                            .and_then(J::as_bool)
                            .unwrap_or(false),
                    )
                    .j();
                self.request(
                    Obj::new()
                        .with("type", "devices/update")
                        .with("device", id)
                        .with("name", to)
                        .j(),
                    Pending::Response {
                        req: req.into(),
                        data,
                        transaction,
                    },
                );
            }
            "device/remove" => {
                let name = v
                    .get("id")
                    .and_then(J::as_str)
                    .or(v.as_str())
                    .unwrap_or("")
                    .to_string();
                let Some(id) = self.device_of(&name) else {
                    return fail(self, format!("Device '{name}' does not exist"));
                };
                let force = v.get("force").and_then(J::as_bool).unwrap_or(false);
                let block = v.get("block").and_then(J::as_bool).unwrap_or(false);
                let data = Obj::new()
                    .with("id", name)
                    .with("block", block)
                    .with("force", force)
                    .j();
                self.request(
                    Obj::new()
                        .with("type", "devices/remove")
                        .with("device", id)
                        .with("force", force)
                        .j(),
                    Pending::Response {
                        req: req.into(),
                        data,
                        transaction,
                    },
                );
            }
            "health_check" => self.respond(
                req,
                Obj::new()
                    .with("data", Obj::new().with("healthy", true).j())
                    .with("status", "ok")
                    .j(),
                transaction,
            ),
            other => fail(self, format!("Request '{other}' is not supported")),
        }
    }

    // -- output ------------------------------------------------------------------

    /// Advances the clock (ms, monotonic).
    pub fn tick(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
        if self.ready && now_ms >= self.next_network_poll {
            self.next_network_poll = now_ms + 60_000;
            if self.coordinator.is_none() || !self.network.contains_key("pan_id") {
                self.request(
                    Obj::new().with("type", "network/info").j(),
                    Pending::NetworkInfo,
                );
                self.request(Obj::new().with("type", "hub/info").j(), Pending::HubInfo);
            }
        }
    }

    /// The next message to publish (`None`: nothing now). Call while the
    /// client accepts messages.
    pub fn next_msg(&mut self) -> Option<Msg> {
        if !self.connected || !self.ready {
            return None;
        }
        if !self.born {
            self.birth();
        }
        if let Some(m) = self.queue.pop_front() {
            self.queued -= m.size();
            return Some(m);
        }
        if self.info_dirty {
            self.info_dirty = false;
            return Some(Msg::json(
                self.t("bridge/info"),
                &self.bridge_info(),
                0,
                true,
            ));
        }
        if self.devices_due.is_some_and(|t| self.now_ms >= t) {
            self.devices_due = None;
            return Some(self.bridge_devices());
        }
        while !self.dirty.is_empty() {
            let id = self.dirty.remove(0);
            if let Some(m) = self.state_msgs(&id) {
                return Some(m);
            }
        }
        while let Some(job) = self.jobs.pop_front() {
            if let Some(m) = self.run_job(job) {
                return Some(m);
            }
            if let Some(m) = self.queue.pop_front() {
                self.queued -= m.size();
                return Some(m);
            }
        }
        None
    }

    /// Puts back a message the client could not take.
    pub fn unsend(&mut self, m: Msg) {
        self.queued += m.size();
        self.queue.push_front(m);
    }

    fn state_msgs(&mut self, id: &str) -> Option<Msg> {
        let d = self.m.devices.get(id)?;
        if d.disabled {
            return None;
        }
        let friendly = self.names.get(id)?.to_string();
        let l = Layout::of(&self.m, d);
        let st = z2m::device_state(&l);
        let (qos, retain) = (self.cfg.qos, self.cfg.retain_state);
        // Endpoint lights also on `<name>/<endpoint>` (HA's JSON schema).
        let mut extra = Vec::new();
        for f in l
            .feats
            .iter()
            .filter(|f| f.kind == Kind::Light && f.suffix.is_some())
        {
            if let Some(s) = &f.e.st {
                let ls = z2m::light_state(&l, f, s);
                extra.push(Msg::json(
                    self.t(&format!("{friendly}/{}", f.suffix.as_deref().unwrap_or(""))),
                    &J::Object(ls),
                    qos,
                    retain,
                ));
            }
        }
        for m in extra {
            self.enqueue(m, false);
        }
        if st.is_empty() {
            return self.queue.pop_front().inspect(|m| self.queued -= m.size());
        }
        Some(Msg::json(self.t(&friendly), &J::Object(st), qos, retain))
    }

    fn run_job(&mut self, job: Job) -> Option<Msg> {
        match job {
            Job::Availability(id) => {
                let d = self.m.devices.get(&id)?;
                if d.disabled {
                    return None;
                }
                let f = self.names.get(&id)?;
                let st = if d.available { "online" } else { "offline" };
                Some(Msg::json(
                    self.t(&format!("{f}/availability")),
                    &Obj::new().with("state", st).j(),
                    1,
                    self.cfg.retain_availability,
                ))
            }
            Job::State(id) => self.state_msgs(&id),
            Job::BridgeDiscovery => {
                let coord = format!("0x{}", self.coordinator.as_deref()?);
                let ctx = discovery::Ctx {
                    base: &self.cfg.base_topic,
                    prefix: &self.cfg.discovery_prefix,
                    coordinator: &coord,
                    version: &self.version,
                };
                let cfgs = discovery::bridge_configs(&ctx);
                for (t, p) in cfgs {
                    self.enqueue(Msg::json(t, &p, 1, true), true);
                }
                None
            }
            Job::Discovery(id) => {
                let coord = format!(
                    "0x{}",
                    self.coordinator.as_deref().unwrap_or("0000000000000000")
                );
                let d = self.m.devices.get(&id)?;
                let friendly = self.names.get(&id)?.to_string();
                let l = Layout::of(&self.m, d);
                let ctx = discovery::Ctx {
                    base: &self.cfg.base_topic,
                    prefix: &self.cfg.discovery_prefix,
                    coordinator: &coord,
                    version: &self.version,
                };
                let cfgs = if d.disabled {
                    Vec::new()
                } else {
                    discovery::device_configs(&ctx, &l, &friendly)
                };
                let topics: Vec<String> = cfgs.iter().map(|(t, _)| t.clone()).collect();
                let old = match self.configs.iter_mut().find(|(d, _)| *d == id) {
                    Some((_, t)) => std::mem::replace(t, topics.clone()),
                    None => {
                        self.configs.push((id.clone(), topics.clone()));
                        Vec::new()
                    }
                };
                for t in old.into_iter().filter(|t| !topics.contains(t)) {
                    self.enqueue(Msg::clear(t), true);
                }
                for (t, p) in cfgs {
                    self.enqueue(Msg::json(t, &p, 1, true), true);
                }
                None
            }
        }
    }

    fn bridge_info(&self) -> J {
        let n = &self.network;
        let ncp = n.get("ncp").cloned().unwrap_or(J::Null);
        Obj::new()
            .with("version", self.version.clone())
            .with("commit", J::Null)
            .with(
                "coordinator",
                Obj::new()
                    .with(
                        "ieee_address",
                        self.coordinator.as_ref().map(|c| format!("0x{c}")),
                    )
                    .with(
                        "type",
                        ncp.get("chip")
                            .and_then(J::as_str)
                            .map_or("zigbee-rs NCP".to_string(), |c| {
                                format!("zigbee-rs NCP ({c})")
                            }),
                    )
                    .with(
                        "meta",
                        Obj::new()
                            .with("revision", ncp.get("fw").cloned())
                            .with("board", ncp.get("board").cloned())
                            .with("product", ncp.get("product").cloned())
                            .with("protocol", ncp.get("protocol").cloned())
                            .j(),
                    )
                    .j(),
            )
            .with(
                "network",
                Obj::new()
                    .with("channel", n.get("channel").cloned())
                    .with("pan_id", n.get("pan_id").cloned())
                    .with(
                        "extended_pan_id",
                        n.get("ext_pan_id")
                            .and_then(J::as_str)
                            .map(|e| format!("0x{e}")),
                    )
                    .j(),
            )
            .with("log_level", "info")
            .with("permit_join", self.permit_left > 0)
            .with("permit_join_timeout", self.permit_left)
            .with("permit_join_end", self.permit_end_ms)
            .with("restart_required", false)
            .with(
                "config",
                Obj::new()
                    .with(
                        "mqtt",
                        Obj::new()
                            .with("base_topic", self.cfg.base_topic.clone())
                            .with(
                                "server",
                                format!("mqtt://{}:{}", self.cfg.host, self.cfg.port),
                            )
                            .j(),
                    )
                    .with(
                        "homeassistant",
                        Obj::new()
                            .with("enabled", self.cfg.discovery)
                            .with("discovery_topic", self.cfg.discovery_prefix.clone())
                            .with(
                                "status_topic",
                                format!("{}/status", self.cfg.discovery_prefix),
                            )
                            .j(),
                    )
                    .with("availability", Obj::new().with("enabled", true).j())
                    .with(
                        "advanced",
                        Obj::new()
                            .with("log_level", "info")
                            .with("last_seen", "disable")
                            .with("output", "json")
                            .with("cache_state", true)
                            .j(),
                    )
                    .with("permit_join", false)
                    .j(),
            )
            .with("config_schema", J::Object(Map::new()))
            .j()
    }

    /// `bridge/devices`: the coordinator, then every device; beyond
    /// [`Limits::devices_bytes`] without the exposes.
    fn bridge_devices(&self) -> Msg {
        let topic = self.t("bridge/devices");
        let render = |with_exposes: bool| -> Option<String> {
            // About 1.2 KB per device with exposes: one allocation, not a
            // doubling series (each step holds the old and the new copy).
            let per = if with_exposes { 1280 } else { 512 };
            let mut s = String::new();
            s.try_reserve((self.m.devices.len() * per + 512).min(self.limits.devices_bytes + 2048))
                .ok()?;
            s.push('[');
            let mut first = true;
            let mut push = |s: &mut String, v: &J| -> Option<()> {
                let t = v.to_string();
                s.try_reserve(t.len() + 1).ok()?;
                if !first {
                    s.push(',');
                }
                first = false;
                s.push_str(&t);
                Some(())
            };
            if let Some(c) = &self.coordinator {
                push(&mut s, &z2m::coordinator_device(c))?;
            }
            for (id, d) in &self.m.devices {
                let Some(friendly) = self.names.get(id) else {
                    continue;
                };
                let l = Layout::of(&self.m, d);
                let mut v = z2m::bridge_device(&l, friendly);
                if !with_exposes && let Some(def) = v["definition"].as_object_mut() {
                    def.insert("exposes".into(), J::Array(Vec::new()));
                }
                push(&mut s, &v)?;
                if with_exposes && s.len() > self.limits.devices_bytes {
                    return None;
                }
            }
            s.push(']');
            Some(s)
        };
        let text = render(true).or_else(|| {
            warn!(
                "mqtt: bridge/devices over {} B: published without exposes",
                self.limits.devices_bytes
            );
            render(false)
        });
        Msg {
            topic,
            payload: text.unwrap_or_else(|| "[]".into()).into_bytes(),
            qos: 0,
            retain: true,
        }
    }
}

/// Firmware version in a hub name (`zigbee-rs-hub 0.3.0 (board)`): the
/// first word that starts with a digit, else empty.
pub(crate) fn hub_version(name: &str) -> String {
    name.split_whitespace()
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))
        .unwrap_or("")
        .to_string()
}

fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
