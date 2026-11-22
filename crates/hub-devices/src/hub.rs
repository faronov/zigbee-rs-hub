//! The device manager and entity runtime (spec 2, 3, 4.5–4.7, 5.3):
//! registry, interview, resolution and materialization, frame dispatch,
//! state updates, events, availability, the sleepy send queue.
//!
//! Sans-IO: inputs are method calls with the current time; outputs go to
//! the [`ZigbeeTransport`] and to the event outbox ([`HubEvent`]), which
//! the WebSocket layer ([`crate::api`]) turns into messages.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::{Map, Value as J, json};

use crate::device::*;
use crate::expr::{ExprCtx, ExprEnv, Value};
use crate::fingerprint::EndpointFp;
use crate::generic::{GenericInput, generic_entities};
use crate::gp::types::GpdId;
use crate::model::*;
use crate::resolve::{Catalog, FpEnv, HUB_FEATURES, MemoryCatalog, ResolveInput, resolve};
use crate::transport::{ApsDst, ApsIndication, ApsRequest, TxOptions, ZigbeeTransport};
use crate::values;
use crate::zcl::frame::{self as zf, global};
use crate::zcl::{
    Direction, FrameType, Status, TypedValue, ZclFrame, ZclHeader, ZclType, ZclValue, cluster as cl,
};
use crate::zdo::{self, ZdoMessage};

/// Hub configuration.
#[derive(Debug, Clone)]
pub struct HubConfig {
    /// Coordinator IEEE (bind destination, IAS CIE address).
    pub coordinator_ieee: u64,
    /// The hub's application endpoint.
    pub hub_endpoint: u8,
    /// Local time offset for Tuya time sync, s.
    pub tz_offset_s: i64,
    /// GP pre-commissioned group (13.1).
    pub gp_group: u16,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            coordinator_ieee: 0x00124b0000000001,
            hub_endpoint: 1,
            tz_offset_s: 0,
            gp_group: 0x0B84,
        }
    }
}

/// An API error (14.1).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApiError {
    /// `not_found`, `invalid_value`, `unsupported`, `timeout`, `zcl_status` …
    pub code: String,
    /// Human-readable message.
    pub message: String,
    /// ZCL status name for `zcl_status`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zcl_status: Option<String>,
}

impl ApiError {
    /// A new error.
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            zcl_status: None,
        }
    }
    /// A ZCL status error.
    pub fn zcl(st: Status) -> Self {
        Self {
            code: "zcl_status".into(),
            message: format!("device answered {}", st.name()),
            zcl_status: Some(st.name().into()),
        }
    }
}

/// Outbox entries (topics of 14.2, 14.3, 13.7).
#[derive(Debug, Clone, PartialEq)]
pub enum HubEvent {
    /// `device_added`.
    DeviceAdded(String),
    /// `device_updated`.
    DeviceUpdated(String),
    /// `device_removed`.
    DeviceRemoved(String),
    /// `interview_progress`.
    InterviewProgress {
        /// Device.
        device: String,
        /// Step name.
        step: String,
    },
    /// `resolution_changed`.
    ResolutionChanged {
        /// Device.
        device: String,
        /// New uids.
        added: Vec<String>,
        /// Orphaned uids (gone after a fingerprint change).
        orphaned: Vec<String>,
        /// Removed uids (gone after a definition change).
        removed: Vec<String>,
    },
    /// `entity_added`.
    EntityAdded(String),
    /// `entity_updated`.
    EntityUpdated(String),
    /// `entity_removed`.
    EntityRemoved(String),
    /// `entity_state` (the state message, 4.5).
    EntityState(J),
    /// `entity_event`.
    EntityEvent {
        /// Entity.
        uid: String,
        /// Event type.
        event_type: String,
        /// Attributes.
        attributes: Map<String, J>,
        /// Time, s.
        time: f64,
    },
    /// Deferred result of `entities/command`.
    CommandResult {
        /// Ticket.
        ticket: u64,
        /// `Ok("ok")` or the error.
        result: Result<String, ApiError>,
    },
    /// Network topic events (`permit_join` …), from the host core.
    Network(J),
    /// Events of a service outside the engine (the OTA server: topic
    /// `ota`).
    App {
        /// Topic.
        topic: &'static str,
        /// Body (with its own `topic` field naming the event).
        body: J,
    },
    /// [`HubEvent::App`] with the body as JSON text (no `Value` tree).
    AppText {
        /// Topic.
        topic: &'static str,
        /// Body: a JSON object with its own `topic` field.
        text: String,
    },
    /// GP topic events (13.7).
    Gp {
        /// `gp/commissioning`, `gp/pending`, `gp/commissioned`, `gp/failed`, `gp/binding`.
        kind: String,
        /// Body.
        body: J,
    },
}

/// What an outstanding request is for.
#[derive(Debug, Clone, PartialEq)]
pub enum Purpose {
    /// An interview step.
    Interview(InterviewStep),
    /// A configure op by key.
    Configure(String),
    /// An entity command.
    Command(u64),
    /// Active availability check.
    Check,
    /// `entities/refresh` or a poll.
    Refresh,
    /// A GP target capability probe.
    GpProbe(u16),
    /// No answer expected/tracked.
    None,
}

/// A configure op ready to run.
#[derive(Debug, Clone, PartialEq)]
pub struct OpRun {
    /// Idempotence key.
    pub key: String,
    /// What to do.
    pub kind: OpKind,
}

/// Configure op kinds.
#[derive(Debug, Clone, PartialEq)]
pub enum OpKind {
    /// An op that cannot be sent (its payload expression failed): recorded
    /// as failed with the reason, retried with the configure backoff.
    Invalid {
        /// Why.
        reason: String,
    },
    /// Bind_req to the hub.
    Bind {
        /// Endpoint.
        ep: u8,
        /// Cluster.
        cluster: u16,
    },
    /// Configure Reporting (≤ 3 records).
    Report {
        /// Endpoint.
        ep: u8,
        /// Cluster.
        cluster: u16,
        /// Manufacturer code.
        mfr: Option<u16>,
        /// Records.
        records: Vec<zf::ReportingConfig>,
    },
    /// Read (≤ 5 attributes; the Tuya magic packet has 6).
    Read {
        /// Endpoint.
        ep: u8,
        /// Cluster.
        cluster: u16,
        /// Manufacturer code.
        mfr: Option<u16>,
        /// Attributes.
        attrs: Vec<u16>,
    },
    /// Write one attribute.
    Write {
        /// Endpoint.
        ep: u8,
        /// Cluster.
        cluster: u16,
        /// Manufacturer code.
        mfr: Option<u16>,
        /// Attribute.
        attr: u16,
        /// Value.
        value: TypedValue,
    },
    /// Cluster command.
    Cmd {
        /// Endpoint.
        ep: u8,
        /// Cluster.
        cluster: u16,
        /// Manufacturer code.
        mfr: Option<u16>,
        /// Command.
        id: u8,
        /// Payload.
        payload: Vec<u8>,
        /// Fire and forget (no Default Response requested).
        no_rsp: bool,
    },
    /// Hub group membership.
    Group {
        /// Hub endpoint.
        ep: u8,
        /// Group.
        group: u16,
    },
}

#[derive(Debug, Clone)]
struct Pending {
    device: String,
    purpose: Purpose,
    sent_at: Millis,
    retries: u8,
    req: ApsRequest,
}

#[derive(Debug, Clone, Default)]
struct Ticket {
    remaining: u32,
    error: Option<ApiError>,
    optimistic: Vec<(String, Option<String>, Value)>,
    tuya_dps: Vec<(String, u8)>,
    deadline: Millis,
}

/// Dispatch key (2, rule 2: table-driven frame dispatch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DKey {
    /// A ZCL attribute (also struct attributes).
    Attr(u8, u16, u16, Option<u16>),
    /// A Tuya DP.
    Tuya(u8),
    /// A GP report attribute.
    GpAttr(u16, u16, Option<u16>),
    /// A GP command value binding.
    GpCmd(u8),
}

/// Decodes `(self-description) CBOR → fragment`; implemented by the `zdef`
/// adapter (decoder + verifier, 12.3 step 5).
pub trait SchemaDecoder {
    /// Decodes and verifies a fragment.
    fn decode(&self, cbor: &[u8]) -> Result<Definition, String>;
}

/// Timeouts.
const TX_TIMEOUT_MS: Millis = 8_000;
const TX_TIMEOUT_SLEEPY_MS: Millis = 20_000;
const AWAKE_AFTER_RX_MS: Millis = 5_000;
const AWAKE_AFTER_JOIN_MS: Millis = 30_000;
const MAINS_SILENCE_MS: Millis = 10 * 60_000;
const DEDUP_MS: Millis = 2_000;
const HOUR_S: u64 = 3600;

/// The engine.
pub struct Hub<T: ZigbeeTransport> {
    /// Transport to the NCP.
    pub transport: T,
    /// Configuration.
    pub cfg: HubConfig,
    pub(crate) catalog: Box<dyn Catalog>,
    pub(crate) user: MemoryCatalog,
    /// Source texts of user definitions by id: `(format, text)`; the hub
    /// never parses them (D15), it keeps them for the editor.
    pub user_sources: BTreeMap<String, (String, String)>,
    pub(crate) devices: BTreeMap<String, Device>,
    pub(crate) dispatch: BTreeMap<String, BTreeMap<DKey, Vec<(usize, Option<usize>)>>>,
    nwk_index: BTreeMap<u16, String>,
    zcl_tsn: u8,
    zdo_tsn: u8,
    pending: BTreeMap<(bool, u8), Pending>,
    tickets: BTreeMap<u64, Ticket>,
    next_ticket: u64,
    pub(crate) events: Vec<HubEvent>,
    schema_decoder: Option<Box<dyn SchemaDecoder>>,
    pub(crate) schema_cache: BTreeMap<Vec<u8>, Arc<Definition>>,
    pub(crate) gp: crate::gp::GpHubState,
    /// The GP frame being processed (for event `bindings` attributes).
    pub(crate) gp_last_frame: Option<(GpdId, u8, Vec<u8>)>,
    /// Features advertised and accepted (8.5).
    pub features: Vec<&'static str>,
    /// Cluster commands devices send to the hub's server side of these
    /// clusters go to the application ([`Hub::take_app_frames`]) instead
    /// of the engine: no default response, no events. The OTA server
    /// (0x0019) lives in the application.
    pub app_clusters: Vec<u16>,
    app_frames: Vec<AppFrame>,
    /// Snapshots of the topics of services outside the engine, sent on
    /// `subscribe` (kept current by the application).
    pub app_snapshots: BTreeMap<&'static str, String>,
}

/// A cluster command for the application (see [`Hub::app_clusters`]).
#[derive(Debug, Clone)]
pub struct AppFrame {
    /// Device.
    pub device: String,
    /// Source endpoint.
    pub ep: u8,
    /// Cluster.
    pub cluster: u16,
    /// ZCL header.
    pub header: ZclHeader,
    /// Payload after the header.
    pub payload: Vec<u8>,
}

/// The ExprEnv of one device.
pub(crate) struct DevEnv<'a> {
    pub dev: &'a Device,
    pub now: Millis,
    pub tz: i64,
}

