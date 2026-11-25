//! Network and hub maintenance API (device-support 14.6, API 1.2): energy
//! scan with a channel recommendation, channel change, TX power, NCP
//! diagnostics, the network map, backup/restore (Open Coordinator Backup),
//! forming a network (a new one after a reset, or where the NCP has
//! none), and the hub's own settings (Wi-Fi,
//! API token, reboot, boot mode, system facts, log tail).
//!
//! Requests reach this module only from authenticated sessions. Everything
//! that talks to the NCP runs on one job thread at a time (the app thread
//! must keep serving events); its result frame and `network` events come
//! back through the app's input channel. Keys only appear in backup files
//! sent to the requesting session and in the host store, never in logs.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use hub_coord::{
    Beacon, Capabilities, CoordError, CoordStatus, Diagnostics, EnergyChannel, FormParams, Kind, NetState, NetworkAdmin,
    NetworkOwnership, RegistryState, RestoreOptions,
};
use hub_devices::api::{ApiServer, ServerMessage, SessionId};
use hub_devices::hub::ApiError;
use hub_store::{KvStore, kv};
use log::{info, warn};
use serde_json::{Value as J, json};

use crate::backend::Records;
use crate::clock;
use crate::jobs::{self, Lane};
use crate::jw::{Jw, oom};
use crate::ocb;
use crate::platform::Platform;
use crate::topology::{self, Known, ScanOpts};

/// Request types handled here.
pub const TYPES: &[&str] = &[
    "network/energy_scan",
    "network/active_scan",
    "network/change_channel",
    "network/set_tx_power",
    "network/diagnostics",
    "network/topology",
    "network/backup",
    "network/backup/last",
    "network/jobs",
    "network/job/result",
    "network/job/cancel",
    "network/restore",
    "network/reset",
    "network/form",
    "network/registry/adopt",
    "network/registry/clear",
    "hub/system",
    "hub/logs",
    "hub/wifi/set",
    "hub/token/rotate",
    "hub/reboot",
    "hub/mode/set",
    "hub/time",
    "hub/time/set",
    "hub/developer/set",
];

/// Host store keys of the time settings.
pub const TZ_KEY: &str = "cfg/tz";
/// SNTP servers, comma separated.
pub const NTP_KEY: &str = "cfg/ntp";
/// Host store key of the developer mode (present: on).
pub const DEVELOPER_KEY: &str = "cfg/dev";

/// Host store key of the last complete backup (OCB text).
pub const LAST_BACKUP_KEY: &str = "bkp/last";
/// Devices a staged restore accepts.
pub const MAX_RESTORE_DEVICES: usize = 2000;

/// A request this module handles: `id`, `type` and the whole frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Head {
    /// Request ID.
    pub id: u64,
    /// Request type.
    pub ty: String,
    /// The frame (parameters are read from it; no serde derives, which
    /// cost kilobytes of firmware each).
    pub v: J,
}

/// The request if its type is one of [`TYPES`].
pub fn head(text: &str) -> Option<Head> {
    if !TYPES.iter().any(|t| text.contains(t)) {
        return None;
    }
    let v: J = serde_json::from_str(text).ok()?;
    let ty = v.get("type")?.as_str().filter(|t| TYPES.contains(t))?.to_string();
    Some(Head { id: v.get("id")?.as_u64()?, ty, v })
}

/// What a job sends back to the app thread.
pub enum JobOut {
    /// A frame for a session.
    Reply(SessionId, String),
    /// A `network` topic event.
    Event(J),
    /// The NCP's network was replaced: import its devices again.
    Reimport,
    /// A restored backup's groups (`metadata.internal.zigbee_rs.groups`)
    /// replace the hub's.
    Groups(Vec<J>),
}

/// Delivery of [`JobOut`]s.
pub type Out = Arc<dyn Fn(JobOut) + Send + Sync>;

/// Everything a request may need from the app thread.
pub struct Ctx<'a> {
    /// The hub's records in the backend's store.
    pub records: &'a Arc<dyn Records>,
    /// The coordinator's status now.
    pub status: &'a CoordStatus,
    /// Network administration (its blocking calls on job threads only).
    pub net: &'a Arc<dyn NetworkAdmin>,
    /// API sessions (token rotation).
    pub api: &'a mut ApiServer,
    /// Devices the engine knows.
    pub known: &'a dyn Fn() -> Vec<Known>,
    /// Application facts for `hub/system`.
    pub app_facts: &'a dyn Fn() -> J,
    /// Delivery of job results.
    pub out: Out,
    /// Wall clock, s.
    pub now_s: f64,
}

/// The hub's time settings and clock state.
struct ClockState {
    tz: clock::Tz,
    tz_text: String,
    servers: Vec<String>,
    source: Option<&'static str>,
    /// Engine ms → Unix ms.
    offset_ms: i64,
    /// When the clock was first known this run, Unix s.
    last_set: Option<i64>,
}

impl Default for ClockState {
    fn default() -> Self {
        ClockState {
            tz: clock::Tz::default(),
            tz_text: clock::DEFAULT_TZ.into(),
            servers: vec![clock::DEFAULT_SERVER.into()],
            source: None,
            offset_ms: 0,
            last_set: None,
        }
    }
}

fn wall_s() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Maintenance state of the app.
pub struct Admin {
    /// The platform (`None`: hub settings unsupported).
    pub platform: Option<Arc<dyn Platform>>,
    /// Hub name and version (`metadata.source` of backups).
    pub source: String,
    /// Stack of the job thread.
    pub job_stack: usize,
    /// Time zone, SNTP servers and the clock's state.
    clock: ClockState,
    /// The long-lived job thread ([`start_worker`](Self::start_worker)).
    worker: Option<jobs::Worker>,
    board: jobs::Shared,
    staged: Option<Staged>,
    /// Developer mode (`hub/developer/set`, images with the advanced
    /// console): shared with the HTTP server, which serves the web UI's
    /// developer tools (the ZCL console module) only while it is on.
    pub developer: Arc<AtomicBool>,
}

fn ncp_error(what: &str, e: CoordError) -> ApiError {
    let (code, why) = match e.meaning() {
        CoordError::Unsupported => ("unsupported", "the NCP firmware does not implement it"),
        CoordError::Backend { kind: Kind::InvalidParameter, .. } => ("invalid_value", "the NCP refused a parameter"),
        CoordError::Backend { kind: Kind::InvalidState, .. } => ("invalid_value", "the network is not in a state for this (not up?)"),
        CoordError::Busy => ("busy", "the NCP is busy"),
        CoordError::Backend { kind: Kind::Denied, .. } => ("unauthorized", "the NCP refused (key export locked?)"),
        CoordError::Backend { kind: Kind::NotFound, .. } => ("not_found", "not found on the NCP"),
        CoordError::Timeout => ("timeout", "no answer from the NCP"),
        CoordError::LinkDown | CoordError::DeliveryUnknown => ("device_unavailable", "NCP link lost"),
        _ => ("failed", ""),
    };
    if why.is_empty() { ApiError::new(code, format!("{what}: {e}")) } else { ApiError::new(code, format!("{what}: {why}")) }
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError::new("invalid_value", msg)
}

pub(crate) fn frame(id: u64, r: Result<J, ApiError>) -> String {
    match r {
        Ok(v) => ServerMessage::ok(id, v),
        Err(e) => ServerMessage::err(id, e),
    }
    .to_text()
}

/// A result frame around JSON text the caller already produced (one copy).
fn raw_frame(id: u64, json_text: &str) -> Result<String, ApiError> {
    let mut w = Jw::result(id, json_text.len() + 64)?;
    w.raw(json_text);
    w.finish()
}

type Job = Box<dyn FnOnce(&Out) -> Result<Reply, ApiError> + Send>;

/// Result of a job.
pub enum Reply {
    /// A small JSON value.
    Json(J),
    /// A complete result frame written with [`Jw`] (large results).
    Frame(String),
}

/// Centre frequency (MHz) of a 2.4 GHz Wi-Fi channel.
fn wifi_mhz(ch: u8) -> i32 {
    if ch == 14 { 2484 } else { 2407 + 5 * i32::from(ch) }
}

