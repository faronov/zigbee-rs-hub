//! Per-device runtime: attribute knowledge, entity states, records for the
//! API (spec 4.1, 4.2, 4.5) and the HA capability derivation (4.3).

use std::collections::{BTreeMap, VecDeque};

use serde_json::{Map, Value as J, json};

use crate::expr::Value;
use crate::fingerprint::Fingerprint;
use crate::generic::{AttrView, Support};
use crate::gp::types::{GpSinkRecord, GpdId};
use crate::model::{Binding, Category, EntityDef, LayerSource, Platform, PowerKind, ValueDef};
use crate::resolve::{Resolution, ResolvedEntity};
use crate::transport::ApsRequest;
use crate::zcl::ZclValue;
use crate::zdo::NodeDescriptor;

/// Milliseconds since the Unix epoch.
pub type Millis = u64;

/// Seconds as the API's float timestamps.
pub fn ts(ms: Millis) -> f64 {
    ms as f64 / 1000.0
}

/// One attribute the hub knows about.
#[derive(Debug, Clone, PartialEq)]
pub struct AttrEntry {
    /// Support.
    pub support: Support,
    /// Last value.
    pub value: Option<ZclValue>,
}

/// Attribute knowledge keyed by `(ep, cluster, attr, mfr)`.
#[derive(Debug, Clone, Default)]
pub struct AttrCache {
    /// Entries.
    pub map: BTreeMap<(u8, u16, u16, Option<u16>), AttrEntry>,
}

impl AttrCache {
    /// Records a value; returns `(support changed, value changed)`.
    pub fn set(
        &mut self,
        ep: u8,
        cluster: u16,
        attr: u16,
        mfr: Option<u16>,
        v: Option<ZclValue>,
    ) -> (bool, bool) {
        let support = if v.is_some() {
            Support::Supported
        } else {
            Support::Unsupported
        };
        let e = AttrEntry { support, value: v };
        let old = self.map.insert((ep, cluster, attr, mfr), e.clone());
        (
            old.as_ref().map(|o| o.support) != Some(support),
            old.as_ref().map(|o| &o.value) != Some(&e.value),
        )
    }
}

impl AttrView for AttrCache {
    fn support(&self, ep: u8, cluster: u16, attr: u16) -> Support {
        self.map
            .get(&(ep, cluster, attr, None))
            .map_or(Support::Unknown, |e| e.support)
    }
    fn value(&self, ep: u8, cluster: u16, attr: u16) -> Option<&ZclValue> {
        self.map
            .get(&(ep, cluster, attr, None))
            .and_then(|e| e.value.as_ref())
    }
}

/// Device kind.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DeviceKind {
    /// A Zigbee node.
    Zigbee {
        /// IEEE.
        ieee: u64,
        /// NWK.
        nwk: u16,
    },
    /// A Green Power device.
    Gp {
        /// GPD ID.
        gpd_id: GpdId,
    },
}

/// Interview progress (3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterviewStep {
    /// Node_Desc_req.
    NodeDesc,
    /// Active_EP_req.
    ActiveEp,
    /// Simple_Desc_req of endpoint index n.
    SimpleDesc(usize),
    /// Basic read batch n.
    Basic(usize),
    /// Device Schema header read.
    SchemaHeader,
    /// Device Schema chunk n.
    SchemaChunk(u16),
    /// Configure.
    Configure,
    /// Done.
    Done,
}

/// Per-entity runtime state.
#[derive(Debug, Clone, Default)]
pub struct EntityState {
    /// Own value (simple platforms).
    pub value: Value,
    /// Part values (composites).
    pub parts: BTreeMap<String, Value>,
    /// Extra attributes (events, pending).
    pub attributes: Map<String, J>,
    /// `last_changed`, ms.
    pub last_changed: Option<Millis>,
    /// `last_reported`, ms.
    pub last_reported: Option<Millis>,
    /// Requested value of a write still in flight / queued.
    pub pending: Option<J>,
    /// Auto-reset deadline.
    pub reset_at: Option<Millis>,
    /// Previous event type.
    pub last_event: Option<String>,
    /// Expression errors and map misses.
    pub errors: u32,
}