impl ExprEnv for DevEnv<'_> {
    fn state(&self, k: &str) -> Value {
        self.dev.state_value(k)
    }
    fn opt(&self, k: &str) -> Value {
        if let Some(v) = self.dev.options.get(k) {
            return v.clone();
        }
        self.dev
            .entities
            .iter()
            .find_map(|e| match (&e.r.def.value.binding, e.r.def.key == k) {
                (Binding::Hub { default }, true) => Some(default.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }
    fn gpd(&self, f: &str) -> Value {
        FpEnv(&self.dev.fp).gpd(f)
    }
    fn fp(&self, f: &str) -> Value {
        FpEnv(&self.dev.fp).fp(f)
    }
    fn battery_percent(&self, mv: f64) -> Value {
        let curve = self
            .dev
            .resolution
            .device
            .battery
            .clone()
            .unwrap_or(BatteryCurve::Linear(2100.0, 3000.0));
        Value::Float(curve.percent(mv))
    }
    fn now(&self) -> i64 {
        (self.now / 1000) as i64
    }
    fn tz_offset(&self) -> i64 {
        self.tz
    }
}

/// `zb:<ieee>`.
pub fn zigbee_id(ieee: u64) -> String {
    format!("zb:{ieee:016x}")
}

impl<T: ZigbeeTransport> Hub<T> {
    /// A new engine.
    pub fn new(transport: T, cfg: HubConfig, catalog: Box<dyn Catalog>) -> Self {
        Self {
            transport,
            cfg,
            catalog,
            user: MemoryCatalog::default(),
            user_sources: BTreeMap::new(),
            devices: BTreeMap::new(),
            dispatch: BTreeMap::new(),
            nwk_index: BTreeMap::new(),
            zcl_tsn: 0,
            zdo_tsn: 0,
            pending: BTreeMap::new(),
            tickets: BTreeMap::new(),
            next_ticket: 1,
            events: Vec::new(),
            schema_decoder: None,
            schema_cache: BTreeMap::new(),
            gp: Default::default(),
            gp_last_frame: None,
            features: HUB_FEATURES.to_vec(),
            app_clusters: Vec::new(),
            app_frames: Vec::new(),
            app_snapshots: BTreeMap::new(),
        }
    }

    /// Takes the frames of [`Hub::app_clusters`].
    pub fn take_app_frames(&mut self) -> Vec<AppFrame> {
        std::mem::take(&mut self.app_frames)
    }

    /// Sends a ZCL frame of the application to a device endpoint from the
    /// hub's endpoint (queued for a sleepy device until it is awake); no
    /// response is tracked.
    pub fn send_app_zcl(
        &mut self,
        dev_id: &str,
        ep: u8,
        cluster: u16,
        frame: ZclFrame,
        now: Millis,
    ) {
        self.send_zcl(dev_id, ep, cluster, frame, Purpose::None, now);
    }

    /// A ZCL transaction sequence number from the engine's counter.
    pub fn new_tsn(&mut self) -> u8 {
        self.next_zcl_tsn()
    }

    /// An event of a service outside the engine on `topic`.
    pub fn app_event(&mut self, topic: &'static str, body: J) {
        self.emit(HubEvent::App { topic, body });
    }

    /// [`Self::app_event`] with the body as JSON text.
    pub fn app_event_text(&mut self, topic: &'static str, text: String) {
        self.emit(HubEvent::AppText { topic, text });
    }

    /// Installs the self-description decoder (zdef).
    pub fn set_schema_decoder(&mut self, d: Box<dyn SchemaDecoder>) {
        self.schema_decoder = Some(d);
    }

    /// Takes the outbox.
    pub fn drain_events(&mut self) -> Vec<HubEvent> {
        std::mem::take(&mut self.events)
    }

    /// All devices.
    pub fn devices(&self) -> impl Iterator<Item = &Device> {
        self.devices.values()
    }

    /// A device.
    pub fn device(&self, id: &str) -> Option<&Device> {
        self.devices.get(id)
    }

    /// Mutable device (tests, API updates).
    pub fn device_mut(&mut self, id: &str) -> Option<&mut Device> {
        self.devices.get_mut(id)
    }

    /// Entity by uid.
    pub fn entity(&self, uid: &str) -> Option<(&Device, &Entity)> {
        let dev = self.devices.get(device_of_uid(uid))?;
        dev.entities.iter().find(|e| e.uid == uid).map(|e| (dev, e))
    }

    /// User definitions.
    pub fn user_definitions(&self) -> &MemoryCatalog {
        &self.user
    }

    /// Catalog.
    pub fn catalog(&self) -> &dyn Catalog {
        self.catalog.as_ref()
    }

    /// Mutable catalog (the store adds fetched definitions; downcast with
    /// [`Catalog::as_any_mut`]). Call [`Hub::reresolve`] after a change.
    pub fn catalog_mut(&mut self) -> &mut dyn Catalog {
        self.catalog.as_mut()
    }

    /// Replaces the catalog (an installed bank) and re-resolves every
    /// device (3.4); returns what changed per device.
    pub fn set_catalog(&mut self, c: Box<dyn Catalog>, now: Millis) -> Vec<serde_json::Value> {
        self.catalog = c;
        self.reresolve(None, now)
    }

    /// Re-resolves `ids` (all devices for `None`) and reconfigures those
    /// whose layers changed; returns `{device, from, to, added, orphaned}`
    /// for each changed device (`from`/`to`: definition IDs above generic).
    pub fn reresolve(&mut self, ids: Option<&[String]>, now: Millis) -> Vec<serde_json::Value> {
        let ids: Vec<String> = match ids {
            Some(v) => v.to_vec(),
            None => self.devices.keys().cloned().collect(),
        };
        let mut report = Vec::new();
        for id in ids {
            let Some(d) = self.devices.get(&id) else {
                continue;
            };
            let layer_ids = |d: &Device| -> Vec<String> {
                d.resolution
                    .layers
                    .iter()
                    .filter(|l| l.source != LayerSource::Generic)
                    .map(|l| l.id.clone())
                    .collect()
            };
            let before = d.resolution.layers.clone();
            let from = layer_ids(d);
            // Vectors, not sets: a collected set brings a sort (flash).
            let live = |d: &Device| -> Vec<String> {
                d.entities
                    .iter()
                    .filter(|e| !e.orphaned)
                    .map(|e| e.uid.clone())
                    .collect()
            };
            let old_uids = live(d);
            self.resolve_device(&id, now);
            let Some(d) = self.devices.get(&id) else {
                continue;
            };
            if d.resolution.layers == before {
                continue;
            }
            let new_uids = live(d);
            let (removed, orphaned): (Vec<&String>, Vec<&String>) = old_uids
                .iter()
                .filter(|u| !new_uids.contains(u))
                .partition(|u| d.entity_index(u).is_none());
            report.push(serde_json::json!({
                "device": id,
                "from": from,
                "to": layer_ids(d),
                "added": new_uids.iter().filter(|u| !old_uids.contains(u)).collect::<Vec<_>>(),
                "orphaned": orphaned,
                "removed": removed,
            }));
            self.redo_configure(&id, now);
        }
        report
    }

    pub(crate) fn next_zcl_tsn(&mut self) -> u8 {
        self.zcl_tsn = self.zcl_tsn.wrapping_add(1);
        self.zcl_tsn
    }

    pub(crate) fn next_zdo_tsn(&mut self) -> u8 {
        self.zdo_tsn = self.zdo_tsn.wrapping_add(1);
        self.zdo_tsn
    }

    /// Maps a NWK address to a device (restore).
    pub(crate) fn index_nwk(&mut self, nwk: u16, id: &str) {
        self.nwk_index.insert(nwk, id.to_string());
    }

    /// A `network` topic event from the host core, e.g.
    /// `{"topic": "permit_join", "remaining_s": 180}`.
    pub fn network_event(&mut self, body: J) {
        self.emit(HubEvent::Network(body));
    }

    pub(crate) fn emit(&mut self, e: HubEvent) {
        self.events.push(e);
    }

    // ------------------------------------------------------------------
    // Sending.

    /// Builds a ZCL request from the hub to a device endpoint.
    pub(crate) fn zcl_request(
        &self,
        dev: &Device,
        ep: u8,
        cluster: u16,
        frame: &ZclFrame,
    ) -> ApsRequest {
        let profile = dev.fp.endpoint(ep).map_or(crate::zcl::PROFILE_HA, |e| {
            if e.profile == crate::zcl::PROFILE_ZLL {
                crate::zcl::PROFILE_HA
            } else {
                e.profile
            }
        });
        ApsRequest {
            dst: ApsDst::Nwk(dev.nwk().unwrap_or(0xFFFE)),
            dst_ep: ep,
            src_ep: self.cfg.hub_endpoint,
            profile,
            cluster,
            payload: frame.encode(),
            options: TxOptions {
                ack: true,
                extended_timeout: dev.sleepy(),
            },
        }
    }

    /// Sends (or queues for a sleepy device) and tracks the response.
    pub(crate) fn send_tracked(
        &mut self,
        dev_id: &str,
        req: ApsRequest,
        purpose: Purpose,
        tsn: u8,
        zdo: bool,
        now: Millis,
    ) {
        let Some(dev) = self.devices.get_mut(dev_id) else {
            return;
        };
        if dev.sleepy() && now > dev.awake_until {
            dev.queue.push_back(Queued { req, purpose, tsn });
            return;
        }
        self.transmit(dev_id, req, purpose, tsn, zdo, now);
    }

    fn transmit(
        &mut self,
        dev_id: &str,
        req: ApsRequest,
        purpose: Purpose,
        tsn: u8,
        zdo: bool,
        now: Millis,
    ) {
        if self.transport.aps_send(req.clone()).is_err() {
            self.fail_purpose(
                dev_id,
                &purpose,
                ApiError::new("busy", "transport refused the request"),
                now,
            );
            return;
        }
        if purpose != Purpose::None {
            self.pending.insert(
                (zdo, tsn),
                Pending {
                    device: dev_id.to_string(),
                    purpose,
                    sent_at: now,
                    retries: 0,
                    req,
                },
            );
        }
    }

    /// Sends a ZCL frame to a device endpoint.
    pub(crate) fn send_zcl(
        &mut self,
        dev_id: &str,
        ep: u8,
        cluster: u16,
        frame: ZclFrame,
        purpose: Purpose,
        now: Millis,
    ) {
        let Some(dev) = self.devices.get(dev_id) else {
            return;
        };
        let req = self.zcl_request(dev, ep, cluster, &frame);
        let tsn = frame.header.tsn;
        self.send_tracked(dev_id, req, purpose, tsn, false, now);
    }

    pub(crate) fn send_zdo(
        &mut self,
        dev_id: &str,
        cluster: u16,
        payload: Vec<u8>,
        purpose: Purpose,
        now: Millis,
    ) {
        let Some(dev) = self.devices.get(dev_id) else {
            return;
        };
        let tsn = payload[0];
        let req = ApsRequest {
            dst: ApsDst::Nwk(dev.nwk().unwrap_or(0xFFFE)),
            dst_ep: 0,
            src_ep: 0,
            profile: 0,
            cluster,
            payload,
            options: TxOptions {
                ack: true,
                extended_timeout: dev.sleepy() || dev.node_desc.is_none(),
            },
        };
        self.send_tracked(dev_id, req, purpose, tsn, true, now);
    }

    pub(crate) fn read_attrs(
        &mut self,
        dev_id: &str,
        ep: u8,
        cluster: u16,
        mfr: Option<u16>,
        attrs: &[u16],
        purpose: Purpose,
        now: Millis,
    ) {
        let tsn = self.next_zcl_tsn();
        let f = ZclFrame::command(
            FrameType::Global,
            tsn,
            global::READ_ATTRIBUTES,
            mfr,
            zf::read_attributes(attrs),
        );
        self.send_zcl(dev_id, ep, cluster, f, purpose, now);
    }

    fn flush_queue(&mut self, dev_id: &str, now: Millis) {
        let Some(dev) = self.devices.get_mut(dev_id) else {
            return;
        };
        let q: Vec<Queued> = dev.queue.drain(..).collect();
        for item in q {
            let zdo = item.req.profile == 0;
            self.transmit(dev_id, item.req, item.purpose, item.tsn, zdo, now);
        }
    }

    // ------------------------------------------------------------------
    // Join, announce, interview.

    /// `EVT_DEVICE_JOINED` with an admitted decision.
    pub fn on_device_joined(&mut self, ieee: u64, nwk: u16, now: Millis) {
        let id = zigbee_id(ieee);
        if let Some(d) = self.devices.get_mut(&id) {
            if let DeviceKind::Zigbee { nwk: n, .. } = &mut d.kind {
                *n = nwk;
            }
            d.awake_until = now + AWAKE_AFTER_JOIN_MS;
            self.nwk_index.insert(nwk, id.clone());
            self.on_rejoin(&id, now);
            return;
        }
        let mut d = Device::new(id.clone(), DeviceKind::Zigbee { ieee, nwk }, now);
        d.awake_until = now + AWAKE_AFTER_JOIN_MS;
        self.devices.insert(id.clone(), d);
        self.nwk_index.insert(nwk, id.clone());
        self.emit(HubEvent::DeviceAdded(id.clone()));
        self.interview_step(&id, InterviewStep::NodeDesc, now);
    }

    /// Restarts the interview (`devices/reinterview`).
    pub fn reinterview(&mut self, id: &str, now: Millis) -> Result<(), ApiError> {
        let d = self
            .devices
            .get_mut(id)
            .ok_or_else(|| ApiError::new("not_found", format!("no device {id}")))?;
        if d.gp.is_some() {
            return Err(ApiError::new(
                "unsupported",
                "GP devices are not interviewed",
            ));
        }
        d.interview_partial = false;
        d.fp.endpoints.clear();
        d.awake_until = d.awake_until.max(now + AWAKE_AFTER_RX_MS);
        self.interview_step(id, InterviewStep::NodeDesc, now);
        Ok(())
    }

    fn on_rejoin(&mut self, id: &str, now: Millis) {
        let Some(d) = self.devices.get(id) else {
            return;
        };
        let interview = d.interview;
        let tuya_announce = d
            .resolution
            .tuya
            .as_ref()
            .is_some_and(|t| t.query_on_announce);
        let schema_ep = schema_endpoint(d);
        if interview != InterviewStep::Done && interview != InterviewStep::Configure {
            self.interview_step(id, interview, now);
            return;
        }
        if tuya_announce {
            self.tuya_query(id, now);
        }
        if let Some(ep) = schema_ep {
            self.read_attrs(
                id,
                ep,
                cl::ZRS_SCHEMA,
                Some(0x6D6B),
                &[0x0000, 0x0001, 0x0002, 0x0003],
                Purpose::Interview(InterviewStep::SchemaHeader),
                now,
            );
        }

        self.gp_target_announced(id, now);
        self.pump_configure(id, now);
    }

    fn progress(&mut self, id: &str, step: InterviewStep) {
        if let Some(d) = self.devices.get_mut(id) {
            d.interview = step;
        }
        let name = match step {
            InterviewStep::NodeDesc => "node_descriptor".to_string(),
            InterviewStep::ActiveEp => "active_endpoints".into(),
            InterviewStep::SimpleDesc(i) => format!("simple_descriptor_{i}"),
            InterviewStep::Basic(i) => format!("basic_{i}"),
            InterviewStep::SchemaHeader => "self_description".into(),
            InterviewStep::SchemaChunk(i) => format!("self_description_chunk_{i}"),
            InterviewStep::Configure => "configure".into(),
            InterviewStep::Done => "done".into(),
        };
        self.emit(HubEvent::InterviewProgress {
            device: id.to_string(),
            step: name,
        });
    }

    const BASIC_BATCHES: [&'static [u16]; 2] = [
        &[0x0004, 0x0005, 0x0007, 0x0000, 0x0001],
        &[0x0002, 0x0003, 0x0006, 0x4000],
    ];

    fn interview_step(&mut self, id: &str, step: InterviewStep, now: Millis) {
        self.progress(id, step);
        let Some(d) = self.devices.get(id) else {
            return;
        };
        let nwk = d.nwk().unwrap_or(0);
        match step {
            InterviewStep::NodeDesc => {
                let tsn = self.next_zdo_tsn();
                self.send_zdo(
                    id,
                    zdo::cluster::NODE_DESC_REQ,
                    zdo::nwk_req(tsn, nwk),
                    Purpose::Interview(step),
                    now,
                );
            }
            InterviewStep::ActiveEp => {
                let tsn = self.next_zdo_tsn();
                self.send_zdo(
                    id,
                    zdo::cluster::ACTIVE_EP_REQ,
                    zdo::nwk_req(tsn, nwk),
                    Purpose::Interview(step),
                    now,
                );
            }
            InterviewStep::SimpleDesc(i) => {
                let eps: Vec<u8> = d
                    .active_eps
                    .iter()
                    .copied()
                    .filter(|e| *e != crate::gp::GP_ENDPOINT)
                    .collect();
                match eps.get(i) {
                    Some(&ep) => {
                        let tsn = self.next_zdo_tsn();
                        self.send_zdo(
                            id,
                            zdo::cluster::SIMPLE_DESC_REQ,
                            zdo::simple_desc_req(tsn, nwk, ep),
                            Purpose::Interview(step),
                            now,
                        );
                    }
                    None => self.interview_step(id, InterviewStep::Basic(0), now),
                }
            }
            InterviewStep::Basic(i) => {
                let ep =
                    d.fp.endpoints
                        .iter()
                        .find(|e| e.in_clusters.contains(&cl::BASIC))
                        .map(|e| e.ep);
                match (ep, Self::BASIC_BATCHES.get(i)) {
                    (Some(ep), Some(attrs)) => self.read_attrs(
                        id,
                        ep,
                        cl::BASIC,
                        None,
                        attrs,
                        Purpose::Interview(step),
                        now,
                    ),
                    _ => {
                        self.resolve_device(id, now);
                        self.after_basic(id, now);
                    }
                }
            }
            InterviewStep::SchemaHeader => match schema_endpoint(d) {
                Some(ep) => self.read_attrs(
                    id,
                    ep,
                    cl::ZRS_SCHEMA,
                    Some(0x6D6B),
                    &[0x0000, 0x0001, 0x0002, 0x0003],
                    Purpose::Interview(step),
                    now,
                ),
                None => self.start_configure(id, now),
            },
            InterviewStep::SchemaChunk(i) => {
                if let Some(ep) = schema_endpoint(d) {
                    self.read_attrs(
                        id,
                        ep,
                        cl::ZRS_SCHEMA,
                        Some(0x6D6B),
                        &[0x0100 + i],
                        Purpose::Interview(step),
                        now,
                    );
                }
            }
            InterviewStep::Configure => self.start_configure(id, now),
            InterviewStep::Done => {}
        }
    }

    fn after_basic(&mut self, id: &str, now: Millis) {
        let has_schema = self
            .devices
            .get(id)
            .is_some_and(|d| schema_endpoint(d).is_some());
        if has_schema {
            self.interview_step(id, InterviewStep::SchemaHeader, now);
        } else {
            self.start_configure(id, now);
        }
    }

    fn handle_zdo(&mut self, id: &str, ind: &ApsIndication, now: Millis) {
        let Ok(msg) = ZdoMessage::parse(ind.cluster, &ind.payload) else {
            return;
        };
        if let ZdoMessage::DeviceAnnce {
            nwk,
            ieee,
            capability,
            ..
        } = msg
        {
            let did = zigbee_id(ieee);
            if self.devices.contains_key(&did) {
                if let Some(DeviceKind::Zigbee { nwk: n, .. }) =
                    self.devices.get_mut(&did).map(|d| &mut d.kind)
                {
                    *n = nwk;
                }
                self.nwk_index.insert(nwk, did.clone());
                if let Some(d) = self.devices.get_mut(&did) {
                    d.awake_until = now + AWAKE_AFTER_RX_MS;
                    if capability & 0x08 == 0 {
                        d.fp.rx_on_when_idle = Some(false);
                    }
                }
                self.on_rejoin(&did, now);
            } else {
                self.on_device_joined(ieee, nwk, now);
            }
            return;
        }
        let Some(p) = self.pending.remove(&(true, msg.tsn())) else {
            return;
        };
        if p.device != id {
            self.pending.insert((true, msg.tsn()), p);
            return;
        }
        let Purpose::Interview(step) = p.purpose else {
            if let Purpose::Configure(key) = p.purpose {
                let ok = matches!(msg, ZdoMessage::Status { status: 0, .. });
                self.configure_done(
                    id,
                    &key,
                    ok,
                    if ok {
                        "SUCCESS".into()
                    } else {
                        "bind failed".into()
                    },
                    now,
                );
            }
            return;
        };
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        match (step, msg) {
            (InterviewStep::NodeDesc, ZdoMessage::NodeDesc { desc: Some(nd), .. }) => {
                d.fp.mfr_code = Some(nd.manufacturer_code);
                d.fp.logical_type = Some(nd.logical_type);
                d.fp.rx_on_when_idle = Some(nd.rx_on_when_idle());
                d.fp.mac_mains = Some(nd.mains_powered());
                d.node_desc = Some(nd);
                self.interview_step(id, InterviewStep::ActiveEp, now);
            }
            (InterviewStep::ActiveEp, ZdoMessage::ActiveEp { endpoints, .. }) => {
                d.active_eps = endpoints;
                d.fp.endpoints.clear();
                self.interview_step(id, InterviewStep::SimpleDesc(0), now);
            }
            (InterviewStep::SimpleDesc(i), ZdoMessage::SimpleDesc { desc, .. }) => {
                if let Some(sd) = desc {
                    d.fp.endpoints.retain(|e| e.ep != sd.endpoint);
                    d.fp.endpoints.push(EndpointFp {
                        ep: sd.endpoint,
                        profile: sd.profile,
                        device_id: sd.device_id,
                        in_clusters: sd.in_clusters,
                        out_clusters: sd.out_clusters,
                    });
                } else {
                    d.interview_partial = true;
                }
                self.interview_step(id, InterviewStep::SimpleDesc(i + 1), now);
            }
            (step, _) => {
                // Failed status: continue with what is known.
                d.interview_partial = true;
                let next = match step {
                    InterviewStep::NodeDesc => InterviewStep::ActiveEp,
                    InterviewStep::ActiveEp => InterviewStep::Basic(0),
                    InterviewStep::SimpleDesc(i) => InterviewStep::SimpleDesc(i + 1),
                    s => s,
                };
                self.interview_step(id, next, now);
            }
        }
    }

    // ------------------------------------------------------------------
    // Resolution and materialization.

    /// Re-resolves one device and materializes the result (3.4).
    pub fn resolve_device(&mut self, id: &str, now: Millis) {
        let Some(d) = self.devices.get(id) else {
            return;
        };
        let fp = d.fp.clone();
        let gp_seen = d.gp.is_some();
        let res = {
            let attrs = &d.attrs;
            let gen_zb = |power: PowerKind| generic_entities(&fp, &GenericInput { attrs, power });
            let gen_gp = |_p: PowerKind| crate::gp::generic::gp_generic_entities(&fp, attrs);
            let generic: &dyn Fn(PowerKind) -> Vec<EntityDef> =
                if gp_seen { &gen_gp } else { &gen_zb };
            let sd = d.schema.fragment.clone();
            resolve(&ResolveInput {
                fp: &fp,
                generic,
                catalog: self.catalog.as_ref(),
                user: &self.user,
                self_description: sd.as_deref(),
                pinned: d.pinned.as_deref(),
                features: &self.features,
            })
        };
        self.materialize(id, res, now);
    }

    fn materialize(&mut self, id: &str, res: crate::resolve::Resolution, now: Millis) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        let old_layers = d.resolution.layers.clone();
        let mut old: BTreeMap<String, Entity> =
            d.entities.drain(..).map(|e| (e.uid.clone(), e)).collect();
        let mut added = Vec::new();
        let mut revived = Vec::new();
        let mut updated = Vec::new();
        let mut entities = Vec::new();
        for r in &res.entities {
            let uid = format!("{}:{}:{}", d.id, r.ep, r.def.key);
            match old.remove(&uid) {
                Some(mut e) => {
                    let before = e.record(&d.id);
                    if e.orphaned {
                        revived.push(uid.clone());
                    }
                    e.r = r.clone();
                    e.orphaned = false;
                    if e.record(&d.id) != before {
                        updated.push(uid.clone());
                    }
                    entities.push(e);
                }
                None => {
                    let mut st = EntityState::default();
                    if let Binding::Hub { default } = &r.def.value.binding {
                        st.value = d
                            .options
                            .get(&r.def.key)
                            .cloned()
                            .unwrap_or_else(|| default.clone());
                    }
                    added.push(uid.clone());
                    entities.push(Entity {
                        uid,
                        r: r.clone(),
                        state: st,
                        orphaned: false,
                        user_name: None,
                        user_enabled: None,
                    });
                }
            }
        }
        // With the same fingerprint an entity vanishes because the
        // definitions changed (a catalog installed, a user definition, a
        // pin): it is gone for good, so it is removed (HA drops it). After a
        // re-interview that changed the fingerprint it is kept as an orphan
        // (its endpoint may come back).
        let same_fp = d.fp_hash == Some(d.fp.hash());
        let mut orphaned = Vec::new();
        let mut removed = Vec::new();
        for (uid, mut e) in old {
            if same_fp && !e.orphaned {
                removed.push(uid);
                continue;
            }
            if !e.orphaned {
                e.orphaned = true;
                orphaned.push(uid.clone());
            }
            entities.push(e);
        }
        d.entities = entities;
        d.fp_hash = Some(d.fp.hash());
        let layers_changed = old_layers != res.layers;
        d.resolution = res;
        let dispatch = build_dispatch(d);
        self.dispatch.insert(id.to_string(), dispatch);
        for u in &added {
            self.emit(HubEvent::EntityAdded(u.clone()));
        }
        for u in updated.iter().chain(&orphaned) {
            self.emit(HubEvent::EntityUpdated(u.clone()));
        }
        for u in &removed {
            self.emit(HubEvent::EntityRemoved(u.clone()));
        }
        if !added.is_empty() || !orphaned.is_empty() || !removed.is_empty() || layers_changed {
            self.emit(HubEvent::ResolutionChanged {
                device: id.to_string(),
                added: added.clone(),
                orphaned,
                removed,
            });
            self.emit(HubEvent::DeviceUpdated(id.to_string()));
        }
        // New entities start from what the hub already knows (a read that
        // made an optional attribute appear, a GP report that created it).
        let d = &self.devices[id];
        let mut work = Vec::new();
        for (i, e) in d.entities.iter().enumerate() {
            if !added.contains(&e.uid) && !revived.contains(&e.uid) {
                continue;
            }
            let vals = std::iter::once((None, &e.r.def.value)).chain(
                e.r.def
                    .parts
                    .iter()
                    .enumerate()
                    .map(|(k, p)| (Some(k), &p.1)),
            );
            for (pi, vd) in vals {
                let key = match &vd.binding {
                    Binding::Zcl(z) => Some((e.r.ep, z.cluster, z.attr, z.mfr)),
                    Binding::Gp(GpBinding::Report {
                        cluster, attr, mfr, ..
                    }) => Some((e.r.ep, *cluster, *attr, *mfr)),
                    _ => None,
                };
                if let Some(v) = key
                    .and_then(|k| d.attrs.map.get(&k))
                    .and_then(|a| a.value.as_ref())
                {
                    work.push((i, pi, values::from_zcl(v)));
                }
            }
        }
        let seeded: BTreeSet<usize> = work.iter().map(|w| w.0).collect();
        let unseeded: Vec<String> = d
            .entities
            .iter()
            .enumerate()
            .filter(|(i, e)| added.contains(&e.uid) && !seeded.contains(i))
            .map(|(_, e)| e.uid.clone())
            .collect();
        self.apply_raw(id, work, now);
        for u in unseeded {
            self.publish_state(&u);
        }
    }

    /// Emits an `entity_state` for a uid.
    pub(crate) fn publish_state(&mut self, uid: &str) {
        let Some((d, e)) = self.entity(uid) else {
            return;
        };
        let msg = e.state_msg(d.available);
        self.emit(HubEvent::EntityState(msg));
    }

    // ------------------------------------------------------------------
    // Indications.

    /// `EVT_APS_DATA_INDICATION`.
    pub fn on_aps_indication(&mut self, ind: ApsIndication, now: Millis) {
        let id = match ind
            .src_ieee
            .map(zigbee_id)
            .filter(|i| self.devices.contains_key(i))
        {
            Some(i) => Some(i),
            None => self.nwk_index.get(&ind.src_nwk).cloned(),
        };
        if ind.profile == 0 && ind.cluster == zdo::cluster::DEVICE_ANNCE {
            self.handle_zdo(id.as_deref().unwrap_or(""), &ind, now);
            return;
        }
        let Some(id) = id else { return };
        self.seen(&id, Some(ind.lqi), Some(ind.rssi), now);
        if let Some(d) = self.devices.get_mut(&id) {
            d.awake_until = now + AWAKE_AFTER_RX_MS;
            d.diag.frame(now, ind.cluster, &ind.payload);
        }
        self.flush_queue(&id, now);
        if ind.profile == 0 {
            self.handle_zdo(&id, &ind, now);
        } else {
            self.handle_zcl(&id, &ind, now);
        }
        self.pump_configure(&id, now);
    }

    /// Any frame from a device: last_seen, link quality, availability.
    pub(crate) fn seen(&mut self, id: &str, lqi: Option<u8>, rssi: Option<i8>, now: Millis) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        let was = d.available;
        // Throttled against the last publication, not the previous frame: a
        // device sending more than once a minute would never be published.
        let minute_passed = d
            .last_seen_published
            .is_none_or(|t| now.saturating_sub(t) >= 60_000);
        if minute_passed || !was {
            d.last_seen_published = Some(now);
        }
        d.last_seen = Some(now);
        if lqi.is_some() {
            d.lqi = lqi;
        }
        if rssi.is_some() {
            d.rssi = rssi;
        }
        d.available = true;
        d.check = None;
        let mut fields = vec![
            (
                DeviceField::Linkquality,
                d.lqi.map_or(Value::Null, |v| Value::Int(i64::from(v))),
            ),
            (
                DeviceField::Rssi,
                d.rssi.map_or(Value::Null, |v| Value::Int(i64::from(v))),
            ),
        ];
        if minute_passed || !was {
            fields.push((DeviceField::LastSeen, Value::Float(ts(now))));
        }
        let mut touched = Vec::new();
        for (f, v) in fields {
            for i in 0..d.entities.len() {
                if !d.entities[i].orphaned
                    && matches!(d.entities[i].r.def.value.binding, Binding::DeviceField(x) if x == f)
                {
                    set_value(d, i, None, v.clone(), now);
                    touched.push(d.entities[i].uid.clone());
                }
            }
        }
        if !was || minute_passed {
            self.emit(HubEvent::DeviceUpdated(id.to_string()));
        }
        if !was {
            let uids: Vec<String> = self.devices[id]
                .entities
                .iter()
                .map(|e| e.uid.clone())
                .collect();
            for u in uids {
                self.publish_state(&u);
            }
        } else {
            for u in touched {
                self.publish_state(&u);
            }
        }
    }

    fn handle_zcl(&mut self, id: &str, ind: &ApsIndication, now: Millis) {
        let Ok(frame) = ZclFrame::parse(&ind.payload) else {
            if let Some(d) = self.devices.get_mut(id) {
                d.diag.count("malformed_zcl");
            }
            return;
        };
        let h = frame.header;
        let ep = ind.src_ep;
        let mut needs_default_rsp = !h.disable_default_response && ind.group.is_none();
        match (h.frame_type, h.cmd) {
            (FrameType::Global, global::READ_ATTRIBUTES_RSP) => {
                needs_default_rsp = false;
                let recs = zf::parse_read_attributes_rsp(&frame.payload).unwrap_or_default();
                let purpose = self.take_pending(id, h.tsn).map(|p| p.purpose);
                let mut values = Vec::new();
                let mut structural = false;
                if let Some(d) = self.devices.get_mut(id) {
                    for r in &recs {
                        match (&r.value, r.status) {
                            (Some(v), _) => {
                                let (sc, vc) = d.attrs.set(
                                    ep,
                                    ind.cluster,
                                    r.attr,
                                    h.mfr,
                                    Some(v.value.clone()),
                                );
                                structural |= sc || (vc && is_structural(ind.cluster, r.attr));
                                values.push((r.attr, v.clone()));
                            }
                            (None, Status::UNSUPPORTED_ATTRIBUTE) => {
                                structural |= d.attrs.set(ep, ind.cluster, r.attr, h.mfr, None).0;
                            }
                            _ => {}
                        }
                    }
                }
                match purpose {
                    Some(Purpose::Interview(step)) => {
                        self.interview_read(id, step, ind.cluster, &recs, now)
                    }
                    Some(Purpose::Configure(key)) => {
                        self.configure_done(id, &key, true, "SUCCESS".into(), now)
                    }
                    Some(Purpose::GpProbe(nwk)) => self.gp_probe_result(nwk, &recs, now),
                    _ => {}
                }
                self.dispatch_attrs(id, ep, ind.cluster, h.mfr, &values, now);
                if structural {
                    self.maybe_reresolve(id, now);
                }
            }
            (FrameType::Global, global::REPORT_ATTRIBUTES) => {
                let recs = zf::parse_report_attributes(&frame.payload).unwrap_or_default();
                let mut structural = false;
                if let Some(d) = self.devices.get_mut(id) {
                    for (a, v) in &recs {
                        let (sc, vc) =
                            d.attrs
                                .set(ep, ind.cluster, *a, h.mfr, Some(v.value.clone()));
                        structural |= sc || (vc && is_structural(ind.cluster, *a));
                    }
                }
                self.dispatch_attrs(id, ep, ind.cluster, h.mfr, &recs, now);
                if structural {
                    self.maybe_reresolve(id, now);
                }
            }
            (FrameType::Global, global::WRITE_ATTRIBUTES_RSP) => {
                needs_default_rsp = false;
                let recs = zf::parse_write_attributes_rsp(&frame.payload).unwrap_or_default();
                let st = recs
                    .iter()
                    .map(|r| r.0)
                    .find(|s| *s != Status::SUCCESS)
                    .unwrap_or(Status::SUCCESS);
                self.complete_tsn(id, h.tsn, st, now);
            }
            (FrameType::Global, global::CONFIGURE_REPORTING_RSP) => {
                needs_default_rsp = false;
                let recs = zf::parse_configure_reporting_rsp(&frame.payload).unwrap_or_default();
                if let Some(d) = self.devices.get_mut(id) {
                    for (st, a) in &recs {
                        if let (Some(a), true) = (a, *st != Status::SUCCESS) {
                            if !d.configure.unreportable.contains(&(ep, ind.cluster, *a)) {
                                d.configure.unreportable.push((ep, ind.cluster, *a));
                                d.configure.poll_at.insert((ep, ind.cluster, *a), now);
                            }
                            d.diag.configure.insert(
                                format!("report:{ep}:0x{:04x}:0x{a:04x}", ind.cluster),
                                st.name().into(),
                            );
                        }
                    }
                }
                self.complete_tsn(id, h.tsn, Status::SUCCESS, now);
            }
            (FrameType::Global, global::DEFAULT_RSP) => {
                needs_default_rsp = false;
                if let Ok((_, st)) = zf::parse_default_rsp(&frame.payload) {
                    self.complete_tsn(id, h.tsn, st, now);
                }
            }
            (FrameType::Global, _) => needs_default_rsp = false,
            (FrameType::Cluster, _)
                if h.direction == Direction::ClientToServer
                    && self.app_clusters.contains(&ind.cluster) =>
            {
                self.app_frames.push(AppFrame {
                    device: id.to_string(),
                    ep,
                    cluster: ind.cluster,
                    header: h,
                    payload: frame.payload,
                });
                return;
            }
            (FrameType::Cluster, cmd) => {
                let handled = match ind.cluster {
                    cl::TUYA => {
                        needs_default_rsp = false;
                        self.handle_tuya(id, ep, &h, &frame.payload, now);
                        true
                    }
                    cl::IAS_ZONE if h.direction == Direction::ServerToClient && cmd == 0x00 => {
                        if frame.payload.len() >= 2 {
                            let zs = u16::from_le_bytes([frame.payload[0], frame.payload[1]]);
                            let tv = TypedValue::uint(ZclType::MAP16, u64::from(zs));
                            if let Some(d) = self.devices.get_mut(id) {
                                d.attrs
                                    .set(ep, cl::IAS_ZONE, 0x0002, None, Some(tv.value.clone()));
                            }
                            self.dispatch_attrs(id, ep, cl::IAS_ZONE, None, &[(0x0002, tv)], now);
                        }
                        true
                    }
                    cl::IAS_ZONE if h.direction == Direction::ServerToClient && cmd == 0x01 => {
                        needs_default_rsp = false;
                        let zone_id = self.zone_id(id);
                        let f = ZclFrame {
                            header: ZclHeader {
                                frame_type: FrameType::Cluster,
                                mfr: None,
                                direction: Direction::ClientToServer,
                                disable_default_response: true,
                                tsn: h.tsn,
                                cmd: 0x00,
                            },
                            payload: vec![0x00, zone_id],
                        };
                        self.send_zcl(id, ep, cl::IAS_ZONE, f, Purpose::None, now);
                        true
                    }
                    cl::POLL_CONTROL if cmd == 0x00 => {
                        needs_default_rsp = false;
                        self.check_in(id, ep, h.tsn, now);
                        true
                    }
                    _ => false,
                };
                if !handled {
                    self.dispatch_command(id, ep, ind.cluster, &frame, now);
                }
            }
        }
        if needs_default_rsp {
            let tsn = h.tsn;
            let f = ZclFrame {
                header: ZclHeader {
                    frame_type: FrameType::Global,
                    mfr: h.mfr,
                    direction: if h.direction == Direction::ClientToServer {
                        Direction::ServerToClient
                    } else {
                        Direction::ClientToServer
                    },
                    disable_default_response: true,
                    tsn,
                    cmd: global::DEFAULT_RSP,
                },
                payload: zf::default_rsp(h.cmd, Status::SUCCESS),
            };
            self.send_zcl(id, ep, ind.cluster, f, Purpose::None, now);
        }
    }

    fn zone_id(&self, id: &str) -> u8 {
        self.devices
            .keys()
            .position(|k| k == id)
            .map_or(0, |p| (p % 254) as u8)
    }

    fn check_in(&mut self, id: &str, ep: u8, tsn: u8, now: Millis) {
        let fast = self
            .devices
            .get(id)
            .is_some_and(|d| !d.queue.is_empty() || !d.configure.queue.is_empty());
        let f = ZclFrame {
            header: ZclHeader {
                frame_type: FrameType::Cluster,
                mfr: None,
                direction: Direction::ClientToServer,
                disable_default_response: true,
                tsn,
                cmd: 0x00,
            },
            payload: vec![u8::from(fast), 0x28, 0x00],
        };
        self.send_zcl(id, ep, cl::POLL_CONTROL, f, Purpose::None, now);
        self.flush_queue(id, now);
        self.pump_configure(id, now);
        if fast {
            let tsn = self.next_zcl_tsn();
            let mut stop = ZclFrame::command(FrameType::Cluster, tsn, 0x01, None, Vec::new());
            stop.header.disable_default_response = true;
            self.send_zcl(id, ep, cl::POLL_CONTROL, stop, Purpose::None, now);
        }
    }

    fn take_pending(&mut self, id: &str, tsn: u8) -> Option<Pending> {
        let p = self.pending.remove(&(false, tsn))?;
        if p.device == id {
            Some(p)
        } else {
            self.pending.insert((false, tsn), p);
            None
        }
    }

    fn complete_tsn(&mut self, id: &str, tsn: u8, st: Status, now: Millis) {
        let Some(p) = self.take_pending(id, tsn) else {
            return;
        };
        match p.purpose {
            Purpose::Command(t) => self.ticket_part_done(
                t,
                if st == Status::SUCCESS {
                    None
                } else {
                    Some(ApiError::zcl(st))
                },
                now,
            ),
            Purpose::Configure(key) => self.configure_done(id, &key, true, st.name().into(), now),
            _ => {}
        }
    }

    fn interview_read(
        &mut self,
        id: &str,
        step: InterviewStep,
        cluster: u16,
        recs: &[zf::ReadRecord],
        now: Millis,
    ) {
        match step {
            InterviewStep::Basic(i) if cluster == cl::BASIC => {
                if let Some(d) = self.devices.get_mut(id) {
                    for r in recs {
                        let Some(v) = &r.value else { continue };
                        let s = || match &v.value {
                            ZclValue::String(s) => {
                                Some(s.trim_end_matches(['\0', ' ']).to_string())
                            }
                            _ => None,
                        };
                        let n = v.value.as_i64().map(|x| x as u32);
                        match r.attr {
                            0x0004 => d.fp.manufacturer_name = s(),
                            0x0005 => d.fp.model_id = s(),
                            0x0007 => d.fp.power_source = n.map(|x| x as u8),
                            0x0000 => d.fp.zcl_version = n,
                            0x0001 => d.fp.app_version = n,
                            0x0002 => d.fp.stack_version = n,
                            0x0003 => d.fp.hw_version = n,
                            0x0006 => d.fp.date_code = s(),
                            0x4000 => d.fp.sw_build_id = s(),
                            _ => {}
                        }
                    }
                }
                self.interview_step(id, InterviewStep::Basic(i + 1), now);
            }
            InterviewStep::SchemaHeader => self.schema_header(id, recs, now),
            InterviewStep::SchemaChunk(i) => self.schema_chunk(id, i, recs, now),
            _ => {}
        }
    }

    // ------------------------------------------------------------------
    // Self-description (12.3).

    fn schema_header(&mut self, id: &str, recs: &[zf::ReadRecord], now: Millis) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        let get = |a: u16| {
            recs.iter()
                .find(|r| r.attr == a)
                .and_then(|r| r.value.as_ref())
                .map(|v| v.value.clone())
        };
        let format = get(0).and_then(|v| v.as_i64()).map(|v| v as u8);
        let hash = match get(1) {
            Some(ZclValue::Octets(b)) => Some(b),
            _ => None,
        };
        let length = get(2).and_then(|v| v.as_i64()).map(|v| v as u16);
        let chunk = get(3).and_then(|v| v.as_i64()).map(|v| v as u8);
        let was_configuring = matches!(d.interview, InterviewStep::Done | InterviewStep::Configure);
        let valid = format.is_some_and(|f| f & 0x0F == 1 && (f >> 4) & 0x03 <= 1)
            && hash.as_ref().is_some_and(|h| h.len() == 8);
        if !valid {
            d.schema.error = Some(format!("unsupported schema format {format:?}"));
            if !was_configuring {
                self.start_configure(id, now);
            }
            return;
        }
        let hash = hash.unwrap_or_default();
        d.fp.self_description = Some(crate::expr::hex(&hash));
        if d.schema.hash.as_ref() == Some(&hash) && d.schema.fragment.is_some() {
            if !was_configuring {
                self.resolve_device(id, now);
                self.start_configure(id, now);
            }
            return;
        }
        if let Some(frag) = self.schema_cache.get(&hash).cloned() {
            let d = self.devices.get_mut(id).unwrap();
            d.schema.hash = Some(hash);
            d.schema.fragment = Some(frag);
            self.resolve_device(id, now);
            if !was_configuring {
                self.start_configure(id, now);
            } else {
                self.redo_configure(id, now);
            }
            return;
        }
        d.schema = SchemaState {
            format,
            hash: Some(hash),
            length,
            chunk_size: chunk,
            ..Default::default()
        };
        if length.unwrap_or(0) == 0 || chunk.unwrap_or(0) == 0 {
            d.schema.error = Some("empty schema".into());
            if !was_configuring {
                self.start_configure(id, now);
            }
            return;
        }
        self.interview_step(id, InterviewStep::SchemaChunk(0), now);
    }

    fn schema_chunk(&mut self, id: &str, i: u16, recs: &[zf::ReadRecord], now: Millis) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        let data = recs
            .iter()
            .find(|r| r.attr == 0x0100 + i)
            .and_then(|r| r.value.as_ref())
            .and_then(|v| match &v.value {
                ZclValue::Octets(b) => Some(b.clone()),
                _ => None,
            });
        let Some(data) = data else {
            d.schema.retries += 1;
            if d.schema.retries > 3 {
                d.schema.error = Some(format!("chunk {i} failed"));
                self.start_configure(id, now);
            } else {
                self.interview_step(id, InterviewStep::SchemaChunk(i), now);
            }
            return;
        };
        d.schema.retries = 0;
        d.schema.chunks.insert(i, data);
        let total: usize = d.schema.chunks.values().map(Vec::len).sum();
        let len = usize::from(d.schema.length.unwrap_or(0));
        if total < len && i < 255 {
            self.interview_step(id, InterviewStep::SchemaChunk(i + 1), now);
            return;
        }
        let blob: Vec<u8> = d.schema.chunks.values().flatten().copied().collect();
        let deflated = d.schema.format.is_some_and(|f| (f >> 4) & 0x03 == 1);
        let result = (|| -> Result<Definition, String> {
            if blob.len() != len {
                return Err(format!("schema length {} != {len}", blob.len()));
            }
            let cbor = if deflated {
                miniz_oxide::inflate::decompress_to_vec_with_limit(&blob, 12 * 1024)
                    .map_err(|e| format!("inflate: {e:?}"))?
            } else {
                blob.clone()
            };
            use sha2::Digest;
            let h = sha2::Sha256::digest(&cbor);
            if Some(&h[..8]) != d.schema.hash.as_deref() {
                return Err("schema hash mismatch".into());
            }
            let dec = self
                .schema_decoder
                .as_ref()
                .ok_or("no zdef schema decoder installed")?;
            let mut frag = dec.decode(&cbor)?;
            frag.source = LayerSource::SelfDescription;
            frag.content_hash = Some(crate::expr::hex(&h));
            Ok(frag)
        })();
        let d = self.devices.get_mut(id).unwrap();
        match result {
            Ok(frag) => {
                let frag = Arc::new(frag);
                if let Some(h) = d.schema.hash.clone() {
                    self.schema_cache.insert(h, frag.clone());
                }
                let d = self.devices.get_mut(id).unwrap();
                d.schema.fragment = Some(frag);
                d.schema.chunks.clear();
                self.resolve_device(id, now);
            }
            Err(e) => {
                d.schema.error = Some(e);
                d.schema.chunks.clear();
            }
        }
        let configuring = matches!(self.devices[id].interview, InterviewStep::Done);
        if configuring {
            self.redo_configure(id, now);
        } else {
            self.start_configure(id, now);
        }
    }

    // ------------------------------------------------------------------
    // Dispatch.

    fn maybe_reresolve(&mut self, id: &str, now: Millis) {
        let interviewing = self.devices.get(id).is_some_and(|d| {
            matches!(
                d.interview,
                InterviewStep::NodeDesc
                    | InterviewStep::ActiveEp
                    | InterviewStep::SimpleDesc(_)
                    | InterviewStep::Basic(_)
            )
        });
        if !interviewing {
            self.resolve_device(id, now);
            self.redo_configure(id, now);
        }
    }

    pub(crate) fn dispatch_attrs(
        &mut self,
        id: &str,
        ep: u8,
        cluster: u16,
        mfr: Option<u16>,
        recs: &[(u16, TypedValue)],
        now: Millis,
    ) {
        let Some(table) = self.dispatch.get(id) else {
            return;
        };
        let mut work: Vec<(usize, Option<usize>, Value)> = Vec::new();
        let mut unknown = Vec::new();
        let d = &self.devices[id];
        for (attr, tv) in recs {
            let mut hit = false;
            if let Some(list) = table.get(&DKey::Attr(ep, cluster, *attr, mfr)) {
                for &(i, p) in list {
                    hit = true;
                    let e = &d.entities[i];
                    let vd = value_of(&e.r.def, p);
                    let raw = match &vd.binding {
                        Binding::Struct { tag, .. } => match &tv.value {
                            ZclValue::Octets(b) => crate::codec::decode_tagged(b)
                                .into_iter()
                                .find(|(t, _)| t == tag)
                                .map_or(Value::Null, |(_, v)| values::from_zcl(&v.value)),
                            _ => Value::Null,
                        },
                        _ => values::from_zcl(&tv.value),
                    };
                    if matches!(vd.binding, Binding::Struct { .. }) && raw.is_null() {
                        continue;
                    }
                    work.push((i, p, raw));
                }
            }
            if !hit {
                unknown.push((*attr, format!("{:?}", tv.value)));
            }
        }
        if let Some(d) = self.devices.get_mut(id) {
            for (a, v) in unknown {
                d.diag.unknown_attrs.insert((ep, cluster, a), v);
            }
        }
        self.apply_raw(id, work, now);
    }

    /// Decodes raw values for `(entity, part)` and applies them.
    pub(crate) fn apply_raw(
        &mut self,
        id: &str,
        work: Vec<(usize, Option<usize>, Value)>,
        now: Millis,
    ) {
        if work.is_empty() {
            return;
        }
        let tz = self.cfg.tz_offset_s;
        let Some(d) = self.devices.get(id) else {
            return;
        };
        let mut decoded = Vec::new();
        {
            let env = DevEnv { dev: d, now, tz };
            let scale = |c: u16, a: u16| {
                d.attrs
                    .map
                    .iter()
                    .find(|((_, cc, aa, m), _)| *cc == c && *aa == a && m.is_none())
                    .and_then(|(_, e)| e.value.as_ref())
                    .and_then(ZclValue::as_f64)
            };
            for (i, p, raw) in work {
                let e = &d.entities[i];
                let vd = value_of(&e.r.def, p);
                let is_bool = match p {
                    None => e.r.def.platform.is_bool(),
                    Some(pi) => e.r.def.parts[pi].0 == "on_off",
                };
                let (v, flags) = values::decode(vd, is_bool, raw, &env, &scale);
                decoded.push((i, p, v, flags));
            }
        }
        let d = self.devices.get_mut(id).unwrap();
        let mut touched = BTreeSet::new();
        for (i, p, v, flags) in decoded {
            if flags.map_miss || flags.expr_error {
                d.entities[i].state.errors += 1;
                d.diag.count(if flags.map_miss {
                    "map_miss"
                } else {
                    "expr_error"
                });
            }
            let part = p.map(|pi| d.entities[i].r.def.parts[pi].0.clone());
            set_value(d, i, part.as_deref(), v, now);
            touched.insert(i);
        }
        // reset_after (occupancy auto-reset).
        let mut resets = Vec::new();
        {
            let env = DevEnv { dev: d, now, tz };
            for &i in &touched {
                let e = &d.entities[i];
                if let (Some(ra), Value::Bool(true)) = (&e.r.def.reset_after, &e.state.value) {
                    if let Some(secs) = ra.eval(&ExprCtx::with_x(Value::Null, &env)).as_f64() {
                        resets.push((i, now + (secs * 1000.0) as Millis));
                    }
                }
            }
        }
        for (i, at) in resets {
            d.entities[i].state.reset_at = Some(at);
        }
        let uids: Vec<String> = touched.iter().map(|&i| d.entities[i].uid.clone()).collect();
        for u in uids {
            self.publish_state(&u);
        }
    }

    fn dispatch_command(&mut self, id: &str, ep: u8, cluster: u16, frame: &ZclFrame, now: Millis) {
        let h = frame.header;
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        // De-duplication: same source, TSN, cluster, command within 2 s.
        let key = (u64::from(ep) << 40)
            | (u64::from(cluster) << 24)
            | (u64::from(h.tsn) << 8)
            | u64::from(h.cmd);
        d.dedup.retain(|(t, _)| now.saturating_sub(*t) < DEDUP_MS);
        if d.dedup.iter().any(|(_, k)| *k == key) {
            d.diag.count("duplicate_command");
            return;
        }
        d.dedup.push_back((now, key));
        let ignored = d.resolution.ignore.iter().any(|r| {
            r.cluster == cluster && r.ep.is_none_or(|e| e == ep) && r.cmd.is_none_or(|c| c == h.cmd)
        });
        let mut fired = Vec::new();
        {
            let env = DevEnv {
                dev: d,
                now,
                tz: self.cfg.tz_offset_s,
            };
            for (i, e) in d.entities.iter().enumerate() {
                if e.orphaned || (ignored && e.r.source == LayerSource::Generic) {
                    continue;
                }
                for t in &e.r.def.triggers {
                    let TriggerSource::Cmd {
                        ep: tep,
                        cluster: tc,
                        id: tid,
                        mfr,
                        global,
                    } = t.source
                    else {
                        continue;
                    };
                    if tc != cluster
                        || tid != h.cmd
                        || tep.is_some_and(|x| x != ep)
                        || mfr.is_some_and(|m| h.mfr != Some(m))
                        || global != (h.frame_type == FrameType::Global)
                    {
                        continue;
                    }
                    if let Some(ev) =
                        eval_trigger(t, &frame.payload, e.state.last_event.as_deref(), &env)
                    {
                        let mut attrs = ev.1;
                        attrs.insert("endpoint".into(), ep.into());
                        fired.push((i, ev.0, attrs));
                        break;
                    }
                }
            }
        }
        if fired.is_empty() {
            d.diag.count("unhandled_command");
        }
        for (i, et, attrs) in fired {
            self.fire_event(id, i, et, attrs, now);
        }
    }

    /// Fires an event on entity `i` (4.7).
    pub(crate) fn fire_event(
        &mut self,
        id: &str,
        i: usize,
        event_type: String,
        mut attrs: Map<String, J>,
        now: Millis,
    ) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        let e = &mut d.entities[i];
        if !e.r.def.event_types.is_empty() && !e.r.def.event_types.contains(&event_type) {
            d.diag.count("event_type_dropped");
            return;
        }
        e.state.last_event = Some(event_type.clone());
        e.state.last_reported = Some(now);
        e.state.last_changed = Some(now);
        let mut st_attrs = Map::new();
        st_attrs.insert("event_type".into(), event_type.clone().into());
        for (k, v) in &attrs {
            st_attrs.insert(k.clone(), v.clone());
        }
        e.state.attributes = st_attrs;
        let uid = e.uid.clone();
        if let Some(extra) = self.gp_binding_attrs(id, i) {
            for (k, v) in extra {
                attrs.insert(k, v);
            }
        }
        self.emit(HubEvent::EntityEvent {
            uid: uid.clone(),
            event_type,
            attributes: attrs,
            time: ts(now),
        });
        self.publish_state(&uid);
    }

    // ------------------------------------------------------------------
    // Tick: timeouts, availability, timers.

    /// Delivery failures reported by the transport: one immediate retry
    /// (the NCP already retried at APS level; a sleepy device's request
    /// waits for its next frame), then the request fails with
    /// `not_delivered` instead of waiting for the response timeout.
    fn delivery_failures(&mut self, now: Millis) {
        for f in self.transport.take_failures() {
            let Some(k) = self
                .pending
                .iter()
                .find(|(_, p)| p.req == f.req)
                .map(|(k, _)| *k)
            else {
                continue; // answered meanwhile, or nothing waits for it
            };
            let Some(mut p) = self.pending.remove(&k) else {
                continue;
            };
            let sleepy = self.devices.get(&p.device).is_some_and(Device::sleepy);
            if sleepy {
                if let Some(d) = self.devices.get_mut(&p.device) {
                    d.queue.push_back(Queued {
                        req: p.req,
                        purpose: p.purpose,
                        tsn: k.1,
                    });
                }
                continue;
            }
            if p.retries == 0 && p.purpose != Purpose::Check {
                p.retries = 1;
                p.sent_at = now;
                if self.transport.aps_send(p.req.clone()).is_ok() {
                    self.pending.insert(k, p);
                    continue;
                }
            }
            let dev = p.device.clone();
            self.fail_purpose(&dev, &p.purpose, not_delivered(&f), now);
        }
    }

    /// Advances timers.
    pub fn tick(&mut self, now: Millis) {
        self.delivery_failures(now);
        // Request timeouts.
        let expired: Vec<(bool, u8)> = self
            .pending
            .iter()
            .filter(|(_, p)| {
                let sleepy = self.devices.get(&p.device).is_some_and(Device::sleepy);
                now.saturating_sub(p.sent_at)
                    > if sleepy {
                        TX_TIMEOUT_SLEEPY_MS
                    } else {
                        TX_TIMEOUT_MS
                    }
            })
            .map(|(k, _)| *k)
            .collect();
        for k in expired {
            let Some(mut p) = self.pending.remove(&k) else {
                continue;
            };
            let sleepy = self.devices.get(&p.device).is_some_and(Device::sleepy);
            if p.retries < 2 && p.purpose != Purpose::Check {
                p.retries += 1;
                p.sent_at = now;
                if sleepy {
                    // Park until the device talks again.
                    if let Some(d) = self.devices.get_mut(&p.device) {
                        d.queue.push_back(Queued {
                            req: p.req.clone(),
                            purpose: p.purpose.clone(),
                            tsn: k.1,
                        });
                    }
                } else if self.transport.aps_send(p.req.clone()).is_ok() {
                    self.pending.insert(k, p);
                }
                continue;
            }
            let dev = p.device.clone();
            self.fail_purpose(
                &dev,
                &p.purpose,
                ApiError::new("timeout", "no response from the device"),
                now,
            );
        }
        // Tickets waiting for Tuya dataResponse.
        let late: Vec<u64> = self
            .tickets
            .iter()
            .filter(|(_, t)| t.deadline > 0 && now > t.deadline)
            .map(|(k, _)| *k)
            .collect();
        for t in late {
            if let Some(tk) = self.tickets.remove(&t) {
                self.clear_pending_state(&tk);
                self.emit(HubEvent::CommandResult {
                    ticket: t,
                    result: Err(ApiError::new("timeout", "no dataResponse from the device")),
                });
            }
        }
        let ids: Vec<String> = self.devices.keys().cloned().collect();
        for id in ids {
            self.tick_device(&id, now);
        }
        self.gp_tick(now);
        self.gp_window_tick(now);
    }

    fn fail_purpose(&mut self, id: &str, purpose: &Purpose, err: ApiError, now: Millis) {
        match purpose {
            Purpose::Interview(step) => {
                if let Some(d) = self.devices.get_mut(id) {
                    d.interview_partial = true;
                }
                let next = match *step {
                    InterviewStep::NodeDesc => InterviewStep::ActiveEp,
                    InterviewStep::ActiveEp => InterviewStep::Basic(0),
                    InterviewStep::SimpleDesc(i) => InterviewStep::SimpleDesc(i + 1),
                    InterviewStep::Basic(i) => InterviewStep::Basic(i + 1),
                    InterviewStep::SchemaHeader | InterviewStep::SchemaChunk(_) => {
                        if let Some(d) = self.devices.get_mut(id) {
                            d.schema.error = Some("schema read timed out".into());
                        }
                        InterviewStep::Configure
                    }
                    s => s,
                };
                self.interview_step(id, next, now);
            }
            Purpose::Configure(key) => self.configure_done(id, key, false, err.code.clone(), now),
            Purpose::Command(t) => self.ticket_part_done(*t, Some(err), now),
            Purpose::GpProbe(nwk) => self.gp_probe_result(*nwk, &[], now),
            _ => {}
        }
    }

    fn tick_device(&mut self, id: &str, now: Millis) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        // Auto-reset timers.
        let mut touched = Vec::new();
        for i in 0..d.entities.len() {
            if d.entities[i].state.reset_at.is_some_and(|t| now >= t) {
                d.entities[i].state.reset_at = None;
                set_value(d, i, None, Value::Bool(false), now);
                touched.push(d.entities[i].uid.clone());
            }
        }
        for u in touched {
            self.publish_state(&u);
        }
        // Availability.
        self.availability_tick(id, now);
        // Configure retries (mains).
        let retry = self
            .devices
            .get(id)
            .and_then(|d| d.configure.retry_at)
            .is_some_and(|t| now >= t);
        if retry {
            if let Some(d) = self.devices.get_mut(id) {
                d.configure.retry_at = None;
            }
            self.redo_configure(id, now);
        }
        // Unreportable primary measurements on mains devices are polled
        // every `max` seconds (5.2); sleepy devices never are.
        self.poll_unreportable(id, now);
        // Periodic Tuya query.
        let q = self
            .devices
            .get(id)
            .and_then(|d| d.tuya_query_at)
            .is_some_and(|t| now >= t);
        if q {
            self.tuya_query(id, now);
            if let Some(d) = self.devices.get_mut(id) {
                let iv = d
                    .resolution
                    .tuya
                    .as_ref()
                    .and_then(|t| t.query_interval_s)
                    .unwrap_or(0);
                d.tuya_query_at = (iv > 0).then(|| now + u64::from(iv) * 1000);
            }
        }
    }

    fn poll_unreportable(&mut self, id: &str, now: Millis) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        if d.sleepy()
            || d.power() != PowerKind::Mains
            || d.gp.is_some()
            || d.configure.unreportable.is_empty()
        {
            return;
        }
        let mut due = Vec::new();
        for &(ep, c, a) in &d.configure.unreportable.clone() {
            let primary = d
                .entities
                .iter()
                .filter(|e| !e.orphaned && e.r.ep == ep && e.r.def.category.is_none())
                .find_map(|e| {
                    e.r.def
                        .values()
                        .find_map(|(_, v)| match (&v.binding, v.report) {
                            (Binding::Zcl(z), Some(ReportSpec::Config { max, .. }))
                                if z.cluster == c && z.attr == a =>
                            {
                                Some((u64::from(max.max(10)), z.mfr))
                            }
                            _ => None,
                        })
                });
            let Some((max, mfr)) = primary else { continue };
            // `poll_at` holds the time of the last poll (or of the failure).
            let last = *d.configure.poll_at.entry((ep, c, a)).or_insert(now);
            if now >= last + max * 1000 {
                d.configure.poll_at.insert((ep, c, a), now);
                due.push((ep, c, a, mfr));
            }
        }
        for (ep, c, a, mfr) in due {
            self.read_attrs(id, ep, c, mfr, &[a], Purpose::Refresh, now);
        }
    }

    /// Availability timeout of a passive device, s (4.6).
    pub fn availability_timeout_s(&self, d: &Device) -> Option<u64> {
        if let Some(t) = d.resolution.device.availability_timeout_s {
            return Some(t);
        }
        if let Some(g) = &d.fp.gp {
            if d.power() == PowerKind::EnergyHarvesting
                || crate::gp::generic::is_switch(g.device_id)
                    && d.resolution
                        .gp
                        .as_ref()
                        .and_then(|x| x.report_interval_s)
                        .is_none()
            {
                return None;
            }
            return Some(
                d.resolution
                    .gp
                    .as_ref()
                    .and_then(|x| x.report_interval_s)
                    .map_or(25 * HOUR_S, |i| 3 * u64::from(i)),
            );
        }
        if d.power() == PowerKind::Mains && !d.sleepy() {
            return None;
        }
        if d.power() == PowerKind::EnergyHarvesting {
            return None;
        }
        let max_report = d
            .entities
            .iter()
            .filter(|e| !e.orphaned)
            .flat_map(|e| e.r.def.values().map(|(_, v)| v.report).collect::<Vec<_>>())
            .filter_map(|r| match r {
                Some(ReportSpec::Config { max, .. }) => Some(u64::from(max)),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        let checkin = u64::from(d.resolution.device.checkin_s.unwrap_or(0));
        Some(
            ((max_report * 5).div_ceil(2))
                .max((checkin * 5).div_ceil(2))
                .max(25 * HOUR_S),
        )
    }

    fn availability_tick(&mut self, id: &str, now: Millis) {
        let Some(d) = self.devices.get(id) else {
            return;
        };
        if !d.available {
            return;
        }
        let silence = now.saturating_sub(d.last_seen.unwrap_or(now));
        let mains_active = d.gp.is_none()
            && d.power() == PowerKind::Mains
            && !d.sleepy()
            && d.resolution.device.availability_timeout_s.is_none();
        if mains_active {
            if silence < MAINS_SILENCE_MS {
                return;
            }
            let check = d.check;
            match check {
                None => {
                    self.devices.get_mut(id).unwrap().check = Some((3, now));
                    self.availability_probe(id, now);
                }
                Some((left, next)) if now >= next => {
                    if left == 0 {
                        self.set_unavailable(id);
                    } else {
                        self.availability_probe(id, now);
                    }
                }
                _ => {}
            }
            return;
        }
        if let Some(t) = self.availability_timeout_s(d) {
            if silence >= t * 1000 {
                self.set_unavailable(id);
            }
        }
    }

    fn availability_probe(&mut self, id: &str, now: Millis) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        let Some((left, _)) = d.check else { return };
        d.check = Some((left.saturating_sub(1), now + 10_000));
        let ep =
            d.fp.endpoints
                .iter()
                .find(|e| e.in_clusters.contains(&cl::BASIC))
                .map_or(1, |e| e.ep);
        self.read_attrs(id, ep, cl::BASIC, None, &[0x0000], Purpose::Check, now);
    }

    fn set_unavailable(&mut self, id: &str) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        d.available = false;
        d.check = None;
        let uids: Vec<String> = d.entities.iter().map(|e| e.uid.clone()).collect();
        self.emit(HubEvent::DeviceUpdated(id.to_string()));
        for u in uids {
            self.publish_state(&u);
        }
    }

    // ------------------------------------------------------------------
    // Tickets.

    pub(crate) fn new_ticket(&mut self) -> u64 {
        let t = self.next_ticket;
        self.next_ticket += 1;
        self.tickets.insert(t, Ticket::default());
        t
    }

    pub(crate) fn ticket_add(
        &mut self,
        t: u64,
        frames: u32,
        optimistic: Vec<(String, Option<String>, Value)>,
    ) {
        if let Some(tk) = self.tickets.get_mut(&t) {
            tk.remaining += frames;
            tk.optimistic.extend(optimistic);
        }
    }

    pub(crate) fn ticket_wait_tuya(&mut self, t: u64, device: &str, dps: &[u8], deadline: Millis) {
        if let Some(tk) = self.tickets.get_mut(&t) {
            tk.tuya_dps
                .extend(dps.iter().map(|d| (device.to_string(), *d)));
            tk.deadline = deadline;
        }
    }

    pub(crate) fn ticket_part_done(&mut self, t: u64, err: Option<ApiError>, now: Millis) {
        let Some(tk) = self.tickets.get_mut(&t) else {
            return;
        };
        tk.remaining = tk.remaining.saturating_sub(1);
        if err.is_some() && tk.error.is_none() {
            tk.error = err;
        }
        if tk.remaining == 0 && tk.tuya_dps.is_empty() {
            let tk = self.tickets.remove(&t).unwrap();
            self.finish_ticket(t, tk, now);
        }
    }

    fn clear_pending_state(&mut self, tk: &Ticket) {
        let uids: BTreeSet<String> = tk.optimistic.iter().map(|o| o.0.clone()).collect();
        for uid in uids {
            if let Some(d) = self.devices.get_mut(device_of_uid(&uid)) {
                if let Some(i) = d.entity_index(&uid) {
                    d.entities[i].state.pending = None;
                }
            }
            self.publish_state(&uid);
        }
    }

    fn finish_ticket(&mut self, t: u64, tk: Ticket, now: Millis) {
        match tk.error.clone() {
            None => {
                let mut uids = BTreeSet::new();
                for (uid, part, v) in &tk.optimistic {
                    if let Some(d) = self.devices.get_mut(device_of_uid(uid)) {
                        if let Some(i) = d.entity_index(uid) {
                            d.entities[i].state.pending = None;
                            if !v.is_null() {
                                set_value(d, i, part.as_deref(), v.clone(), now);
                            }
                            uids.insert(uid.clone());
                        }
                    }
                }
                for u in uids {
                    self.publish_state(&u);
                }
                self.emit(HubEvent::CommandResult {
                    ticket: t,
                    result: Ok("ok".into()),
                });
            }
            Some(e) => {
                self.clear_pending_state(&tk);
                self.emit(HubEvent::CommandResult {
                    ticket: t,
                    result: Err(e),
                });
            }
        }
    }

    /// A Tuya DP arrived: completes tickets waiting for it.
    pub(crate) fn tuya_confirm(&mut self, id: &str, dp: u8, now: Millis) {
        let done: Vec<u64> = self
            .tickets
            .iter_mut()
            .filter_map(|(k, t)| {
                let n = t.tuya_dps.len();
                t.tuya_dps.retain(|(d, p)| !(d == id && *p == dp));
                (n != t.tuya_dps.len() && t.tuya_dps.is_empty() && t.remaining == 0).then_some(*k)
            })
            .collect();
        for t in done {
            if let Some(mut tk) = self.tickets.remove(&t) {
                tk.optimistic.clear();
                self.finish_ticket(t, tk, now);
            }
        }
    }

    // ------------------------------------------------------------------
    // Registry operations used by the API.

    /// Removes a device (`devices/remove`).
    pub fn remove_device(&mut self, id: &str, force: bool, now: Millis) -> Result<(), ApiError> {
        let d = self
            .devices
            .get(id)
            .ok_or_else(|| ApiError::new("not_found", format!("no device {id}")))?;
        match d.kind.clone() {
            DeviceKind::Zigbee { ieee, nwk } => {
                if !force {
                    let tsn = self.next_zdo_tsn();
                    self.send_zdo(
                        id,
                        zdo::cluster::MGMT_LEAVE_REQ,
                        zdo::mgmt_leave_req(tsn, ieee),
                        Purpose::None,
                        now,
                    );
                }
                let _ = self.transport.remove_device(ieee);
                self.nwk_index.remove(&nwk);
            }
            DeviceKind::Gp { gpd_id } => self.gp_remove(gpd_id, true, now)?,
        }
        if let Some(d) = self.devices.remove(id) {
            for e in &d.entities {
                self.emit(HubEvent::EntityRemoved(e.uid.clone()));
            }
        }
        self.dispatch.remove(id);
        self.pending.retain(|_, p| p.device != id);
        self.emit(HubEvent::DeviceRemoved(id.to_string()));
        Ok(())
    }

    /// Uploads (or replaces) a user definition; affected devices
    /// re-resolve. Returns the IDs of devices it matches.
    pub fn upload_user_definition(
        &mut self,
        mut def: Definition,
        now: Millis,
    ) -> Result<Vec<String>, ApiError> {
        let missing = crate::resolve::missing_features(&def, &self.features);
        if !missing.is_empty() {
            return Err(ApiError::new(
                "unsupported",
                format!("requires {}", missing.join(", ")),
            ));
        }
        def.source = LayerSource::User;
        let matching: Vec<String> = self
            .devices
            .values()
            .filter(|d| crate::resolve::score(&def, &d.fp).is_some())
            .map(|d| d.id.clone())
            .collect();
        self.user.insert(def);
        for id in &matching {
            self.resolve_device(id, now);
            self.redo_configure(id, now);
        }
        Ok(matching)
    }

    /// Deletes a user definition.
    pub fn delete_user_definition(&mut self, id: &str, now: Millis) -> Result<(), ApiError> {
        self.user_sources.remove(id);
        if !self.user.remove(id) {
            return Err(ApiError::new(
                "not_found",
                format!("no user definition {id}"),
            ));
        }
        let ids: Vec<String> = self.devices.keys().cloned().collect();
        for d in ids {
            self.resolve_device(&d, now);
        }
        Ok(())
    }

    /// Pins a definition (`devices/pin_definition`).
    pub fn pin_definition(
        &mut self,
        id: &str,
        def: Option<String>,
        now: Millis,
    ) -> Result<(), ApiError> {
        if let Some(x) = &def {
            if self.user.get(x).is_none() && self.catalog.get(x).is_none() {
                return Err(ApiError::new("not_found", format!("no definition {x}")));
            }
        }
        let d = self
            .devices
            .get_mut(id)
            .ok_or_else(|| ApiError::new("not_found", format!("no device {id}")))?;
        d.pinned = def;
        self.resolve_device(id, now);
        self.redo_configure(id, now);
        Ok(())
    }

    /// Deletes an orphaned entity (`entities/delete`).
    pub fn delete_entity(&mut self, uid: &str) -> Result<(), ApiError> {
        let d = self
            .devices
            .get_mut(device_of_uid(uid))
            .ok_or_else(|| ApiError::new("not_found", uid.to_string()))?;
        let i = d
            .entity_index(uid)
            .ok_or_else(|| ApiError::new("not_found", uid.to_string()))?;
        if !d.entities[i].orphaned {
            return Err(ApiError::new(
                "invalid_value",
                "only orphaned entities can be deleted",
            ));
        }
        d.entities.remove(i);
        let did = d.id.clone();
        let table = build_dispatch(self.devices.get(&did).unwrap());
        self.dispatch.insert(did, table);
        self.emit(HubEvent::EntityRemoved(uid.to_string()));
        Ok(())
    }
}

