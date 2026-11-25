//! The OTA Upgrade server (ZCL r8 11.13), sans-IO.
//!
//! Inputs are the client commands of one device (`on_command`), API
//! actions (`start`, `abort`, `check`) and time (`poll`). Outputs are
//! [`Action`]s the caller turns into ZCL frames on the device's OTA client
//! endpoint, plus `Changed` notes for events and persistence. Images and
//! their bytes come from [`Images`]: the store, or a client streaming them.
//!
//! Policy: a device downloads only an image it was offered, and it is
//! offered one when an `ota/start` approved it (`manual`, the default), or
//! automatically inside the schedule window (`auto`). Every other Query
//! Next Image is answered NO_IMAGE_AVAILABLE, but the image is recorded as
//! available. Block requests carry the whole image identity and offset, so
//! a download continues after a hub restart from the persisted job. `off`
//! approves nothing: `ota/start` is refused and a running job ends
//! `aborted`. Upgrade End without an approving job (aborted, `off`, never
//! started) gets Default Response ABORT, not the go-ahead.
//!
//! Pacing: block responses leave at most `blocks_per_s` per second over
//! all devices and not faster than `min_block_period_ms` per device; a
//! response that would wait longer than [`MAX_HOLD_MS`] becomes
//! WAIT_FOR_DATA. A device has at most one block response queued: a new
//! request supersedes it. An Image Page Request is answered one block at a
//! time as each falls due ([`MAX_PAGE_BLOCKS`] at most, spaced by the
//! request's ResponseSpacing and at least `min_block_period_ms`), so a page
//! holds no buffers and takes the shared pacing slots in turn with the
//! other devices. At most `max_concurrent` devices download at once; others
//! are told NO_IMAGE_AVAILABLE and notified when a slot frees.

use std::collections::BTreeMap;

use serde_json::{Value as J, json};

use crate::images::{Fetch, Images, Offer};
use crate::json::Obj;

/// Milliseconds of the hub's (wall) clock.
pub type Millis = u64;

/// The cluster.
pub const CLUSTER: u16 = 0x0019;

/// Command IDs.
pub mod cmd {
    /// Image Notify (server → client).
    pub const IMAGE_NOTIFY: u8 = 0x00;
    /// Query Next Image Request.
    pub const QUERY_NEXT_IMAGE_REQ: u8 = 0x01;
    /// Query Next Image Response.
    pub const QUERY_NEXT_IMAGE_RSP: u8 = 0x02;
    /// Image Block Request.
    pub const IMAGE_BLOCK_REQ: u8 = 0x03;
    /// Image Page Request.
    pub const IMAGE_PAGE_REQ: u8 = 0x04;
    /// Image Block Response.
    pub const IMAGE_BLOCK_RSP: u8 = 0x05;
    /// Upgrade End Request.
    pub const UPGRADE_END_REQ: u8 = 0x06;
    /// Upgrade End Response.
    pub const UPGRADE_END_RSP: u8 = 0x07;
    /// Query Device Specific File Request.
    pub const QUERY_SPECIFIC_FILE_REQ: u8 = 0x08;
    /// Query Device Specific File Response.
    pub const QUERY_SPECIFIC_FILE_RSP: u8 = 0x09;
}

/// ZCL status codes of the cluster.
pub mod status {
    /// Success.
    pub const SUCCESS: u8 = 0x00;
    /// Not authorized.
    pub const NOT_AUTHORIZED: u8 = 0x7E;
    /// Malformed command.
    pub const MALFORMED_COMMAND: u8 = 0x80;
    /// Unsupported cluster command.
    pub const UNSUP_CLUSTER_COMMAND: u8 = 0x81;
    /// Abort.
    pub const ABORT: u8 = 0x95;
    /// Invalid image.
    pub const INVALID_IMAGE: u8 = 0x96;
    /// Wait for data.
    pub const WAIT_FOR_DATA: u8 = 0x97;
    /// No image available.
    pub const NO_IMAGE_AVAILABLE: u8 = 0x98;
    /// Require more image.
    pub const REQUIRE_MORE_IMAGE: u8 = 0x99;
}

/// OTA client attributes.
pub mod attr {
    /// CurrentFileVersion.
    pub const CURRENT_FILE_VERSION: u16 = 0x0002;
}

/// ZCL header (3 B) and the fixed fields of an Image Block Response (14 B).
pub const BLOCK_OVERHEAD: usize = 17;
/// `max_asdu` of an NCP that does not report it (the zigbee-rs NCP: 82).
pub const DEFAULT_ASDU: usize = 82;
/// A paced response that would wait longer becomes WAIT_FOR_DATA.
pub const MAX_HOLD_MS: u64 = 3_000;
/// WAIT_FOR_DATA delay while a streamed block is fetched from the client, s.
pub const STREAM_WAIT_S: u32 = 1;
/// Largest page served by an Image Page Request.
pub const MAX_PAGE: u32 = 4096;
/// Most block responses one Image Page Request gets (a page of tiny blocks
/// is cut short; the device asks again for the rest).
pub const MAX_PAGE_BLOCKS: u32 = 128;
/// A page whose streamed data is not here for this long is dropped (the
/// device asks again).
pub const PAGE_WAIT_MS: u64 = 2_000;

/// Default OTA image index the clients (web UI, Home Assistant) fetch:
/// Koenkk/zigbee-OTA (CORS open on raw.githubusercontent.com).
pub const DEFAULT_INDEX: &str =
    "https://raw.githubusercontent.com/Koenkk/zigbee-OTA/master/index.json";

/// Seconds from 1970 to the Zigbee epoch (2000-01-01).
const ZIGBEE_EPOCH: u64 = 946_684_800;

/// The error of a job ended by the `off` policy.
const POLICY_OFF: &str = "the device's OTA policy is off";

/// When a device may be offered an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Only after `ota/start`.
    Manual,
    /// Automatically, inside the schedule window.
    Auto,
    /// Never (queries are still recorded).
    Off,
}

impl Policy {
    /// API name.
    pub fn name(self) -> &'static str {
        match self {
            Policy::Manual => "manual",
            Policy::Auto => "auto",
            Policy::Off => "off",
        }
    }

    /// From the API name.
    pub fn parse(s: &str) -> Option<Policy> {
        match s {
            "manual" => Some(Policy::Manual),
            "auto" => Some(Policy::Auto),
            "off" => Some(Policy::Off),
            _ => None,
        }
    }
}

/// Server settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Policy of devices without their own.
    pub policy: Policy,
    /// Schedule window of `auto`: `[start, end)` in minutes of the local
    /// day (may wrap midnight); `None` = always.
    pub window: Option<(u16, u16)>,
    /// Local time offset, s.
    pub tz_offset_s: i64,
    /// Devices downloading at once.
    pub max_concurrent: u8,
    /// Largest block (also capped by the NCP's `max_asdu`). 48 B (the
    /// zigbee-rs client's request) leaves 17 B of the NCP's 82-B ASDU for
    /// a source route's relays: `max_asdu` does not depend on the
    /// destination (zigpy serves 40 B).
    pub block_max: u8,
    /// Per device: least time between blocks, ms (told to clients with
    /// WAIT_FOR_DATA's MinimumBlockPeriod).
    pub min_block_period_ms: u16,
    /// Block responses per second over all devices (0 = unlimited).
    pub blocks_per_s: u16,
    /// Delay of the upgrade after the download, s (0 = at once).
    pub upgrade_delay_s: u32,
    /// A download without a block request for this long frees its slot
    /// (it can still continue), s.
    pub stall_s: u32,
    /// The image index clients fetch (the hub itself downloads nothing).
    pub index_url: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            policy: Policy::Manual,
            window: None,
            tz_offset_s: 0,
            max_concurrent: 2,
            block_max: 48,
            min_block_period_ms: 0,
            blocks_per_s: 20,
            upgrade_delay_s: 0,
            stall_s: 300,
            index_url: DEFAULT_INDEX.into(),
        }
    }
}