/// Centre frequency (MHz) of a Zigbee channel.
fn zigbee_mhz(ch: u8) -> i32 {
    2405 + 5 * (i32::from(ch) - 11)
}

/// Zigbee channel under a 20 MHz Wi-Fi channel (±11 MHz of its centre):
/// Wi-Fi 1 → 11–14, 6 → 16–19, 11 → 21–24.
pub fn wifi_overlap(zigbee: u8, wifi: u8) -> bool {
    (zigbee_mhz(zigbee) - wifi_mhz(wifi)).abs() < 12
}

/// Score per channel (lower is better) and the recommendation: energy,
/// plus a penalty under the hub's Wi-Fi channel and next to it, plus one
/// for 26 (lower power or unsupported on some devices); the Light Link
/// primaries 11/15/20/25 win ties.
pub fn recommend(scan: &[EnergyChannel], wifi: Option<u8>) -> (Option<u8>, Vec<(u8, bool, u32)>) {
    let scored: Vec<(u8, bool, u32)> = scan
        .iter()
        .map(|c| {
            let overlap = wifi.is_some_and(|w| wifi_overlap(c.channel, w));
            let near = wifi.is_some_and(|w| (zigbee_mhz(c.channel) - wifi_mhz(w)).abs() < 17);
            let mut score = u32::from(c.energy) * 4;
            if overlap {
                score += 1000;
            } else if near {
                score += 120;
            }
            if c.channel == 26 {
                score += 160;
            }
            if !matches!(c.channel, 11 | 15 | 20 | 25) {
                score += 30;
            }
            (c.channel, overlap, score)
        })
        .collect();
    let best = scored.iter().min_by_key(|(ch, _, s)| (*s, *ch)).map(|(ch, _, _)| *ch);
    (best, scored)
}

fn be_hex(b: &[u8]) -> String {
    b.iter().rev().map(|x| format!("{x:02x}")).collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

struct ScanReq {
    passes: Option<u8>,
    channels: Option<Vec<u8>>,
    duration_exp: Option<u8>,
    count: Option<u8>,
}

struct ChannelReq {
    channel: u8,
}

struct TxPowerReq {
    dbm: i8,
}

struct DiagReq {
    reset: bool,
    groups: Option<u8>,
}

struct TopologyReq {
    refresh: bool,
    routes: bool,
}

struct BackupReq {
    keys: bool,
}

/// A restore being staged: the network's records and the device batches
/// received so far, with the first request's options.
struct Staged {
    restore: ocb::Restore,
    groups: Option<Vec<J>>,
    allow_ieee_change: bool,
    force: bool,
    skip_backup: bool,
}

struct ResetReq {
    confirm: String,
    channel: Option<u8>,
    channels: Option<Vec<u8>>,
    pan_id: Option<u16>,
    ext_pan_id: Option<String>,
    tx_power: Option<i8>,
    skip_backup: bool,
}

struct WifiReq {
    ssid: String,
    password: String,
}

struct ModeReq {
    mode: String,
}

struct LogsReq {
    lines: Option<usize>,
}

/// Request parameters from the frame's JSON (no serde derives: each derive
/// costs kilobytes of firmware).
fn field<'a>(v: &'a J, k: &str) -> Option<&'a J> {
    v.get(k).filter(|x| !x.is_null())
}

fn num<T: TryFrom<i64>>(v: &J, k: &str) -> Result<Option<T>, ApiError> {
    field(v, k)
        .map(|x| x.as_i64().and_then(|n| T::try_from(n).ok()).ok_or_else(|| bad(format!("{k}: an integer in range"))))
        .transpose()
}

fn flag(v: &J, k: &str, default: bool) -> Result<bool, ApiError> {
    field(v, k).map_or(Ok(default), |x| x.as_bool().ok_or_else(|| bad(format!("{k}: true or false"))))
}

fn text<'a>(v: &'a J, k: &str) -> Result<Option<&'a str>, ApiError> {
    field(v, k).map(|x| x.as_str().ok_or_else(|| bad(format!("{k}: a string")))).transpose()
}

fn need<T>(x: Option<T>, k: &str) -> Result<T, ApiError> {
    x.ok_or_else(|| ApiError::new("invalid_format", format!("missing {k}")))
}

fn channels(v: &J, k: &str) -> Result<Option<Vec<u8>>, ApiError> {
    field(v, k)
        .map(|x| {
            x.as_array()
                .and_then(|a| a.iter().map(|c| c.as_u64().and_then(|c| u8::try_from(c).ok())).collect::<Option<Vec<u8>>>())
                .ok_or_else(|| bad(format!("{k}: a list of channel numbers")))
        })
        .transpose()
}

impl ScanReq {
    fn from(v: &J) -> Result<Self, ApiError> {
        Ok(Self { channels: channels(v, "channels")?, duration_exp: num(v, "duration_exp")?, count: num(v, "count")?, passes: num(v, "passes")? })
    }
}

impl ChannelReq {
    fn from(v: &J) -> Result<Self, ApiError> {
        Ok(Self { channel: need(num(v, "channel")?, "channel")? })
    }
}

impl TxPowerReq {
    fn from(v: &J) -> Result<Self, ApiError> {
        Ok(Self { dbm: need(num(v, "dbm")?, "dbm")? })
    }
}

impl DiagReq {
    fn from(v: &J) -> Result<Self, ApiError> {
        Ok(Self { reset: flag(v, "reset", false)?, groups: num(v, "groups")? })
    }
}

impl TopologyReq {
    fn from(v: &J) -> Result<Self, ApiError> {
        Ok(Self { refresh: flag(v, "refresh", true)?, routes: flag(v, "routes", true)? })
    }
}

impl BackupReq {
    fn from(v: &J) -> Result<Self, ApiError> {
        Ok(Self { keys: flag(v, "keys", true)? })
    }
}

impl ResetReq {
    fn from(v: &J) -> Result<Self, ApiError> {
        Ok(Self {
            confirm: text(v, "confirm")?.unwrap_or_default().to_string(),
            channel: num(v, "channel")?,
            channels: channels(v, "channels")?,
            pan_id: num(v, "pan_id")?,
            ext_pan_id: text(v, "ext_pan_id")?.map(str::to_string),
            tx_power: num(v, "tx_power")?,
            skip_backup: flag(v, "skip_backup", false)?,
        })
    }
}

impl WifiReq {
    fn from(v: &J) -> Result<Self, ApiError> {
        Ok(Self { ssid: need(text(v, "ssid")?, "ssid")?.to_string(), password: text(v, "password")?.unwrap_or_default().to_string() })
    }
}

impl ModeReq {
    fn from(v: &J) -> Result<Self, ApiError> {
        Ok(Self { mode: need(text(v, "mode")?, "mode")?.to_string() })
    }
}

impl LogsReq {
    fn from(v: &J) -> Result<Self, ApiError> {
        Ok(Self { lines: num(v, "lines")? })
    }
}

fn mask_of(channels: &[u8]) -> Result<u32, ApiError> {
    channels.iter().try_fold(0u32, |m, c| if (11..=26).contains(c) { Ok(m | 1 << c) } else { Err(bad(format!("channel {c} outside 11–26"))) })
}

/// A backup: the result frame of request `id` holding the OCB file, and
/// where the file is inside it.
struct Backup {
    frame: String,
    file: std::ops::Range<usize>,
}

impl Backup {
    fn file(&self) -> &str {
        &self.frame[self.file.clone()]
    }
}