/// The schema endpoint of a device (0xFC6B server).
fn schema_endpoint(d: &Device) -> Option<u8> {
    d.fp.endpoints
        .iter()
        .find(|e| e.in_clusters.contains(&cl::ZRS_SCHEMA))
        .map(|e| e.ep)
}

/// Attributes whose value changes the generic entity set or capabilities.
fn is_structural(cluster: u16, attr: u16) -> bool {
    matches!(
        (cluster, attr),
        (cl::IAS_ZONE, 0x0001)
            | (cl::COLOR, 0x400A..=0x400C)
            | (cl::THERMOSTAT, 0x001B | 0x0015 | 0x0016)
            | (cl::FAN_CONTROL, 0x0001)
            | (cl::METERING, 0x0300)
            | (
                cl::BINARY_INPUT | cl::ANALOG_INPUT | cl::MULTISTATE_INPUT,
                0x001C
            )
    ) || matches!(
        (cluster, attr),
        (cl::ON_OFF, 0x4003)
            | (cl::LEVEL, 0x0010 | 0x0011)
            | (cl::OCCUPANCY, 0x0010)
            | (cl::POWER_CONFIG, 0x0020 | 0x0021)
            | (cl::DEVICE_TEMPERATURE, 0)
            | (cl::THERMOSTAT, 0x0010 | 0x0008)
            | (cl::WINDOW_COVERING, 0x0009)
            | (cl::METERING, 0x0400)
            | (cl::ELECTRICAL, 0x0510 | 0x0300)
            | (cl::DIAGNOSTICS, 0x011C | 0x011D)
    )
}