/// Live entity: resolved definition + state.
#[derive(Debug, Clone)]
pub struct Entity {
    /// `uid`.
    pub uid: String,
    /// Resolved definition.
    pub r: ResolvedEntity,
    /// State.
    pub state: EntityState,
    /// Orphaned (3.4).
    pub orphaned: bool,
    /// User name override.
    pub user_name: Option<String>,
    /// User enable override.
    pub user_enabled: Option<bool>,
}

/// Per-device diagnostics (14.2 `devices/diagnostics`).
#[derive(Debug, Clone, Default)]
pub struct Diagnostics {
    /// Last 50 frames: (ms, cluster, hex).
    pub frames: VecDeque<(Millis, u16, String)>,
    /// Attributes seen without an entity.
    pub unknown_attrs: BTreeMap<(u8, u16, u16), String>,
    /// Tuya DPs seen without an entity: dp → (type, hex, ms).
    pub tuya_dps: BTreeMap<u8, (u8, String, Millis)>,
    /// GP data without an entity (manufacturer reports, 0xB0–0xBF).
    pub gp_data: BTreeMap<u8, String>,
    /// Configure results by op key.
    pub configure: BTreeMap<String, String>,
    /// Dropped / counted events.
    pub counters: BTreeMap<String, u64>,
}

impl Diagnostics {
    /// Increments a counter.
    pub fn count(&mut self, k: &str) {
        *self.counters.entry(k.to_string()).or_default() += 1;
    }
    /// Remembers a frame.
    pub fn frame(&mut self, now: Millis, cluster: u16, payload: &[u8]) {
        if self.frames.len() >= 50 {
            self.frames.pop_front();
        }
        self.frames
            .push_back((now, cluster, crate::expr::hex(payload)));
    }
}

/// A frame waiting for a sleepy device to wake up.
#[derive(Debug, Clone)]
pub struct Queued {
    /// The request.
    pub req: ApsRequest,
    /// What it is for.
    pub purpose: crate::hub::Purpose,
    /// ZCL/ZDO TSN.
    pub tsn: u8,
}

/// GP specifics of a device.
#[derive(Debug, Clone)]
pub struct GpState {
    /// Sink record from `EVT_GP_COMMISSIONED`.
    pub sink: Option<GpSinkRecord>,
    /// Last accepted frame counter.
    pub frame_counter: u32,
    /// Security level of the last frame.
    pub security_level: u8,
    /// Contact bitmap of the last 0x69/0x6A (generic per-contact events).
    pub contacts: u8,
    /// Hub-side de-duplication: (frame counter, command) of recent frames.
    pub recent: VecDeque<(Millis, u32, u8)>,
}

/// Configure state (5.3).
#[derive(Debug, Clone, Default)]
pub struct ConfigureState {
    /// Hash of the current op list.
    pub hash: Option<String>,
    /// Results by op key: `true` done.
    pub done: BTreeMap<String, bool>,
    /// Next retry of failed ops (mains).
    pub retry_at: Option<Millis>,
    /// Current backoff, s.
    pub backoff_s: u64,
    /// Ops queued for execution.
    pub queue: VecDeque<crate::hub::OpRun>,
    /// One op in flight.
    pub in_flight: bool,
    /// Failed reporting configs (polled if primary on mains).
    pub unreportable: Vec<(u8, u16, u16)>,
    /// Last poll (or failure) of an unreportable primary measurement.
    pub poll_at: BTreeMap<(u8, u16, u16), Millis>,
}