fn hm(m: u16) -> String {
    format!("{:02}:{:02}", m / 60, m % 60)
}

fn parse_hm(s: &str) -> Option<u16> {
    let (h, m) = s.split_once(':')?;
    let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

impl Settings {
    /// As JSON.
    pub fn to_json(&self) -> J {
        json!({
            "policy": self.policy.name(),
            "window": self.window.map(|(a, b)| json!({"start": hm(a), "end": hm(b)})),
            "max_concurrent": self.max_concurrent,
            "block_max": self.block_max,
            "min_block_period_ms": self.min_block_period_ms,
            "blocks_per_s": self.blocks_per_s,
            "upgrade_delay_s": self.upgrade_delay_s,
            "stall_s": self.stall_s,
            "index_url": self.index_url,
        })
    }

    /// Applies the fields present in `v` (all or nothing).
    pub fn apply(&mut self, v: &J) -> Result<(), String> {
        let mut s = self.clone();
        if let Some(p) = v.get("policy") {
            s.policy = p
                .as_str()
                .and_then(Policy::parse)
                .ok_or("policy: manual, auto or off")?;
        }
        if let Some(w) = v.get("window") {
            s.window = if w.is_null() {
                None
            } else {
                let a = w.get("start").and_then(J::as_str).and_then(parse_hm);
                let b = w.get("end").and_then(J::as_str).and_then(parse_hm);
                match (a, b) {
                    (Some(a), Some(b)) if a != b => Some((a, b)),
                    _ => return Err("window: {start, end} as HH:MM, different".into()),
                }
            };
        }
        let int = |k: &str, lo: u64, hi: u64| -> Result<Option<u64>, String> {
            match v.get(k) {
                None => Ok(None),
                Some(x) => x
                    .as_u64()
                    .filter(|n| (lo..=hi).contains(n))
                    .map(Some)
                    .ok_or(format!("{k}: {lo}..={hi}")),
            }
        };
        if let Some(n) = int("max_concurrent", 1, 8)? {
            s.max_concurrent = n as u8;
        }
        if let Some(n) = int("block_max", 16, 223)? {
            s.block_max = n as u8;
        }
        if let Some(n) = int("min_block_period_ms", 0, 60_000)? {
            s.min_block_period_ms = n as u16;
        }
        if let Some(n) = int("blocks_per_s", 0, 1000)? {
            s.blocks_per_s = n as u16;
        }
        if let Some(n) = int("upgrade_delay_s", 0, 86_400)? {
            s.upgrade_delay_s = n as u32;
        }
        if let Some(n) = int("stall_s", 30, 86_400)? {
            s.stall_s = n as u32;
        }
        if let Some(u) = v.get("index_url") {
            let u = u
                .as_str()
                .filter(|u| u.is_empty() || u.starts_with("https://") || u.starts_with("http://"));
            let u = u.ok_or("index_url: an http(s) URL or empty")?;
            s.index_url = if u.is_empty() {
                DEFAULT_INDEX.into()
            } else {
                u.chars().take(200).collect()
            };
        }
        *self = s;
        Ok(())
    }

    /// Persisted form (the API JSON plus the time zone).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = self.to_json();
        v["tz_offset_s"] = self.tz_offset_s.into();
        v.to_string().into_bytes()
    }

    /// From [`to_bytes`](Self::to_bytes); unknown or bad fields keep defaults.
    pub fn from_bytes(b: &[u8]) -> Settings {
        let mut s = Settings::default();
        // `from_str` shares the deserializer instance the API already links.
        if let Ok(v) = std::str::from_utf8(b)
            .map_err(drop)
            .and_then(|t| serde_json::from_str::<J>(t).map_err(drop))
        {
            let _ = s.apply(&v);
            if let Some(t) = v.get("tz_offset_s").and_then(J::as_i64) {
                s.tz_offset_s = t;
            }
        }
        s
    }
}

/// `(manufacturer, image type, file version)`.
pub type Ident = (u16, u16, u32);

/// State of an upgrade job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Approved: the device was notified, its query is awaited.
    Waiting,
    /// Approved, waiting for a free download slot.
    Queued,
    /// Downloading.
    Downloading,
    /// Downloaded; the device was told to upgrade.
    Installing,
    /// The device runs the new version.
    Done,
    /// Failed (see the error).
    Failed,
    /// Aborted by the user.
    Aborted,
}

impl JobState {
    /// API name.
    pub fn name(self) -> &'static str {
        match self {
            JobState::Waiting => "waiting",
            JobState::Queued => "queued",
            JobState::Downloading => "downloading",
            JobState::Installing => "installing",
            JobState::Done => "done",
            JobState::Failed => "failed",
            JobState::Aborted => "aborted",
        }
    }

    fn parse(s: &str) -> Option<JobState> {
        [
            JobState::Waiting,
            JobState::Queued,
            JobState::Downloading,
            JobState::Installing,
            JobState::Done,
            JobState::Failed,
            JobState::Aborted,
        ]
        .into_iter()
        .find(|j| j.name() == s)
    }

    /// The job still runs.
    pub fn active(self) -> bool {
        matches!(
            self,
            JobState::Waiting | JobState::Queued | JobState::Downloading | JobState::Installing
        )
    }
}

/// One upgrade of one device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// The image (`None`: the best one the device's query qualifies for).
    pub target: Option<Ident>,
    /// Image size.
    pub size: u32,
    /// State.
    pub state: JobState,
    /// Highest offset served.
    pub offset: u32,
    /// Started with `ota/start` (not by the `auto` policy).
    pub manual: bool,
    /// Offer even if not newer than the current version.
    pub force: bool,
    /// Error of a failed job.
    pub error: Option<String>,
    /// Wall clock of the start, s.
    pub started_s: u64,
    /// Last request or state change (RAM).
    pub updated: Millis,
    /// Image Notifies sent for this job (RAM).
    pub notified: u8,
    /// Last Image Notify or version read (RAM).
    pub poked: Millis,
    /// When the device was told to upgrade (RAM).
    pub installing_since: Millis,
    /// MinimumBlockPeriod already told (RAM).
    pub period_told: bool,
}

impl Job {
    fn new(target: Option<Ident>, size: u32, manual: bool, force: bool, now: Millis) -> Job {
        Job {
            target,
            size,
            state: JobState::Waiting,
            offset: 0,
            manual,
            force,
            error: None,
            started_s: now / 1000,
            updated: now,
            notified: 0,
            poked: 0,
            installing_since: 0,
            period_told: false,
        }
    }
}

/// What the server knows of one device (persisted).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DevState {
    /// OTA client endpoint.
    pub ep: Option<u8>,
    /// Manufacturer code, image type and current file version, from its
    /// last query (or the version read from CurrentFileVersion).
    pub current: Option<Ident>,
    /// Hardware version from its query.
    pub hw: Option<u16>,
    /// The device's own policy.
    pub policy: Option<Policy>,
    /// The upgrade job.
    pub job: Option<Job>,
    /// Last query, wall clock s.
    pub last_query_s: Option<u64>,
    /// When its last block response leaves (RAM).
    pub last_block: Millis,
    /// Offset of the last persisted progress (RAM).
    pub persisted_at: u32,
    /// Last progress publication: time, percent (RAM).
    pub published: (Millis, u32),
    /// Endpoint of an outstanding CurrentFileVersion read (RAM).
    pub reading: Option<u8>,
}

fn hex32(v: u32) -> String {
    format!("0x{v:08X}")
}

impl DevState {
    /// Persisted form.
    pub fn to_bytes(&self) -> Vec<u8> {
        let job = self.job.as_ref().map(|j| {
            json!({
                "target": j.target.map(|(m, t, v)| [u64::from(m), u64::from(t), u64::from(v)]),
                "size": j.size, "state": j.state.name(), "offset": j.offset, "manual": j.manual,
                "force": j.force, "error": j.error, "started": j.started_s,
            })
        });
        json!({
            "ep": self.ep,
            "current": self.current.map(|(m, t, v)| [u64::from(m), u64::from(t), u64::from(v)]),
            "hw": self.hw,
            "policy": self.policy.map(Policy::name),
            "job": job,
            "q": self.last_query_s,
        })
        .to_string()
        .into_bytes()
    }