/// The device ID part of a uid.
pub fn device_of_uid(uid: &str) -> &str {
    // zb:<16 hex>:<ep>:<key> / gp:<8 hex>:<ep>:<key> / gp:<ieee>:<ep>:<ep>:<key>
    let mut it = uid.match_indices(':').map(|(i, _)| i);
    let first = it.next();
    let second = it.next();
    match (uid.get(..3), first, second) {
        (Some("gp:"), Some(_), Some(s)) if uid[3..s].len() == 16 => {
            let third = it.next().unwrap_or(uid.len());
            &uid[..third]
        }
        (_, Some(_), Some(s)) => &uid[..s],
        _ => uid,
    }
}

/// The value definition of `(entity, part)`.
pub(crate) fn value_of(e: &EntityDef, p: Option<usize>) -> &ValueDef {
    p.map_or(&e.value, |i| &e.parts[i].1)
}

/// Applies a decoded value to an entity (or part).
pub(crate) fn set_value(d: &mut Device, i: usize, part: Option<&str>, v: Value, now: Millis) {
    let e = &mut d.entities[i];
    let changed = match part {
        None => {
            let c = e.state.value != v;
            e.state.value = v;
            c
        }
        Some(p) => {
            let c = e.state.parts.get(p) != Some(&v);
            e.state.parts.insert(p.to_string(), v);
            c
        }
    };
    e.state.last_reported = Some(now);
    if changed || e.state.last_changed.is_none() {
        e.state.last_changed = Some(now);
    }
    e.state.pending = None;
}

