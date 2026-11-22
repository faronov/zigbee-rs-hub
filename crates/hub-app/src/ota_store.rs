//! The OTA image store of the app (feature `ota-store`, the 8 MB layout):
//! uploaded images (`ota/upload/*`, `ota/images/*`, `ota/store/format`) on
//! a raw flash region, on the hub the **inactive app slot**.
//!
//! The inactive app slot is shared with the hub's own firmware update:
//! [`APP_SLOT`] arbitrates. A hub update ([`AppSlot::begin_hub_update`])
//! is refused while a device downloads from the slot or an upload runs;
//! once it began, the stored images are dropped and the store refuses
//! every request until the hub restarts. After the update the slot holds
//! the previous app; the store, opened as expendable, starts empty.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use hub_devices::device::Millis;
use hub_devices::hub::{ApiError, Hub as Engine};
use hub_devices::transport::ZigbeeTransport;
use hub_ncp::store::Flash;
use hub_ota::json::Obj;
use hub_ota::server::{JobState, Server};
use hub_ota::store::{Image, ImageState};
use hub_ota::{ImageStore, StoreError, UploadMeta};
use log::{info, warn};
use serde_json::{Value as J, json};

use crate::ota::{Ota, err};

/// Largest `ota/upload/chunk` (decoded): one flash sector; the frame
/// (base64), its parsed copy and the decoded bytes stay below 16 KiB of
/// heap on a hub without PSRAM.
pub const CHUNK_MAX: usize = 4 * 1024;

const FREE: u8 = 0;
const DEVICES: u8 = 1;
const HUB_UPDATE: u8 = 2;

/// Who uses the inactive app slot.
#[derive(Debug, Default)]
pub struct AppSlot(AtomicU8);

/// The arbiter of the inactive app slot (see the module docs).
pub static APP_SLOT: AppSlot = AppSlot(AtomicU8::new(FREE));

impl AppSlot {
    /// The hub's own firmware update takes the slot. Fails while a device
    /// downloads an image from it or an image is uploaded; afterwards the
    /// device images are gone (lost with the update, by design).
    pub fn begin_hub_update(&self) -> Result<(), &'static str> {
        match self.0.compare_exchange(FREE, HUB_UPDATE, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) | Err(HUB_UPDATE) => Ok(()),
            Err(_) => Err("a device is downloading a firmware image from the inactive app slot, or an image is being uploaded: abort it (ota/abort, ota/upload/abort) or wait"),
        }
    }

    /// A failed hub update gives the slot back (the device images are
    /// gone regardless).
    pub fn end_hub_update(&self) {
        let _ = self.0.compare_exchange(HUB_UPDATE, FREE, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// A hub update has the slot.
    pub fn hub_updating(&self) -> bool {
        self.0.load(Ordering::SeqCst) == HUB_UPDATE
    }

    /// Devices use the slot (`busy`) or not; false while a hub update has it.
    fn set_devices(&self, busy: bool) -> bool {
        let (from, to) = if busy { (FREE, DEVICES) } else { (DEVICES, FREE) };
        let _ = self.0.compare_exchange(from, to, Ordering::SeqCst, Ordering::SeqCst);
        !self.hub_updating()
    }
}

/// The store's flash handed to the app (taken once at start).
#[derive(Clone, Default)]
pub struct OtaFlash(Arc<Mutex<Option<Region>>>);

/// A region and whether it is the inactive app slot.
type Region = (Box<dyn Flash>, bool);

impl OtaFlash {
    /// A region for the store (a file or RAM on a desktop).
    pub fn new(flash: Box<dyn Flash>) -> Self {
        Self(Arc::new(Mutex::new(Some((flash, false)))))
    }

    /// The inactive app slot: its content is expendable, and the hub's own
    /// update takes it over ([`APP_SLOT`]).
    pub fn app_slot(flash: Box<dyn Flash>) -> Self {
        Self(Arc::new(Mutex::new(Some((flash, true)))))
    }

    fn take(&self) -> Option<Region> {
        self.0.lock().ok()?.take()
    }
}

impl std::fmt::Debug for OtaFlash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let present = self.0.lock().is_ok_and(|g| g.is_some());
        write!(f, "OtaFlash({})", if present { "region" } else { "none" })
    }
}

/// The store and why it is missing.
#[derive(Default)]
pub struct StoreState {
    /// The store.
    pub store: Option<ImageStore>,
    error: Option<String>,
    /// The store is the inactive app slot.
    app_slot: bool,
}

fn store_err(e: StoreError) -> ApiError {
    let code = match e {
        StoreError::NoSpace { .. } | StoreError::TooMany => "no_space",
        StoreError::NotFound => "not_found",
        StoreError::Exists(_) => "exists",
        StoreError::Hash => "verification_failed",
        StoreError::Foreign => "unsupported",
        StoreError::Io(_) => "failed",
        _ => "invalid_value",
    };
    err(code, e.to_string())
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    let v: Option<Vec<u8>> = (0..32).map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()).collect();
    v?.try_into().ok()
}