/// Export (keys optional) and write the OCB file straight into a result
/// frame (one buffer, reserved fallibly); complete backups are also kept in
/// the host store.
fn take_backup(net: &dyn NetworkAdmin, kv: &dyn KvStore, keys: bool, known: &[Known], source: &str, now_s: f64, id: u64) -> Result<Backup, ApiError> {
    let mut export = ocb::Export::default();
    net.backup_export(keys, &mut export).map_err(|e| ncp_error("backup export", e))?;
    let ncp = net.status().and_then(|s| s.ncp);
    let info = ocb::ExportInfo {
        source: source.to_string(),
        creation_time: ocb::rfc3339(now_s as u64),
        ncp_fw: ncp.as_ref().map(|n| n.fw.clone()).unwrap_or_default(),
        ncp_chip: ncp.as_ref().map_or(0, |n| n.chip),
        ncp_build_id: ncp.as_ref().map(|n| hex(&n.build_id)).unwrap_or_default(),
        host_devices: known.iter().map(|k| (k.ieee.to_le_bytes(), k.nwk)).collect(),
        // The hub's groups as stored (device-support 14.9).
        groups: kv::with(kv, |s| {
            let v: Vec<&str> = s
                .prefix(hub_devices::hub::zgroups::GROUP_KEY)
                .filter_map(|(_, v)| std::str::from_utf8(v).ok())
                .collect();
            format!("[{}]", v.join(","))
        }),
    };
    let mut frame = Jw::result(id, 0)?.finish()?;
    frame.pop();
    let start = frame.len();
    let summary = ocb::from_export(export, &info, &mut frame).map_err(|e| match e {
        ocb::ExportError::NoMemory => oom(),
        ocb::ExportError::Invalid(e) => ApiError::new("failed", e),
    })?;
    let end = frame.len();
    frame.try_reserve(1).map_err(|_| oom())?;
    frame.push('}');
    let b = Backup { frame, file: start..end };
    if summary.complete {
        // A kept backup of another network (registry foreign) stays.
        kv::with(kv, |s| if s.gate() == hub_store::Gate::Open { s.put(LAST_BACKUP_KEY, b.file().as_bytes()) } else { Ok(()) })
            .map_err(|e| ApiError::new("failed", format!("backup not stored: {e}")))?;
    }
    info!("admin: backup taken ({} devices, {} link keys, complete {})", summary.devices, summary.link_keys, summary.complete);
    Ok(b)
}

impl Admin {
    /// No platform, default job stack.
    pub fn new(platform: Option<Arc<dyn Platform>>, source: String) -> Self {
        Self {
            platform,
            source,
            job_stack: 16 * 1024,
            board: jobs::Shared::default(),
            staged: None,
            worker: None,
            clock: ClockState::default(),
            developer: Arc::default(),
        }
    }

    /// Start the long-lived job thread (with `job_stack`); call once,
    /// early, while the heap is in one piece.
    pub fn start_worker(&mut self) {
        if self.worker.is_none() {
            self.worker = jobs::Worker::start(self.job_stack);
            if self.worker.is_none() {
                warn!("admin: no job worker thread, jobs get threads of their own");
            }
        }
    }