/// Self-description progress (12.3).
#[derive(Debug, Clone, Default)]
pub struct SchemaState {
    /// Format byte.
    pub format: Option<u8>,
    /// Hash (8 bytes).
    pub hash: Option<Vec<u8>>,
    /// Blob length.
    pub length: Option<u16>,
    /// Chunk size.
    pub chunk_size: Option<u8>,
    /// Collected chunks.
    pub chunks: BTreeMap<u16, Vec<u8>>,
    /// Retries per chunk.
    pub retries: u8,
    /// Decoded fragment.
    pub fragment: Option<std::sync::Arc<crate::model::Definition>>,
    /// Reason it was rejected.
    pub error: Option<String>,
}

/// One device.
#[derive(Debug, Clone)]
pub struct Device {
    /// `zb:<ieee>` / `gp:<srcid>`.
    pub id: String,
    /// Kind.
    pub kind: DeviceKind,
    /// Fingerprint so far.
    pub fp: Fingerprint,
    /// Node descriptor.
    pub node_desc: Option<NodeDescriptor>,
    /// Endpoints from Active_EP_rsp.
    pub active_eps: Vec<u8>,
    /// Interview step.
    pub interview: InterviewStep,
    /// The interview could not complete some step.
    pub interview_partial: bool,
    /// Attribute knowledge.
    pub attrs: AttrCache,
    /// Current resolution.
    pub resolution: Resolution,
    /// Entities (live and orphaned).
    pub entities: Vec<Entity>,
    /// Last frame, ms.
    pub last_seen: Option<Millis>,
    /// When `last_seen` was last published (at most once a minute).
    pub last_seen_published: Option<Millis>,
    /// LQI.
    pub lqi: Option<u8>,
    /// RSSI.
    pub rssi: Option<i8>,
    /// Availability.
    pub available: bool,
    /// Active availability check: tries left and next try.
    pub check: Option<(u8, Millis)>,
    /// Configure.
    pub configure: ConfigureState,
    /// Sleepy send queue.
    pub queue: VecDeque<Queued>,
    /// Sleepy device considered awake until.
    pub awake_until: Millis,
    /// User name.
    pub user_name: Option<String>,
    /// Disabled by the user.
    pub disabled: bool,
    /// Pinned definition.
    pub pinned: Option<String>,
    /// Hub-binding option values (`opt.<key>`).
    pub options: BTreeMap<String, Value>,
    /// Warnings (`gp_unsecured`, configure failures).
    pub warnings: Vec<String>,
    /// Diagnostics.
    pub diag: Diagnostics,
    /// Tuya sequence number.
    pub tuya_seq: u16,
    /// Tuya last time push, ms.
    pub tuya_time_at: Option<Millis>,
    /// Tuya / ZCL event de-duplication memory.
    pub dedup: VecDeque<(Millis, u64)>,
    /// GP specifics.
    pub gp: Option<GpState>,
    /// Self-description.
    pub schema: SchemaState,
    /// Fingerprint hash of the current resolution.
    pub fp_hash: Option<String>,
    /// Next periodic Tuya query.
    pub tuya_query_at: Option<Millis>,
}

impl Device {
    /// A new device.
    pub fn new(id: String, kind: DeviceKind, now: Millis) -> Self {
        Self {
            id,
            kind,
            fp: Fingerprint::default(),
            node_desc: None,
            active_eps: Vec::new(),
            interview: InterviewStep::NodeDesc,
            interview_partial: false,
            attrs: AttrCache::default(),
            resolution: Resolution::default(),
            entities: Vec::new(),
            last_seen: Some(now),
            last_seen_published: None,
            lqi: None,
            rssi: None,
            available: true,
            check: None,
            configure: ConfigureState::default(),
            queue: VecDeque::new(),
            awake_until: now,
            user_name: None,
            disabled: false,
            pinned: None,
            options: BTreeMap::new(),
            warnings: Vec::new(),
            diag: Diagnostics::default(),
            tuya_seq: 0,
            tuya_time_at: None,
            dedup: VecDeque::new(),
            gp: None,
            schema: SchemaState::default(),
            fp_hash: None,
            tuya_query_at: None,
        }
    }

    /// IEEE of a Zigbee device.
    pub fn ieee(&self) -> Option<u64> {
        match self.kind {
            DeviceKind::Zigbee { ieee, .. } => Some(ieee),
            DeviceKind::Gp { .. } => None,
        }
    }

