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
use crate::reporting::value_report;
use crate::resolve::{Catalog, FpEnv, HUB_FEATURES, MemoryCatalog, ResolveInput, resolve};
use crate::transport::{
    ApsDst, ApsIndication, ApsRequest, CoordError, Request, RequestId, Terminal, TxOptions,
    ZigbeeTransport,
};
use crate::values;
use crate::zcl::frame::{self as zf, global};
use crate::zcl::{
    Direction, FrameType, Status, TypedValue, ZclFrame, ZclHeader, ZclType, ZclValue, cluster as cl,
};
use crate::zdo::{self, ZdoMessage};

pub(crate) mod control;
pub mod import;
pub(crate) mod lumi;
pub mod zgroups;

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
    /// The hub's clock for the Time cluster server (the app keeps it current).
    pub clock: Clock,
    /// Groups the hub endpoint can be a member of: the coordinator's group
    /// (multicast) table, from the backend's capabilities (`None`: not
    /// known, no limit).
    pub hub_groups_max: Option<u16>,
}

/// What the Time cluster (0x000A) server on the hub endpoint reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Clock {
    /// The hub knows UTC (SNTP or a clock kept over a restart).
    pub synced: bool,
    /// Standard time offset, s (TimeZone).
    pub tz_s: i32,
    /// This year's daylight saving time, ZCL time (s since 2000-01-01 UTC):
    /// `(start, end, shift s)`.
    pub dst: Option<(u32, u32, i32)>,
    /// Last synchronisation, ZCL time.
    pub last_set: Option<u32>,
}

/// Unix time of the ZCL epoch (2000-01-01 00:00 UTC).
pub const ZCL_EPOCH_UNIX: i64 = 946_684_800;

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            coordinator_ieee: 0x00124b0000000001,
            hub_endpoint: 1,
            tz_offset_s: 0,
            gp_group: 0x0B84,
            clock: Clock::default(),
            hub_groups_max: None,
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
    /// `entity_reporting`: a sleeping device answered a queued readback
    /// (the `entities/reporting` body with `device` and `uid`).
    EntityReporting(J),
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
    /// Deferred result of a request that a control request decided
    /// (permit join, groups, GP sink): the result as it would have been
    /// answered at once.
    Answer {
        /// Ticket.
        ticket: u64,
        /// The result or the error.
        result: Result<J, ApiError>,
    },
    /// Deferred result of a ZCL console request (14.7): the decoded answer.
    #[cfg(feature = "advanced")]
    ConsoleResult {
        /// Ticket.
        ticket: u64,
        /// The answer or the error.
        result: Result<J, ApiError>,
    },
    /// A frame to or from a device (topic `frames`, only while subscribed).
    #[cfg(feature = "advanced")]
    Frame(J),
    /// Network topic events (`permit_join` …), from the host core.
    Network(J),
    /// Groups topic events (14.9): `group_updated`, `group_removed`,
    /// `bindings_updated`, `group_membership`, `group_view`, `scene_view`,
    /// `group_error`.
    Groups {
        /// Event name.
        kind: String,
        /// Body.
        body: J,
    },
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
    /// A ZCL console request; `cluster_answer`: a cluster-specific frame
    /// with its TSN answers it too.
    #[cfg(feature = "advanced")]
    Console {
        /// Ticket.
        ticket: u64,
        /// Cluster-specific command sent.
        cluster_answer: bool,
    },
    /// A groups, scenes or bindings request (14.9).
    Group(zgroups::ZgOp),
    /// Basic DateCode and SWBuildID read again ([`Hub::refresh_identity`]).
    Identity,
    /// A frame of the application ([`Hub::send_app_zcl`]): no answer
    /// tracked, its outcome goes to [`Hub::take_app_outcomes`].
    App,
    /// Read Reporting Configuration of `entities/reporting`.
    Reporting {
        /// The readback it belongs to.
        read: u64,
        /// Device endpoint.
        ep: u8,
        /// Cluster.
        cluster: u16,
        /// Attributes asked.
        attrs: Vec<u16>,
    },
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
pub(crate) struct Pending {
    pub(crate) device: String,
    pub(crate) purpose: Purpose,
    sent_at: Millis,
    /// Times it was sent before this transmission (a sleepy device's
    /// request keeps the count while parked, [`Queued::tries`]).
    retries: u8,
    req: ApsRequest,
    /// The request in flight (a retry has its own).
    rid: RequestId,
}

/// How many times a request is sent before it fails, timeouts and failed
/// deliveries alike. A sleepy device's request waits for the device's
/// next frame between its tries, so for it these bound the wake-ups one
/// interview step takes; zigbee-herdsman's interview tries the Node
/// descriptor 6 times, the endpoint list and each Basic attribute twice
/// (a Simple descriptor: its ZDO request's own retry, also 2 here).
fn max_tries(purpose: &Purpose, sleepy: bool) -> u8 {
    use InterviewStep::{ActiveEp, Basic, NodeDesc, SimpleDesc};
    match purpose {
        Purpose::Check => 1,
        Purpose::Interview(NodeDesc) if sleepy => 6,
        Purpose::Interview(ActiveEp | SimpleDesc(_) | Basic(_)) if sleepy => 2,
        _ => 3,
    }
}