fn image_text(i: &Image, sector: u32, devices: &[String]) -> String {
    let h = i.header.as_ref();
    let mut o = Obj::new();
    o.num("image", i.id)
        .str("state", if i.state == ImageState::Ready { "ready" } else { "uploading" })
        .str("name", &i.name)
        .num("size", i.size)
        .num("written", i.written)
        .num("footprint", i.footprint(sector))
        .opt("manufacturer_code", h.map(|h| h.manufacturer))
        .opt("image_type", h.map(|h| h.image_type))
        .opt("file_version", h.map(|h| h.file_version))
        .opt("stack_version", h.map(|h| h.stack_version))
        .ostr("label", h.map(|h| h.label()).as_deref())
        .opt("hw_min", h.and_then(|h| h.hardware).map(|x| x.0))
        .opt("hw_max", h.and_then(|h| h.hardware).map(|x| x.1))
        .ostr("destination", h.and_then(|h| h.destination).map(|d| format!("{d:016x}")).as_deref())
        .opt("min_file_version", i.min_file_version)
        .opt("max_file_version", i.max_file_version)
        .ostr("sha256", i.sha256.filter(|_| i.state == ImageState::Ready).map(|s| s.iter().map(|b| format!("{b:02x}")).collect::<String>()).as_deref())
        .num("added", i.added)
        .raw("warnings", &hub_ota::json::array(i.warnings.iter().map(|w| format!("{:?}", w))))
        .raw("devices", &hub_ota::json::array(devices.iter().map(|d| format!("\"{d}\""))));
    o.end()
}

impl StoreState {
    /// Opens the store on `flash`.
    pub fn open(flash: &OtaFlash) -> Self {
        let Some((f, app_slot)) = flash.take() else {
            return Self { error: Some("no image store region".into()), ..Default::default() };
        };
        let opened = if app_slot { ImageStore::open_expendable(f) } else { ImageStore::open(f) };
        match opened {
            Ok(s) => {
                if s.is_foreign() {
                    warn!("ota: the store region holds other data; ota/store/format takes it over");
                }
                info!("ota: {} images, {} of {} KiB used{}", s.images().len(), s.used() / 1024, s.capacity() / 1024, if app_slot { " (inactive app slot)" } else { "" });
                Self { store: Some(s), error: None, app_slot }
            }
            Err(e) => {
                warn!("ota: image store unusable: {e}");
                Self { store: None, error: Some(e.to_string()), app_slot }
            }
        }
    }

    /// Tracks the app-slot arbitration; true if the store was dropped
    /// (a hub update took the slot).
    pub fn poll(&mut self, server: &Server) -> bool {
        if !self.app_slot || self.store.is_none() {
            return false;
        }
        let s = self.store.as_ref().unwrap();
        let busy = s.upload().is_some()
            || server.devices.values().any(|d| {
                d.job.as_ref().is_some_and(|j| j.state == JobState::Downloading && j.target.is_some_and(|t| s.find(t.0, t.1, t.2).is_some()))
            });
        if APP_SLOT.set_devices(busy) {
            return false;
        }
        self.store = None;
        self.error = Some("the inactive app slot is in use by a hub firmware update: device images are gone until the hub restarts".into());
        warn!("ota: {}", self.error.as_deref().unwrap());
        true
    }

    /// `area` of `ota/status`.
    pub fn area_json(&self) -> J {
        match &self.store {
            Some(s) => json!({
                "capacity": s.capacity(),
                "used": s.used(),
                "free": s.capacity() - s.used(),
                "largest_free": s.largest_free(),
                "foreign": s.is_foreign(),
                "store": true,
                "app_slot": self.app_slot,
            }),
            None => json!({"capacity": 0, "store": true, "error": self.error}),
        }
    }

    /// Store fields of `ota/status`.
    pub fn status(&self, r: &mut J) {
        r["chunk_max"] = CHUNK_MAX.into();
        r["upload"] = self
            .store
            .as_ref()
            .and_then(|s| s.upload())
            .map(|u| json!({"upload": u.id, "name": u.name, "size": u.size, "written": u.written}))
            .unwrap_or(J::Null);
    }

    /// Store fields of `hub/info` `ota`.
    pub fn info(&self, v: &mut J) {
        v["chunk_max"] = CHUNK_MAX.into();
        v["capacity"] = self.store.as_ref().map_or(0, |s| s.capacity()).into();
    }

    /// The stored images as JSON texts.
    pub fn images_text(&self, targets: impl Fn(u32) -> Vec<String>) -> Vec<String> {
        let Some(s) = &self.store else { return Vec::new() };
        s.images().iter().map(|i| image_text(i, s.sector_size(), &targets(u32::from(i.id)))).collect()
    }