    /// NWK of a Zigbee device.
    pub fn nwk(&self) -> Option<u16> {
        match self.kind {
            DeviceKind::Zigbee { nwk, .. } => Some(nwk),
            DeviceKind::Gp { .. } => None,
        }
    }

    /// Effective power.
    pub fn power(&self) -> PowerKind {
        self.resolution.power.unwrap_or_else(|| self.fp.power())
    }

    /// Sleepy (RxOnWhenIdle = 0 or `device.sleepy`).
    pub fn sleepy(&self) -> bool {
        self.resolution
            .device
            .sleepy
            .unwrap_or_else(|| self.fp.sleepy())
    }

    /// Live entity by `(ep, key)`.
    pub fn entity(&self, ep: u8, key: &str) -> Option<&Entity> {
        self.entities
            .iter()
            .find(|e| !e.orphaned && e.r.ep == ep && e.r.def.key == key)
    }

    /// Entity index by uid.
    pub fn entity_index(&self, uid: &str) -> Option<usize> {
        self.entities.iter().position(|e| e.uid == uid)
    }

    /// The state of entity `key` as an expression value (`state.<key>`).
    pub fn state_value(&self, key: &str) -> Value {
        self.entities
            .iter()
            .find(|e| !e.orphaned && e.r.def.key == key)
            .map_or(Value::Null, |e| e.state.value.clone())
    }

    /// Display manufacturer.
    pub fn manufacturer(&self) -> Option<String> {
        self.resolution
            .vendor
            .clone()
            .or_else(|| self.fp.manufacturer_name.clone())
            .or_else(|| {
                self.fp
                    .gp
                    .as_ref()
                    .and_then(|g| g.manufacturer_id)
                    .map(|m| format!("0x{m:04X}"))
            })
    }

    /// Display model.
    pub fn model(&self) -> Option<String> {
        self.resolution
            .model
            .clone()
            .or_else(|| self.fp.model_id.clone())
            .or_else(|| {
                self.fp
                    .gp
                    .as_ref()
                    .map(|g| format!("GPD 0x{:02X}", g.device_id))
            })
    }

    /// Device record (4.1).
    pub fn record(&self) -> J {
        let (ieee, src_id) = match &self.kind {
            DeviceKind::Zigbee { ieee, .. } => (Some(format!("{ieee:016x}")), None),
            DeviceKind::Gp { gpd_id } => (None, Some(gpd_id.device_id()[3..].to_string())),
        };
        let model_id = match &self.fp.gp {
            Some(g) => match (g.manufacturer_id, g.model_id) {
                (Some(m), Some(md)) => Some(format!("0x{m:04X}/0x{md:04X}")),
                _ => None,
            },
            None => self.fp.model_id.clone(),
        };
        let power = match self.power() {
            PowerKind::Mains => "mains",
            PowerKind::Battery => "battery",
            PowerKind::EnergyHarvesting => "energy_harvesting",
        };
        json!({
            "id": self.id,
            "ieee": ieee,
            "nwk": self.nwk(),
            "src_id": src_id,
            "manufacturer": self.manufacturer(),
            "model": self.model(),
            "model_id": model_id,
            "sw_version": self.fp.sw_build_id,
            "hw_version": self.fp.hw_version,
            "name": self.user_name.clone().or_else(|| self.model()),
            "via": "hub",
            "power": power,
            "available": self.available,
            "last_seen": self.last_seen.map(ts),
            "lqi": self.lqi,
            "rssi": self.rssi,
            "warnings": self.warnings,
            "interview": match (self.interview, self.interview_partial) {
                (InterviewStep::Done, false) => "complete",
                (InterviewStep::Done, true) => "partial",
                _ => "in_progress",
            },
            "disabled": self.disabled,
            "definition": self.resolution.layers.iter().map(|l| json!({
                "source": l.source, "id": l.id, "revision": l.revision, "content_hash": l.content_hash,
            })).collect::<Vec<_>>(),
        })
    }
}