/// Evaluates a trigger: `when`, `event`, `attrs`.
pub(crate) fn eval_trigger(
    t: &Trigger,
    payload: &[u8],
    last: Option<&str>,
    env: &dyn ExprEnv,
) -> Option<(String, Map<String, J>)> {
    let ctx = ExprCtx {
        x: Value::Null,
        payload: Some(payload),
        last,
        env,
    };
    if let Some(w) = &t.when {
        if w.eval(&ctx).truthy() != Some(true) {
            return None;
        }
    }
    let et = match &t.event {
        EventSel::Fixed(s) => s.clone(),
        EventSel::Expr(e) => match e.eval(&ctx) {
            Value::Str(s) => s,
            _ => return None,
        },
    };
    let mut attrs = Map::new();
    for (k, e) in &t.attrs {
        let v = e.eval(&ctx);
        if !v.is_null() {
            attrs.insert(k.clone(), v.to_json());
        }
    }
    Some((et, attrs))
}

/// Builds the dispatch table of a device.
pub(crate) fn build_dispatch(d: &Device) -> BTreeMap<DKey, Vec<(usize, Option<usize>)>> {
    let mut t: BTreeMap<DKey, Vec<(usize, Option<usize>)>> = BTreeMap::new();
    for (i, e) in d.entities.iter().enumerate() {
        if e.orphaned {
            continue;
        }
        let mut add = |vd: &ValueDef, p: Option<usize>| {
            let k = match &vd.binding {
                Binding::Zcl(z) => Some(DKey::Attr(e.r.ep, z.cluster, z.attr, z.mfr)),
                Binding::Struct {
                    cluster, attr, mfr, ..
                } => Some(DKey::Attr(e.r.ep, *cluster, *attr, *mfr)),
                Binding::Tuya { dp, .. } => Some(DKey::Tuya(*dp)),
                Binding::Gp(GpBinding::Report {
                    cluster, attr, mfr, ..
                }) => Some(DKey::GpAttr(*cluster, *attr, *mfr)),
                Binding::Gp(GpBinding::Cmd(c)) => Some(DKey::GpCmd(*c)),
                _ => None,
            };
            if let Some(k) = k {
                t.entry(k).or_default().push((i, p));
            }
        };
        add(&e.r.def.value, None);
        for (pi, (_, vd)) in e.r.def.parts.iter().enumerate() {
            add(vd, Some(pi));
        }
    }
    t
}