    fn get(&mut self) -> Result<&mut ImageStore, ApiError> {
        if self.app_slot && APP_SLOT.hub_updating() {
            return Err(err("busy", "the inactive app slot is in use by a hub firmware update"));
        }
        let e = self.error.clone();
        self.store.as_mut().ok_or_else(|| err("unsupported", e.unwrap_or_else(|| "no image store".into())))
    }
}

/// The store's `ota/*` requests.
pub fn request<T: ZigbeeTransport>(ota: &mut Ota, engine: &mut Engine<T>, ty: &str, v: &J, now: Millis) -> Result<J, ApiError> {
    let str_ = |k: &str| v.get(k).and_then(J::as_str);
    let u16_ = |k: &str| v.get(k).and_then(J::as_u64).and_then(|n| u16::try_from(n).ok());
    match ty {
        "ota/images/delete" => {
            let id = u16_("image").ok_or_else(|| err("invalid_value", "image (a stored image; streams: ota/stream/close)"))?;
            let ident = ota.store.store.as_ref().and_then(|s| s.image(id)).and_then(Image::ident);
            let busy = ident.is_some_and(|t| {
                ota.server.devices.values().any(|d| d.job.as_ref().is_some_and(|j| j.target == Some(t) && j.state == JobState::Downloading))
            });
            if busy && v.get("force").and_then(J::as_bool) != Some(true) {
                return Err(err("busy", "a device is downloading this image (force to delete)"));
            }
            ota.store.get()?.delete(id).map_err(store_err)?;
            ota.images_changed(engine, now);
            Ok(J::Null)
        }
        "ota/upload/begin" => {
            let size = v.get("size").and_then(J::as_u64).and_then(|n| u32::try_from(n).ok()).ok_or_else(|| err("invalid_value", "size"))?;
            let sha256 = match str_("sha256") {
                Some(h) => Some(unhex32(h).ok_or_else(|| err("invalid_value", "sha256: 64 hex digits"))?),
                None => None,
            };
            let fv = |k: &str| v.get(k).and_then(J::as_u64).map(|n| n as u32);
            let meta = UploadMeta {
                name: str_("name").unwrap_or_default().chars().take(48).collect(),
                sha256,
                min_file_version: fv("min_file_version"),
                max_file_version: fv("max_file_version"),
            };
            let (id, offset) = ota.store.get()?.begin(size, meta, (now / 1000) as u32).map_err(store_err)?;
            Ok(json!({"upload": id, "offset": offset, "chunk_max": CHUNK_MAX}))
        }
        "ota/upload/chunk" => {
            let id = u16_("upload").ok_or_else(|| err("invalid_value", "upload"))?;
            let offset = v.get("offset").and_then(J::as_u64).and_then(|n| u32::try_from(n).ok()).ok_or_else(|| err("invalid_value", "offset"))?;
            let data = hub_ota::json::base64_decode(str_("data").ok_or_else(|| err("invalid_value", "data"))?)
                .map_err(|e| err("invalid_format", format!("data: {e}")))?;
            if data.len() > CHUNK_MAX {
                return Err(err("invalid_value", format!("chunks of at most {CHUNK_MAX} B")));
            }
            let written = ota.store.get()?.write(id, offset, &data).map_err(store_err)?;
            Ok(json!({"written": written}))
        }
        "ota/upload/commit" => {
            let id = u16_("upload").ok_or_else(|| err("invalid_value", "upload"))?;
            let replace = v.get("replace").and_then(J::as_bool).unwrap_or(false);
            ota.store.get()?.commit(id, replace).map_err(store_err)?;
            ota.images_changed(engine, now);
            let targets = ota.targets_of(engine, u32::from(id));
            let s = ota.store.store.as_ref().unwrap();
            let text = image_text(s.image(id).unwrap(), s.sector_size(), &targets);
            info!("ota: image stored: {text}");
            serde_json::from_str(&text).map_err(|e| err("failed", e.to_string()))
        }
        "ota/upload/abort" => {
            let id = u16_("upload").ok_or_else(|| err("invalid_value", "upload"))?;
            let s = ota.store.get()?;
            if s.image(id).is_none_or(|i| i.state != ImageState::Uploading) {
                return Err(err("not_found", "no such upload"));
            }
            s.delete(id).map_err(store_err)?;
            ota.images_changed(engine, now);
            Ok(J::Null)
        }
        "ota/store/format" => {
            if str_("confirm") != Some("FORMAT") {
                return Err(err("invalid_value", "confirm: \"FORMAT\" (deletes every image)"));
            }
            ota.store.get()?.format().map_err(store_err)?;
            ota.images_changed(engine, now);
            Ok(ota.store.area_json())
        }
        _ => Err(err("unsupported", ty.to_string())),
    }
}