    /// From [`to_bytes`](Self::to_bytes).
    pub fn from_bytes(b: &[u8], now: Millis) -> Option<DevState> {
        let v: J = serde_json::from_str(std::str::from_utf8(b).ok()?).ok()?;
        let ident = |x: &J| -> Option<Ident> {
            let a = x.as_array()?;
            Some((
                a.first()?.as_u64()? as u16,
                a.get(1)?.as_u64()? as u16,
                a.get(2)?.as_u64()? as u32,
            ))
        };
        let job = v.get("job").filter(|j| j.is_object()).and_then(|j| {
            let mut job = Job::new(
                j.get("target").and_then(ident),
                j["size"].as_u64()? as u32,
                j["manual"].as_bool().unwrap_or(false),
                j["force"].as_bool().unwrap_or(false),
                now,
            );
            job.state = JobState::parse(j["state"].as_str()?)?;
            job.offset = j["offset"].as_u64().unwrap_or(0) as u32;
            job.error = j["error"].as_str().map(str::to_string);
            job.started_s = j["started"].as_u64().unwrap_or(0);
            if job.state == JobState::Installing {
                job.installing_since = now;
            }
            Some(job)
        });
        Some(DevState {
            ep: v["ep"].as_u64().map(|e| e as u8),
            current: v.get("current").and_then(ident),
            hw: v["hw"].as_u64().map(|h| h as u16),
            policy: v["policy"].as_str().and_then(Policy::parse),
            job,
            last_query_s: v["q"].as_u64(),
            ..Default::default()
        })
    }
}

/// The device a command came from or an action is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// Engine device ID.
    pub id: String,
    /// IEEE.
    pub ieee: u64,
    /// Sleepy end device.
    pub sleepy: bool,
}

/// What the caller does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// A cluster command, server to client, default response disabled.
    Send {
        /// Device.
        device: String,
        /// Its OTA client endpoint.
        ep: u8,
        /// TSN of the request this answers (`None`: a new one).
        tsn: Option<u8>,
        /// Command.
        cmd: u8,
        /// Payload.
        payload: Vec<u8>,
    },
    /// Default Response to a client command.
    DefaultRsp {
        /// Device.
        device: String,
        /// Endpoint.
        ep: u8,
        /// Request TSN.
        tsn: u8,
        /// Request command.
        cmd: u8,
        /// Status.
        status: u8,
    },
    /// Read CurrentFileVersion of the device's OTA client.
    ReadVersion {
        /// Device.
        device: String,
        /// Endpoint.
        ep: u8,
    },
    /// A job ended with the device running its target image: what else
    /// identifies the firmware (Basic SWBuildID, DateCode) may have
    /// changed too.
    Upgraded {
        /// Device.
        device: String,
    },
    /// The device's OTA record changed: publish it; `persist` when the
    /// stored form changed too; `structural` when more than progress.
    Changed {
        /// Device.
        device: String,
        /// Persist.
        persist: bool,
        /// Not only progress.
        structural: bool,
    },
}

/// A client command.
#[derive(Debug, Clone)]
pub struct Command<'a> {
    /// Source endpoint.
    pub ep: u8,
    /// TSN.
    pub tsn: u8,
    /// Command ID.
    pub cmd: u8,
    /// Payload.
    pub payload: &'a [u8],
    /// The client wants a Default Response.
    pub default_rsp: bool,
}

/// An Image Page Request being answered: its blocks are read when they
/// fall due, none is made ahead.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Page {
    ep: u8,
    tsn: u8,
    ident: Ident,
    id: u32,
    /// The request: offset, page size, MaxDataSize (repeats are ignored).
    req: (u32, u16, u8),
    /// Next offset and the end of what is served.
    next: u32,
    end: u32,
    block: usize,
    /// ResponseSpacing, at least the MinimumBlockPeriod, ms.
    spacing: u64,
    /// When the next block may leave.
    at: Millis,
    /// Since when a streamed block is awaited.
    waiting: Option<Millis>,
}

/// API errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiErr {
    /// `not_found`, `invalid_value`, `unsupported`, `busy`.
    pub code: &'static str,
    /// Message.
    pub message: String,
}

fn api_err(code: &'static str, message: impl Into<String>) -> ApiErr {
    ApiErr {
        code,
        message: message.into(),
    }
}

/// The server.
#[derive(Debug, Default)]
pub struct Server {
    /// Settings.
    pub settings: Settings,
    /// Per device.
    pub devices: BTreeMap<String, DevState>,
    /// The NCP's largest unfragmented ASDU.
    pub asdu_max: usize,
    /// The devices' own policies are unknown (their stored states are not
    /// the host's yet): `auto` acts as `manual`, nothing starts by itself.
    pub hold_auto: bool,
    /// Paced actions.
    due: Vec<(Millis, Action)>,
    /// Image Page Requests being answered, per device.
    pages: BTreeMap<String, Page>,
    /// When the last block response leaves (global pacing).
    next_slot: Millis,
    last_tick: Millis,
    last_auto: Millis,
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?]))
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

/// Zigbee UTC (s since 2000) of a synchronised wall clock.
pub fn zigbee_utc(now: Millis) -> Option<u32> {
    let s = now / 1000;
    (s > 1_600_000_000).then(|| (s - ZIGBEE_EPOCH) as u32)
}