impl<T: ZigbeeTransport> Hub<T> {
    /// Helper for tests and the API: the state message of an entity.
    pub fn state_of(&self, uid: &str) -> Option<J> {
        self.entity(uid).map(|(d, e)| e.state_msg(d.available))
    }

    /// Diagnostics of a device (`devices/diagnostics`).
    pub fn diagnostics(&self, id: &str) -> Option<J> {
        let d = self.devices.get(id)?;
        Some(json!({
            "fingerprint": d.fp,
            "fingerprint_hash": d.fp_hash,
            "layers": d.resolution.layers,
            "resolution_diagnostics": d.resolution.diagnostics,
            "configure_hash": d.configure.hash,
            "configure": d.diag.configure,
            "unknown_attributes": d.diag.unknown_attrs.iter().map(|((ep, c, a), v)| json!({"ep": ep, "cluster": format!("0x{c:04x}"), "attr": format!("0x{a:04x}"), "value": v})).collect::<Vec<_>>(),
            "tuya_dps": d.diag.tuya_dps.iter().map(|(dp, (t, hex, at))| json!({"dp": dp, "type": t, "data": hex, "time": ts(*at)})).collect::<Vec<_>>(),
            "gp_data": d.diag.gp_data,
            "counters": d.diag.counters,
            "expression_errors": d.entities.iter().filter(|e| e.state.errors > 0).map(|e| json!({"uid": e.uid, "errors": e.state.errors})).collect::<Vec<_>>(),
            "frames": d.diag.frames.iter().map(|(t, c, h)| json!({"time": ts(*t), "cluster": format!("0x{c:04x}"), "hex": h})).collect::<Vec<_>>(),
            "schema_error": d.schema.error,
            "interview_partial": d.interview_partial,
        }))
    }
}

/// The error of an undeliverable request.
fn not_delivered(f: &crate::transport::ApsFailure) -> ApiError {
    let message = match f.zb_status {
        None => "the NCP refused the request (queue full or network down)".to_string(),
        Some(s) => match crate::transport::zb_status_text(s, f.legacy_codes) {
            Some(t) => format!("not delivered: {t} (zb_status 0x{s:02X})"),
            None => format!("not delivered (zb_status 0x{s:02X})"),
        },
    };
    ApiError::new("not_delivered", message)
}