/// A request to a device goes to its current address when it is sent
/// again (a retry, a request parked for a sleeping device): after a
/// rejoin under a new NWK address, also the address of interest of the
/// ZDO descriptor requests about it (Node, Power, Simple, Active EP).
fn readdress(req: &mut ApsRequest, nwk: Option<u16>) {
    let (ApsDst::Nwk(old), Some(nwk)) = (req.dst, nwk) else {
        return;
    };
    if old == nwk {
        return;
    }
    req.dst = ApsDst::Nwk(nwk);
    if req.profile == 0
        && (0x0002..=0x0005).contains(&req.cluster)
        && req.payload.get(1..3) == Some(&old.to_le_bytes()[..])
    {
        req.payload[1..3].copy_from_slice(&nwk.to_le_bytes());
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Ticket {
    remaining: u32,
    error: Option<ApiError>,
    optimistic: Vec<(String, Option<String>, Value)>,
    tuya_dps: Vec<(String, u8)>,
    deadline: Millis,
}

/// Whether a command still waits for DP `dp` of device `id`.
pub(crate) fn tuya_awaited(tickets: &BTreeMap<u64, Ticket>, id: &str, dp: u8) -> bool {
    tickets
        .values()
        .any(|t| t.tuya_dps.iter().any(|(d, p)| d == id && *p == dp))
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
/// A frame of the application without an outcome this long is reported
/// [`Terminal::Unknown`] (backends report every request within a minute).
const APP_OUTCOME_MS: Millis = 10 * 60_000;
const TX_TIMEOUT_SLEEPY_MS: Millis = 20_000;
const AWAKE_AFTER_RX_MS: Millis = 5_000;
const AWAKE_AFTER_JOIN_MS: Millis = 30_000;
/// A rejoin reads Basic DateCode and SWBuildID again at most this often.
const IDENTITY_REREAD_MS: Millis = 3_600_000;
/// A partial interview resumes when the device rejoins or announces itself
/// (the moment a sleepy one is awake): at most this many times per device
/// and hub run (the count is not stored, a restart allows as many again),
/// so a device that keeps failing a step is not interviewed at every
/// announcement ...
const INTERVIEW_RESUMES: u8 = 5;
/// ... the first at once, the next ones at least this long after the
/// previous one, doubling (10, 20, 40, 80 min), however often it
/// announces.
const INTERVIEW_RESUME_MS: Millis = 10 * 60_000;
const MAINS_SILENCE_MS: Millis = 10 * 60_000;
/// The interview's Basic reads of the identity (ModelIdentifier,
/// ManufacturerName) come first; a failure of a later one does not make
/// the interview partial.
const BASIC_IDENTITY: usize = 2;
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
    pub(crate) pending: BTreeMap<(bool, u8), Pending>,
    pub(crate) tickets: BTreeMap<u64, Ticket>,
    /// APS indications handled (wrapping): tells a request sent while one
    /// is handled (a sleepy device's queue, flushed by its uplink) from
    /// the frame that released it.
    pub(crate) rx_n: u32,
    /// Control requests in flight.
    pub(crate) controls: BTreeMap<RequestId, control::Control>,
    /// Answer tickets issued.
    answers: u64,
    /// Times the events were drained.
    drains: u64,
    /// The answer ticket of the request being handled, when its result
    /// follows later.
    pub(crate) deferred: Option<u64>,
    /// Emit [`HubEvent::Frame`] for every frame (set while the API has
    /// `frames` subscribers).
    #[cfg(feature = "advanced")]
    pub frame_tap: bool,
    pub(crate) next_ticket: u64,
    pub(crate) events: Vec<HubEvent>,
    schema_decoder: Option<Box<dyn SchemaDecoder>>,
    pub(crate) schema_cache: BTreeMap<Vec<u8>, Arc<Definition>>,
    pub(crate) gp: crate::gp::GpHubState,
    /// The GP frame being processed (for event `bindings` attributes).
    pub(crate) gp_last_frame: Option<(GpdId, u8, Vec<u8>)>,
    /// Features advertised and accepted (8.5).
    pub features: Vec<&'static str>,
    /// Groups, scenes and bindings (14.9).
    pub zg: zgroups::ZgState,
    /// Global reporting defaults (5.2).
    pub(crate) report_defaults: crate::reporting::ReportDefaults,
    /// Reporting readbacks in progress.
    pub(crate) reporting_reads: crate::vecmap::VecMap<u64, crate::reporting::ReportingRead>,
    /// Last readback number.
    pub(crate) reporting_seq: u64,
    /// Cluster commands devices send to the hub's server side of these
    /// clusters go to the application ([`Hub::take_app_frames`]) instead
    /// of the engine: no default response, no events. The OTA server
    /// (0x0019) lives in the application.
    pub app_clusters: Vec<u16>,
    app_frames: Vec<AppFrame>,
    /// Frames of the application in flight: their outcomes go to
    /// [`Hub::take_app_outcomes`].
    app_sent: Vec<AppSent>,
    app_outcomes: Vec<AppOutcome>,
    /// Snapshots of the topics of services outside the engine, sent on
    /// `subscribe` (kept current by the application).
    pub app_snapshots: BTreeMap<&'static str, String>,
    /// Imported devices, heard from, waiting to be configured again
    /// ([`import`]).
    pub(crate) import_queue: std::collections::VecDeque<String>,
    /// Last probe of an imported mains device.
    pub(crate) import_probe_last: Option<Millis>,
    /// IEEE addresses of the Zigbee devices `devices/remove` removed, for
    /// the application ([`Hub::take_removed`]).
    removed: Vec<u64>,
    /// Devices whose interview the lumi quirk completed ([`lumi`]) this
    /// run: their diagnostics' `interview_quirk`. Not a [`Device`] field:
    /// a larger `Device` hits the Xtensa LLVM backend's instruction
    /// selection bug in the device map's B-tree code ("Cannot select:
    /// Constant<12288>").
    pub(crate) interview_quirks: Vec<String>,
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

/// A frame of the application in flight.
#[derive(Debug, Clone)]
struct AppSent {
    id: RequestId,
    device: String,
    cluster: u16,
    at: Millis,
}

/// The final outcome of a frame of the application ([`Hub::send_app_zcl`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppOutcome {
    /// Device.
    pub device: String,
    /// Cluster.
    pub cluster: u16,
    /// The request; `None`: a frame queued for a sleepy device that the
    /// transport refused when the device woke up (nothing was sent).
    pub id: Option<RequestId>,
    /// What happened to it.
    pub terminal: Terminal,
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
            rx_n: 0,
            controls: BTreeMap::new(),
            answers: 0,
            drains: 0,
            deferred: None,
            #[cfg(feature = "advanced")]
            frame_tap: false,
            next_ticket: 1,
            events: Vec::new(),
            schema_decoder: None,
            schema_cache: BTreeMap::new(),
            gp: Default::default(),
            gp_last_frame: None,
            features: HUB_FEATURES.to_vec(),
            zg: Default::default(),
            report_defaults: Default::default(),
            reporting_reads: Default::default(),
            reporting_seq: 0,
            app_clusters: Vec::new(),
            app_frames: Vec::new(),
            app_sent: Vec::new(),
            app_outcomes: Vec::new(),
            app_snapshots: BTreeMap::new(),
            import_queue: std::collections::VecDeque::new(),
            import_probe_last: None,
            removed: Vec::new(),
            interview_quirks: Vec::new(),
        }
    }

    /// The IEEE addresses of the Zigbee devices `devices/remove` removed
    /// since the last call (the application remembers them, so that the
    /// coordinator's tables do not bring them back).
    pub fn take_removed(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.removed)
    }

    /// Takes the frames of [`Hub::app_clusters`].
    pub fn take_app_frames(&mut self) -> Vec<AppFrame> {
        std::mem::take(&mut self.app_frames)
    }

    /// Sends a ZCL frame of the application to a device endpoint from the
    /// hub's endpoint; no response is tracked. `Ok(Some(id))`: submitted,
    /// its final outcome follows in [`Self::take_app_outcomes`];
    /// `Ok(None)`: queued for a sleepy device until it is awake (submitted
    /// then, its outcome carries the ID it got); `Err`: refused, nothing
    /// was sent.
    pub fn send_app_zcl(
        &mut self,
        dev_id: &str,
        ep: u8,
        cluster: u16,
        frame: ZclFrame,
        now: Millis,
    ) -> Result<Option<RequestId>, CoordError> {
        let Some(dev) = self.devices.get(dev_id) else {
            return Err(CoordError::Other("unknown device".into()));
        };
        let req = self.zcl_request(dev, ep, cluster, &frame);
        let tsn = frame.header.tsn;
        self.send_or_queue(dev_id, req, Purpose::App, tsn, false, now)
    }

    /// The final outcomes of the application's frames since the last call,
    /// in the order they arrived.
    pub fn take_app_outcomes(&mut self) -> Vec<AppOutcome> {
        std::mem::take(&mut self.app_outcomes)
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
        self.drains += 1;
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

    /// A Zigbee device's NWK address (a join, its Device_annce, a frame
    /// with its IEEE): what waits for it is sent to the new address
    /// ([`readdress`]), the old one no longer finds it.
    fn renumber(&mut self, id: &str, nwk: u16) {
        let Some(DeviceKind::Zigbee { nwk: n, .. }) = self.devices.get_mut(id).map(|d| &mut d.kind)
        else {
            return;
        };
        let old = std::mem::replace(n, nwk);
        if old != nwk && self.nwk_index.get(&old).is_some_and(|x| x == id) {
            self.nwk_index.remove(&old);
        }
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
        let _ = self.send_or_queue(dev_id, req, purpose, tsn, zdo, now);
    }

    /// [`Self::send_tracked`]: the request's ID, `None` when queued.
    fn send_or_queue(
        &mut self,
        dev_id: &str,
        req: ApsRequest,
        purpose: Purpose,
        tsn: u8,
        zdo: bool,
        now: Millis,
    ) -> Result<Option<RequestId>, CoordError> {
        let Some(dev) = self.devices.get_mut(dev_id) else {
            return Err(CoordError::Other("unknown device".into()));
        };
        if dev.sleepy() && now > dev.awake_until {
            dev.queue.push_back(Queued {
                req,
                purpose,
                tsn,
                tries: 0,
            });
            return Ok(None);
        }
        self.transmit(dev_id, req, purpose, tsn, zdo, 0, now)
            .map(Some)
    }

    /// Sends a request; `tries`: times it was sent before.
    #[allow(clippy::too_many_arguments)]
    fn transmit(
        &mut self,
        dev_id: &str,
        mut req: ApsRequest,
        purpose: Purpose,
        tsn: u8,
        zdo: bool,
        tries: u8,
        now: Millis,
    ) -> Result<RequestId, CoordError> {
        readdress(&mut req, self.devices.get(dev_id).and_then(Device::nwk));
        let rid = match self.transport.submit(Request::Aps(req.clone())) {
            Ok(rid) => rid,
            Err(e) => {
                self.fail_purpose(
                    dev_id,
                    &purpose,
                    ApiError::new("busy", "transport refused the request"),
                    now,
                );
                return Err(e);
            }
        };
        #[cfg(feature = "advanced")]
        self.tap_tx(dev_id, &req, now);
        self.tuya_note_sent(dev_id, &req, now);
        match purpose {
            Purpose::None => {}
            Purpose::App => self.app_sent.push(AppSent {
                id: rid,
                device: dev_id.to_string(),
                cluster: req.cluster,
                at: now,
            }),
            purpose => {
                self.pending.insert(
                    (zdo, tsn),
                    Pending {
                        device: dev_id.to_string(),
                        purpose,
                        sent_at: now,
                        retries: tries,
                        req,
                        rid,
                    },
                );
            }
        }
        Ok(rid)
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
        let groups = q.iter().any(|i| matches!(i.purpose, Purpose::Group(_)));
        for item in q {
            let zdo = item.req.profile == 0;
            let (app, cluster) = (matches!(item.purpose, Purpose::App), item.req.cluster);
            if let Err(e) = self.transmit(
                dev_id,
                item.req,
                item.purpose,
                item.tsn,
                zdo,
                item.tries,
                now,
            ) && app
            {
                self.app_outcomes.push(AppOutcome {
                    device: dev_id.to_string(),
                    cluster,
                    id: None,
                    terminal: Terminal::Refused(e),
                });
            }
        }
        // Their records say `queued` no more.
        if groups {
            self.zg_queue_changed(dev_id);
        }
    }

    /// Parks a sleepy device's request that timed out or was not
    /// delivered until the device is next heard from (one more try
    /// counted), or fails it once it had its tries.
    fn park_or_fail(&mut self, mut p: Pending, tsn: u8, err: ApiError, now: Millis) {
        let dev = p.device.clone();
        p.retries += 1;
        let parked = p.retries < max_tries(&p.purpose, true);
        if parked {
            let group = matches!(p.purpose, Purpose::Group(_));
            if let Some(d) = self.devices.get_mut(&dev) {
                d.queue.push_back(Queued {
                    req: p.req,
                    purpose: p.purpose.clone(),
                    tsn,
                    tries: p.retries,
                });
            }
            if group {
                self.zg_queue_changed(&dev);
            }
        } else {
            self.fail_purpose(&dev, &p.purpose, err, now);
        }
        // A lumi device stuck in its interview (hub::lumi).
        if parked && matches!(p.purpose, Purpose::Interview(_)) {
            self.lumi_quirk(&dev, false, now);
        }
    }

    // ------------------------------------------------------------------
    // Join, announce, interview.

    /// `EVT_DEVICE_JOINED` with an admitted decision.
    pub fn on_device_joined(&mut self, ieee: u64, nwk: u16, now: Millis) {
        let id = zigbee_id(ieee);
        if let Some(d) = self.devices.get_mut(&id) {
            d.awake_until = now + AWAKE_AFTER_JOIN_MS;
            let left = d.left;
            self.renumber(&id, nwk);
            // A device that had left is back.
            if left {
                self.seen(&id, None, None, now);
            }
            self.on_rejoin(&id, now);
            // Awake: what waits for it goes out (to its new address).
            self.flush_queue(&id, now);
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
        // Asked for: the automatic resumptions start over.
        d.interview_resumes = 0;
        d.interview_resume_at = 0;
        d.fp.endpoints.clear();
        d.awake_until = d.awake_until.max(now + AWAKE_AFTER_RX_MS);
        self.interview_quirks.retain(|x| x != id);
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
            self.continue_interview(id, interview, now);
            return;
        }
        if self.devices.get(id).is_some_and(|d| d.imported != 0) {
            self.import_contact(id, now);
            if self.devices.get(id).is_some_and(|d| {
                d.interview != InterviewStep::Done && d.interview != InterviewStep::Configure
            }) {
                return;
            }
        }
        if tuya_announce {
            self.tuya_query(id, now);
        }
        if self.resume_interview(id, now) {
            // Basic, the self-description and configure follow in it.
            self.gp_target_announced(id, now);
            return;
        }
        // New firmware (an OTA upgrade, a reflash) shows in these first.
        self.refresh_identity(id, false, now);
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

    /// A rejoin in the middle of the interview: its current step again.
    /// A request of it in flight to the device's current address is waited
    /// for instead (a join and the Device_annce of one rejoin both come
    /// here: one chain of requests, not two); one parked for the sleeping
    /// device or sent to its previous address is dropped and the step sent
    /// now.
    fn continue_interview(&mut self, id: &str, step: InterviewStep, now: Millis) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        d.queue
            .retain(|q| !matches!(q.purpose, Purpose::Interview(_)));
        let dst = ApsDst::Nwk(d.nwk().unwrap_or(0xFFFE));
        let mut in_flight = false;
        let mut stale = Vec::new();
        for (k, p) in &self.pending {
            if p.device == id && matches!(p.purpose, Purpose::Interview(_)) {
                if p.req.dst == dst {
                    in_flight = true;
                } else {
                    stale.push(*k);
                }
            }
        }
        for k in stale {
            self.pending.remove(&k);
        }
        if !in_flight {
            self.interview_step(id, step, now);
        }
    }

    /// Resumes a partial interview when the device rejoins or announces
    /// itself ([`INTERVIEW_RESUMES`]): only the answers it lacks are asked
    /// ([`resume_step`], [`next_asked`]), so what is known stays, and so do
    /// the user's names and settings. Once the interview is done (a device
    /// still being configured finishes first), not for imported devices
    /// (their verification comes first). `true`: resumed.
    fn resume_interview(&mut self, id: &str, now: Millis) -> bool {
        let Some(d) = self.devices.get_mut(id) else {
            return false;
        };
        if !d.interview_partial
            || d.interview != InterviewStep::Done
            || d.gp.is_some()
            || d.imported != 0
            || d.interview_resumes >= INTERVIEW_RESUMES
            || now < d.interview_resume_at
        {
            return false;
        }
        d.interview_resume_at = now + (INTERVIEW_RESUME_MS << d.interview_resumes);
        d.interview_resumes += 1;
        d.interview_partial = false;
        let step = resume_step(d);
        self.interview_quirks.retain(|x| x != id);
        self.interview_step(id, step, now);
        true
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

    /// Basic attributes the interview reads, one per request as
    /// zigbee-herdsman does (some devices, old Xiaomi ones among them, do
    /// not answer a read of several): the identity first
    /// ([`BASIC_IDENTITY`] of them), then the rest, which are optional (a
    /// failure does not make the interview partial).
    const BASIC_ATTRS: [u16; 9] = [
        0x0005, 0x0004, 0x0007, 0x0000, 0x0001, 0x0002, 0x0003, 0x0006, 0x4000,
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
            InterviewStep::SimpleDesc(i) => match interview_eps(d).get(i) {
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
            },
            InterviewStep::Basic(i) => {
                let ep =
                    d.fp.endpoints
                        .iter()
                        .find(|e| e.in_clusters.contains(&cl::BASIC))
                        .map(|e| e.ep);
                match (ep, Self::BASIC_ATTRS.get(i)) {
                    (Some(ep), Some(attr)) => self.read_attrs(
                        id,
                        ep,
                        cl::BASIC,
                        None,
                        &[*attr],
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
                self.renumber(&did, nwk);
                if let Some(d) = self.devices.get_mut(&did) {
                    d.awake_until = now + AWAKE_AFTER_RX_MS;
                    if capability & 0x08 == 0 {
                        d.fp.rx_on_when_idle = Some(false);
                    }
                }
                // An uplink like any other: heard from, awake, what waits
                // for it goes out (after the interview's own resumption).
                self.seen(&did, Some(ind.lqi), Some(ind.rssi), now);
                self.on_rejoin(&did, now);
                self.flush_queue(&did, now);
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
        if let Purpose::Group(op) = p.purpose {
            self.zg_zdo(id, op, ind, now);
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
                let next = next_asked(d, InterviewStep::ActiveEp);
                self.interview_step(id, next, now);
            }
            (
                InterviewStep::ActiveEp,
                ZdoMessage::ActiveEp {
                    status: 0,
                    endpoints,
                    ..
                },
            ) => {
                // Descriptors of the endpoints it still has stay until
                // read again (`reinterview` clears them all).
                d.fp.endpoints.retain(|e| endpoints.contains(&e.ep));
                d.active_eps = endpoints;
                let next = next_asked(d, InterviewStep::SimpleDesc(0));
                self.interview_step(id, next, now);
            }
            (InterviewStep::SimpleDesc(i), ZdoMessage::SimpleDesc { desc, .. }) => {
                if let Some(sd) = desc {
                    let e = EndpointFp {
                        ep: sd.endpoint,
                        profile: sd.profile,
                        device_id: sd.device_id,
                        in_clusters: sd.in_clusters,
                        out_clusters: sd.out_clusters,
                    };
                    // Replaced in place, else added in the interview's
                    // order (as a full interview stores them); a failed
                    // read keeps what was known.
                    let order = interview_eps(d);
                    let at = |ep: u8| order.iter().position(|x| *x == ep).unwrap_or(usize::MAX);
                    let eps = &mut d.fp.endpoints;
                    match eps.iter().position(|x| x.ep == e.ep) {
                        Some(j) => eps[j] = e,
                        None => {
                            let j = eps
                                .iter()
                                .position(|x| at(x.ep) > at(e.ep))
                                .unwrap_or(eps.len());
                            eps.insert(j, e);
                        }
                    }
                } else if !gp_step(d, step) {
                    d.interview_partial = true;
                }
                let next = next_asked(d, InterviewStep::SimpleDesc(i + 1));
                self.interview_step(id, next, now);
            }
            (step, _) => {
                // Failed status: continue with what is known (a lumi
                // device: its quirk, hub::lumi).
                if !gp_step(d, step) && self.lumi_quirk(id, true, now) {
                    return;
                }
                let Some(d) = self.devices.get_mut(id) else {
                    return;
                };
                d.interview_partial |= !gp_step(d, step);
                let next = match step {
                    InterviewStep::NodeDesc => InterviewStep::ActiveEp,
                    InterviewStep::ActiveEp => InterviewStep::Basic(0),
                    InterviewStep::SimpleDesc(i) => InterviewStep::SimpleDesc(i + 1),
                    s => s,
                };
                let next = next_asked(d, next);
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
        let res = self.resolution_of(d);
        self.materialize(id, res, now);
    }

    /// What the definitions make of `d` now (nothing materialized).
    pub(crate) fn resolution_of(&self, d: &Device) -> crate::resolve::Resolution {
        let fp = &d.fp;
        let gp_seen = d.gp.is_some();
        let attrs = &d.attrs;
        let defaults = &self.report_defaults;
        let gen_zb = |power: PowerKind| {
            let mut v = generic_entities(fp, &GenericInput { attrs, power });
            defaults.apply(&mut v);
            v
        };
        let gen_gp = |_p: PowerKind| crate::gp::generic::gp_generic_entities(fp, attrs);
        let generic: &dyn Fn(PowerKind) -> Vec<EntityDef> = if gp_seen { &gen_gp } else { &gen_zb };
        let sd = d.schema.fragment.clone();
        resolve(&ResolveInput {
            fp,
            generic,
            catalog: self.catalog.as_ref(),
            user: &self.user,
            self_description: sd.as_deref(),
            pinned: d.pinned.as_deref(),
            features: &self.features,
        })
    }

    fn materialize(&mut self, id: &str, res: crate::resolve::Resolution, now: Millis) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        let old_layers = d.resolution.layers.clone();
        let mut old: BTreeMap<String, Entity> =
            crate::vecmap::btree_map(d.entities.drain(..).map(|e| (e.uid.clone(), e)));
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
        // Another fingerprint (a re-interview) has other index keys: none
        // of its shards counts as checked any more, so Find definition and
        // `missing` fetch its own.
        if d.fp_hash.is_some() && !same_fp {
            d.refused_version = 0;
            d.refused_checked.clear();
        }
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
        let seeded: BTreeSet<usize> = crate::vecmap::btree_set(work.iter().map(|w| w.0));
        let unseeded: Vec<String> = d
            .entities
            .iter()
            .enumerate()
            .filter(|(i, e)| added.contains(&e.uid) && !seeded.contains(i))
            .map(|(_, e)| e.uid.clone())
            .collect();
        self.apply_raw(id, work, false, now);
        for u in unseeded {
            self.publish_state(&u);
        }
    }

    /// Emits an `entity_state` for a uid.
    pub(crate) fn publish_state(&mut self, uid: &str) {
        self.publish(uid, false);
    }

    /// [`Self::publish_state`]; `pushed`: the device sent the value on its
    /// own (an attribute report, a command, a Tuya DP, a GP frame; never a
    /// read response or a value the hub set): the event's state message
    /// says so with `pushed: true` for local automations (`reported`
    /// triggers); the API drops it.
    pub(crate) fn publish(&mut self, uid: &str, pushed: bool) {
        let Some((d, e)) = self.entity(uid) else {
            return;
        };
        let mut msg = e.state_msg(d.available);
        if pushed {
            msg["pushed"] = true.into();
        }
        self.emit(HubEvent::EntityState(msg));
        self.zg_member_changed(uid);
    }

    // ------------------------------------------------------------------
    // Indications.

    /// `EVT_APS_DATA_INDICATION`.
    pub fn on_aps_indication(&mut self, ind: ApsIndication, now: Millis) {
        self.rx_n = self.rx_n.wrapping_add(1);
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
        // Its IEEE with another short address: it rejoined unseen.
        if ind.src_ieee.map(zigbee_id).as_ref() == Some(&id)
            && ind.src_nwk < 0xFFF8
            && self
                .devices
                .get(&id)
                .and_then(Device::nwk)
                .is_some_and(|n| n != ind.src_nwk)
        {
            self.renumber(&id, ind.src_nwk);
        }
        #[cfg(feature = "advanced")]
        self.tap_rx(&id, &ind, now);
        self.seen(&id, Some(ind.lqi), Some(ind.rssi), now);
        if let Some(d) = self.devices.get_mut(&id) {
            d.awake_until = now + AWAKE_AFTER_RX_MS;
            d.diag.frame(now, ind.cluster, &ind.payload);
        }
        self.flush_queue(&id, now);
        // Before the frame is handled: what it changes (a new SWBuildID)
        // finds the imported device verified.
        self.import_contact(&id, now);
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
        d.left = false;
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
        #[cfg(feature = "advanced")]
        self.console_answer(id, &frame);
        let h = frame.header;
        let ep = ind.src_ep;
        self.stub_endpoint(id, ep, ind.profile, ind.cluster);
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
                // Identity strings and versions from any answer (an
                // interview read stores them, others only fill gaps).
                if ind.cluster == cl::BASIC && !matches!(purpose, Some(Purpose::Identity)) {
                    let solicited =
                        matches!(purpose, Some(Purpose::Interview(InterviewStep::Basic(_))));
                    structural |= self.learn_basic(id, &values, solicited, now);
                }
                match purpose {
                    Some(Purpose::Interview(step)) => {
                        self.interview_read(id, step, ind.cluster, &recs, now)
                    }
                    Some(Purpose::Configure(key)) => {
                        self.configure_done(id, &key, true, "SUCCESS".into(), now)
                    }
                    Some(Purpose::GpProbe(nwk)) => self.gp_probe_result(nwk, &recs, now),
                    Some(Purpose::Identity) => self.identity_read(id, &recs, now),
                    _ => {}
                }
                self.dispatch_attrs(id, ep, ind.cluster, h.mfr, &values, false, now);
                if structural {
                    self.maybe_reresolve(id, now);
                }
            }
            (FrameType::Global, global::REPORT_ATTRIBUTES) => {
                let recs = zf::parse_report_lenient(ind.cluster, &frame.payload);
                let mfr = lumi::report_mfr(ind.cluster, h.mfr, &recs);
                let mut structural = false;
                if let Some(d) = self.devices.get_mut(id) {
                    for (a, v) in &recs {
                        let (sc, vc) = d.attrs.set(ep, ind.cluster, *a, mfr, Some(v.value.clone()));
                        structural |= sc || (vc && is_structural(ind.cluster, *a));
                    }
                }
                if ind.cluster == cl::BASIC {
                    structural |= self.learn_basic(id, &recs, false, now);
                }
                self.dispatch_attrs(id, ep, ind.cluster, mfr, &recs, true, now);
                if ind.cluster == cl::ON_OFF {
                    self.client_on_off(id, ep, h.tsn, &recs, now);
                }
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
            (FrameType::Global, global::READ_REPORTING_CONFIG_RSP) => {
                needs_default_rsp = false;
                self.reporting_rsp(id, ep, ind.cluster, h.tsn, &frame.payload, now);
            }
            (FrameType::Global, global::DEFAULT_RSP) => {
                needs_default_rsp = false;
                if let Ok((_, st)) = zf::parse_default_rsp(&frame.payload) {
                    self.complete_tsn(id, h.tsn, st, now);
                }
            }
            (FrameType::Global, global::READ_ATTRIBUTES)
                if ind.cluster == cl::TIME && ind.dst_ep == self.cfg.hub_endpoint =>
            {
                needs_default_rsp = false;
                let attrs = zf::parse_read_attributes(&frame.payload).unwrap_or_default();
                self.serve_time(id, ep, h.tsn, &attrs, now);
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
                        // A notification sent again (same TSN) is no new alarm.
                        if frame.payload.len() >= 2 && !self.duplicate(id, ep, ind.cluster, &h, now)
                        {
                            let zs = u16::from_le_bytes([frame.payload[0], frame.payload[1]]);
                            let tv = TypedValue::uint(ZclType::MAP16, u64::from(zs));
                            if let Some(d) = self.devices.get_mut(id) {
                                d.attrs
                                    .set(ep, cl::IAS_ZONE, 0x0002, None, Some(tv.value.clone()));
                            }
                            self.dispatch_attrs(
                                id,
                                ep,
                                cl::IAS_ZONE,
                                None,
                                &[(0x0002, tv)],
                                true,
                                now,
                            );
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
                    cl::GROUPS | cl::SCENES
                        if h.direction == Direction::ServerToClient
                            && self.zg_zcl(id, ind.cluster, h.tsn, cmd, &frame.payload) =>
                    {
                        needs_default_rsp = false;
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

    /// A frame from an endpoint of a device whose endpoint list the
    /// interview has not got: kept as a stub endpoint (zigbee-herdsman
    /// creates endpoints the same way), with Basic as a server cluster
    /// once Basic frames came from it, so that the interview's Basic reads
    /// find it. Stubs ([`STUB_DEVICE_ID`]) give no entities; a Simple
    /// descriptor replaces them.
    fn stub_endpoint(&mut self, id: &str, ep: u8, profile: u16, cluster: u16) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        if !d.active_eps.is_empty()
            || !d.fp.endpoints.iter().all(is_stub)
            || d.gp.is_some()
            || !(1..=240).contains(&ep)
        {
            return;
        }
        let basic = cluster == cl::BASIC;
        match d.fp.endpoints.iter_mut().find(|e| e.ep == ep) {
            Some(e) if is_stub(e) && basic && !e.in_clusters.contains(&cl::BASIC) => {
                e.in_clusters.push(cl::BASIC)
            }
            Some(_) => {}
            None => d.fp.endpoints.push(EndpointFp {
                ep,
                profile,
                device_id: STUB_DEVICE_ID,
                in_clusters: if basic { vec![cl::BASIC] } else { Vec::new() },
                out_clusters: Vec::new(),
            }),
        }
    }

    /// Basic attributes into the fingerprint, from any Read Attributes
    /// Response or Report Attributes: an interview read (`solicited`)
    /// stores what it got, any other frame only fills what is unknown
    /// (lumi devices report their ModelIdentifier unasked, and may never
    /// answer a read). `true`: the identity changed; outside the interview
    /// the device record follows.
    fn learn_basic(
        &mut self,
        id: &str,
        recs: &[(u16, TypedValue)],
        solicited: bool,
        now: Millis,
    ) -> bool {
        let Some(d) = self.devices.get_mut(id) else {
            return false;
        };
        let fp = &mut d.fp;
        let mut changed = false;
        for (attr, v) in recs {
            let n = v.value.as_i64().map(|x| x as u32);
            let mut text = |slot: &mut Option<String>| {
                if let Some(t) = basic_text(&v.value)
                    && (solicited || slot.is_none())
                    && slot.as_deref() != Some(t.as_str())
                {
                    *slot = Some(t);
                    changed = true;
                }
            };
            match attr {
                0x0004 => text(&mut fp.manufacturer_name),
                0x0005 => text(&mut fp.model_id),
                0x0006 => text(&mut fp.date_code),
                0x4000 => text(&mut fp.sw_build_id),
                _ => {
                    let slot = match attr {
                        0x0000 => &mut fp.zcl_version,
                        0x0001 => &mut fp.app_version,
                        0x0002 => &mut fp.stack_version,
                        0x0003 => &mut fp.hw_version,
                        0x0007 => {
                            let p = n.map(|x| x as u8);
                            if p.is_some() && (solicited || fp.power_source.is_none()) {
                                changed |= fp.power_source != p;
                                fp.power_source = p;
                            }
                            continue;
                        }
                        _ => continue,
                    };
                    if n.is_some() && (solicited || slot.is_none()) {
                        changed |= *slot != n;
                        *slot = n;
                    }
                }
            }
        }
        if !changed {
            return false;
        }
        let interviewing = matches!(
            d.interview,
            InterviewStep::NodeDesc
                | InterviewStep::ActiveEp
                | InterviewStep::SimpleDesc(_)
                | InterviewStep::Basic(_)
        );
        if interviewing {
            // A lumi device stuck in its interview: now known.
            if !solicited && self.interview_stuck(id) {
                self.lumi_quirk(id, false, now);
            }
        } else {
            self.emit(HubEvent::DeviceUpdated(id.to_string()));
        }
        true
    }

    /// An interview request of the device failed at least once (it waits
    /// parked for the device, or was sent again).
    pub(crate) fn interview_stuck(&self, id: &str) -> bool {
        self.devices.get(id).is_some_and(|d| {
            d.queue
                .iter()
                .any(|q| matches!(q.purpose, Purpose::Interview(_)) && q.tries > 0)
        }) || self
            .pending
            .values()
            .any(|p| p.device == id && matches!(p.purpose, Purpose::Interview(_)) && p.retries > 0)
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

    /// Read Attributes on the hub's Time cluster server: thermostats,
    /// clocks and other devices take the hub's time from it.
    fn serve_time(&mut self, id: &str, ep: u8, tsn: u8, attrs: &[u16], now: Millis) {
        let recs: Vec<_> = attrs
            .iter()
            .map(|&a| zf::ReadRecord {
                attr: a,
                ..time_attr(&self.cfg.clock, now, a)
            })
            .collect();
        let Ok(payload) = zf::read_attributes_rsp(&recs) else {
            return;
        };
        let f = ZclFrame {
            header: ZclHeader {
                frame_type: FrameType::Global,
                mfr: None,
                direction: Direction::ServerToClient,
                disable_default_response: true,
                tsn,
                cmd: global::READ_ATTRIBUTES_RSP,
            },
            payload,
        };
        self.send_zcl(id, ep, cl::TIME, f, Purpose::None, now);
    }

    pub(crate) fn take_pending(&mut self, id: &str, tsn: u8) -> Option<Pending> {
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
            Purpose::Group(op) => self.zg_status(id, op, st),
            r @ Purpose::Reporting { .. } => self.reporting_failed(id, &r, Ok(st), now),
            // A Default Response instead of the answer of an interview
            // read (UNSUP_GENERAL_COMMAND …): the step failed, the
            // interview goes on.
            i @ Purpose::Interview(_) => self.fail_purpose(id, &i, ApiError::zcl(st), now),
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
            // What it answered is stored already (`learn_basic`).
            InterviewStep::Basic(i) if cluster == cl::BASIC => {
                if let Some(d) = self.devices.get_mut(id) {
                    d.identity_read_at = Some(now);
                }
                self.interview_step(id, InterviewStep::Basic(i + 1), now);
            }
            InterviewStep::SchemaHeader => self.schema_header(id, recs, now),
            InterviewStep::SchemaChunk(i) => self.schema_chunk(id, i, recs, now),
            _ => {}
        }
    }

    /// Reads Basic DateCode and SWBuildID again and re-resolves the device
    /// when they changed (zigbee-herdsman-converters selects definitions
    /// and white labels by them; Zigbee2MQTT reads them after an OTA
    /// upgrade). `force`: after an upgrade; otherwise (a rejoin) at most
    /// once an hour, and never for lumi devices (they answer few reads;
    /// `hub::lumi`), which are read only when forced (an upgrade, an
    /// imported device's probe). Interviewed Zigbee devices with Basic
    /// only.
    pub fn refresh_identity(&mut self, id: &str, force: bool, now: Millis) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        if !matches!(d.kind, DeviceKind::Zigbee { .. })
            || (!force && lumi::is_lumi(&d.fp))
            || !matches!(d.interview, InterviewStep::Done | InterviewStep::Configure)
            || (!force
                && d.identity_read_at
                    .is_some_and(|t| now.saturating_sub(t) < IDENTITY_REREAD_MS))
        {
            return;
        }
        let Some(ep) =
            d.fp.endpoints
                .iter()
                .find(|e| e.in_clusters.contains(&cl::BASIC))
                .map(|e| e.ep)
        else {
            return;
        };
        d.identity_read_at = Some(now);
        self.read_attrs(
            id,
            ep,
            cl::BASIC,
            None,
            &[0x0006, 0x4000],
            Purpose::Identity,
            now,
        );
    }

    fn identity_read(&mut self, id: &str, recs: &[zf::ReadRecord], now: Millis) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        let mut changed = false;
        for r in recs {
            let Some(v) = r.value.as_ref().and_then(|v| basic_text(&v.value)) else {
                continue;
            };
            let slot = match r.attr {
                0x0006 => &mut d.fp.date_code,
                0x4000 => &mut d.fp.sw_build_id,
                _ => continue,
            };
            if slot.as_deref() != Some(v.as_str()) {
                *slot = Some(v);
                changed = true;
            }
        }
        if changed {
            self.reresolve(Some(&[id.to_string()]), now);
            self.emit(HubEvent::DeviceUpdated(id.to_string()));
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
        push: bool,
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
                            // A ZCL structure (Xiaomi Basic 0xFF02): the
                            // tag is the element's index.
                            ZclValue::Struct(items) => items
                                .get(usize::from(*tag))
                                .map_or(Value::Null, |(_, v)| values::from_zcl(v)),
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
        self.apply_raw(id, work, push, now);
    }

    /// Decodes raw values for `(entity, part)` and applies them; `push`:
    /// the device sent them on its own (see [`Self::publish`]).
    pub(crate) fn apply_raw(
        &mut self,
        id: &str,
        work: Vec<(usize, Option<usize>, Value)>,
        push: bool,
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
            let scale = |c: u16, a: u16| d.attrs.number(c, a);
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
        if d.resolution.metering_zero_debounce {
            crate::templates::zero_debounce(d, &mut decoded, now);
        }
        // An IAS Zone bit (the zone status feeds alarm, tamper, battery
        // low…) is pushed only when set, and either just set or no other
        // bit changed: a repeated alarm is a new press, a battery-low or
        // tamper change is not one, nor is the alarm clearing.
        let mut pushed = BTreeSet::new();
        if push {
            let zone: Vec<Option<bool>> = decoded
                .iter()
                .map(|(i, p, v, _)| {
                    let e = &d.entities[*i];
                    let bit = p.is_none()
                        && matches!(&e.r.def.value.binding,
                            Binding::Zcl(z) if z.cluster == cl::IAS_ZONE && z.attr == 0x0002);
                    bit.then(|| e.state.value != *v)
                })
                .collect();
            for (k, (i, _, v, _)) in decoded.iter().enumerate() {
                let own = match zone[k] {
                    None => true,
                    Some(changed) => {
                        *v == Value::Bool(true)
                            && (changed
                                || !zone
                                    .iter()
                                    .enumerate()
                                    .any(|(j, c)| j != k && *c == Some(true)))
                    }
                };
                if own {
                    pushed.insert(*i);
                }
            }
        }
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
        let uids: Vec<(String, bool)> = touched
            .iter()
            .map(|&i| (d.entities[i].uid.clone(), pushed.contains(&i)))
            .collect();
        for (u, p) in uids {
            self.publish(&u, p);
        }
    }

    /// De-duplication of commands: the same endpoint, cluster, TSN and
    /// command from device `id` within 2 s.
    fn duplicate(&mut self, id: &str, ep: u8, cluster: u16, h: &ZclHeader, now: Millis) -> bool {
        let Some(d) = self.devices.get_mut(id) else {
            return true;
        };
        let key = (u64::from(ep) << 40)
            | (u64::from(cluster) << 24)
            | (u64::from(h.tsn) << 8)
            | u64::from(h.cmd);
        d.dedup.retain(|(t, _)| now.saturating_sub(*t) < DEDUP_MS);
        if d.dedup.iter().any(|(_, k)| *k == key) {
            d.diag.count("duplicate_command");
            return true;
        }
        d.dedup.push_back((now, key));
        false
    }

    fn dispatch_command(&mut self, id: &str, ep: u8, cluster: u16, frame: &ZclFrame, now: Millis) {
        let h = frame.header;
        if self.duplicate(id, ep, cluster, &h, now) {
            return;
        }
        let d = self.devices.get_mut(id).unwrap();
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
        self.publish(&uid, true);
    }

    // ------------------------------------------------------------------
    // Tick: timeouts, availability, timers.

    /// The final outcome of an APS request. A delivery failure: one
    /// immediate retry (the NCP already retried at APS level; a sleepy
    /// device's request waits for its next frame), then the request fails
    /// with `not_delivered` instead of waiting for the response timeout.
    /// Other outcomes leave it to the response.
    fn aps_done(&mut self, id: RequestId, t: Terminal, now: Millis) {
        if let Some(i) = self.app_sent.iter().position(|s| s.id == id) {
            let s = self.app_sent.swap_remove(i);
            self.app_outcomes.push(AppOutcome {
                device: s.device,
                cluster: s.cluster,
                id: Some(id),
                terminal: t,
            });
            return;
        }
        let err = match &t {
            Terminal::NotDelivered(e) => not_delivered(e),
            Terminal::Refused(_) => ApiError::new(
                "not_delivered",
                "the NCP refused the request (queue full or network down)",
            ),
            _ => return,
        };
        let Some(k) = self
            .pending
            .iter()
            .find(|(_, p)| p.rid == id)
            .map(|(k, _)| *k)
        else {
            return; // answered meanwhile, or nothing waits for it
        };
        let Some(mut p) = self.pending.remove(&k) else {
            return;
        };
        let sleepy = self.devices.get(&p.device).is_some_and(Device::sleepy);
        if sleepy {
            self.park_or_fail(p, k.1, err, now);
            return;
        }
        if p.retries == 0 && p.purpose != Purpose::Check {
            p.retries = 1;
            p.sent_at = now;
            readdress(
                &mut p.req,
                self.devices.get(&p.device).and_then(Device::nwk),
            );
            if let Ok(rid) = self.transport.submit(Request::Aps(p.req.clone())) {
                p.rid = rid;
                let (dev, interview) =
                    (p.device.clone(), matches!(p.purpose, Purpose::Interview(_)));
                self.pending.insert(k, p);
                if interview {
                    self.lumi_quirk(&dev, false, now);
                }
                return;
            }
        }
        let dev = p.device.clone();
        self.fail_purpose(&dev, &p.purpose, err, now);
    }

    /// Advances timers.
    pub fn tick(&mut self, now: Millis) {
        // The application's frames whose outcome never came.
        while let Some(s) = self
            .app_sent
            .iter()
            .find(|s| now.saturating_sub(s.at) > APP_OUTCOME_MS)
        {
            self.aps_done(s.id, Terminal::Unknown(CoordError::Timeout), now);
        }
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
            let timeout = ApiError::new("timeout", "no response from the device");
            if self.devices.get(&p.device).is_some_and(Device::sleepy) {
                self.park_or_fail(p, k.1, timeout, now);
                continue;
            }
            if p.retries + 1 < max_tries(&p.purpose, false) {
                p.retries += 1;
                p.sent_at = now;
                readdress(
                    &mut p.req,
                    self.devices.get(&p.device).and_then(Device::nwk),
                );
                if let Ok(rid) = self.transport.submit(Request::Aps(p.req.clone())) {
                    p.rid = rid;
                    let (dev, interview) =
                        (p.device.clone(), matches!(p.purpose, Purpose::Interview(_)));
                    self.pending.insert(k, p);
                    if interview {
                        self.lumi_quirk(&dev, false, now);
                    }
                }
                continue;
            }
            let dev = p.device.clone();
            self.fail_purpose(&dev, &p.purpose, timeout, now);
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
        self.zg_tick(now);
        self.import_tick(now);
    }

    fn fail_purpose(&mut self, id: &str, purpose: &Purpose, err: ApiError, now: Millis) {
        match purpose {
            Purpose::Interview(step) => {
                let optional = matches!(*step, InterviewStep::Basic(i) if i >= BASIC_IDENTITY);
                let descriptor = matches!(
                    *step,
                    InterviewStep::NodeDesc
                        | InterviewStep::ActiveEp
                        | InterviewStep::SimpleDesc(_)
                        | InterviewStep::Basic(_)
                );
                if descriptor
                    && !optional
                    && !self.devices.get(id).is_some_and(|d| gp_step(d, *step))
                    && self.lumi_quirk(id, true, now)
                {
                    return;
                }
                if let Some(d) = self.devices.get_mut(id) {
                    d.interview_partial |= !gp_step(d, *step) && !optional;
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
                let next = self.devices.get(id).map_or(next, |d| next_asked(d, next));
                self.interview_step(id, next, now);
            }
            Purpose::Configure(key) => self.configure_done(id, key, false, err.code.clone(), now),
            Purpose::Command(t) => self.ticket_part_done(*t, Some(err), now),
            #[cfg(feature = "advanced")]
            Purpose::Console { ticket, .. } => self.emit(HubEvent::ConsoleResult {
                ticket: *ticket,
                result: Err(err),
            }),
            Purpose::GpProbe(nwk) => self.gp_probe_result(*nwk, &[], now),
            Purpose::Group(op) => self.zg_failed(id, op.clone(), err),
            Purpose::Reporting { .. } => self.reporting_failed(id, purpose, Err(err), now),
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
        // Held zero readings (template tuya_metering_zero_debounce).
        for (i, v) in crate::templates::due(d, now) {
            set_value(d, i, None, v, now);
            touched.push(d.entities[i].uid.clone());
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
                    e.r.def.values().find_map(|(_, v)| {
                        match (&v.binding, value_report(d, e.r.ep, v)) {
                            (Binding::Zcl(z), Some(ReportSpec::Config { max, .. }))
                                if z.cluster == c && z.attr == a =>
                            {
                                Some((u64::from(max.max(10)), z.mfr))
                            }
                            _ => None,
                        }
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
            .flat_map(|e| {
                e.r.def
                    .values()
                    .map(|(_, v)| value_report(d, e.r.ep, v))
                    .collect::<Vec<_>>()
            })
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

    /// The coordinator reports that a device left the network (whether it
    /// rejoins is not said): it stays, unavailable with the reason `left`
    /// (`device_updated`, its entities' states), until its next frame or
    /// rejoin. Unknown devices are ignored.
    pub fn on_device_left(&mut self, ieee: u64) {
        let id = zigbee_id(ieee);
        let Some(d) = self.devices.get_mut(&id) else {
            return;
        };
        if d.left {
            return;
        }
        d.left = true;
        self.set_unavailable(&id);
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
        let uids: BTreeSet<String> =
            crate::vecmap::btree_set(tk.optimistic.iter().map(|o| o.0.clone()));
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

    /// Removes a device (`devices/remove`): a Zigbee device is asked to
    /// leave (ZDO Mgmt_Leave_req, unless `force`), then the coordinator
    /// purges it from its tables ([`Request::RemoveDevice`], whatever
    /// `force`) and its IEEE goes to [`Self::take_removed`].
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
                let req = Request::RemoveDevice {
                    ieee,
                    nwk: Some(nwk),
                };
                self.control(req, control::Then::Nothing, None, now);
                self.nwk_index.remove(&nwk);
                self.removed.push(ieee);
            }
            DeviceKind::Gp { gpd_id } => {
                let removed = control::GpRemoved::Device(id.to_string());
                return self.gp_remove(gpd_id, true, removed, now).map(drop);
            }
        }
        self.forget_device(id);
        Ok(())
    }

    /// Drops a device, its entities and what waits for it, without network
    /// traffic (a removed device; the hub's records of a network cleared).
    pub fn forget_device(&mut self, id: &str) {
        if let Some(d) = self.devices.remove(id) {
            for e in &d.entities {
                self.emit(HubEvent::EntityRemoved(e.uid.clone()));
            }
        }
        self.dispatch.remove(id);
        self.pending.retain(|_, p| p.device != id);
        self.interview_quirks.retain(|x| x != id);
        self.emit(HubEvent::DeviceRemoved(id.to_string()));
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
        // A user definition of the same model under another ID is replaced.
        for old in self.user.insert(def) {
            self.user_sources.remove(&old);
        }
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

/// The endpoints of `active` whose Simple descriptors the interview reads
/// and stores, in order: all, the GP endpoint (242) last. 242 is kept for
/// matching (zigbee-herdsman-converters' fingerprints list it);
/// GP-profile endpoints get no entities, binds or reporting.
pub fn interview_endpoints(active: &[u8]) -> Vec<u8> {
    let gp = crate::gp::GP_ENDPOINT;
    let mut eps: Vec<u8> = active.iter().copied().filter(|e| *e != gp).collect();
    if active.contains(&gp) {
        eps.push(gp);
    }
    eps
}

fn interview_eps(d: &Device) -> Vec<u8> {
    interview_endpoints(&d.active_eps)
}

/// Where a partial interview resumes: the first answer it lacks
/// ([`lacking`]).
fn resume_step(d: &Device) -> InterviewStep {
    lacking(d, InterviewStep::NodeDesc)
}

/// The first descriptor step from `from` on whose answer the device lacks
/// (the Node descriptor, the endpoint list, a Simple descriptor), else
/// the Basic reads (which batch failed is not kept: both are read again);
/// any later step as it is.
fn lacking(d: &Device, from: InterviewStep) -> InterviewStep {
    use InterviewStep::{ActiveEp, Basic, NodeDesc, SimpleDesc};
    let first = match from {
        NodeDesc if d.node_desc.is_none() => return NodeDesc,
        NodeDesc | ActiveEp if d.active_eps.is_empty() => return ActiveEp,
        NodeDesc | ActiveEp => 0,
        SimpleDesc(i) => i,
        s => return s,
    };
    let missing = interview_eps(d)
        .iter()
        .skip(first)
        .position(|&ep| d.fp.endpoint(ep).is_none_or(is_stub));
    missing.map_or(Basic(0), |j| SimpleDesc(first + j))
}

/// The step asked after one, `next` in the interview's order: a resumed
/// interview ([`Hub::resume_interview`]; `interview_resumes` is 0 in a
/// new or requested one) asks only what the device lacks, from `next` on,
/// so that a failure there loses nothing it knew.
fn next_asked(d: &Device, next: InterviewStep) -> InterviewStep {
    if d.interview_resumes > 0 {
        lacking(d, next)
    } else {
        next
    }
}

/// Device ID of a stub endpoint ([`Hub::stub_endpoint`]): one frames came
/// from while the device's endpoint list is unknown.
pub const STUB_DEVICE_ID: u16 = 0xFFFF;

/// An endpoint known from frames only, not from its Simple descriptor.
pub fn is_stub(e: &EndpointFp) -> bool {
    e.device_id == STUB_DEVICE_ID
}

/// The interview step asks the GP endpoint's descriptor: optional, a
/// failure does not make the interview partial.
fn gp_step(d: &Device, step: InterviewStep) -> bool {
    matches!(step, InterviewStep::SimpleDesc(i)
        if interview_eps(d).get(i) == Some(&crate::gp::GP_ENDPOINT))
}

/// The schema endpoint of a device (0xFC6B server).
fn schema_endpoint(d: &Device) -> Option<u8> {
    d.fp.endpoints
        .iter()
        .find(|e| e.in_clusters.contains(&cl::ZRS_SCHEMA))
        .map(|e| e.ep)
}

/// A Basic character string as the fingerprint keeps it: up to a NUL,
/// trailing spaces stripped; nothing when empty.
fn basic_text(v: &ZclValue) -> Option<String> {
    match v {
        ZclValue::String(s) => {
            let t = s.split('\0').next().unwrap_or("").trim_end();
            (!t.is_empty()).then(|| t.to_string())
        }
        _ => None,
    }
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
    e.state.restored = false;
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
    /// Records what the catalog service found for device `id` in its
    /// shard `shard` of catalog `version` (9.3): the definitions it matches
    /// that this firmware cannot run. They replace what that shard found
    /// before; the other shards' findings stay. `checked`: the shard counts
    /// as checked under `version` (not when storing a definition for the
    /// device failed). Once all of `device_shards` are checked, findings of
    /// other shards (no longer the device's) are dropped. A change emits
    /// `device_updated` (the record's `definition_approximate`; the
    /// registry keeps it).
    pub fn note_refused(
        &mut self,
        id: &str,
        version: u64,
        shard: u8,
        device_shards: &[u8],
        checked: bool,
        found: Vec<crate::device::Refused>,
    ) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        let old = (
            d.refused.clone(),
            d.refused_version,
            d.refused_checked.clone(),
            d.refused_recheck,
        );
        if d.refused_version != version {
            d.refused_version = version;
            d.refused_checked.clear();
        }
        d.refused.retain(|r| r.shard != shard);
        for mut r in found {
            r.shard = shard;
            d.refused.push(r);
        }
        if checked && !d.refused_checked.contains(&shard) {
            d.refused_checked.push(shard);
        }
        if device_shards.iter().all(|n| d.refused_checked.contains(n)) {
            d.refused.retain(|r| device_shards.contains(&r.shard));
            d.refused_recheck = false;
        }
        let now = (
            &d.refused,
            d.refused_version,
            &d.refused_checked,
            d.refused_recheck,
        );
        if now != (&old.0, old.1, &old.2, old.3) {
            self.emit(HubEvent::DeviceUpdated(id.to_string()));
        }
    }

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
            "catalog_refused": d.refused_better().iter().map(|r| r.message()).collect::<Vec<_>>(),
            "definition_approximate": d.approximate_json(),
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
            "interview_resumes": d.interview_resumes,
            "interview_quirk": self.interview_quirks.iter().any(|x| x == id).then_some("lumi"),
        }))
    }
}

/// The error of an undeliverable request: the delivery status (ZRHP:
/// `EVT_APS_DATA_CONFIRM.zb_status`, named in the NCP's numbering).
fn not_delivered(e: &CoordError) -> ApiError {
    let message = match e.meaning() {
        CoordError::NoAck(c) | CoordError::Backend { code: c, .. } => match c.text {
            Some(t) => format!("not delivered: {t} (zb_status 0x{:02X})", c.value),
            None => format!("not delivered (zb_status 0x{:02X})", c.value),
        },
        _ => format!("not delivered: {e}"),
    };
    ApiError::new("not_delivered", message)
}

/// One Time cluster attribute (ZCL 3.12.2) for the hub's clock at `now`.
pub fn time_attr(c: &Clock, now: Millis, attr: u16) -> zf::ReadRecord {
    let utc = (crate::device::wall_ms(now).div_euclid(1000) - ZCL_EPOCH_UNIX).clamp(0, 0xFFFF_FFFE)
        as u32;
    let in_dst = c.dst.is_some_and(|(s, e, _)| {
        if s <= e {
            utc >= s && utc < e
        } else {
            utc >= s || utc < e
        }
    });
    let shift = c.dst.map_or(0, |d| d.2);
    let std = utc.wrapping_add_signed(c.tz_s);
    let (ty, v): (ZclType, u64) = match attr {
        0x0000 => (
            ZclType::UTC,
            if c.synced {
                u64::from(utc)
            } else {
                0xFFFF_FFFF
            },
        ),
        // Master | Synchronized | MasterZoneDst once synced.
        0x0001 => (ZclType(0x18), if c.synced { 0x07 } else { 0 }),
        0x0002 => (ZclType(0x2B), c.tz_s as u32 as u64),
        0x0003 => (ZclType(0x23), u64::from(c.dst.map_or(0, |d| d.0))),
        0x0004 => (ZclType(0x23), u64::from(c.dst.map_or(0, |d| d.1))),
        0x0005 => (ZclType(0x2B), shift as u32 as u64),
        0x0006 => (ZclType(0x23), u64::from(std)),
        0x0007 => (
            ZclType(0x23),
            u64::from(if in_dst {
                std.wrapping_add_signed(shift)
            } else {
                std
            }),
        ),
        0x0008 => (ZclType::UTC, u64::from(c.last_set.unwrap_or(0xFFFF_FFFF))),
        0x0009 => (ZclType::UTC, 0xFFFF_FFFF),
        _ => {
            return zf::ReadRecord {
                attr,
                status: Status::UNSUPPORTED_ATTRIBUTE,
                value: None,
            };
        }
    };
    let value = if matches!(ty.0, 0x2B) {
        ZclValue::Int(i64::from(v as u32 as i32))
    } else {
        ZclValue::Uint(v)
    };
    zf::ReadRecord {
        attr,
        status: Status::SUCCESS,
        value: Some(TypedValue { ty, value }),
    }
}
