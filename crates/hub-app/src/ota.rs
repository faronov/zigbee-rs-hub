//! The OTA Upgrade server (cluster 0x0019, `hub-ota`) on the app thread:
//! client commands the engine hands over ([`Engine::take_app_frames`]),
//! the per-device state in the host store journal (`ota/d/<device>`,
//! settings `ota/cfg`), the `ota/*` requests and the `ota` topic
//! (device-support 14.7).
//!
//! Images come from two sources:
//!
//! - **streams** (always): a connected client (Home Assistant, a browser
//!   tab, `hubctl ota-stream`) offers an image and serves its bytes on
//!   demand (`ota/block_needed` → `ota/block_data`); the hub keeps an 8 KiB
//!   window per stream. The 4 MB layout has nothing else;
//! - the **store** (feature `ota-store`, [`crate::ota_store`]): uploaded
//!   images in the inactive app slot of the 8 MB layout.
//!
//! The hub downloads nothing itself.

use std::collections::BTreeSet;

use hub_devices::device::{Device as EngineDevice, Millis};
use hub_devices::hub::{ApiError, Hub as Engine};
use hub_devices::transport::ZigbeeTransport;
use hub_devices::zcl::frame::global;
use hub_devices::zcl::{Direction, FrameType, ZclFrame, ZclHeader, ZclValue};
use hub_ncp::Hub as Ncp;
use hub_ota::images::{Images, Offer, Sources, View};
use hub_ota::json::{Obj, array, try_array};
use hub_ota::server::{Action, CLUSTER, Command, DEFAULT_ASDU, attr};
use hub_ota::stream::{CHUNK as STREAM_CHUNK, Streams};
use hub_ota::{Device, Policy, Server, Settings};
use log::{info, warn};
use serde_json::{Value as J, json};

/// Journal key of the settings.
pub const SETTINGS_KEY: &str = "ota/cfg";
/// Journal key prefix of the device states.
pub const DEV_PREFIX: &str = "ota/d/";

/// Request types served here (the store's answer `unsupported` without
/// the store).
pub const TYPES: &[&str] = &[
    "ota/status",
    "ota/settings/set",
    "ota/images/list",
    "ota/images/delete",
    "ota/upload/begin",
    "ota/upload/chunk",
    "ota/upload/commit",
    "ota/upload/abort",
    "ota/store/format",
    "ota/stream/offer",
    "ota/stream/close",
    "ota/block_data",
    "ota/devices/list",
    "ota/devices/check",
    "ota/devices/policy",
    "ota/start",
    "ota/abort",
];

/// The request's `type` if it is one of [`TYPES`].
pub fn head(text: &str) -> Option<(u64, String, J)> {
    if !text.contains("\"ota/") {
        return None;
    }
    let v: J = serde_json::from_str(text).ok()?;
    let ty = v.get("type")?.as_str().filter(|t| TYPES.contains(t))?.to_string();
    Some((v.get("id")?.as_u64()?, ty, v))
}

pub(crate) fn err(code: &str, m: impl Into<String>) -> ApiError {
    ApiError::new(code, m)
}

/// The device's OTA client endpoint (client cluster 0x0019).
pub fn ota_ep(d: &EngineDevice) -> Option<u8> {
    d.fp.endpoints.iter().find(|e| e.out_clusters.contains(&CLUSTER)).map(|e| e.ep)
}

/// A request's result: a JSON value, or JSON text (lists, written without
/// a `Value` tree).
pub enum Reply {
    /// Value.
    Json(J),
    /// Text.
    Text(String),
}

impl Reply {
    /// The result frame of request `id`.
    pub fn frame(self, id: u64) -> String {
        match self {
            Reply::Json(v) => hub_devices::api::ServerMessage::ok(id, v).to_text(),
            Reply::Text(t) => format!("{{\"id\":{id},\"type\":\"result\",\"success\":true,\"result\":{t}}}"),
        }
    }
}