impl Server {
    /// A server with `settings`.
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            asdu_max: DEFAULT_ASDU,
            ..Default::default()
        }
    }

    /// Largest block the server sends.
    pub fn block_cap(&self) -> usize {
        usize::from(self.settings.block_max)
            .min(self.asdu_max.saturating_sub(BLOCK_OVERHEAD))
            .max(1)
    }

    fn policy_of(&self, id: &str) -> Policy {
        let p = self
            .devices
            .get(id)
            .and_then(|d| d.policy)
            .unwrap_or(self.settings.policy);
        if self.hold_auto && p == Policy::Auto {
            Policy::Manual
        } else {
            p
        }
    }

    /// Policy `off`: the device's running job ends `aborted`. `None`: the
    /// policy is not `off`; else whether a job ended.
    fn off(&mut self, id: &str, now: Millis) -> Option<bool> {
        if self.policy_of(id) != Policy::Off {
            return None;
        }
        let job = self
            .devices
            .get_mut(id)
            .and_then(|d| d.job.as_mut())
            .filter(|j| j.state.active());
        let ended = job.is_some();
        if let Some(j) = job {
            j.state = JobState::Aborted;
            j.error = Some(POLICY_OFF.into());
            j.updated = now;
            self.drop_queued(id);
        }
        Some(ended)
    }

    /// Drops the device's queued block responses and its page.
    fn drop_queued(&mut self, id: &str) {
        self.due.retain(
            |(_, a)| !matches!(a, Action::Send { device, cmd: cmd::IMAGE_BLOCK_RSP, .. } if device == id),
        );
        self.pages.remove(id);
    }

    /// Minutes until the window opens (0 = open), `None` = clock unknown.
    fn window_wait(&self, now: Millis) -> Option<u32> {
        let Some((a, b)) = self.settings.window else {
            return Some(0);
        };
        zigbee_utc(now)?;
        let local = (now / 1000) as i64 + self.settings.tz_offset_s;
        let m = (local.rem_euclid(86_400) / 60) as u16;
        let inside = if a < b {
            (a..b).contains(&m)
        } else {
            m >= a || m < b
        };
        Some(if inside {
            0
        } else {
            u32::from((a + 1440 - m) % 1440)
        })
    }

    /// The best image for a device in state `st`, ignoring policy.
    pub fn candidate(
        &self,
        images: &dyn Images,
        ieee: u64,
        st: &DevState,
        force: bool,
    ) -> Option<Offer> {
        let (m, t, cur) = st.current?;
        images
            .offers()
            .into_iter()
            .filter(|o| {
                let h = &o.header;
                h.manufacturer == m
                    && h.image_type == t
                    && (force || h.file_version > cur)
                    && h.hardware_matches(st.hw)
                    && h.destination.is_none_or(|d| d == ieee)
                    && o.min_file_version.is_none_or(|v| cur >= v)
                    && o.max_file_version.is_none_or(|v| cur <= v)
            })
            .max_by_key(|o| o.header.file_version)
    }

    /// Devices downloading now (not stalled).
    fn active(&self, now: Millis, except: &str) -> usize {
        let stall = u64::from(self.settings.stall_s) * 1000;
        self.devices
            .iter()
            .filter(|(id, d)| {
                id.as_str() != except
                    && d.job.as_ref().is_some_and(|j| {
                        j.state == JobState::Downloading && now.saturating_sub(j.updated) < stall
                    })
            })
            .count()
    }

    fn changed(&self, out: &mut Vec<Action>, id: &str, persist: bool) {
        out.push(Action::Changed {
            device: id.to_string(),
            persist,
            structural: true,
        });
    }

    fn notify(id: &str, ep: u8, target: Option<Ident>) -> Action {
        let payload = match target {
            Some((m, t, v)) => {
                let mut p = vec![0x03, 100];
                p.extend(m.to_le_bytes());
                p.extend(t.to_le_bytes());
                p.extend(v.to_le_bytes());
                p
            }
            None => vec![0x00, 100],
        };
        Action::Send {
            device: id.to_string(),
            ep,
            tsn: None,
            cmd: cmd::IMAGE_NOTIFY,
            payload,
        }
    }

    /// A client command from `dev`.
    pub fn on_command(
        &mut self,
        dev: &Device,
        c: &Command<'_>,
        images: &mut dyn Images,
        now: Millis,
    ) -> Vec<Action> {
        let mut out = Vec::new();
        let st = self.devices.entry(dev.id.clone()).or_default();
        if st.ep != Some(c.ep) {
            st.ep = Some(c.ep);
        }
        match c.cmd {
            cmd::QUERY_NEXT_IMAGE_REQ => self.on_query(dev, c, images, now, &mut out),
            cmd::IMAGE_BLOCK_REQ => self.on_block(dev, c, images, now, &mut out),
            cmd::IMAGE_PAGE_REQ => self.on_page(dev, c, images, now, &mut out),
            cmd::UPGRADE_END_REQ => self.on_end(dev, c, images, now, &mut out),
            cmd::QUERY_SPECIFIC_FILE_REQ => out.push(Action::Send {
                device: dev.id.clone(),
                ep: c.ep,
                tsn: Some(c.tsn),
                cmd: cmd::QUERY_SPECIFIC_FILE_RSP,
                payload: vec![status::NO_IMAGE_AVAILABLE],
            }),
            _ => {
                if c.default_rsp {
                    out.push(default_rsp(dev, c, status::UNSUP_CLUSTER_COMMAND));
                }
            }
        }
        out
    }

    fn record_query(
        &mut self,
        dev: &Device,
        c: &Command<'_>,
        now: Millis,
        out: &mut Vec<Action>,
    ) -> Option<Ident> {
        let p = c.payload;
        let fc = *p.first()?;
        let ident = (u16_at(p, 1)?, u16_at(p, 3)?, u32_at(p, 5)?);
        let hw = if fc & 1 != 0 { u16_at(p, 9) } else { None };
        let st = self.devices.entry(dev.id.clone()).or_default();
        let persist = st.current != Some(ident) || st.hw != hw;
        st.current = Some(ident);
        st.hw = hw;
        st.last_query_s = Some(now / 1000);
        // A device that queries with the version it was upgraded to runs it.
        if let Some(j) = st.job.as_mut()
            && j.state == JobState::Installing
            && let Some(t) = j.target
        {
            if (t.0, t.1) == (ident.0, ident.1) && ident.2 == t.2 {
                j.state = JobState::Done;
                j.updated = now;
                out.push(Action::Upgraded {
                    device: dev.id.clone(),
                });
            } else if now.saturating_sub(j.installing_since) > 120_000 {
                j.state = JobState::Failed;
                j.error = Some(format!("the device still runs {}", hex32(ident.2)));
                j.updated = now;
            }
        }
        let _ = persist;
        Some(ident)
    }

    fn query_rsp(&self, dev: &Device, c: &Command<'_>, offer: Option<(Ident, u32)>) -> Action {
        let payload = match offer {
            Some(((m, t, v), size)) => {
                let mut p = vec![status::SUCCESS];
                p.extend(m.to_le_bytes());
                p.extend(t.to_le_bytes());
                p.extend(v.to_le_bytes());
                p.extend(size.to_le_bytes());
                p
            }
            None => vec![status::NO_IMAGE_AVAILABLE],
        };
        Action::Send {
            device: dev.id.clone(),
            ep: c.ep,
            tsn: Some(c.tsn),
            cmd: cmd::QUERY_NEXT_IMAGE_RSP,
            payload,
        }
    }

    fn on_query(
        &mut self,
        dev: &Device,
        c: &Command<'_>,
        images: &mut dyn Images,
        now: Millis,
        out: &mut Vec<Action>,
    ) {
        if self.record_query(dev, c, now, out).is_none() {
            if c.default_rsp {
                out.push(default_rsp(dev, c, status::MALFORMED_COMMAND));
            }
            return;
        }
        if self.off(&dev.id, now).is_some() {
            out.push(self.query_rsp(dev, c, None));
            self.changed(out, &dev.id, true);
            return;
        }
        let st = self.devices[&dev.id].clone();
        let policy = self.policy_of(&dev.id);
        let job = st.job.clone().filter(|j| {
            matches!(
                j.state,
                JobState::Waiting | JobState::Queued | JobState::Downloading
            )
        });
        let force = job.as_ref().is_some_and(|j| j.force);
        let img = match job.as_ref().and_then(|j| j.target) {
            Some(ident) => images.find(ident).filter(|o| {
                let cur = st.current.map_or(0, |c| c.2);
                let h = &o.header;
                (h.manufacturer, h.image_type) == (st.current.unwrap().0, st.current.unwrap().1)
                    && (force || h.file_version > cur)
                    && h.hardware_matches(st.hw)
            }),
            None => self.candidate(images, dev.ieee, &st, force),
        }
        .map(|o| (o.ident(), o.size));
        let approved = job.is_some()
            || (policy == Policy::Auto && img.is_some() && self.window_wait(now) == Some(0));
        let full = self.active(now, &dev.id) >= usize::from(self.settings.max_concurrent)
            && job
                .as_ref()
                .is_none_or(|j| j.state != JobState::Downloading);
        let offer = match img {
            Some(o) if approved => {
                let d = self.devices.get_mut(&dev.id).unwrap();
                if job.is_none() {
                    d.job = Some(Job::new(Some(o.0), o.1, false, false, now));
                }
                let j = d.job.as_mut().unwrap();
                if j.target != Some(o.0) {
                    j.offset = 0;
                }
                j.target = Some(o.0);
                j.size = o.1;
                j.updated = now;
                j.error = None;
                if full {
                    j.state = JobState::Queued;
                    None
                } else {
                    j.state = JobState::Downloading;
                    Some(o)
                }
            }
            None if job.is_some() => {
                // Approved, but nothing for this device (any more).
                let j = self.devices.get_mut(&dev.id).unwrap().job.as_mut().unwrap();
                j.state = JobState::Failed;
                j.error = Some(match st.current {
                    Some((m, t, v)) => format!(
                        "no image for manufacturer 0x{m:04X}, image type 0x{t:04X} newer than {}",
                        hex32(v)
                    ),
                    None => "no image for this device".into(),
                });
                j.updated = now;
                None
            }
            _ => None,
        };
        out.push(self.query_rsp(dev, c, offer));
        self.changed(out, &dev.id, true);
    }

    fn on_block(
        &mut self,
        dev: &Device,
        c: &Command<'_>,
        images: &mut dyn Images,
        now: Millis,
        out: &mut Vec<Action>,
    ) {
        let p = c.payload;
        let parsed = (|| {
            let fc = *p.first()?;
            let ident = (u16_at(p, 1)?, u16_at(p, 3)?, u32_at(p, 5)?);
            let offset = u32_at(p, 9)?;
            let max = *p.get(13)?;
            let mut at = 14;
            if fc & 1 != 0 {
                at += 8;
            }
            let period = if fc & 2 != 0 { u16_at(p, at) } else { None };
            Some((ident, offset, max, period))
        })();
        let Some((ident, offset, max, period)) = parsed else {
            if c.default_rsp {
                out.push(default_rsp(dev, c, status::MALFORMED_COMMAND));
            }
            return;
        };
        let Some((id, size)) = self.admit(dev, ident, images, now, out) else {
            out.push(block_status(dev, c, status::ABORT));
            return;
        };
        let n = usize::from(max)
            .min(self.block_cap())
            .min(size.saturating_sub(offset) as usize);
        if n == 0 {
            if c.default_rsp || offset >= size {
                out.push(default_rsp(dev, c, status::MALFORMED_COMMAND));
            }
            return;
        }
        // Outside the window an automatic download waits for it.
        let auto = self.devices[&dev.id]
            .job
            .as_ref()
            .is_some_and(|j| !j.manual);
        if auto && let Some(w) = self.window_wait(now).filter(|w| *w > 0) {
            out.push(wait_for_data(
                dev,
                c,
                now,
                w * 60,
                self.settings.min_block_period_ms,
            ));
            return;
        }
        // Tell the per-device period once.
        let want = self.settings.min_block_period_ms;
        let job = self.devices.get_mut(&dev.id).unwrap().job.as_mut().unwrap();
        if want > 0 && !job.period_told && period.is_none_or(|p| p < want) {
            job.period_told = true;
            out.push(wait_for_data(dev, c, now, 0, want));
            return;
        }
        let rsp = match self.block_rsp(&dev.id, c.ep, c.tsn, ident, id, offset, n, images, now) {
            Ok(rsp) => rsp,
            // Streamed and not here yet: the client was asked for it.
            Err(Fetch::Wait) => {
                out.push(wait_for_data(dev, c, now, STREAM_WAIT_S, want));
                return;
            }
            Err(_) => {
                out.push(block_status(dev, c, status::ABORT));
                return;
            }
        };
        let at = self.pace(&dev.id, now);
        if at.saturating_sub(now) > MAX_HOLD_MS {
            let s = (at - now).div_ceil(1000) as u32;
            out.push(wait_for_data(dev, c, now, s, want));
            return;
        }
        self.progress(&dev.id, offset + n as u32, now, out);
        // A repeated or newer request supersedes what is still queued.
        self.drop_queued(&dev.id);
        if at > now {
            self.due.push((at, rsp));
        } else {
            out.push(rsp);
        }
    }

    fn on_page(
        &mut self,
        dev: &Device,
        c: &Command<'_>,
        images: &mut dyn Images,
        now: Millis,
        out: &mut Vec<Action>,
    ) {
        let p = c.payload;
        let parsed = (|| {
            Some((
                (u16_at(p, 1)?, u16_at(p, 3)?, u32_at(p, 5)?),
                u32_at(p, 9)?,
                *p.get(13)?,
                u16_at(p, 14)?,
                u16_at(p, 16)?,
            ))
        })();
        let Some((ident, offset, max, page, spacing)) = parsed else {
            if c.default_rsp {
                out.push(default_rsp(dev, c, status::MALFORMED_COMMAND));
            }
            return;
        };
        let Some((id, size)) = self.admit(dev, ident, images, now, out) else {
            out.push(block_status(dev, c, status::ABORT));
            return;
        };
        let n = usize::from(max).min(self.block_cap());
        if n == 0 || page == 0 || offset >= size {
            if c.default_rsp {
                out.push(default_rsp(dev, c, status::MALFORMED_COMMAND));
            }
            return;
        }
        // The same request again while its page is being sent (a client's
        // retry): the page goes on.
        if self
            .pages
            .get(&dev.id)
            .is_some_and(|pg| pg.ident == ident && pg.req == (offset, page, max))
        {
            return;
        }
        let want = self.settings.min_block_period_ms;
        let auto = self.devices[&dev.id]
            .job
            .as_ref()
            .is_some_and(|j| !j.manual);
        if auto && let Some(w) = self.window_wait(now).filter(|w| *w > 0) {
            self.drop_queued(&dev.id);
            out.push(wait_for_data(dev, c, now, w * 60, want));
            return;
        }
        let end = offset
            .saturating_add(u32::from(page).min(MAX_PAGE))
            .min(size)
            .min(offset.saturating_add(n as u32 * MAX_PAGE_BLOCKS));
        // A new page supersedes the device's queued responses.
        self.drop_queued(&dev.id);
        // The first block like a block request's; the rest as they fall due.
        let len = (n as u32).min(end - offset) as usize;
        let rsp = match self.block_rsp(&dev.id, c.ep, c.tsn, ident, id, offset, len, images, now) {
            Ok(rsp) => rsp,
            Err(Fetch::Wait) => {
                out.push(wait_for_data(dev, c, now, STREAM_WAIT_S, want));
                return;
            }
            Err(_) => {
                out.push(block_status(dev, c, status::ABORT));
                return;
            }
        };
        let at = self.pace(&dev.id, now);
        if at.saturating_sub(now) > MAX_HOLD_MS {
            let s = (at - now).div_ceil(1000) as u32;
            out.push(wait_for_data(dev, c, now, s, want));
            return;
        }
        self.progress(&dev.id, offset + len as u32, now, out);
        if at > now {
            self.due.push((at, rsp));
        } else {
            out.push(rsp);
        }
        let next = offset + len as u32;
        if next < end {
            let spacing = u64::from(spacing).max(u64::from(want));
            self.pages.insert(
                dev.id.clone(),
                Page {
                    ep: c.ep,
                    tsn: c.tsn,
                    ident,
                    id,
                    req: (offset, page, max),
                    next,
                    end,
                    block: n,
                    spacing,
                    at: at + spacing,
                    waiting: None,
                },
            );
        }
    }

    /// The next blocks of the pages being sent, as they fall due: each
    /// takes the next pacing slot (shared with the other devices), and the
    /// one after it is read once this one has left.
    fn pump_pages(&mut self, images: &mut dyn Images, now: Millis, out: &mut Vec<Action>) {
        let due: Vec<String> = self
            .pages
            .iter()
            .filter(|(_, pg)| pg.at <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            let Some(mut pg) = self.pages.remove(&id) else {
                continue;
            };
            let downloading = self
                .devices
                .get(&id)
                .and_then(|d| d.job.as_ref())
                .is_some_and(|j| j.state == JobState::Downloading && j.target == Some(pg.ident));
            if !downloading {
                continue;
            }
            let len = (pg.block as u32).min(pg.end - pg.next) as usize;
            match self.block_rsp(
                &id, pg.ep, pg.tsn, pg.ident, pg.id, pg.next, len, images, now,
            ) {
                Ok(rsp) => {
                    let at = self.pace(&id, now);
                    if at > now {
                        self.due.push((at, rsp));
                    } else {
                        out.push(rsp);
                    }
                    pg.next += len as u32;
                    pg.waiting = None;
                    self.progress(&id, pg.next, now, out);
                    if pg.next < pg.end {
                        pg.at = at + pg.spacing;
                        self.pages.insert(id, pg);
                    }
                }
                // The stream was asked for it: try again shortly, then
                // leave the rest to the device's next request.
                Err(Fetch::Wait) => {
                    let since = *pg.waiting.get_or_insert(now);
                    if now.saturating_sub(since) < PAGE_WAIT_MS {
                        pg.at = now + 100;
                        self.pages.insert(id, pg);
                    }
                }
                Err(_) => {}
            }
        }
    }

    /// The image a block or page request may get (ID, size), creating or
    /// resuming the job; `None` = ABORT.
    fn admit(
        &mut self,
        dev: &Device,
        ident: Ident,
        images: &dyn Images,
        now: Millis,
        out: &mut Vec<Action>,
    ) -> Option<(u32, u32)> {
        if let Some(ended) = self.off(&dev.id, now) {
            if ended {
                self.changed(out, &dev.id, true);
            }
            return None;
        }
        let img = images.find(ident)?;
        let id = img.id;
        let size = img.size;
        let ok_dest = img.header.destination.is_none_or(|d| d == dev.ieee);
        let policy = self.policy_of(&dev.id);
        let in_window = self.window_wait(now) == Some(0);
        let st = self.devices.get_mut(&dev.id)?;
        if !ok_dest {
            return None;
        }
        match st.job.as_mut() {
            Some(j)
                if j.target == Some(ident)
                    && matches!(
                        j.state,
                        JobState::Waiting | JobState::Queued | JobState::Downloading
                    ) =>
            {
                if j.state != JobState::Downloading {
                    j.state = JobState::Downloading;
                    out.push(Action::Changed {
                        device: dev.id.clone(),
                        persist: true,
                        structural: true,
                    });
                }
                j.updated = now;
                Some((id, size))
            }
            Some(j) if j.target == Some(ident) && j.state == JobState::Aborted => None,
            // The device resumes a download the hub no longer approves of
            // only under `auto`.
            _ if policy == Policy::Auto && in_window => {
                let mut j = Job::new(Some(ident), size, false, false, now);
                j.state = JobState::Downloading;
                st.job = Some(j);
                out.push(Action::Changed {
                    device: dev.id.clone(),
                    persist: true,
                    structural: true,
                });
                Some((id, size))
            }
            _ => None,
        }
    }

    /// When the next block response to `id` may leave.
    fn pace(&mut self, id: &str, now: Millis) -> Millis {
        let mut at = now;
        if self.settings.blocks_per_s > 0 {
            at = at.max(self.next_slot);
            self.next_slot = at + 1000 / u64::from(self.settings.blocks_per_s);
        }
        let Some(d) = self.devices.get_mut(id) else {
            return at;
        };
        if self.settings.min_block_period_ms > 0 && d.last_block > 0 {
            at = at.max(d.last_block + u64::from(self.settings.min_block_period_ms));
        }
        d.last_block = at;
        at
    }

    #[allow(clippy::too_many_arguments)]
    fn block_rsp(
        &self,
        device: &str,
        ep: u8,
        tsn: u8,
        ident: Ident,
        id: u32,
        offset: u32,
        n: usize,
        images: &mut dyn Images,
        now: Millis,
    ) -> Result<Action, Fetch> {
        let mut p = Vec::with_capacity(14 + n);
        p.push(status::SUCCESS);
        p.extend(ident.0.to_le_bytes());
        p.extend(ident.1.to_le_bytes());
        p.extend(ident.2.to_le_bytes());
        p.extend(offset.to_le_bytes());
        p.push(n as u8);
        let at = p.len();
        p.resize(at + n, 0);
        match images.read(id, offset, &mut p[at..], now) {
            Fetch::Ready => {}
            f => return Err(f),
        }
        Ok(Action::Send {
            device: device.to_string(),
            ep,
            tsn: Some(tsn),
            cmd: cmd::IMAGE_BLOCK_RSP,
            payload: p,
        })
    }

    /// Records download progress; publishes at most every percent or 5 s
    /// and persists every 10 %.
    fn progress(&mut self, id: &str, reached: u32, now: Millis, out: &mut Vec<Action>) {
        let Some(d) = self.devices.get_mut(id) else {
            return;
        };
        let Some(j) = d.job.as_mut() else { return };
        j.offset = j.offset.max(reached).min(j.size);
        j.updated = now;
        let pct = percent(j.offset, j.size);
        let (t, last) = d.published;
        if pct != last || now.saturating_sub(t) >= 5_000 {
            let persist = j.offset >= d.persisted_at + j.size / 10 || j.offset == j.size;
            if persist {
                d.persisted_at = j.offset;
            }
            d.published = (now, pct);
            out.push(Action::Changed {
                device: id.to_string(),
                persist,
                structural: false,
            });
        }
    }

    fn on_end(
        &mut self,
        dev: &Device,
        c: &Command<'_>,
        images: &mut dyn Images,
        now: Millis,
        out: &mut Vec<Action>,
    ) {
        let p = c.payload;
        let parsed = (|| Some((*p.first()?, (u16_at(p, 1)?, u16_at(p, 3)?, u32_at(p, 5)?))))();
        let Some((st_code, ident)) = parsed else {
            if c.default_rsp {
                out.push(default_rsp(dev, c, status::MALFORMED_COMMAND));
            }
            return;
        };
        let known = images.find(ident).is_some();
        let d = self.devices.entry(dev.id.clone()).or_default();
        let job = d
            .job
            .as_mut()
            .filter(|j| j.target == Some(ident) && j.state.active());
        if st_code != status::SUCCESS {
            if let Some(j) = job {
                j.state = JobState::Failed;
                j.error = Some(match st_code {
                    status::INVALID_IMAGE => "the device rejected the image (INVALID_IMAGE)".into(),
                    status::REQUIRE_MORE_IMAGE => {
                        "the device requires more images (REQUIRE_MORE_IMAGE)".into()
                    }
                    status::ABORT => "the device aborted the download".into(),
                    s => format!("the device ended the download with status 0x{s:02X}"),
                });
                j.updated = now;
            }
            self.drop_queued(&dev.id);
            if c.default_rsp {
                out.push(default_rsp(dev, c, status::SUCCESS));
            }
            self.changed(out, &dev.id, true);
            self.notify_queued(now, out);
            return;
        }
        // The go-ahead needs a job that approves this image: a running one,
        // or `auto` inside the window. An aborted job, the `off` policy or
        // no approval: Default Response ABORT, the cluster's "do not
        // upgrade" (sent even when the client disabled default responses:
        // an error status).
        let running = job.is_some();
        let aborted = d
            .job
            .as_ref()
            .is_some_and(|j| j.target == Some(ident) && j.state == JobState::Aborted);
        let policy = self.policy_of(&dev.id);
        let auto = policy == Policy::Auto && known && self.window_wait(now) == Some(0);
        if let Some(ended) = self.off(&dev.id, now) {
            out.push(default_rsp(dev, c, status::ABORT));
            if ended {
                self.changed(out, &dev.id, true);
                self.notify_queued(now, out);
            }
            return;
        }
        if aborted || !(running || auto) {
            out.push(default_rsp(dev, c, status::ABORT));
            return;
        }
        self.drop_queued(&dev.id);
        let d = self.devices.get_mut(&dev.id).unwrap();
        if !running {
            d.job = Some(Job::new(Some(ident), 0, false, false, now));
        }
        let j = d.job.as_mut().unwrap();
        j.offset = j.size;
        j.state = JobState::Installing;
        j.installing_since = now;
        j.poked = now;
        j.updated = now;
        let delay = self.settings.upgrade_delay_s;
        let (cur, at) = match (delay, zigbee_utc(now)) {
            (0, _) => (0, 0),
            (d, Some(utc)) => (utc, utc + d),
            (d, None) => (0, d),
        };
        let mut payload = Vec::with_capacity(16);
        payload.extend(ident.0.to_le_bytes());
        payload.extend(ident.1.to_le_bytes());
        payload.extend(ident.2.to_le_bytes());
        payload.extend(cur.to_le_bytes());
        payload.extend(at.to_le_bytes());
        out.push(Action::Send {
            device: dev.id.clone(),
            ep: c.ep,
            tsn: Some(c.tsn),
            cmd: cmd::UPGRADE_END_RSP,
            payload,
        });
        self.changed(out, &dev.id, true);
        self.notify_queued(now, out);
    }

    /// Notify queued devices while download slots are free.
    fn notify_queued(&mut self, now: Millis, out: &mut Vec<Action>) {
        let mut free =
            usize::from(self.settings.max_concurrent).saturating_sub(self.active(now, ""));
        for (id, d) in self.devices.iter_mut() {
            if free == 0 {
                break;
            }
            let Some(ep) = d.ep else { continue };
            if let Some(j) = d
                .job
                .as_mut()
                .filter(|j| j.state == JobState::Queued && now.saturating_sub(j.poked) >= 30_000)
            {
                j.poked = now;
                j.notified = j.notified.saturating_add(1);
                let target = j.target;
                out.push(Self::notify(id, ep, target));
                free -= 1;
            }
        }
    }

    /// CurrentFileVersion read from the device.
    pub fn on_version(&mut self, id: &str, version: u32, now: Millis) -> Vec<Action> {
        let mut out = Vec::new();
        let Some(d) = self.devices.get_mut(id) else {
            return out;
        };
        let mut persist = false;
        if let Some(c) = d.current.as_mut()
            && c.2 != version
        {
            c.2 = version;
            persist = true;
        }
        if let Some(j) = d.job.as_mut()
            && j.state == JobState::Installing
            && j.target.is_some_and(|t| t.2 == version)
        {
            j.state = JobState::Done;
            j.updated = now;
            persist = true;
            out.push(Action::Upgraded {
                device: id.to_string(),
            });
        }
        if persist {
            self.changed(&mut out, id, true);
        }
        out
    }

    /// `ota/start`: approve an upgrade of `dev` (OTA client `ep`) to
    /// `image` (a store image or a stream) or the best image for it, and
    /// notify it.
    pub fn start(
        &mut self,
        dev: &Device,
        ep: Option<u8>,
        image: Option<u32>,
        force: bool,
        images: &dyn Images,
        now: Millis,
    ) -> Result<Vec<Action>, ApiErr> {
        let st = self.devices.entry(dev.id.clone()).or_default();
        if let Some(e) = ep {
            st.ep.get_or_insert(e);
        }
        let ep = st.ep.ok_or_else(|| {
            api_err(
                "unsupported",
                "the device has no OTA client (cluster 0x0019)",
            )
        })?;
        if self.policy_of(&dev.id) == Policy::Off {
            return Err(api_err("invalid_value", POLICY_OFF));
        }
        let st = self.devices.get_mut(&dev.id).unwrap();
        if st
            .job
            .as_ref()
            .is_some_and(|j| matches!(j.state, JobState::Downloading | JobState::Installing))
            && !force
        {
            return Err(api_err("busy", "an upgrade of this device is running"));
        }
        let st = st.clone();
        let (target, size) = match image {
            Some(i) => {
                let img = images
                    .get(i)
                    .ok_or_else(|| api_err("not_found", format!("no image {i}")))?;
                let h = &img.header;
                if let Some((m, t, v)) = st.current {
                    if (h.manufacturer, h.image_type) != (m, t) {
                        return Err(api_err(
                            "invalid_value",
                            format!(
                                "image {i} is for manufacturer 0x{:04X} type 0x{:04X}, the device reports 0x{m:04X}/0x{t:04X}",
                                h.manufacturer, h.image_type
                            ),
                        ));
                    }
                    if h.file_version <= v && !force {
                        return Err(api_err(
                            "invalid_value",
                            format!(
                                "the device runs {} already (force to install {})",
                                hex32(v),
                                hex32(h.file_version)
                            ),
                        ));
                    }
                }
                if !h.hardware_matches(st.hw) {
                    return Err(api_err(
                        "invalid_value",
                        "the image's hardware version range excludes the device",
                    ));
                }
                (Some(img.ident()), img.size)
            }
            None if st.current.is_some() => {
                let img = self
                    .candidate(images, dev.ieee, &st, force)
                    .ok_or_else(|| api_err("not_found", "no update for this device"))?;
                (Some(img.ident()), img.size)
            }
            // Identity unknown: the device's query decides.
            None => (None, 0),
        };
        let mut job = Job::new(target, size, true, force, now);
        job.notified = 1;
        job.poked = now;
        let mut out = vec![Self::notify(&dev.id, ep, target)];
        let d = self.devices.get_mut(&dev.id).unwrap();
        d.job = Some(job);
        d.persisted_at = 0;
        d.published = (0, u32::MAX);
        self.changed(&mut out, &dev.id, true);
        Ok(out)
    }

    /// `ota/abort`.
    pub fn abort(&mut self, id: &str, now: Millis) -> Result<Vec<Action>, ApiErr> {
        let j = self
            .devices
            .get_mut(id)
            .and_then(|d| d.job.as_mut())
            .filter(|j| j.state.active())
            .ok_or_else(|| api_err("not_found", "no upgrade running"))?;
        j.state = JobState::Aborted;
        j.error = Some("aborted".into());
        j.updated = now;
        self.pages.remove(id);
        self.due
            .retain(|(_, a)| !matches!(a, Action::Send { device, .. } if device == id));
        let mut out = Vec::new();
        self.changed(&mut out, id, true);
        self.notify_queued(now, &mut out);
        Ok(out)
    }

    /// A streamed image is gone for good (its client did not come back):
    /// the jobs downloading it end `aborted` (the devices got ABORT; they
    /// keep what they have for a later offer).
    pub fn image_gone(&mut self, ident: Ident, reason: &str, now: Millis) -> Vec<Action> {
        let mut out = Vec::new();
        let ids: Vec<String> = self
            .devices
            .iter_mut()
            .filter_map(|(id, d)| {
                let j = d.job.as_mut()?;
                (j.target == Some(ident)
                    && matches!(
                        j.state,
                        JobState::Waiting | JobState::Queued | JobState::Downloading
                    ))
                .then(|| {
                    j.state = JobState::Aborted;
                    j.error = Some(reason.into());
                    j.updated = now;
                    id.clone()
                })
            })
            .collect();
        for id in &ids {
            self.pages.remove(id);
            self.due
                .retain(|(_, a)| !matches!(a, Action::Send { device, .. } if device == id));
            self.changed(&mut out, id, true);
        }
        if !ids.is_empty() {
            self.notify_queued(now, &mut out);
        }
        out
    }

    /// `ota/devices/check`: make the device query (Image Notify) and read
    /// its CurrentFileVersion.
    pub fn check(&mut self, dev: &Device, ep: Option<u8>) -> Result<Vec<Action>, ApiErr> {
        let st = self.devices.entry(dev.id.clone()).or_default();
        if let Some(e) = ep {
            st.ep.get_or_insert(e);
        }
        let ep = st.ep.ok_or_else(|| {
            api_err(
                "unsupported",
                "the device has no OTA client (cluster 0x0019)",
            )
        })?;
        Ok(vec![
            Self::notify(&dev.id, ep, None),
            Action::ReadVersion {
                device: dev.id.clone(),
                ep,
            },
        ])
    }

    /// Responses made and queued ahead, and pages being sent (tests).
    #[cfg(all(test, feature = "store"))]
    pub(crate) fn queued(&self) -> (usize, usize) {
        (self.due.len(), self.pages.len())
    }

    /// The next paced action's time (a queued response or a page's next
    /// block).
    pub fn next_due(&self) -> Option<Millis> {
        let pages = self.pages.values().map(|pg| pg.at);
        self.due.iter().map(|(t, _)| *t).chain(pages).min()
    }

    /// Paced actions due, the next blocks of pages, and once a second the
    /// housekeeping: stalled downloads, queued and waiting devices, version
    /// reads of installing devices, the `off` and `auto` policies.
    /// `ieee` gives a device's IEEE (`None`: not a Zigbee device any more).
    pub fn poll(
        &mut self,
        images: &mut dyn Images,
        ieee: &dyn Fn(&str) -> Option<u64>,
        now: Millis,
    ) -> Vec<Action> {
        let mut out = Vec::new();
        if self.due.iter().any(|(t, _)| *t <= now) {
            let (ready, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.due)
                .into_iter()
                .partition(|(t, _)| *t <= now);
            self.due = later;
            out.extend(ready.into_iter().map(|(_, a)| a));
        }
        if self.pages.values().any(|pg| pg.at <= now) {
            self.pump_pages(images, now, &mut out);
        }
        if now.saturating_sub(self.last_tick) < 1000 {
            return out;
        }
        self.last_tick = now;
        let stall = u64::from(self.settings.stall_s) * 1000;
        let window_open = self.window_wait(now) == Some(0);
        let global = self.settings.policy;
        let mut changed = Vec::new();
        let mut ended = Vec::new();
        for (id, d) in self.devices.iter_mut() {
            let Some(ep) = d.ep else { continue };
            let off = d.policy.unwrap_or(global) == Policy::Off;
            let Some(j) = d.job.as_mut() else { continue };
            match j.state {
                // The policy was set to `off` while the job ran.
                s if off && s.active() => {
                    j.state = JobState::Aborted;
                    j.error = Some(POLICY_OFF.into());
                    j.updated = now;
                    ended.push(id.clone());
                }
                JobState::Downloading
                    if now.saturating_sub(j.updated) >= stall && j.error.is_none() =>
                {
                    j.error = Some("stalled: no block request".into());
                    changed.push(id.clone());
                }
                JobState::Downloading
                    if now.saturating_sub(j.updated) < stall && j.error.is_some() =>
                {
                    j.error = None;
                    changed.push(id.clone());
                }
                // Re-notify a waiting device every 5 minutes, 4 times
                // (sleepy devices get it queued until they wake).
                JobState::Waiting if j.notified < 4 && now.saturating_sub(j.poked) >= 300_000 => {
                    j.notified += 1;
                    j.poked = now;
                    let target = j.target;
                    out.push(Self::notify(id, ep, target));
                }
                JobState::Installing => {
                    let since = now.saturating_sub(j.installing_since);
                    if since > 1_800_000 {
                        j.state = JobState::Failed;
                        j.error =
                            Some("the new version was not confirmed within 30 minutes".into());
                        changed.push(id.clone());
                    } else if since >= 30_000 && now.saturating_sub(j.poked) >= 60_000 {
                        j.poked = now;
                        out.push(Action::ReadVersion {
                            device: id.clone(),
                            ep,
                        });
                    }
                }
                _ => {}
            }
        }
        for id in &ended {
            self.drop_queued(id);
        }
        for id in changed.into_iter().chain(ended) {
            self.changed(&mut out, &id, true);
        }
        self.notify_queued(now, &mut out);
        // `auto`: offer available updates inside the window, within the
        // concurrency limit, once a minute.
        if window_open && now.saturating_sub(self.last_auto) >= 60_000 {
            self.last_auto = now;
            let mut free = usize::from(self.settings.max_concurrent)
                .saturating_sub(self.active(now, ""))
                .saturating_sub(
                    self.devices
                        .values()
                        .filter(|d| {
                            d.job.as_ref().is_some_and(|j| {
                                matches!(j.state, JobState::Waiting | JobState::Queued)
                            })
                        })
                        .count(),
                );
            let ids: Vec<String> = self.devices.keys().cloned().collect();
            for id in ids {
                if free == 0 {
                    break;
                }
                if self.policy_of(&id) != Policy::Auto {
                    continue;
                }
                let d = &self.devices[&id];
                if d.job.as_ref().is_some_and(|j| {
                    j.state.active()
                        || (j.state == JobState::Failed
                            && now.saturating_sub(j.updated) < 3_600_000)
                }) {
                    continue;
                }
                let Some(ieee) = ieee(&id) else { continue };
                let Some(ep) = d.ep else { continue };
                let Some(img) = self.candidate(images, ieee, d, false) else {
                    continue;
                };
                let target = Some(img.ident());
                let mut job = Job::new(target, img.size, false, false, now);
                job.notified = 1;
                job.poked = now;
                let n = Self::notify(&id, ep, target);
                self.devices.get_mut(&id).unwrap().job = Some(job);
                out.push(n);
                self.changed(&mut out, &id, true);
                free -= 1;
            }
        }
        out
    }

    /// The API record of a device (`ota/devices/list`, events) as JSON
    /// text, `extra` written into it.
    pub fn record(
        &self,
        id: &str,
        ieee: u64,
        images: &dyn Images,
        now: Millis,
        extra: impl FnOnce(&mut Obj),
    ) -> String {
        let d = self.devices.get(id).cloned().unwrap_or_default();
        let available = self
            .candidate(images, ieee, &d, false)
            .map(|o| image_brief(&o));
        let stall = u64::from(self.settings.stall_s) * 1000;
        let job = d.job.as_ref().map(|j| {
            let mut o = Obj::new();
            o.str("state", j.state.name())
                .opt("file_version", j.target.map(|t| t.2))
                .opt("image", j.target.and_then(|t| images.find(t)).map(|o| o.id))
                .num("offset", j.offset)
                .num("size", j.size)
                .num("progress", percent(j.offset, j.size))
                .bool("manual", j.manual)
                .ostr("error", j.error.as_deref())
                .num("started", j.started_s)
                .bool(
                    "stalled",
                    j.state == JobState::Downloading && now.saturating_sub(j.updated) >= stall,
                );
            o.end()
        });
        let mut o = Obj::new();
        o.str("device", id)
            .opt("ep", d.ep)
            .opt("manufacturer_code", d.current.map(|c| c.0))
            .opt("image_type", d.current.map(|c| c.1))
            .opt("current_version", d.current.map(|c| c.2))
            .opt("hw_version", d.hw)
            .ostr("policy", d.policy.map(Policy::name))
            .str("effective_policy", self.policy_of(id).name())
            .raw("available", available.as_deref().unwrap_or("null"))
            .raw("job", job.as_deref().unwrap_or("null"))
            .opt("last_query", d.last_query_s);
        extra(&mut o);
        o.end()
    }
}

