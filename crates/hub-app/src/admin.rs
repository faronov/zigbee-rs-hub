//! Network and hub maintenance API (device-support 14.6, API 1.2): energy
//! scan with a channel recommendation, channel change, TX power, NCP
//! diagnostics, the network map, backup/restore (Open Coordinator Backup),
//! forming a new network after a reset, and the hub's own settings (Wi-Fi,
//! API token, reboot, boot mode, system facts, log tail).
//!
//! Requests reach this module only from authenticated sessions. Everything
//! that talks to the NCP runs on one job thread at a time (the app thread
//! must keep serving events); its result frame and `network` events come
//! back through the app's input channel. Keys only appear in backup files
//! sent to the requesting session and in the host store, never in logs.

use std::sync::{Arc, Mutex};

use hub_devices::api::{ApiServer, SessionId, ServerMessage};
use hub_devices::hub::ApiError;
use hub_ncp::lifecycle::{EnergyChannel, FormRequest, RestoreOptions};
use hub_ncp::{Error, Hub as Ncp};
use log::{info, warn};
use serde::Deserialize;
use serde_json::{Value as J, json};
use zigbee_zrhp::msg::system::caps;
use zigbee_zrhp::types::Status;

use crate::ocb;
use crate::platform::Platform;
use crate::topology::{self, Known, ScanOpts};

/// Request types handled here.
pub const TYPES: &[&str] = &[
    "network/energy_scan",
    "network/change_channel",
    "network/set_tx_power",
    "network/diagnostics",
    "network/topology",
    "network/backup",
    "network/backup/last",
    "network/restore",
    "network/reset",
    "hub/system",
    "hub/logs",
    "hub/wifi/set",
    "hub/token/rotate",
    "hub/reboot",
    "hub/mode/set",
];

/// Host store key of the last complete backup (OCB text).
pub const LAST_BACKUP_KEY: &str = "bkp/last";
/// Largest backup accepted by `network/restore` (chunked).
pub const MAX_RESTORE_BYTES: usize = 256 * 1024;

/// `id` and `type` of a request this module handles.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Head {
    /// Request ID.
    pub id: u64,
    /// Request type.
    #[serde(rename = "type")]
    pub ty: String,
}

/// The request's head if its type is one of [`TYPES`] (the rest of the
/// frame is parsed by the handler, typed).
pub fn head(text: &str) -> Option<Head> {
    if !TYPES.iter().any(|t| text.contains(t)) {
        return None;
    }
    let h: Head = serde_json::from_str(text).ok()?;
    TYPES.contains(&h.ty.as_str()).then_some(h)
}

/// What a job sends back to the app thread.
pub enum JobOut {
    /// A frame for a session.
    Reply(SessionId, String),
    /// A `network` topic event.
    Event(J),
    /// The last network map (text), served by `network/topology` without
    /// `refresh`.
    Topology(String),
    /// The NCP's network was replaced: import its devices again.
    Reimport,
}

/// Delivery of [`JobOut`]s.
pub type Out = Arc<dyn Fn(JobOut) + Send + Sync>;

/// Everything a request may need from the app thread.
pub struct Ctx<'a> {
    /// Host core.
    pub ncp: &'a Ncp,
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

/// Maintenance state of the app.
pub struct Admin {
    /// The platform (`None`: hub settings unsupported).
    pub platform: Option<Arc<dyn Platform>>,
    /// Hub name and version (`metadata.source` of backups).
    pub source: String,
    /// Stack of the job thread.
    pub job_stack: usize,
    busy: Arc<Mutex<Option<&'static str>>>,
    restore_buf: Option<(usize, String)>,
    /// Last network map.
    pub topology: Option<String>,
}

fn ncp_error(what: &str, e: Error) -> ApiError {
    let (code, msg) = match &e {
        Error::Status(Status::UnsupportedCommand) => ("unsupported", format!("{what}: the NCP firmware does not implement it")),
        Error::Status(Status::InvalidParameter) => ("invalid_value", format!("{what}: the NCP refused a parameter")),
        Error::Status(Status::InvalidState) => ("invalid_value", format!("{what}: the network is not in a state for this (not up?)")),
        Error::Status(Status::Busy | Status::InProgress) => ("busy", format!("{what}: the NCP is busy")),
        Error::Status(Status::AccessDenied) => ("unauthorized", format!("{what}: the NCP refused (key export locked?)")),
        Error::Status(Status::NotFound) => ("not_found", format!("{what}: not found on the NCP")),
        Error::Timeout => ("timeout", format!("{what}: no answer from the NCP")),
        Error::LinkReset | Error::Closed => ("device_unavailable", format!("{what}: NCP link lost ({e})")),
        _ => ("failed", format!("{what}: {e}")),
    };
    ApiError::new(code, msg)
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError::new("invalid_value", msg)
}