/// An offer as an image-list entry (JSON text).
pub(crate) fn offer_text(o: &Offer, devices: &[String]) -> String {
    let h = &o.header;
    let mut x = Obj::new();
    x.num("image", o.id)
        .str("state", if o.streamed { "streamed" } else { "ready" })
        .str("name", &o.name)
        .num("size", o.size)
        .num("manufacturer_code", h.manufacturer)
        .num("image_type", h.image_type)
        .num("file_version", h.file_version)
        .num("stack_version", h.stack_version)
        .str("label", &h.label())
        .opt("hw_min", h.hardware.map(|x| x.0))
        .opt("hw_max", h.hardware.map(|x| x.1))
        .opt("min_file_version", o.min_file_version)
        .opt("max_file_version", o.max_file_version)
        .raw("devices", &array(devices.iter().map(|d| format!("\"{d}\""))));
    x.end()
}

/// The OTA server of the app.
pub struct Ota {
    /// The server.
    pub server: Server,
    /// Streamed images.
    pub streams: Streams,
    /// The image store.
    #[cfg(feature = "ota-store")]
    pub store: crate::ota_store::StoreState,
    /// Devices whose state needs writing.
    dirty: BTreeSet<String>,
    snapshot_dirty: bool,
    last_sweep: Millis,
}

/// Mutable image sources of an [`Ota`] (store and streams).
macro_rules! sources {
    ($s:expr) => {{
        let src = Sources::new(&mut $s.streams);
        #[cfg(feature = "ota-store")]
        let src = src.with_store($s.store.store.as_mut());
        src
    }};
}

/// Read-only image sources of an [`Ota`] (field borrows only).
macro_rules! view {
    ($s:expr) => {{
        let v = View::new(&$s.streams);
        #[cfg(feature = "ota-store")]
        let v = v.with_store($s.store.store.as_ref());
        v
    }};
}

impl Ota {
    /// The server; `cfg` holds the store's flash.
    pub fn new(cfg: &crate::AppConfig) -> Self {
        let mut server = Server::new(Settings::default());
        server.settings.tz_offset_s = cfg.tz_offset_s;
        Self {
            server,
            streams: Streams::default(),
            #[cfg(feature = "ota-store")]
            store: crate::ota_store::StoreState::open(&cfg.ota_flash),
            dirty: BTreeSet::new(),
            snapshot_dirty: true,
            last_sweep: 0,
        }
    }