/// Percent of `of` done (0–100).
pub fn percent(done: u32, of: u32) -> u32 {
    if of == 0 {
        0
    } else {
        (u64::from(done.min(of)) * 100 / u64::from(of)) as u32
    }
}

/// A short image description (JSON text).
pub fn image_brief(i: &Offer) -> String {
    let mut o = Obj::new();
    o.num("image", i.id)
        .num("file_version", i.header.file_version)
        .num("size", i.size)
        .str("label", &i.header.label())
        .str("name", &i.name)
        .bool("streamed", i.streamed);
    o.end()
}

fn default_rsp(dev: &Device, c: &Command<'_>, status: u8) -> Action {
    Action::DefaultRsp {
        device: dev.id.clone(),
        ep: c.ep,
        tsn: c.tsn,
        cmd: c.cmd,
        status,
    }
}

fn block_status(dev: &Device, c: &Command<'_>, status: u8) -> Action {
    Action::Send {
        device: dev.id.clone(),
        ep: c.ep,
        tsn: Some(c.tsn),
        cmd: cmd::IMAGE_BLOCK_RSP,
        payload: vec![status],
    }
}

/// WAIT_FOR_DATA: ask again in `secs` s, at most every `period_ms`.
fn wait_for_data(dev: &Device, c: &Command<'_>, now: Millis, secs: u32, period_ms: u16) -> Action {
    let cur = zigbee_utc(now).unwrap_or(0);
    let mut p = vec![status::WAIT_FOR_DATA];
    p.extend(cur.to_le_bytes());
    p.extend(cur.wrapping_add(secs).to_le_bytes());
    p.extend(period_ms.to_le_bytes());
    Action::Send {
        device: dev.id.clone(),
        ep: c.ep,
        tsn: Some(c.tsn),
        cmd: cmd::IMAGE_BLOCK_RSP,
        payload: p,
    }
}