fn params<'a, T: Deserialize<'a>>(text: &'a str) -> Result<T, ApiError> {
    serde_json::from_str(text).map_err(|e| ApiError::new("invalid_format", e.to_string()))
}

fn frame(id: u64, r: Result<J, ApiError>) -> String {
    match r {
        Ok(v) => ServerMessage::ok(id, v),
        Err(e) => ServerMessage::err(id, e),
    }
    .to_text()
}

/// A result frame around JSON text the caller already produced.
fn raw_frame(id: u64, json_text: &str) -> String {
    format!("{{\"id\":{id},\"type\":\"result\",\"success\":true,\"result\":{json_text}}}")
}

type Job = Box<dyn FnOnce(&Out) -> Result<Reply, ApiError> + Send>;

/// Result of a job.
pub enum Reply {
    /// A JSON value.
    Json(J),
    /// JSON text (large results: backups, maps).
    Raw(String),
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

fn counter_name(id: u16) -> String {
    const LINK: [&str; 28] = [
        "", "tx_frames", "tx_bytes", "tx_retransmissions", "tx_rto_expiries", "tx_rejected", "tx_blocked_credit_ms",
        "tx_blocked_window_ms", "tx_discarded_no_host", "naks_sent", "naks_received", "sessions_established",
        "link_down_events", "", "", "", "", "rx_frames", "rx_bytes", "rx_bad_frame", "rx_duplicate", "rx_out_of_order",
        "rx_epoch_mismatch", "rx_not_in_session", "rx_rejected_sent", "rx_overflow", "rx_stale_reset", "peer_restarted",
    ];
    let name = match id >> 8 {
        0x01 if id == 0x011C => "peer_resync",
        0x01 => LINK.get(usize::from(id & 0xFF)).copied().unwrap_or(""),
        0x07 => ["", "aps_queue_high_water", "busy_responses", "indications_dropped", "log_dropped", "bad_batches"]
            .get(usize::from(id & 0xFF))
            .copied()
            .unwrap_or(""),
        _ => "",
    };
    if name.is_empty() { format!("0x{id:04X}") } else { name.to_string() }
}

fn group_name(id: u16) -> &'static str {
    ["", "link", "mac", "nwk", "aps", "security", "tables", "queues", "gp"].get(usize::from(id >> 8)).copied().unwrap_or("other")
}