    /// `hub/info` `maintenance`: what this hub and its NCP (`caps`)
    /// support.
    pub fn features(&self, caps: Option<&Capabilities>) -> Vec<&'static str> {
        let mut f = vec!["diagnostics", "topology", "change_channel", "reset"];
        if let Some(c) = caps {
            for (has, name) in [
                (c.radio.energy_scan, "energy_scan"),
                (c.radio.tx_power, "tx_power"),
                (c.backup.export, "backup"),
                (c.backup.restore, "restore"),
            ] {
                if has {
                    f.push(name);
                }
            }
        }
        f.extend(self.platform.as_ref().map(|p| p.features()).unwrap_or_default());
        f
    }

    /// Handle one request; returns frames to send now (a job's frame
    /// follows through [`Ctx::out`]).
    pub fn handle(&mut self, ctx: Ctx<'_>, sid: SessionId, head: &Head) -> Vec<String> {
        let id = head.id;
        let v = &head.v;
        let r = match head.ty.as_str() {
            "hub/system" => Ok(self.system(&ctx)),
            "hub/logs" => self.logs(v),
            "hub/time" => return vec![self.time_frame(id)],
            "hub/time/set" => match self.set_time(&**ctx.records, v) {
                Ok(()) => return vec![self.time_frame(id)],
                Err(e) => Err(e),
            },
            "hub/developer/set" => self.set_developer(&**ctx.records, ctx.api, &ctx.out, v),
            "hub/wifi/set" => self.wifi(v),
            "hub/token/rotate" => self.rotate_token(ctx.api),
            "hub/reboot" => self.platform().and_then(|p| p.reboot().map_err(|e| ApiError::new("failed", e))).map(|_| json!({"rebooting": true})),
            "hub/mode/set" => self.mode(v),
            "network/backup/last" => {
                let r = kv::with(&**ctx.records, |s| {
                    let last = s.get(LAST_BACKUP_KEY).and_then(|b| std::str::from_utf8(b).ok()).unwrap_or("null");
                    let mut w = Jw::result(id, last.len() + 80)?;
                    w.obj().key("backup").raw(last).end_obj();
                    w.finish()
                });
                return vec![r.unwrap_or_else(|e| frame(id, Err(e)))];
            }
            "network/topology" => match TopologyReq::from(v) {
                Ok(q) if !q.refresh => {
                    let b = self.board.lock().unwrap();
                    let r = raw_frame(id, b.result(None, Some("topology")).map_or("null", |(_, _, t)| t));
                    return vec![r.unwrap_or_else(|e| frame(id, Err(e)))];
                }
                Ok(q) => return self.job_topology(ctx, sid, id, q),
                Err(e) => Err(e),
            },
            "network/jobs" => {
                let r = self.board.lock().unwrap().list_frame(id);
                return vec![r.unwrap_or_else(|e| frame(id, Err(e)))];
            }
            "network/job/result" => {
                let job = field(v, "job").and_then(J::as_u64);
                let r = self.board.lock().unwrap().result_reply(id, job, field(v, "kind").and_then(J::as_str));
                return vec![r.unwrap_or_else(|e| frame(id, Err(e)))];
            }
            "network/job/cancel" => match field(v, "job").and_then(J::as_u64) {
                Some(job) if self.board.lock().unwrap().cancel(job) => {
                    return vec![format!("{{\"id\":{id},\"type\":\"result\",\"success\":true,\"result\":{{\"cancelling\":true}}}}")];
                }
                Some(_) => Err(ApiError::new("not_found", "no such running job")),
                None => Err(bad("job: the job ID")),
            },
            "network/restore" => return self.restore(ctx, sid, id, v),
            _ => return self.ncp_job(ctx, sid, head, v),
        };
        vec![frame(id, r)]
    }

    fn platform(&self) -> Result<&Arc<dyn Platform>, ApiError> {
        self.platform.as_ref().ok_or_else(|| ApiError::new("unsupported", "no platform settings here"))
    }

    fn system(&self, ctx: &Ctx<'_>) -> J {
        let mut v = self.platform.as_ref().map(|p| p.system()).unwrap_or_else(|| json!({}));
        if let Some(o) = v.as_object_mut() {
            o.insert("app".into(), (ctx.app_facts)());
            let (store, ncp) = ctx.records.system(ctx.status);
            o.insert("store".into(), store);
            o.insert("ncp".into(), ncp);
            o.insert("time".into(), ctx.now_s.into());
            o.insert("time_synced".into(), hub_devices::device::time_synced().into());
        }
        v
    }

    fn logs(&self, v: &J) -> Result<J, ApiError> {
        let q = LogsReq::from(v)?;
        let p = self.platform()?;
        Ok(json!({"lines": p.logs(q.lines.unwrap_or(100).min(500))}))
    }

    /// Time settings from the host store; hands them to the platform.
    #[inline(never)]
    pub fn load_clock(&mut self, kv: &dyn KvStore) {
        let (tz, ntp) = kv::with(kv, |s| {
            let get = |k| s.get(k).and_then(|b| std::str::from_utf8(b).ok()).map(str::to_string);
            (get(TZ_KEY), get(NTP_KEY))
        });
        if let Some(t) = tz.as_deref().and_then(clock::parse) {
            self.clock.tz = t;
            self.clock.tz_text = tz.unwrap_or_default();
        }
        if let Some(n) = ntp {
            self.clock.servers = n.split(',').map(str::to_string).collect();
        }
        if let Some(p) = &self.platform
            && let Err(e) = p.set_time(&self.clock.servers)
        {
            warn!("time: {e}");
        }
    }

    /// Once a second: the wall-clock offset, the engine's time zone and its
    /// Time cluster clock.
    #[inline(never)]
    pub fn clock_tick(&mut self, engine: &mut hub_devices::hub::HubConfig, now: hub_devices::device::Millis) {
        let source = match &self.platform {
            Some(p) => p.time_source(),
            None => (wall_s() >= clock::VALID_AFTER_UNIX).then_some("system"),
        };
        let wall_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
        let c = &mut self.clock;
        c.source = source;
        if source.is_some() {
            let off = wall_ms - now as i64;
            // Small corrections are SNTP slewing; the offset only follows jumps.
            if (off - c.offset_ms).abs() > 1000 {
                c.offset_ms = off;
                hub_devices::device::set_wall_offset(off);
            }
            c.last_set.get_or_insert(wall_ms / 1000);
        }
        hub_devices::device::set_time_synced(source.is_some());
        let t = wall_ms / 1000;
        let off = c.tz.offset_at(t).0;
        if i64::from(off) != engine.tz_offset_s
            && let Some(p) = &self.platform
        {
            p.set_utc_offset(off);
        }
        engine.tz_offset_s = i64::from(off);
        engine.clock = clock::zcl_clock(&c.tz, t, source.is_some(), c.last_set);
    }

    /// The `hub/time` result frame.
    fn time_frame(&self, id: u64) -> String {
        let c = &self.clock;
        let t = wall_s();
        let (off, in_dst) = c.tz.offset_at(t);
        let synced = c.source.is_some();
        let w = Jw::result(id, 400).map(|mut w| {
            let opt = |w: &mut Jw, k: &str, v: Option<i64>| {
                w.key(k);
                match v {
                    Some(v) => w.num(v),
                    None => w.null(),
                };
            };
            w.obj();
            opt(&mut w, "time_utc", synced.then_some(t));
            w.key("time_synced").num(synced).key("source");
            match c.source {
                Some(s) => w.str(s),
                None => w.null(),
            };
            w.key("local");
            if synced { w.str(&clock::civil(t, off)) } else { w.null() };
            w.key("tz").str(&c.tz_text).key("tz_offset_s").num(off).key("dst");
            match c.tz.dst_utc(clock::civil_from_days(t.div_euclid(86_400)).0) {
                Some((s, e, shift)) => w.obj().key("start").num(s).key("end").num(e).key("shift_s").num(shift).key("active").num(in_dst).end_obj(),
                None => w.null(),
            };
            w.key("servers").arr();
            for s in &c.servers {
                w.str(s);
            }
            w.end_arr();
            opt(&mut w, "last_set", c.last_set);
            w.end_obj();
            w
        });
        w.and_then(Jw::finish).unwrap_or_else(|e| frame(id, Err(e)))
    }

    /// The kept developer mode (`hub/info` `developer_mode`, with the
    /// advanced console only) and the image's `edition`.
    pub fn load_developer(&mut self, kv: &dyn KvStore, api: &mut ApiServer) {
        api.set_info("edition", if cfg!(feature = "dev") { "devtools" } else { "standard" }.into());
        if cfg!(feature = "advanced") {
            let on = kv::with(kv, |s| s.get(DEVELOPER_KEY).is_some());
            self.developer.store(on, Ordering::Relaxed);
            api.set_info("developer_mode", on.into());
        }
    }

    fn set_developer(&mut self, kv: &dyn KvStore, api: &mut ApiServer, out: &Out, v: &J) -> Result<J, ApiError> {
        if !cfg!(feature = "advanced") {
            return Err(ApiError::new("unsupported", "this image has no developer tools"));
        }
        let on = field(v, "enabled").and_then(J::as_bool).ok_or_else(|| bad("enabled: true or false"))?;
        kv::with(kv, |s| if on { s.put(DEVELOPER_KEY, b"1") } else { s.delete(DEVELOPER_KEY) })
            .map_err(|e| ApiError::new("failed", e.to_string()))?;
        self.developer.store(on, Ordering::Relaxed);
        api.set_info("developer_mode", on.into());
        // Other pages follow without a reconnect (topic `network`).
        out(JobOut::Event(json!({"topic": "hub_updated", "developer_mode": on})));
        info!("admin: developer mode {}", if on { "on" } else { "off" });
        Ok(json!({"developer_mode": on}))
    }

    #[inline(never)]
    fn set_time(&mut self, kv: &dyn KvStore, v: &J) -> Result<(), ApiError> {
        if let Some(tz) = text(v, "tz")? {
            let t = clock::parse(tz).ok_or_else(|| bad("tz: a POSIX TZ string such as EET-2EEST,M3.5.0/3,M10.5.0/4"))?;
            kv::with(kv, |s| s.put(TZ_KEY, tz.trim().as_bytes())).map_err(|e| ApiError::new("failed", e.to_string()))?;
            self.clock.tz = t;
            self.clock.tz_text = tz.trim().to_string();
        }
        if let Some(list) = field(v, "servers") {
            let servers: Vec<String> = list
                .as_array()
                .and_then(|a| a.iter().map(|s| s.as_str().map(|s| s.trim().to_string())).collect::<Option<Vec<_>>>())
                .filter(|l| (1..=3).contains(&l.len()) && l.iter().all(|s| (1..=64).contains(&s.len()) && !s.contains(',')))
                .ok_or_else(|| bad("servers: 1–3 host names"))?;
            kv::with(kv, |s| s.put(NTP_KEY, servers.join(",").as_bytes())).map_err(|e| ApiError::new("failed", e.to_string()))?;
            self.clock.servers = servers;
        }
        if let Some(p) = &self.platform {
            p.set_time(&self.clock.servers).map_err(|e| ApiError::new("failed", e))?;
        }
        info!("time: zone {}, servers {}", self.clock.tz_text, self.clock.servers.join(","));
        Ok(())
    }

    fn wifi(&self, v: &J) -> Result<J, ApiError> {
        let q = WifiReq::from(v)?;
        if q.ssid.is_empty() || q.ssid.len() > 32 {
            return Err(bad("ssid: 1–32 bytes"));
        }
        if !q.password.is_empty() && !(8..=63).contains(&q.password.len()) {
            return Err(bad("password: empty (open network) or 8–63 characters"));
        }
        self.platform()?.set_wifi(&q.ssid, &q.password).map_err(|e| ApiError::new("failed", e))?;
        info!("admin: Wi-Fi credentials changed (SSID {:?})", q.ssid);
        Ok(json!({"stored": true, "reconnecting": true}))
    }

    fn rotate_token(&self, api: &mut ApiServer) -> Result<J, ApiError> {
        let p = self.platform()?;
        let mut b = [0u8; 12];
        p.random(&mut b);
        let token = hex(&b);
        p.save_token(&token).map_err(|e| ApiError::new("failed", e))?;
        api.set_token(token.clone());
        info!("admin: API token rotated");
        Ok(json!({"token": token}))
    }

    fn mode(&self, v: &J) -> Result<J, ApiError> {
        let q = ModeReq::from(v)?;
        if q.mode != "hub" && q.mode != "bridge" {
            return Err(bad("mode: hub or bridge"));
        }
        self.platform()?.set_mode(&q.mode).map_err(|e| ApiError::new("failed", e))?;
        Ok(json!({"mode": q.mode, "rebooting": true}))
    }

    /// Run `f` on the job thread; `busy` while another job runs.
    fn spawn(
        &mut self,
        name: &'static str,
        out: Out,
        sid: SessionId,
        id: u64,
        f: impl FnOnce(&Out) -> Result<Reply, ApiError> + Send + 'static,
    ) -> Vec<String> {
        // One thread entry for every job (no per-closure copy of it).
        self.spawn_job(name, out, sid, id, Box::new(f))
    }

    fn spawn_job(&mut self, name: &'static str, out: Out, sid: SessionId, id: u64, f: Job) -> Vec<String> {
        let body: jobs::BgBody = Box::new(move |bg| {
            f(bg.out()).map(|r| match r {
                Reply::Json(v) => frame(id, Ok(v)),
                Reply::Frame(t) => t,
            })
        });
        self.launch(name, false, 1, Duration::from_secs(1), out, sid, id, body)
    }

    /// A background job on the heavy lane: answered at once with its ID;
    /// progress and the end come as `network` events, the result as a
    /// `job_result` frame to this session (and `network/job/result`).
    #[allow(clippy::too_many_arguments)]
    fn start_bg(&mut self, kind: &'static str, passes: u32, estimate: Duration, out: Out, sid: SessionId, id: u64, body: jobs::BgBody) -> Vec<String> {
        self.launch(kind, true, passes, estimate, out, sid, id, body)
    }

    /// Register and start a job: a request/response one (its body returns
    /// the whole result frame) or a background one.
    #[allow(clippy::too_many_arguments)]
    fn launch(&mut self, kind: &'static str, background: bool, passes: u32, estimate: Duration, out: Out, sid: SessionId, id: u64, body: jobs::BgBody) -> Vec<String> {
        // Reads and settings do not wait for a scan.
        let lane = if !background && matches!(kind, "diagnostics" | "TX power") { Lane::Light } else { Lane::Heavy };
        let (job, cancel) = match self.board.lock().unwrap().start(lane, kind, background, passes, estimate) {
            Ok(j) => j,
            Err(e) => return vec![frame(id, Err(e))],
        };
        // A job thread needs its stack in one block plus room for its
        // result and the frames in flight: without it, refuse instead of
        // running the heap dry (the S3 has no PSRAM). The worker's stack
        // exists already; only a second job needs its own.
        let own_stack = self.worker.as_ref().is_none_or(jobs::Worker::is_busy);
        let need = if own_stack { self.job_stack } else { 0 };
        if let Some((free, largest)) = self.platform.as_ref().and_then(|p| p.heap())
            && (free < need + JOB_HEAP_RESERVE || largest < need)
        {
            self.board.lock().unwrap().finish(job);
            let e = ApiError::new("busy", format!("low memory ({} KB free): try again when the running job ended", free / 1024));
            return vec![frame(id, Err(e))];
        }
        let exec = jobs::Exec { worker: self.worker.as_ref(), stack: self.job_stack, platform: self.platform.clone() };
        match jobs::run(self.board.clone(), job, kind, cancel, out, sid, exec, (!background).then_some(id), body) {
            Ok(()) if background => {
                vec![format!("{{\"id\":{id},\"type\":\"result\",\"success\":true,\"result\":{{\"job\":{job},\"kind\":\"{kind}\",\"passes\":{passes}}}}}")]
            }
            Ok(()) => Vec::new(),
            Err(e) => vec![frame(id, Err(e))],
        }
    }

    /// Progress events of the running background jobs (the app sends them
    /// once a second).
    #[inline(never)]
    pub fn progress_events(&self) -> Vec<J> {
        self.board.lock().unwrap().progress_events()
    }

    /// Maintenance jobs running (on their threads).
    pub fn jobs_running(&self) -> usize {
        self.board.lock().unwrap().running()
    }

    /// Scan progress from the coordinator: `done` of `total` channels.
    #[inline(never)]
    pub fn scan_progress(&self, done: u8, total: u8) {
        self.board.lock().unwrap().scan_progress(done, total);
    }

    fn ncp_job(&mut self, ctx: Ctx<'_>, sid: SessionId, head: &Head, v: &J) -> Vec<String> {
        let id = head.id;
        let records = ctx.records.clone();
        let net = ctx.net.clone();
        let out = ctx.out.clone();
        let fail = |e: ApiError| vec![frame(id, Err(e))];
        match head.ty.as_str() {
            "network/energy_scan" => {
                let (mask, duration, passes) = match scan_params(v, 8) {
                    Ok(p) => p,
                    Err(e) => return fail(e),
                };
                let wifi = self.platform.as_ref().and_then(|p| p.wifi_channel());
                let est = scan_estimate(mask, duration);
                self.start_bg(
                    "energy_scan",
                    passes,
                    est,
                    out,
                    sid,
                    id,
                    Box::new(move |bg| {
                        let t = std::time::Instant::now();
                        // Per channel 11..=26: (sum, max) of the energy, max RSSI.
                        let mut agg = [(0u32, 0u8, i8::MIN); 16];
                        let mut done = 0;
                        let mut text = String::new();
                        while done < passes && !bg.cancelled() {
                            let p = std::time::Instant::now();
                            let scan = net.energy_scan(mask, duration).map_err(|e| ncp_error("energy scan", e))?;
                            for c in scan.iter().filter(|c| (11..=26).contains(&c.channel)) {
                                let a = &mut agg[usize::from(c.channel - 11)];
                                *a = (a.0 + u32::from(c.energy), a.1.max(c.energy), a.2.max(c.max_rssi_dbm));
                            }
                            done += 1;
                            bg.pass_done(done, p.elapsed());
                            text = energy_result(&agg, mask, done, passes, bg.cancelled(), wifi, current_channel(&*net), t)?;
                            if done < passes && !bg.cancelled() {
                                bg.partial(&text);
                            }
                        }
                        Ok(text)
                    }),
                )
            }
            "network/active_scan" => {
                let (mask, duration, _) = match scan_params(v, 6) {
                    Ok(p) => p,
                    Err(e) => return fail(e),
                };
                self.start_bg(
                    "active_scan",
                    1,
                    scan_estimate(mask, duration),
                    out,
                    sid,
                    id,
                    Box::new(move |_| {
                        let t = std::time::Instant::now();
                        let found = net.active_scan(mask, duration).map_err(|e| ncp_error("active scan", e))?;
                        let own = net.status().and_then(|s| s.network).map(|n| (n.pan_id, n.ext_pan_id));
                        // Every router of a network beacons: one entry per
                        // network and channel, its best beacon, and the count.
                        let mut nets: Vec<(&Beacon, u32)> = Vec::new();
                        for b in &found {
                            match nets.iter_mut().find(|(n, _)| (n.channel, n.pan_id, n.ext_pan_id) == (b.channel, b.pan_id, b.ext_pan_id)) {
                                Some((n, c)) => {
                                    *c += 1;
                                    if b.lqi > n.lqi {
                                        *n = b;
                                    }
                                }
                                None => nets.push((b, 1)),
                            }
                        }
                        let mut w = Jw::value(100 + 200 * nets.len())?;
                        w.obj().key("networks").arr();
                        for (b, n) in nets {
                            w.obj().key("channel").num(b.channel).key("pan_id").num(b.pan_id).key("ext_pan_id").str(&be_hex(&b.ext_pan_id));
                            w.key("src").num(b.src).key("lqi").num(b.lqi).key("rssi").num(b.rssi).key("permit_join").num(b.permit_join);
                            w.key("stack_profile").num(b.stack_profile).key("update_id").num(b.update_id);
                            w.key("beacons").num(n).key("own").num(own == Some((b.pan_id, b.ext_pan_id))).end_obj();
                        }
                        w.end_arr().key("duration_ms").num(t.elapsed().as_millis() as u64).end_obj();
                        w.finish_value()
                    }),
                )
            }
            "network/change_channel" => {
                let q = match ChannelReq::from(v) {
                    Ok(q) => q,
                    Err(e) => return fail(e),
                };
                if !(11..=26).contains(&q.channel) {
                    return fail(bad("channel: 11–26"));
                }
                self.spawn("channel change", out, sid, id, move |out| {
                    let r = net.change_channel(q.channel).map_err(|e| ncp_error("channel change", e))?;
                    out(JobOut::Event(
                        json!({"topic": "network_updated", "reason": "channel_changed", "channel": r.channel, "nwk_update_id": r.nwk_update_id}),
                    ));
                    Ok(Reply::Json(json!({"channel": r.channel, "nwk_update_id": r.nwk_update_id})))
                })
            }
            "network/set_tx_power" => {
                let q = match TxPowerReq::from(v) {
                    Ok(q) => q,
                    Err(e) => return fail(e),
                };
                self.spawn("TX power", out, sid, id, move |out| {
                    let applied = net.set_tx_power(q.dbm).map_err(|e| ncp_error("TX power", e))?;
                    records.tx_power_set(q.dbm);
                    out(JobOut::Event(json!({"topic": "network_updated", "reason": "tx_power", "tx_power": applied})));
                    Ok(Reply::Json(json!({"requested_dbm": q.dbm, "applied_dbm": applied})))
                })
            }
            "network/diagnostics" => {
                let q = match DiagReq::from(v) {
                    Ok(q) => q,
                    Err(e) => return fail(e),
                };
                let groups = q.groups.unwrap_or(0xFF);
                self.spawn("diagnostics", out, sid, id, move |_| diagnostics(&net.diagnostics(groups, q.reset), id).map(Reply::Frame))
            }
            "network/backup" => {
                let q = match BackupReq::from(v) {
                    Ok(q) => q,
                    Err(e) => return fail(e),
                };
                if q.keys && ctx.api.token().is_empty() {
                    return fail(ApiError::new("unauthorized", "backups with keys need an API token"));
                }
                if !ctx.status.caps.as_ref().is_some_and(|c| c.backup.export) {
                    return fail(ApiError::new("unsupported", "the NCP has no BACKUP_RESTORE"));
                }
                let known = (ctx.known)();
                let source = self.source.clone();
                let now_s = ctx.now_s;
                self.spawn("backup", out, sid, id, move |_| {
                    take_backup(&*net, &*records, q.keys, &known, &source, now_s, id).map(|b| Reply::Frame(b.frame))
                })
            }
            "network/reset" => self.reset(ctx, sid, id, v, false),
            "network/form" => self.reset(ctx, sid, id, v, true),
            other => fail(ApiError::new("unsupported", other.to_string())),
        }
    }

    fn job_topology(&mut self, ctx: Ctx<'_>, sid: SessionId, id: u64, q: TopologyReq) -> Vec<String> {
        let net = ctx.net.clone();
        let known = (ctx.known)();
        let now_s = ctx.now_s;
        self.start_bg(
            "topology",
            1,
            Duration::from_secs(10),
            ctx.out.clone(),
            sid,
            id,
            Box::new(move |bg| {
                let opts = ScanOpts { routes: q.routes, cancel: Some(bg.cancel_flag()), ..ScanOpts::default() };
                let mut progress = |scanned: usize, queued: usize| bg.fraction((scanned * 1000 / (scanned + queued).max(1)) as u32);
                topology::scan(&*net, &known, opts, now_s, &mut progress)
            }),
        )
    }

    /// `network/restore`: `backup` (OCB, its `devices` optional), then any
    /// number of `devices` batches while `more` is true; the last request
    /// starts the restore. Each request is one small JSON value: a large
    /// backup never has to be in RAM at once.
    fn restore(&mut self, ctx: Ctx<'_>, sid: SessionId, id: u64, v: &J) -> Vec<String> {
        let fail = |e: ApiError| vec![frame(id, Err(e))];
        let more = match flag(v, "more", false) {
            Ok(m) => m,
            Err(e) => return fail(e),
        };
        if let Some(b) = field(v, "backup") {
            let opts = (flag(v, "allow_ieee_change", true), flag(v, "force", false), flag(v, "skip_backup", false));
            let (Ok(allow_ieee_change), Ok(force), Ok(skip_backup)) = opts else {
                return fail(bad("allow_ieee_change, force, skip_backup: true or false"));
            };
            self.staged = None;
            let groups = b.pointer("/metadata/internal/zigbee_rs/groups").and_then(J::as_array).cloned();
            let encoder = match ctx.net.restore_encoder() {
                Ok(e) => e,
                Err(e) => return fail(ncp_error("restore", e)),
            };
            match ocb::to_restore(b, encoder) {
                Ok(restore) => self.staged = Some(Staged { restore, groups, allow_ieee_change, force, skip_backup }),
                Err(e) => return fail(bad(e)),
            }
        } else if let Some(devices) = field(v, "devices") {
            let Some(st) = self.staged.as_mut() else {
                return fail(bad("no restore staged: send the backup first"));
            };
            if let Err(e) = ocb::add_devices(&mut st.restore, devices) {
                self.staged = None;
                return fail(bad(e));
            }
        } else {
            return fail(bad("backup (OCB object) or devices (a batch after the backup)"));
        }
        let staged = self.staged.as_ref().map_or(0, |s| s.restore.devices.len());
        if staged > MAX_RESTORE_DEVICES {
            self.staged = None;
            return fail(bad(format!("more than {MAX_RESTORE_DEVICES} devices")));
        }
        if more {
            return vec![frame(id, Ok(json!({"staged_devices": staged})))];
        }
        let Some(Staged { restore, groups, allow_ieee_change, force, skip_backup }) = self.staged.take() else {
            return fail(bad("no restore staged"));
        };
        let records = ctx.records.clone();
        let net = ctx.net.clone();
        if !ctx.status.caps.as_ref().is_some_and(|c| c.backup.restore) {
            return fail(ApiError::new("unsupported", "the NCP has no BACKUP_RESTORE"));
        }
        let st = ctx.status;
        if st.network_state != NetState::None && !force {
            let pan = st.network.map_or(String::from("?"), |n| format!("0x{:04X}", n.pan_id));
            return fail(bad(format!(
                "the NCP runs a network (PAN {pan}); set force to replace it (it is backed up first unless skip_backup)"
            )));
        }
        let known = (ctx.known)();
        let source = self.source.clone();
        let now_s = ctx.now_s;
        let opts =
            RestoreOptions { allow_ieee_change, nwk_margin: 10_000, aps_margin: if restore.tclk_counter_known { 10_000 } else { ocb::UNKNOWN_APS_MARGIN } };
        self.spawn("restore", ctx.out.clone(), sid, id, move |out| {
            let mut previous = false;
            if network_state(&*net) == NetState::Up && !skip_backup {
                drop(take_backup(&*net, &*records, true, &known, &source, now_s, id).map_err(|e| {
                    ApiError::new(&e.code, format!("automatic backup before the restore failed ({}); set skip_backup to restore anyway", e.message))
                })?);
                previous = true;
            }
            let report = replace(&*net, &*records, || net.backup_restore(restore.encoder.finish(), opts).map_err(|e| ncp_error("restore", e)))?;
            out(JobOut::Reimport);
            if let Some(g) = groups {
                out(JobOut::Groups(g));
            }
            out(JobOut::Event(json!({"topic": "network_updated", "reason": "restored"})));
            let note = |n: u8| ["", "host_backed", "child_demoted", "ignored", "floor_raised"].get(usize::from(n)).copied().unwrap_or("unknown");
            let mut w = Jw::result(id, 400 + 40 * report.notes.len())?;
            w.obj().key("pan_id").num(restore.pan_id).key("channel").num(restore.channel);
            w.key("coordinator_ieee").str(&be_hex(&restore.coordinator_ieee)).key("devices").num(restore.devices.len());
            w.key("nwk_fc_start").num(report.nwk_fc_start).key("aps_fc_start").num(report.aps_fc_start);
            w.key("link_keys_on_ncp").num(report.link_keys_on_ncp).key("link_keys_host_backed").num(report.link_keys_host_backed);
            w.key("children").num(report.children).key("notes").arr();
            for (i, n) in &report.notes {
                w.obj().key("index").num(i).key("note").str(note(*n)).end_obj();
            }
            w.end_arr().key("previous_network_backed_up").num(previous).end_obj();
            Ok(Reply::Frame(w.finish()?))
        })
    }

    /// `network/reset` (leave the network, backed up first unless
    /// `skip_backup`, then form a new one) and `network/form` (`form_only`:
    /// only where the backend reports no network; nothing to confirm).
    fn reset(&mut self, ctx: Ctx<'_>, sid: SessionId, id: u64, v: &J, form_only: bool) -> Vec<String> {
        let fail = |e: ApiError| vec![frame(id, Err(e))];
        let q = match ResetReq::from(v) {
            Ok(q) => q,
            Err(e) => return fail(e),
        };
        if form_only && ctx.status.ownership != NetworkOwnership::NoNetwork {
            return fail(bad(format!(
                "the NCP reports {} network ownership, not no_network: network/reset replaces a network",
                ctx.status.ownership.as_str()
            )));
        }
        // Forming takes the records: only the hub's own of no formed
        // network (or none yet). Foreign or unverified ones, or those of a
        // network the NCP lost, need adopt/clear (with confirm), or a
        // network/reset ("RESET").
        if form_only && !ctx.records.may_form() {
            let why = match ctx.records.registry() {
                RegistryState::Owned => "the device registry holds a network the NCP no longer runs".to_string(),
                s => format!("the device registry is {}", s.as_str()),
            };
            return fail(ApiError::new(
                "registry_read_only",
                format!("{why}: network/registry/adopt or clear it, or replace the network with network/reset"),
            ));
        }
        if !form_only && q.confirm != "RESET" {
            return fail(bad("confirm must be \"RESET\""));
        }
        let channels = match (q.channel, &q.channels) {
            (Some(c), _) => vec![c],
            (None, Some(list)) => list.clone(),
            (None, None) => Vec::new(),
        };
        let mask = match mask_of(&channels) {
            Ok(0) => FormParams::default().channel_mask,
            Ok(m) => m,
            Err(e) => return fail(e),
        };
        if q.pan_id.is_some_and(|p| p == 0 || p >= 0xFFF7) {
            return fail(bad("pan_id: 0x0001–0xFFF6"));
        }
        let epid = match q.ext_pan_id.as_deref().map(|h| {
            let digits: String = h.chars().filter(|c| c.is_ascii_hexdigit()).collect();
            let mut b = [0u8; 8];
            if digits.len() != 16 {
                return Err(bad("ext_pan_id: 16 hex digits (big-endian)"));
            }
            for i in 0..8 {
                b[7 - i] = u8::from_str_radix(&digits[2 * i..2 * i + 2], 16).map_err(|_| bad("ext_pan_id"))?;
            }
            if b == [0; 8] || b == [0xFF; 8] { Err(bad("ext_pan_id: not all zero or all ones")) } else { Ok(b) }
        }) {
            Some(Err(e)) => return fail(e),
            Some(Ok(b)) => Some(b),
            None => None,
        };
        let records = ctx.records.clone();
        let net = ctx.net.clone();
        let known = (ctx.known)();
        let source = self.source.clone();
        let now_s = ctx.now_s;
        let skip_backup = q.skip_backup;
        let tx_power = q.tx_power;
        self.spawn(if form_only { "form" } else { "network reset" }, ctx.out.clone(), sid, id, move |out| {
            let mut backup = None;
            let state = if form_only { NetState::None } else { network_state(&*net) };
            if state == NetState::Up && !skip_backup {
                let b = take_backup(&*net, &*records, true, &known, &source, now_s, id).map_err(|e| {
                    ApiError::new(&e.code, format!("automatic backup failed ({}); nothing was changed; set skip_backup to reset anyway", e.message))
                })?;
                backup = Some(b);
            }
            let formed = replace(&*net, &*records, || {
                if state != NetState::None {
                    net.leave_reset().map_err(|e| ncp_error("leave/reset", e))?;
                }
                let req = FormParams {
                    channel_mask: mask,
                    pan_id: q.pan_id,
                    ext_pan_id: records.forming(epid),
                    network_key: None,
                    tx_power: tx_power.or(net.tx_power_setting()).unwrap_or(0),
                };
                net.form(req).map_err(|e| ncp_error("form", e))
            })?;
            if let Some(dbm) = tx_power {
                match net.set_tx_power(dbm) {
                    Ok(_) => records.tx_power_set(dbm),
                    Err(e) => warn!("admin: TX power after forming: {e}"),
                }
            }
            out(JobOut::Reimport);
            out(JobOut::Event(json!({"topic": "network_updated", "reason": "formed"})));
            let file = backup.as_ref().map_or("null", |b| b.file());
            let mut w = Jw::result(id, file.len() + 160)?;
            w.obj().key("network").obj().key("pan_id").num(formed.pan_id).key("ext_pan_id").str(&be_hex(&formed.ext_pan_id));
            w.key("channel").num(formed.channel).end_obj().key("backup").raw(file).end_obj();
            drop(backup);
            Ok(Reply::Frame(w.finish()?))
        })
    }
}