fn platform_name_key(e: &EntityDef) -> Option<String> {
    e.translation_key.clone().or_else(|| {
        if e.name.is_none() && e.device_class.is_none() {
            Some(e.key.clone())
        } else {
            None
        }
    })
}

fn map_labels(v: &ValueDef) -> Vec<J> {
    v.transforms
        .map
        .iter()
        .map(|(_, l)| J::String(l.clone()))
        .collect()
}

fn part<'a>(e: &'a EntityDef, n: &str) -> Option<&'a ValueDef> {
    e.parts.iter().find(|(k, _)| k == n).map(|(_, v)| v)
}

/// HA capabilities of an entity (4.3).
pub fn capabilities(e: &EntityDef) -> Map<String, J> {
    let mut c = Map::new();
    let v = &e.value;
    match e.platform {
        Platform::Sensor => {
            if !v.transforms.map.is_empty() {
                c.insert("options".into(), J::Array(map_labels(v)));
            }
            if let Some(p) = v.transforms.precision {
                c.insert("suggested_display_precision".into(), p.into());
            } else if let Some(d) = v
                .transforms
                .div
                .filter(|d| *d >= 10.0 && (d.log10().fract() == 0.0))
            {
                c.insert(
                    "suggested_display_precision".into(),
                    (d.log10() as u32).into(),
                );
            }
        }
        Platform::Number => {
            c.insert(
                "native_min_value".into(),
                json!(v.limits.min.unwrap_or(0.0)),
            );
            c.insert(
                "native_max_value".into(),
                json!(v.limits.max.unwrap_or(100.0)),
            );
            c.insert(
                "native_step".into(),
                json!(
                    v.limits
                        .step
                        .unwrap_or_else(|| v.transforms.div.map_or(1.0, |d| 1.0 / d))
                ),
            );
            c.insert(
                "mode".into(),
                json!(v.limits.mode.clone().unwrap_or_else(|| "auto".into())),
            );
        }
        Platform::Select => {
            c.insert("options".into(), J::Array(map_labels(v)));
        }
        Platform::Text => {
            c.insert(
                "native_min".into(),
                json!(v.limits.min.unwrap_or(0.0) as u64),
            );
            c.insert(
                "native_max".into(),
                json!(v.limits.max.unwrap_or(255.0) as u64),
            );
            c.insert(
                "mode".into(),
                json!(v.limits.mode.clone().unwrap_or_else(|| "text".into())),
            );
            if let Some(crate::model::Codec::TuyaScheduleDay {
                transitions,
                variant: crate::model::ScheduleVariant::MultiDp,
                ..
            }) = &v.transforms.codec
            {
                let one = r"\d{2}:\d{2}/\d+(\.\d)?";
                c.insert(
                    "pattern".into(),
                    json!(format!(
                        "^{one}( {one}){{{}}}$",
                        transitions.saturating_sub(1)
                    )),
                );
            }
        }
        Platform::Event => {
            c.insert("event_types".into(), json!(e.event_types));
        }
        Platform::Light => {
            let mut modes = Vec::new();
            if part(e, "color_temp").is_some() {
                modes.push("color_temp");
            }
            if part(e, "color_x").is_some() {
                modes.push("xy");
            }
            if part(e, "hue").is_some() {
                modes.push("hs");
            }
            if modes.is_empty() {
                modes.push(if part(e, "brightness").is_some() {
                    "brightness"
                } else {
                    "onoff"
                });
            }
            c.insert("supported_color_modes".into(), json!(modes));
            c.insert("supported_features".into(), json!(["transition"]));
        }
        Platform::Cover => {
            let mut f = vec!["open", "close", "stop"];
            if part(e, "position").is_some() {
                f.push("set_position");
            }
            if part(e, "tilt").is_some() {
                f.push("set_tilt_position");
            }
            c.insert("supported_features".into(), json!(f));
        }
        Platform::Climate => {
            let mut f = Vec::new();
            if let Some(m) = part(e, "hvac_mode") {
                c.insert("hvac_modes".into(), J::Array(map_labels(m)));
                if m.transforms.map.iter().any(|(_, l)| l == "off") {
                    f.push("turn_off");
                    f.push("turn_on");
                }
            }
            if let Some(t) = part(e, "target_temperature") {
                f.push("target_temperature");
                c.insert("min_temp".into(), json!(t.limits.min.unwrap_or(7.0)));
                c.insert("max_temp".into(), json!(t.limits.max.unwrap_or(35.0)));
                c.insert(
                    "target_temperature_step".into(),
                    json!(t.limits.step.unwrap_or(0.5)),
                );
            }
            if let Some(p) = part(e, "preset") {
                f.push("preset_mode");
                c.insert("preset_modes".into(), J::Array(map_labels(p)));
            }
            if let Some(p) = part(e, "fan_mode") {
                f.push("fan_mode");
                c.insert("fan_modes".into(), J::Array(map_labels(p)));
            }
            c.insert("supported_features".into(), json!(f));
            c.insert("temperature_unit".into(), json!("°C"));
        }
        Platform::Lock => {
            c.insert("supported_features".into(), json!([]));
        }
        Platform::Fan => {
            c.insert("speed_count".into(), json!(3));
            c.insert("preset_modes".into(), json!([]));
            c.insert(
                "supported_features".into(),
                json!(["set_speed", "preset_mode", "turn_on", "turn_off"]),
            );
        }
        Platform::Valve => {
            c.insert(
                "reports_position".into(),
                json!(part(e, "position").is_some()),
            );
            let mut f = vec!["open", "close"];
            if part(e, "position").is_some() {
                f.push("set_position");
            }
            c.insert("supported_features".into(), json!(f));
        }
        Platform::Siren => {
            c.insert("supported_features".into(), json!(["turn_on", "turn_off"]));
        }
        _ => {}
    }
    for (k, v) in &e.capabilities {
        c.insert(k.clone(), v.clone());
    }
    c
}