fn be_hex(b: &[u8]) -> String {
    b.iter().rev().map(|x| format!("{x:02x}")).collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

struct ScanReq {
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

#[derive(Deserialize)]
struct RestoreReq {
    #[serde(default)]
    backup: Option<ocb::Ocb>,
    #[serde(default)]
    chunk: Option<String>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    total: Option<usize>,
    #[serde(default = "yes")]
    allow_ieee_change: bool,
    #[serde(default)]
    force: bool,
    #[serde(default)]
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
        Ok(Self { channels: channels(v, "channels")?, duration_exp: num(v, "duration_exp")?, count: num(v, "count")? })
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
        Ok(Self {
            ssid: need(text(v, "ssid")?, "ssid")?.to_string(),
            password: text(v, "password")?.unwrap_or_default().to_string(),
        })
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

fn yes() -> bool {
    true
}

fn mask_of(channels: &[u8]) -> Result<u32, ApiError> {
    channels.iter().try_fold(0u32, |m, c| {
        if (11..=26).contains(c) { Ok(m | 1 << c) } else { Err(bad(format!("channel {c} outside 11–26"))) }
    })
}

/// Export (keys optional) and render the OCB file; complete backups are
/// also kept in the host store.
fn take_backup(ncp: &Ncp, keys: bool, known: &[Known], source: &str, now_s: f64) -> Result<(String, ocb::ExportSummary), ApiError> {
    let stream = ncp.backup_export(keys).map_err(|e| ncp_error("backup export", e))?;
    let session = ncp.client().session();
    let info = ocb::ExportInfo {
        source: source.to_string(),
        creation_time: ocb::rfc3339(now_s as u64),
        ncp_fw: session.as_ref().map(|s| s.negotiated.fw.clone()).unwrap_or_default(),
        ncp_chip: session.as_ref().map_or(0, |s| s.negotiated.chip),
        ncp_build_id: session.as_ref().map(|s| hex(&s.negotiated.build_id)).unwrap_or_default(),
        host_devices: known.iter().map(|k| (k.ieee.to_le_bytes(), k.nwk)).collect(),
    };
    let (text, summary) = ocb::from_records(&stream, &info).map_err(|e| ApiError::new("failed", e))?;
    if summary.complete {
        ncp.with_store(|s| s.kv_put(LAST_BACKUP_KEY, text.as_bytes()))
            .map_err(|e| ApiError::new("failed", format!("backup not stored: {e}")))?;
    }
    info!("admin: backup taken ({} devices, {} link keys, complete {})", summary.devices, summary.link_keys, summary.complete);
    Ok((text, summary))
}

impl Admin {
    /// No platform, default job stack.
    pub fn new(platform: Option<Arc<dyn Platform>>, source: String) -> Self {
        Self { platform, source, job_stack: 16 * 1024, busy: Arc::default(), restore_buf: None, topology: None }
    }

    /// `hub/info` `maintenance`: what this hub and its NCP support.
    pub fn features(&self, ncp: &Ncp) -> Vec<&'static str> {
        let mut f = vec!["diagnostics", "topology", "change_channel", "reset"];
        for (bit, name) in [
            (caps::ENERGY_SCAN, "energy_scan"),
            (caps::TX_POWER, "tx_power"),
            (caps::BACKUP_RESTORE, "backup"),
            (caps::BACKUP_RESTORE, "restore"),
        ] {
            if ncp.ncp_has(bit) {
                f.push(name);
            }
        }
        f.extend(self.platform.as_ref().map(|p| p.features()).unwrap_or_default());
        f
    }

    /// Handle one request; returns frames to send now (a job's frame
    /// follows through [`Ctx::out`]).
    pub fn handle(&mut self, ctx: Ctx<'_>, sid: SessionId, head: &Head, text: &str) -> Vec<String> {
        let id = head.id;
        if head.ty == "network/restore" {
            return self.restore(ctx, sid, id, text);
        }
        let v: J = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => return vec![frame(id, Err(ApiError::new("invalid_format", e.to_string())))],
        };
        let r = match head.ty.as_str() {
            "hub/system" => Ok(self.system(&ctx)),
            "hub/logs" => self.logs(&v),
            "hub/wifi/set" => self.wifi(&v),
            "hub/token/rotate" => self.rotate_token(ctx.api),
            "hub/reboot" => self.platform().and_then(|p| p.reboot().map_err(|e| ApiError::new("failed", e))).map(|_| json!({"rebooting": true})),
            "hub/mode/set" => self.mode(&v),
            "network/backup/last" => {
                let last = ctx.ncp.with_store(|s| s.kv_get(LAST_BACKUP_KEY).map(|b| String::from_utf8_lossy(b).into_owned()));
                return vec![raw_frame(id, &format!("{{\"backup\":{}}}", last.as_deref().unwrap_or("null")))];
            }
            "network/topology" => match TopologyReq::from(&v) {
                Ok(q) if !q.refresh => {
                    return vec![raw_frame(id, self.topology.as_deref().unwrap_or("null"))];
                }
                Ok(q) => return self.job_topology(ctx, sid, id, q),
                Err(e) => Err(e),
            },
            _ => return self.ncp_job(ctx, sid, head, &v),
        };
        vec![frame(id, r)]
    }

    fn platform(&self) -> Result<&Arc<dyn Platform>, ApiError> {
        self.platform.as_ref().ok_or_else(|| ApiError::new("unsupported", "no platform settings here"))
    }

    fn system(&self, ctx: &Ctx<'_>) -> J {
        let mut v = self.platform.as_ref().map(|p| p.system()).unwrap_or_else(|| json!({}));
        let st = ctx.ncp.status();
        if let Some(o) = v.as_object_mut() {
            o.insert("app".into(), (ctx.app_facts)());
            o.insert(
                "store".into(),
                json!({
                    "volatile": st.store_volatile,
                    "journal_sectors": st.journal.sectors,
                    "journal_live_sectors": st.journal.live_sectors,
                    "journal_generation": st.journal.generation,
                }),
            );
            o.insert(
                "ncp".into(),
                json!({
                    "fw": st.ncp.as_ref().map(|s| s.negotiated.fw.clone()),
                    "resumes": st.resumes,
                    "restarts": st.ncp_restarts,
                    "last_error": st.last_error,
                    "slots": st.slots.iter().map(|s| json!({"slot": s.slot, "state": s.state, "version": format!("0x{:08X}", s.version)})).collect::<Vec<_>>(),
                }),
            );
            o.insert("time".into(), ctx.now_s.into());
        }
        v
    }

    fn logs(&self, v: &J) -> Result<J, ApiError> {
        let q = LogsReq::from(v)?;
        let p = self.platform()?;
        Ok(json!({"lines": p.logs(q.lines.unwrap_or(100).min(500))}))
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
        {
            let mut busy = self.busy.lock().unwrap();
            if let Some(running) = *busy {
                return vec![frame(id, Err(ApiError::new("busy", format!("{running} is running"))))];
            }
            *busy = Some(name);
        }
        struct Done(Arc<Mutex<Option<&'static str>>>);
        impl Drop for Done {
            fn drop(&mut self) {
                if let Ok(mut b) = self.0.lock() {
                    *b = None;
                }
            }
        }
        let done = Done(self.busy.clone());
        let spawned = std::thread::Builder::new().name("hub-admin".into()).stack_size(self.job_stack).spawn(move || {
            let r = f(&out);
            drop(done);
            let text = match r {
                Ok(Reply::Json(v)) => frame(id, Ok(v)),
                Ok(Reply::Raw(t)) => raw_frame(id, &t),
                Err(e) => {
                    warn!("admin: {name}: {}", e.message);
                    frame(id, Err(e))
                }
            };
            out(JobOut::Reply(sid, text));
        });
        match spawned {
            Ok(_) => Vec::new(),
            Err(e) => {
                *self.busy.lock().unwrap() = None;
                vec![frame(id, Err(ApiError::new("failed", format!("job thread: {e}"))))]
            }
        }
    }

    fn ncp_job(&mut self, ctx: Ctx<'_>, sid: SessionId, head: &Head, v: &J) -> Vec<String> {
        let id = head.id;
        let ncp = ctx.ncp.clone();
        let out = ctx.out.clone();
        let fail = |e: ApiError| vec![frame(id, Err(e))];
        match head.ty.as_str() {
            "network/energy_scan" => {
                let q = match ScanReq::from(v) {
                    Ok(q) => q,
                    Err(e) => return fail(e),
                };
                let mask = match q.channels.as_deref().map(mask_of).transpose() {
                    Ok(m) => m.unwrap_or(0x07FF_F800),
                    Err(e) => return fail(e),
                };
                if mask == 0 {
                    return fail(bad("no channels"));
                }
                let duration = q.duration_exp.unwrap_or(4).min(8);
                let count = q.count.unwrap_or(1).clamp(1, 4);
                let wifi = self.platform.as_ref().and_then(|p| p.wifi_channel());
                self.spawn("energy scan", out, sid, id, move |_| {
                    let t = std::time::Instant::now();
                    let scan = ncp.energy_scan(mask, duration, count).map_err(|e| ncp_error("energy scan", e))?;
                    let (best, scored) = recommend(&scan, wifi);
                    let current = ncp.status().network.map(|n| n.channel);
                    let channels: Vec<J> = scan
                        .iter()
                        .zip(&scored)
                        .map(|(c, (_, overlap, score))| {
                            json!({"channel": c.channel, "energy": c.energy, "max_rssi_dbm": c.max_rssi_dbm, "wifi_overlap": overlap, "score": score})
                        })
                        .collect();
                    Ok(Reply::Json(json!({
                        "channels": channels,
                        "current": current,
                        "wifi_channel": wifi,
                        "recommended": best,
                        "duration_ms": t.elapsed().as_millis() as u64,
                    })))
                })
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
                    let r = ncp.change_channel(q.channel).map_err(|e| ncp_error("channel change", e))?;
                    out(JobOut::Event(json!({"topic": "network_updated", "reason": "channel_changed", "channel": r.channel, "nwk_update_id": r.nwk_update_id})));
                    Ok(Reply::Json(json!({"channel": r.channel, "nwk_update_id": r.nwk_update_id})))
                })
            }
            "network/set_tx_power" => {
                let q = match TxPowerReq::from(v) {
                    Ok(q) => q,
                    Err(e) => return fail(e),
                };
                self.spawn("TX power", out, sid, id, move |out| {
                    let applied = ncp.set_tx_power(q.dbm).map_err(|e| ncp_error("TX power", e))?;
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
                self.spawn("diagnostics", out, sid, id, move |_| Ok(Reply::Json(diagnostics(&ncp, groups, q.reset))))
            }
            "network/backup" => {
                let q = match BackupReq::from(v) {
                    Ok(q) => q,
                    Err(e) => return fail(e),
                };
                if q.keys && ctx.api.token().is_empty() {
                    return fail(ApiError::new("unauthorized", "backups with keys need an API token"));
                }
                if !ncp.ncp_has(caps::BACKUP_RESTORE) {
                    return fail(ApiError::new("unsupported", "the NCP has no BACKUP_RESTORE"));
                }
                let known = (ctx.known)();
                let source = self.source.clone();
                let now_s = ctx.now_s;
                self.spawn("backup", out, sid, id, move |_| {
                    take_backup(&ncp, q.keys, &known, &source, now_s).map(|(text, _)| Reply::Raw(text))
                })
            }
            "network/reset" => self.reset(ctx, sid, id, v),
            other => fail(ApiError::new("unsupported", other.to_string())),
        }
    }

    fn job_topology(&mut self, ctx: Ctx<'_>, sid: SessionId, id: u64, q: TopologyReq) -> Vec<String> {
        let ncp = ctx.ncp.clone();
        let known = (ctx.known)();
        let now_s = ctx.now_s;
        let opts = ScanOpts { routes: q.routes, ..ScanOpts::default() };
        self.spawn("network map", ctx.out.clone(), sid, id, move |out| {
            let mut progress = |scanned: usize, queued: usize| {
                out(JobOut::Event(json!({"topic": "topology_progress", "scanned": scanned, "queued": queued})));
            };
            let map = topology::scan(&ncp, &known, opts, now_s, &mut progress).to_string();
            out(JobOut::Topology(map.clone()));
            Ok(Reply::Raw(map))
        })
    }

    fn restore(&mut self, ctx: Ctx<'_>, sid: SessionId, id: u64, text: &str) -> Vec<String> {
        let fail = |e: ApiError| vec![frame(id, Err(e))];
        let q: RestoreReq = match params(text) {
            Ok(q) => q,
            Err(e) => return fail(e),
        };
        let backup = match (q.backup, q.chunk) {
            (Some(b), _) => b,
            (None, Some(chunk)) => {
                let (offset, total) = (q.offset.unwrap_or(0), q.total.unwrap_or(0));
                if total == 0 || total > MAX_RESTORE_BYTES {
                    self.restore_buf = None;
                    return fail(bad(format!("total: 1–{MAX_RESTORE_BYTES} bytes")));
                }
                if offset == 0 {
                    self.restore_buf = Some((total, String::with_capacity(total)));
                }
                let Some((want, buf)) = self.restore_buf.as_mut().filter(|(t, b)| *t == total && b.len() == offset) else {
                    self.restore_buf = None;
                    return fail(bad("chunk out of order; start again at offset 0"));
                };
                buf.push_str(&chunk);
                if buf.len() > *want {
                    self.restore_buf = None;
                    return fail(bad("more bytes than total"));
                }
                if buf.len() < *want {
                    return vec![frame(id, Ok(json!({"received": buf.len(), "total": total})))];
                }
                let (_, buf) = self.restore_buf.take().unwrap();
                match serde_json::from_str::<ocb::Ocb>(&buf) {
                    Ok(b) => b,
                    Err(e) => return fail(ApiError::new("invalid_format", format!("backup: {e}"))),
                }
            }
            (None, None) => return fail(bad("backup (object) or chunk/offset/total")),
        };
        let restore = match ocb::to_records(&backup) {
            Ok(r) => r,
            Err(e) => return fail(bad(e)),
        };
        drop(backup);
        let ncp = ctx.ncp.clone();
        if !ncp.ncp_has(caps::BACKUP_RESTORE) {
            return fail(ApiError::new("unsupported", "the NCP has no BACKUP_RESTORE"));
        }
        let st = ncp.status();
        if st.network_state != 0 && !q.force {
            let pan = st.network.map_or(String::from("?"), |n| format!("0x{:04X}", n.pan_id));
            return fail(bad(format!(
                "the NCP runs a network (PAN {pan}); set force to replace it (it is backed up first unless skip_backup)"
            )));
        }
        let known = (ctx.known)();
        let source = self.source.clone();
        let now_s = ctx.now_s;
        let opts = RestoreOptions {
            allow_ieee_change: q.allow_ieee_change,
            nwk_margin: 10_000,
            aps_margin: if restore.tclk_counter_known { 10_000 } else { ocb::UNKNOWN_APS_MARGIN },
        };
        let skip_backup = q.skip_backup;
        self.spawn("restore", ctx.out.clone(), sid, id, move |out| {
            let mut previous = false;
            if ncp.status().network_state == 2 && !skip_backup {
                take_backup(&ncp, true, &known, &source, now_s).map_err(|e| {
                    ApiError::new(&e.code, format!("automatic backup before the restore failed ({}); set skip_backup to restore anyway", e.message))
                })?;
                previous = true;
            }
            let report = ncp.backup_restore(&restore.stream, opts).map_err(|e| ncp_error("restore", e))?;
            out(JobOut::Reimport);
            out(JobOut::Event(json!({"topic": "network_updated", "reason": "restored"})));
            let note = |n: u8| ["", "host_backed", "child_demoted", "ignored", "floor_raised"].get(usize::from(n)).copied().unwrap_or("unknown");
            Ok(Reply::Json(json!({
                "pan_id": restore.pan_id,
                "channel": restore.channel,
                "coordinator_ieee": be_hex(&restore.coordinator_ieee),
                "devices": restore.devices.len(),
                "nwk_fc_start": report.nwk_fc_start,
                "aps_fc_start": report.aps_fc_start,
                "link_keys_on_ncp": report.link_keys_on_ncp,
                "link_keys_host_backed": report.link_keys_host_backed,
                "children": report.children,
                "notes": report.notes.iter().map(|(i, n)| json!({"index": i, "note": note(*n)})).collect::<Vec<_>>(),
                "previous_network_backed_up": previous,
            })))
        })
    }

    fn reset(&mut self, ctx: Ctx<'_>, sid: SessionId, id: u64, v: &J) -> Vec<String> {
        let fail = |e: ApiError| vec![frame(id, Err(e))];
        let q = match ResetReq::from(v) {
            Ok(q) => q,
            Err(e) => return fail(e),
        };
        if q.confirm != "RESET" {
            return fail(bad("confirm must be \"RESET\""));
        }
        let channels = match (q.channel, &q.channels) {
            (Some(c), _) => vec![c],
            (None, Some(list)) => list.clone(),
            (None, None) => Vec::new(),
        };
        let mask = match mask_of(&channels) {
            Ok(0) => FormRequest::default().channel_mask,
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
        let ncp = ctx.ncp.clone();
        let known = (ctx.known)();
        let source = self.source.clone();
        let now_s = ctx.now_s;
        let skip_backup = q.skip_backup;
        let tx_power = q.tx_power;
        self.spawn("network reset", ctx.out.clone(), sid, id, move |out| {
            let mut backup = None;
            let st = ncp.status();
            if st.network_state != 0 {
                if st.network_state == 2 && !skip_backup {
                    let (text, _) = take_backup(&ncp, true, &known, &source, now_s).map_err(|e| {
                        ApiError::new(&e.code, format!("automatic backup failed ({}); nothing was changed; set skip_backup to reset anyway", e.message))
                    })?;
                    backup = Some(text);
                }
                ncp.leave_reset().map_err(|e| ncp_error("leave/reset", e))?;
            }
            let req = FormRequest {
                channel_mask: mask,
                pan_id: q.pan_id,
                ext_pan_id: epid,
                network_key: None,
                tx_power: tx_power.or(ncp.stored_tx_power()).unwrap_or(0),
            };
            let net = ncp.form(req).map_err(|e| ncp_error("form", e))?;
            if let Some(dbm) = tx_power
                && let Err(e) = ncp.set_tx_power(dbm)
            {
                warn!("admin: TX power after forming: {e}");
            }
            out(JobOut::Reimport);
            out(JobOut::Event(json!({"topic": "network_updated", "reason": "formed"})));
            let network = format!(
                "{{\"pan_id\":{},\"ext_pan_id\":\"{}\",\"channel\":{}}}",
                net.pan_id,
                be_hex(&net.ext_pan_id),
                net.channel
            );
            Ok(Reply::Raw(format!("{{\"network\":{network},\"backup\":{}}}", backup.as_deref().unwrap_or("null"))))
        })
    }

    /// Whether a job is running (and which).
    pub fn running(&self) -> Option<&'static str> {
        *self.busy.lock().unwrap()
    }
}

fn diagnostics(ncp: &Ncp, groups: u8, reset: bool) -> J {
    let mut unsupported = Vec::new();
    let mut errors = Vec::new();
    let mut note = |what: &str, e: Error| match e {
        Error::Status(Status::UnsupportedCommand) => unsupported.push(what.to_string()),
        e => errors.push(format!("{what}: {e}")),
    };
    if reset && let Err(e) = ncp.diag_reset_counters(groups) {
        note("DIAG_RESET_COUNTERS", e);
    }
    let counters: Vec<J> = match ncp.diag_counters(groups) {
        Ok(list) => list
            .iter()
            .map(|(id, v)| json!({"id": format!("0x{id:04X}"), "group": group_name(*id), "name": counter_name(*id), "value": v}))
            .collect(),
        Err(e) => {
            note("DIAG_GET_COUNTERS", e);
            Vec::new()
        }
    };
    let status = match ncp.ncp_status() {
        Ok(s) => json!({
            "network_state": s.network_state,
            "uptime_s": s.uptime_s,
            "free_aps_slots": s.free_aps_slots,
            "host_absent_queue": s.host_absent_queue,
            "dirty_writebacks": s.dirty_writebacks,
            "restore_in_progress": s.restore_in_progress != 0,
            "key_export_locked": s.flags & 0x01 != 0,
            "paging_active": s.flags & 0x02 != 0,
            "pta_coex": s.flags & 0x04 != 0,
            "gp_commissioning": s.flags & 0x08 != 0,
            "permit_join": s.flags & 0x10 != 0,
        }),
        Err(e) => {
            note("SYS_GET_STATUS", e);
            J::Null
        }
    };
    let fc = match ncp.frame_counters() {
        Ok(f) => json!({"nwk": f.nwk_current, "nwk_limit": f.nwk_limit, "aps": f.aps_current, "aps_limit": f.aps_limit, "stride": f.stride}),
        Err(e) => {
            note("SEC_GET_FRAME_COUNTERS", e);
            J::Null
        }
    };
    let host: Vec<J> = ncp
        .client()
        .counters()
        .entries()
        .iter()
        .filter(|(_, v)| *v != 0)
        .map(|(id, v)| json!({"name": counter_name(*id), "value": v}))
        .collect();
    let pta_capable = ncp.ncp_has(caps::PTA_COEX);
    let pta_enabled = status.get("pta_coex").and_then(J::as_bool);
    json!({
        "ncp": status,
        "counters": counters,
        "frame_counters": fc,
        "host_link": host,
        "pta": {"capable": pta_capable, "enabled": pta_enabled},
        "tx_power_set": ncp.stored_tx_power(),
        "unsupported": unsupported,
        "errors": errors,
    })
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

    #[test]
    fn counter_names() {
        assert_eq!(counter_name(0x0101), "tx_frames");
        assert_eq!(counter_name(0x0113), "rx_bad_frame");
        assert_eq!(counter_name(0x011C), "peer_resync");
        assert_eq!(counter_name(0x0702), "busy_responses");
        assert_eq!(counter_name(0x0805), "0x0805");
        assert_eq!(group_name(0x0805), "gp");
    }

    #[test]
    fn heads_only_for_maintenance_types() {
        assert_eq!(head(r#"{"id": 3, "type": "network/energy_scan"}"#).unwrap().id, 3);
        assert!(head(r#"{"id": 3, "type": "network/info"}"#).is_none());
        assert!(head(r#"{"id": 3, "type": "devices/update", "name": "network/reset"}"#).is_none());
        assert!(head("not json network/reset").is_none());
    }
}