    /// Read-only image sources.
    pub(crate) fn view(&self) -> View<'_> {
        view!(self)
    }

    /// Whether this build has an image store.
    pub fn has_store(&self) -> bool {
        cfg!(feature = "ota-store")
    }

    /// Settings and device states from the journal.
    pub fn load<T: ZigbeeTransport>(&mut self, ncp: &Ncp, engine: &Engine<T>, now: Millis) {
        type Records = Vec<(String, Vec<u8>)>;
        let (cfg, devs): (Option<Vec<u8>>, Records) = ncp.with_store(|s| {
            (s.kv_get(SETTINGS_KEY).map(<[u8]>::to_vec), s.kv_prefix(DEV_PREFIX).map(|(k, v)| (k.to_string(), v.to_vec())).collect())
        });
        if let Some(b) = cfg {
            let tz = self.server.settings.tz_offset_s;
            self.server.settings = Settings::from_bytes(&b);
            self.server.settings.tz_offset_s = tz;
        }
        let mut stale = Vec::new();
        for (k, v) in devs {
            let id = &k[DEV_PREFIX.len()..];
            match hub_ota::server::DevState::from_bytes(&v, now) {
                Some(st) if engine.device(id).is_some() => {
                    self.server.devices.insert(id.to_string(), st);
                }
                _ => stale.push(k),
            }
        }
        if !stale.is_empty() {
            ncp.with_store(|s| {
                for k in &stale {
                    let _ = s.kv_delete(k);
                }
            });
        }
        self.snapshot_dirty = true;
    }

    /// The next paced block response.
    pub fn next_due(&self) -> Option<Millis> {
        self.server.next_due()
    }

    fn device<T: ZigbeeTransport>(engine: &Engine<T>, id: &str) -> Option<Device> {
        let d = engine.device(id)?;
        Some(Device { id: id.to_string(), ieee: d.ieee()?, sleepy: d.sleepy() })
    }

    /// Client commands the engine handed over.
    pub fn frames<T: ZigbeeTransport>(&mut self, engine: &mut Engine<T>, now: Millis) {
        for f in engine.take_app_frames() {
            if f.cluster != CLUSTER {
                continue;
            }
            let Some(dev) = Self::device(engine, &f.device) else { continue };
            let c = Command {
                ep: f.ep,
                tsn: f.header.tsn,
                cmd: f.header.cmd,
                payload: &f.payload,
                default_rsp: !f.header.disable_default_response,
            };
            // Manufacturer-specific commands of the cluster are not ours.
            if f.header.mfr.is_some() {
                if c.default_rsp {
                    self.exec(engine, vec![Action::DefaultRsp { device: dev.id, ep: c.ep, tsn: c.tsn, cmd: c.cmd, status: 0x83 }], now);
                }
                continue;
            }
            let out = {
                let mut src = sources!(self);
                self.server.on_command(&dev, &c, &mut src, now)
            };
            self.exec(engine, out, now);
        }
    }

    /// Frames for API sessions (`ota/block_needed`).
    pub fn outbox(&mut self) -> Vec<(u64, String)> {
        std::mem::take(&mut self.streams.outbox)
    }

    /// Streams whose API session is gone wait for a client.
    pub fn prune(&mut self, alive: impl Fn(u64) -> bool, now: Millis) {
        let gone: Vec<u64> = self.streams.list().iter().filter_map(|s| s.owner).filter(|o| !alive(*o)).collect();
        for sid in gone {
            info!("ota: the client of a stream went away");
            self.streams.disconnected(sid, now);
        }
    }

    /// Paced responses, housekeeping, version reads, persistence and the
    /// topic snapshot.
    pub fn poll<T: ZigbeeTransport>(&mut self, engine: &mut Engine<T>, ncp: &Ncp, now: Millis) {
        if !self.streams.poll(now).is_empty() {
            info!("ota: a stream without a client was dropped");
            self.images_changed(engine, now);
        }
        #[cfg(feature = "ota-store")]
        if self.store.poll(&self.server) {
            self.images_changed(engine, now);
        }
        let out = {
            let e: &Engine<T> = engine;
            let v = view!(self);
            self.server.poll(&v, &|id| e.device(id)?.ieee(), now)
        };
        self.exec(engine, out, now);
        // CurrentFileVersion answers land in the engine's attribute cache.
        let got: Vec<(String, u32)> = self
            .server
            .devices
            .iter()
            .filter_map(|(id, d)| {
                let ep = d.reading?;
                match engine.device(id)?.attrs.map.get(&(ep, CLUSTER, attr::CURRENT_FILE_VERSION, None))?.value {
                    Some(ZclValue::Uint(v)) => Some((id.clone(), v as u32)),
                    _ => None,
                }
            })
            .collect();
        for (id, v) in got {
            if let Some(d) = self.server.devices.get_mut(&id) {
                d.reading = None;
            }
            let out = self.server.on_version(&id, v, now);
            self.exec(engine, out, now);
        }
        // Devices removed from the engine.
        if now.saturating_sub(self.last_sweep) >= 60_000 {
            self.last_sweep = now;
            let gone: Vec<String> = self.server.devices.keys().filter(|id| engine.device(id).is_none()).cloned().collect();
            for id in gone {
                self.server.devices.remove(&id);
                ncp.with_store(|s| {
                    let _ = s.kv_delete(&format!("{DEV_PREFIX}{id}"));
                });
                self.snapshot_dirty = true;
            }
        }
        if !self.dirty.is_empty() {
            let dirty = std::mem::take(&mut self.dirty);
            ncp.with_store(|s| {
                for id in &dirty {
                    if let Some(st) = self.server.devices.get(id)
                        && let Err(e) = s.kv_put(&format!("{DEV_PREFIX}{id}"), &st.to_bytes())
                    {
                        warn!("ota: persist {id}: {e}");
                    }
                }
            });
        }
        if self.snapshot_dirty {
            self.snapshot_dirty = false;
            let snap = self.summary(engine);
            engine.app_snapshots.insert("ota", snap.to_string());
        }
    }

    /// The `ota` snapshot and `ota_images` body: the area and counts
    /// (clients list devices and images with `ota/devices/list` and
    /// `ota/images/list`).
    fn summary<T: ZigbeeTransport>(&self, engine: &Engine<T>) -> J {
        let devices = engine.devices().filter(|d| ota_ep(d).is_some() || self.server.devices.contains_key(&d.id)).count();
        json!({"area": self.area_json(), "devices": devices, "images": self.view().offers().len()})
    }

    fn area_json(&self) -> J {
        #[cfg(feature = "ota-store")]
        return self.store.area_json();
        #[cfg(not(feature = "ota-store"))]
        json!({"capacity": 0, "store": false})
    }

    pub(crate) fn exec<T: ZigbeeTransport>(&mut self, engine: &mut Engine<T>, actions: Vec<Action>, now: Millis) {
        for a in actions {
            match a {
                Action::Send { device, ep, tsn, cmd, payload } => {
                    let tsn = tsn.unwrap_or_else(|| engine.new_tsn());
                    let header = ZclHeader {
                        frame_type: FrameType::Cluster,
                        mfr: None,
                        direction: Direction::ServerToClient,
                        disable_default_response: true,
                        tsn,
                        cmd,
                    };
                    engine.send_app_zcl(&device, ep, CLUSTER, ZclFrame { header, payload }, now);
                }
                Action::DefaultRsp { device, ep, tsn, cmd, status } => {
                    let header = ZclHeader {
                        frame_type: FrameType::Global,
                        mfr: None,
                        direction: Direction::ServerToClient,
                        disable_default_response: true,
                        tsn,
                        cmd: global::DEFAULT_RSP,
                    };
                    engine.send_app_zcl(&device, ep, CLUSTER, ZclFrame { header, payload: vec![cmd, status] }, now);
                }
                Action::ReadVersion { device, ep } => {
                    // A fresh answer replaces the cached value.
                    if let Some(d) = engine.device_mut(&device) {
                        d.attrs.map.remove(&(ep, CLUSTER, attr::CURRENT_FILE_VERSION, None));
                    }
                    // Attributes of the client cluster: server → client.
                    let header = ZclHeader {
                        frame_type: FrameType::Global,
                        mfr: None,
                        direction: Direction::ServerToClient,
                        disable_default_response: true,
                        tsn: engine.new_tsn(),
                        cmd: global::READ_ATTRIBUTES,
                    };
                    let payload = attr::CURRENT_FILE_VERSION.to_le_bytes().to_vec();
                    engine.send_app_zcl(&device, ep, CLUSTER, ZclFrame { header, payload }, now);
                    if let Some(d) = self.server.devices.get_mut(&device) {
                        d.reading = Some(ep);
                    }
                }
                Action::Changed { device, persist, structural } => {
                    if persist {
                        self.dirty.insert(device.clone());
                    }
                    if structural {
                        self.snapshot_dirty = true;
                    }
                    self.device_event(engine, &device, now);
                }
            }
        }
    }

    fn device_text<T: ZigbeeTransport>(&self, engine: &Engine<T>, id: &str, now: Millis) -> Option<String> {
        let d = engine.device(id)?;
        let ep = self.server.devices.get(id).and_then(|s| s.ep).or_else(|| ota_ep(d));
        Some(self.server.record(id, d.ieee()?, &self.view(), now, |o| {
            o.opt("ota_ep", ep)
                .ostr("name", d.user_name.clone().or_else(|| d.model()).as_deref())
                .ostr("model", d.model().as_deref())
                .ostr("manufacturer", d.manufacturer().as_deref())
                .ostr("sw_version", d.fp.sw_build_id.as_deref())
                .bool("sleepy", d.sleepy())
                .bool("available_device", d.available);
        }))
    }

    /// `ota_device` with the device's record.
    fn device_event<T: ZigbeeTransport>(&self, engine: &mut Engine<T>, id: &str, now: Millis) {
        if let Some(rec) = self.device_text(engine, id, now).and_then(|t| serde_json::from_str::<J>(&t).ok()) {
            engine.app_event("ota", json!({"topic": "ota_device", "device": rec}));
        }
    }

    fn devices_text<T: ZigbeeTransport>(&self, engine: &Engine<T>, now: Millis) -> Result<String, ApiError> {
        try_array(
            engine
                .devices()
                .filter(|d| ota_ep(d).is_some() || self.server.devices.contains_key(&d.id))
                .filter_map(|d| self.device_text(engine, &d.id, now)),
        )
        .map_err(|_| err("no_space", "the device list does not fit the hub's free memory"))
    }

    pub(crate) fn images_changed<T: ZigbeeTransport>(&mut self, engine: &mut Engine<T>, now: Millis) {
        self.snapshot_dirty = true;
        let mut body = self.summary(engine);
        body["topic"] = "ota_images".into();
        engine.app_event("ota", body);
        // Available updates may have changed for every device.
        let ids: Vec<String> = self.server.devices.keys().cloned().collect();
        for id in ids {
            self.device_event(engine, &id, now);
        }
    }

    /// Devices image `image` is the update for.
    pub(crate) fn targets_of<T: ZigbeeTransport>(&self, engine: &Engine<T>, image: u32) -> Vec<String> {
        let v = self.view();
        self.server
            .devices
            .iter()
            .filter(|(id, st)| {
                engine
                    .device(id)
                    .and_then(|d| d.ieee())
                    .and_then(|ieee| self.server.candidate(&v, ieee, st, false))
                    .is_some_and(|i| i.id == image)
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Every image (stored and streamed) as JSON text.
    fn images_text<T: ZigbeeTransport>(&self, engine: &Engine<T>) -> String {
        #[allow(unused_mut)]
        let mut items: Vec<String> = Vec::new();
        #[cfg(feature = "ota-store")]
        items.extend(self.store.images_text(|id| self.targets_of(engine, id)));
        items.extend(self.streams.offers().iter().map(|o| offer_text(o, &self.targets_of(engine, o.id))));
        array(items)
    }

    /// One `ota/*` request of session `sid`.
    pub fn request<T: ZigbeeTransport>(&mut self, engine: &mut Engine<T>, ncp: &Ncp, sid: u64, ty: &str, v: &J, now: Millis) -> Result<Reply, ApiError> {
        self.request_json(engine, ncp, sid, ty, v, now).map(|r| match r {
            J::String(t) if ty.ends_with("/list") => Reply::Text(t),
            v => Reply::Json(v),
        })
    }

    fn request_json<T: ZigbeeTransport>(&mut self, engine: &mut Engine<T>, ncp: &Ncp, sid: u64, ty: &str, v: &J, now: Millis) -> Result<J, ApiError> {
        let str_ = |k: &str| v.get(k).and_then(J::as_str);
        let u32_ = |k: &str| v.get(k).and_then(J::as_u64).and_then(|n| u32::try_from(n).ok());
        let device = || -> Result<Device, ApiError> {
            let id = str_("device").ok_or_else(|| err("invalid_value", "device"))?;
            Self::device(engine, id).ok_or_else(|| err("not_found", format!("no device {id}")))
        };
        match ty {
            "ota/status" => {
                let streams: Vec<J> = self
                    .streams
                    .list()
                    .iter()
                    .map(|s| json!({"session": s.offer.id, "name": s.offer.name, "file_version": s.offer.header.file_version, "size": s.offer.size, "received": s.received, "client": s.owner.is_some()}))
                    .collect();
                let mut r = json!({
                    "area": self.area_json(),
                    "store": self.has_store(),
                    "settings": self.server.settings.to_json(),
                    "block_max": self.server.block_cap(),
                    "asdu_max": self.server.asdu_max,
                    "stream_chunk": STREAM_CHUNK,
                    "streams": streams,
                });
                #[cfg(feature = "ota-store")]
                self.store.status(&mut r);
                let _ = &mut r;
                Ok(r)
            }
            "ota/settings/set" => {
                self.server.settings.apply(v).map_err(|m| err("invalid_value", m))?;
                ncp.with_store(|s| s.kv_put(SETTINGS_KEY, &self.server.settings.to_bytes())).map_err(|e| err("failed", e.to_string()))?;
                self.snapshot_dirty = true;
                Ok(self.server.settings.to_json())
            }
            // Lists are JSON text (see `request`).
            "ota/images/list" => Ok(J::String(self.images_text(engine))),
            "ota/devices/list" => Ok(J::String(self.devices_text(engine, now)?)),
            "ota/stream/offer" => {
                let size = u32_("size").ok_or_else(|| err("invalid_value", "size"))?;
                let head = hub_ota::json::base64_decode(str_("header").ok_or_else(|| err("invalid_value", "header: the file's first bytes (base64)"))?)
                    .map_err(|e| err("invalid_format", format!("header: {e}")))?;
                if head.len() > 512 {
                    return Err(err("invalid_value", "header: at most 512 B"));
                }
                let id = self
                    .streams
                    .offer(sid, &head, size, str_("name").unwrap_or_default(), u32_("min_file_version"), u32_("max_file_version"))
                    .map_err(|m| err("invalid_value", m))?;
                let o = self.streams.list().iter().find(|s| s.offer.id == id).map(|s| s.offer.clone()).unwrap();
                info!("ota: stream {id}: {} 0x{:08X}, {} B", o.name, o.header.file_version, o.size);
                self.images_changed(engine, now);
                Ok(json!({
                    "session": id,
                    "manufacturer_code": o.header.manufacturer,
                    "image_type": o.header.image_type,
                    "file_version": o.header.file_version,
                    "chunk": STREAM_CHUNK,
                    "devices": self.targets_of(engine, id),
                }))
            }
            "ota/block_data" => {
                let id = u32_("session").ok_or_else(|| err("invalid_value", "session"))?;
                let offset = u32_("offset").ok_or_else(|| err("invalid_value", "offset"))?;
                let data = hub_ota::json::base64_decode(str_("data").ok_or_else(|| err("invalid_value", "data"))?)
                    .map_err(|e| err("invalid_format", format!("data: {e}")))?;
                self.streams.data(sid, id, offset, &data).map_err(|m| err("invalid_value", m))?;
                Ok(J::Null)
            }
            "ota/stream/close" => {
                let id = u32_("session").ok_or_else(|| err("invalid_value", "session"))?;
                if !self.streams.close(sid, id) {
                    return Err(err("not_found", "no such stream of this session"));
                }
                self.images_changed(engine, now);
                Ok(J::Null)
            }
            "ota/devices/check" => {
                let d = device()?;
                let ep = engine.device(&d.id).and_then(ota_ep);
                let out = self.server.check(&d, ep).map_err(|e| err(e.code, e.message))?;
                self.exec(engine, out, now);
                Ok(json!({"sleepy": d.sleepy}))
            }
            "ota/devices/policy" => {
                let d = device()?;
                let p = match v.get("policy") {
                    Some(J::Null) | None => None,
                    Some(p) => Some(p.as_str().and_then(Policy::parse).ok_or_else(|| err("invalid_value", "policy: manual, auto, off or null"))?),
                };
                self.server.devices.entry(d.id.clone()).or_default().policy = p;
                if let Some(e) = engine.device(&d.id).and_then(ota_ep) {
                    self.server.devices.get_mut(&d.id).unwrap().ep.get_or_insert(e);
                }
                self.exec(engine, vec![Action::Changed { device: d.id.clone(), persist: true, structural: true }], now);
                Ok(J::Null)
            }
            "ota/start" => {
                let d = device()?;
                let ep = engine.device(&d.id).and_then(ota_ep);
                let force = v.get("force").and_then(J::as_bool).unwrap_or(false);
                let out = {
                    let view = view!(self);
                    self.server.start(&d, ep, u32_("image"), force, &view, now)
                }
                .map_err(|e| err(e.code, e.message))?;
                self.exec(engine, out, now);
                Ok(json!({"sleepy": d.sleepy}))
            }
            "ota/abort" => {
                let d = device()?;
                let out = self.server.abort(&d.id, now).map_err(|e| err(e.code, e.message))?;
                self.exec(engine, out, now);
                Ok(J::Null)
            }
            #[cfg(feature = "ota-store")]
            _ => crate::ota_store::request(self, engine, ty, v, now),
            #[cfg(not(feature = "ota-store"))]
            _ => Err(err("unsupported", "this hub has no image store (4 MB layout): stream the image (ota/stream/offer)")),
        }
    }

    /// The NCP's `max_asdu` (blocks are cut to fit unfragmented frames).
    pub fn set_asdu(&mut self, asdu: Option<u32>) {
        let a = asdu.map_or(DEFAULT_ASDU, |a| a as usize);
        if a != self.server.asdu_max {
            info!("ota: NCP max_asdu {a}, blocks up to {} B", a.saturating_sub(hub_ota::server::BLOCK_OVERHEAD));
            self.server.asdu_max = a;
        }
    }

    /// `hub/info` `ota`.
    pub fn info(&self) -> J {
        #[allow(unused_mut)]
        let mut v = json!({"store": self.has_store(), "stream": true, "stream_chunk": STREAM_CHUNK});
        #[cfg(feature = "ota-store")]
        self.store.info(&mut v);
        v
    }
}