/// The NCP's network state as the backend reports it now.
fn network_state(net: &dyn NetworkAdmin) -> NetState {
    net.status().map_or(NetState::None, |s| s.network_state)
}

/// The channel the network runs on now.
fn current_channel(net: &dyn NetworkAdmin) -> Option<u8> {
    net.status().and_then(|s| s.network).map(|n| n.channel)
}

/// `f` replaces the NCP's network: meanwhile the records decide nothing
/// on what the NCP reports, then the new network becomes theirs.
fn replace<T>(net: &dyn NetworkAdmin, records: &dyn Records, f: impl FnOnce() -> Result<T, ApiError>) -> Result<T, ApiError> {
    records.replacing();
    let r = f();
    let now = r.as_ref().ok().and_then(|_| net.status()).and_then(|s| Some((s.ownership, s.epoch?)));
    records.replaced(now.as_ref().map(|(o, e)| (o, *e)));
    r
}

/// Heap a job needs besides its stack (its result, frames in flight).
const JOB_HEAP_RESERVE: usize = 16 * 1024;

/// `(channel mask, duration_exp, passes)` of a scan request.
#[inline(never)]
fn scan_params(v: &J, max_exp: u8) -> Result<(u32, u8, u32), ApiError> {
    let q = ScanReq::from(v)?;
    let mask = q.channels.as_deref().map(mask_of).transpose()?.unwrap_or(0x07FF_F800);
    if mask == 0 {
        return Err(bad("no channels"));
    }
    Ok((mask, q.duration_exp.unwrap_or(3).min(max_exp), u32::from(q.passes.unwrap_or(q.count.unwrap_or(1)).clamp(1, 10))))
}