impl Entity {
    /// Entity record (4.2).
    pub fn record(&self, device_id: &str) -> J {
        let e = &self.r.def;
        let name = self
            .user_name
            .clone()
            .or_else(|| e.name.clone())
            .or_else(|| {
                // Disambiguate keys on several endpoints by name, not key (4.2).
                self.r
                    .ep_name
                    .as_ref()
                    .map(|n| format!("{} {n}", e.key.replace('_', " ")))
            });
        json!({
            "uid": self.uid,
            "device": device_id,
            "endpoint": self.r.ep,
            "key": e.key,
            "platform": e.platform,
            "device_class": e.device_class,
            "unit": e.unit,
            "state_class": e.state_class,
            "category": e.category,
            "enabled_default": self.user_enabled.unwrap_or(e.enabled),
            "name": name,
            "translation_key": platform_name_key(e),
            "precision": e.value.transforms.precision,
            "capabilities": capabilities(e),
            "source": self.r.source,
            "orphaned": self.orphaned,
        })
    }

    /// HA state + attributes (4.5).
    pub fn ha_state(&self) -> (J, Map<String, J>) {
        let e = &self.r.def;
        let s = &self.state;
        let mut attrs = s.attributes.clone();
        let pv = |n: &str| s.parts.get(n).cloned().unwrap_or_default();
        let onoff = |b: Value| match b.truthy() {
            Some(true) => J::from("on"),
            Some(false) => J::from("off"),
            None => J::Null,
        };
        let state = match e.platform {
            Platform::Light => {
                let mut put = |k: &str, v: J| {
                    if !v.is_null() {
                        attrs.insert(k.into(), v);
                    }
                };
                put("brightness", pv("brightness").to_json());
                put("color_temp_kelvin", pv("color_temp").to_json());
                if let (Some(x), Some(y)) = (pv("color_x").as_f64(), pv("color_y").as_f64()) {
                    put("xy_color", json!([x, y]));
                }
                if let (Some(h), Some(sa)) = (pv("hue").as_f64(), pv("saturation").as_f64()) {
                    put("hs_color", json!([h, sa]));
                }
                put("color_mode", pv("color_mode").to_json());
                for (k, v) in &s.parts {
                    if !matches!(
                        k.as_str(),
                        "on_off"
                            | "brightness"
                            | "color_temp"
                            | "color_x"
                            | "color_y"
                            | "hue"
                            | "saturation"
                            | "color_mode"
                    ) {
                        put(k, v.to_json());
                    }
                }
                onoff(pv("on_off"))
            }
            Platform::Fan => {
                let mode = pv("fan_mode");
                let pct = match mode.as_str() {
                    Some("off") => Some(0),
                    Some("low") => Some(33),
                    Some("medium") => Some(66),
                    Some("high" | "on") => Some(100),
                    _ => None,
                };
                if let Some(p) = pct {
                    attrs.insert("percentage".into(), p.into());
                }
                if let Some(m @ ("auto" | "smart")) = mode.as_str() {
                    attrs.insert("preset_mode".into(), m.into());
                }
                match mode.as_str() {
                    Some("off") => J::from("off"),
                    Some(_) => J::from("on"),
                    None => onoff(pv("on_off")),
                }
            }
            Platform::Cover | Platform::Valve => {
                for (k, v) in &s.parts {
                    if k != "state" {
                        let name = match k.as_str() {
                            "position" => "current_position",
                            "tilt" => "current_tilt_position",
                            o => o,
                        };
                        attrs.insert(name.into(), v.to_json());
                    }
                }
                match (s.parts.get("state"), pv("position").as_f64()) {
                    (Some(st), _) if !st.is_null() => st.to_json(),
                    (_, Some(p)) => J::from(if p > 0.0 { "open" } else { "closed" }),
                    _ => J::Null,
                }
            }
            Platform::Climate | Platform::Lock | Platform::Siren => {
                let primary = match e.platform {
                    Platform::Climate => "hvac_mode",
                    Platform::Lock => "state",
                    _ => "on_off",
                };
                for (k, v) in &s.parts {
                    if k != primary {
                        attrs.insert(k.clone(), v.to_json());
                    }
                }
                match e.platform {
                    Platform::Siren => onoff(if e.parts.is_empty() {
                        s.value.clone()
                    } else {
                        pv(primary)
                    }),
                    _ => pv(primary).to_json(),
                }
            }
            Platform::Event => s.last_reported.map_or(J::Null, |t| json!(ts(t))),
            Platform::Sensor if e.device_class.as_deref() == Some("timestamp") => {
                s.value.as_f64().map_or(J::Null, |t| json!(t))
            }
            _ => s.value.to_json(),
        };
        if let Some(p) = &s.pending {
            attrs.insert("pending".into(), p.clone());
        }
        (state, attrs)
    }

    /// State message (4.5).
    pub fn state_msg(&self, available: bool) -> J {
        let (state, attrs) = self.ha_state();
        json!({
            "uid": self.uid,
            "state": state,
            "attributes": attrs,
            "available": available && !self.orphaned,
            "last_changed": self.state.last_changed.map(ts),
            "last_reported": self.state.last_reported.map(ts),
        })
    }
}

/// `true` for value definitions the hub reads at configure.
pub fn readable(e: &EntityDef, v: &ValueDef) -> bool {
    e.access_of(v).read && matches!(v.binding, Binding::Zcl(_))
}

/// Category check helper.
pub fn is_config(e: &EntityDef) -> bool {
    e.category == Some(Category::Config)
}

/// Source label for diagnostics.
pub fn source_name(s: LayerSource) -> &'static str {
    match s {
        LayerSource::Generic => "generic",
        LayerSource::Catalog => "catalog",
        LayerSource::Fetched => "fetched",
        LayerSource::SelfDescription => "self_description",
        LayerSource::User => "user",
    }
}