/// Radio time of one scan pass (spec 7.2: 15.36 ms × (2ⁿ + 1) per channel),
/// ×1.5 for the NCP interleaving it with the stack.
fn scan_estimate(mask: u32, duration_exp: u8) -> Duration {
    let per_us = 15_360 * ((1u64 << duration_exp.min(14)) + 1);
    Duration::from_micros(per_us * u64::from((mask & 0x07FF_F800).count_ones()) * 3 / 2 + 200_000)
}

/// The energy-scan result after `done` passes: average and peak energy per
/// channel, the recommendation on the averages.
#[allow(clippy::too_many_arguments)]
fn energy_result(
    agg: &[(u32, u8, i8); 16],
    mask: u32,
    done: u32,
    passes: u32,
    cancelled: bool,
    wifi: Option<u8>,
    current: Option<u8>,
    t: std::time::Instant,
) -> Result<String, ApiError> {
    let avg: Vec<EnergyChannel> = (11u8..=26)
        .filter(|c| mask & (1 << c) != 0)
        .map(|c| {
            let a = agg[usize::from(c - 11)];
            EnergyChannel { channel: c, max_rssi_dbm: a.2, energy: (a.0 / done.max(1)) as u8 }
        })
        .collect();
    let (best, scored) = recommend(&avg, wifi);
    let mut w = Jw::value(200 + 110 * avg.len())?;
    w.obj().key("passes").num(done).key("passes_requested").num(passes).key("cancelled").num(cancelled).key("channels").arr();
    for (c, (_, overlap, score)) in avg.iter().zip(&scored) {
        w.obj().key("channel").num(c.channel).key("energy").num(c.energy).key("energy_max").num(agg[usize::from(c.channel - 11)].1);
        w.key("max_rssi_dbm").num(c.max_rssi_dbm).key("wifi_overlap").num(overlap).key("score").num(score).end_obj();
    }
    w.end_arr();
    for (k, v) in [("current", current), ("wifi_channel", wifi), ("recommended", best)] {
        w.key(k);
        match v {
            Some(v) => w.num(v),
            None => w.null(),
        };
    }
    w.key("duration_ms").num(t.elapsed().as_millis() as u64).end_obj();
    w.finish_value()
}

/// The `network/diagnostics` result frame.
fn diagnostics(d: &Diagnostics, id: u64) -> Result<String, ApiError> {
    let mut w = Jw::result(id, 700 + 75 * d.counters.len() + 50 * d.host_link.len())?;
    w.obj().key("ncp");
    match &d.status {
        Some(s) => {
            w.obj().key("network_state").num(s.network_state).key("uptime_s").num(s.uptime_s);
            w.key("free_aps_slots").num(s.free_aps_slots).key("host_absent_queue").num(s.host_absent_queue);
            w.key("dirty_writebacks").num(s.dirty_writebacks).key("restore_in_progress").num(s.restore_in_progress);
            for (on, k) in [
                (s.key_export_locked, "key_export_locked"),
                (s.paging_active, "paging_active"),
                (s.pta_coex, "pta_coex"),
                (s.gp_commissioning, "gp_commissioning"),
                (s.permit_join, "permit_join"),
            ] {
                w.key(k).num(on);
            }
            w.end_obj()
        }
        None => w.null(),
    };
    let mut hexid = String::new();
    w.key("counters").arr();
    for c in &d.counters {
        hexid.clear();
        let _ = std::fmt::Write::write_fmt(&mut hexid, format_args!("0x{:04X}", c.id));
        w.obj().key("id").str(&hexid).key("group").str(c.group);
        w.key("name").str(c.name.unwrap_or(&hexid)).key("value").num(c.value).end_obj();
    }
    w.end_arr().key("frame_counters");
    match &d.frame_counters {
        Some(f) => {
            w.obj().key("nwk").num(f.nwk).key("nwk_limit").num(f.nwk_limit).key("aps").num(f.aps);
            w.key("aps_limit").num(f.aps_limit).key("stride").num(f.stride).end_obj()
        }
        None => w.null(),
    };
    w.key("host_link").arr();
    for c in d.host_link.iter().filter(|c| c.value != 0) {
        w.obj().key("name").str(c.name.unwrap_or("?")).key("value").num(c.value).end_obj();
    }
    w.end_arr().key("pta").obj().key("capable").num(d.coex).key("enabled");
    match &d.status {
        Some(s) => w.num(s.pta_coex),
        None => w.null(),
    };
    w.end_obj().key("tx_power_set");
    match d.tx_power_set {
        Some(p) => w.num(p),
        None => w.null(),
    };
    w.key("unsupported").arr();
    for u in &d.unsupported {
        w.str(u);
    }
    w.end_arr().key("errors").arr();
    for e in &d.errors {
        w.str(e);
    }
    w.end_arr().end_obj();
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wifi_overlap_matches_the_usual_chart() {
        let under = |w| (11..=26).filter(|z| wifi_overlap(*z, w)).collect::<Vec<u8>>();
        assert_eq!(under(1), vec![11, 12, 13, 14]);
        assert_eq!(under(6), vec![16, 17, 18, 19]);
        assert_eq!(under(11), vec![21, 22, 23, 24]);
        assert_eq!(under(13), vec![23, 24, 25, 26]);
    }

    #[test]
    fn recommendation_avoids_the_hub_wifi_channel() {
        let scan: Vec<EnergyChannel> = (11..=26)
            .map(|c| EnergyChannel { channel: c, max_rssi_dbm: -90, energy: if c == 20 { 5 } else { 40 } })
            .collect();
        assert_eq!(recommend(&scan, None).0, Some(20), "quietest");
        // Wi-Fi 9 (2452 MHz) covers 19–22: channel 20 is out.
        let (best, scored) = recommend(&scan, Some(9));
        assert!(scored.iter().any(|(c, o, _)| *c == 20 && *o));
        assert_eq!(best, Some(11), "the lowest Light Link primary away from Wi-Fi");
        let busy: Vec<EnergyChannel> = scan.iter().map(|c| EnergyChannel { energy: if c.channel == 15 { 250 } else { 40 }, ..*c }).collect();
        assert_eq!(recommend(&busy, Some(1)).0, Some(20));
    }

    /// Every ZRHP status and host core error keeps its API code and message
    /// (as before the coordinator boundary).
    #[test]
    fn ncp_errors_keep_their_api_codes_and_messages() {
        use hub_ncp::Error;
        use zigbee_zrhp::types::Status;
        let named = |code: &str, why: &str| (code.to_string(), format!("x: {why}"));
        let mut table: Vec<(Error, (String, String))> = vec![
            (Error::Status(Status::Success), named("failed", "NCP status Success")),
            (Error::Status(Status::Failure), named("failed", "NCP status Failure")),
            (Error::Status(Status::Busy), named("busy", "the NCP is busy")),
            (Error::Status(Status::InvalidParameter), named("invalid_value", "the NCP refused a parameter")),
            (Error::Status(Status::UnsupportedCommand), named("unsupported", "the NCP firmware does not implement it")),
            (Error::Status(Status::InvalidState), named("invalid_value", "the network is not in a state for this (not up?)")),
            (Error::Status(Status::NotFound), named("not_found", "not found on the NCP")),
            (Error::Status(Status::NoSpace), named("failed", "NCP status NoSpace")),
            (Error::Status(Status::Timeout), named("failed", "NCP status Timeout")),
            (Error::Status(Status::LinkReset), named("failed", "NCP status LinkReset")),
            (Error::Status(Status::AccessDenied), named("unauthorized", "the NCP refused (key export locked?)")),
            (Error::Status(Status::NotFormed), named("failed", "NCP status NotFormed")),
            (Error::Status(Status::AlreadyExists), named("failed", "NCP status AlreadyExists")),
            (Error::Status(Status::StorageError), named("failed", "NCP status StorageError")),
            (Error::Status(Status::SecurityError), named("failed", "NCP status SecurityError")),
            (Error::Status(Status::VersionMismatch), named("failed", "NCP status VersionMismatch")),
            (Error::Status(Status::InProgress), named("busy", "the NCP is busy")),
            (Error::Status(Status::TooLarge), named("failed", "NCP status TooLarge")),
            (Error::Status(Status::WrongEndpoint), named("failed", "NCP status WrongEndpoint")),
            (Error::Status(Status::Conflict), named("failed", "NCP status Conflict")),
            (Error::Status(Status::Aborted), named("failed", "NCP status Aborted")),
            (Error::Status(Status::LinkRejected), named("failed", "NCP status LinkRejected")),
        ];
        // Unnamed statuses, 0x16–0xFF.
        table.extend((0x16..=0xFF).map(|b| (Error::Status(Status::from_u8(b)), named("failed", &format!("NCP status Other({b})")))));
        table.extend([
            (Error::Timeout, named("timeout", "no answer from the NCP")),
            (Error::LinkReset, named("device_unavailable", "NCP link lost")),
            (Error::Closed, named("device_unavailable", "NCP link lost")),
            (Error::Rejected(3), named("failed", "frame rejected (reason 3)")),
            (Error::Codec, named("failed", "encode/decode error")),
            (Error::Other("formation failed: state 1 zb_status 0xC2".into()), named("failed", "formation failed: state 1 zb_status 0xC2")),
        ]);
        for (i, (e, want)) in table.into_iter().enumerate() {
            if i < 0x16 {
                assert_eq!(e, Error::Status(Status::from_u8(i as u8)), "the table follows the status values");
            }
            let a = ncp_error("x", hub_ncp::coord::coord_error(e.clone()));
            assert_eq!((a.code, a.message), want, "{e:?}");
        }
    }

    #[test]
    fn heads_only_for_maintenance_types() {
        assert_eq!(head(r#"{"id": 3, "type": "network/energy_scan"}"#).unwrap().id, 3);
        assert!(head(r#"{"id": 3, "type": "network/info"}"#).is_none());
        assert!(head(r#"{"id": 3, "type": "devices/update", "name": "network/reset"}"#).is_none());
        assert!(head("not json network/reset").is_none());
    }
}
