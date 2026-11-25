//! The hub's own firmware update (`firmware/*` requests, maintenance
//! feature `firmware`): an image uploaded through the API in chunks goes
//! into the other app slot of SONOFF's table with ESP-IDF's OTA functions
//! (sequential writes: each sector is erased as it is reached), is checked
//! (image header, chip, appended SHA-256 by `esp_ota_end`, the upload's
//! SHA-256, a `zrh-donglem` image) and selected in `otadata`; the client
//! then reboots the hub (`hub/reboot`, which saves the registry first).
//!
//! The hub runs from `ota_1` after its installation, so **its first update
//! writes over SONOFF's application in `ota_0`**, the way back to the
//! stock firmware (`stock confirm`). That needs `"replace_stock": true` in
//! `firmware/begin`; later updates alternate between the two slots.
//!
//! | Request | Fields | Result |
//! |---|---|---|
//! | `firmware/status` | | running and target slot, what the target holds, upload state |
//! | `firmware/begin` | `size`, `sha256` (hex, optional), `replace_stock` | `chunk` size |
//! | `firmware/chunk` | `offset`, `data` (base64, at most [`CHUNK_MAX`] bytes) | `written` |
//! | `firmware/end` | | the image's version; selected for the next boot |
//! | `firmware/abort` | | |

use std::ffi::CStr;
use std::sync::Mutex;

use base64::Engine;
use esp_idf_svc::sys;
use hub_devices::hub::ApiError;
use log::{info, warn};
use serde_json::{Value as J, json};
use sha2::{Digest, Sha256};

/// Largest decoded chunk.
pub const CHUNK_MAX: usize = 4096;
/// Version prefix of this firmware's images (`CONFIG_APP_PROJECT_VER`).
pub const IMAGE_PREFIX: &str = "zrh-donglem";

struct Upload {
    handle: sys::esp_ota_handle_t,
    part: *const sys::esp_partition_t,
    size: u32,
    written: u32,
    sha: Sha256,
    expect: Option<[u8; 32]>,
}

// The partition is a static ESP-IDF table entry; the handle is ESP-IDF's.
unsafe impl Send for Upload {}

static UPLOAD: Mutex<Option<Upload>> = Mutex::new(None);
/// The image written and selected in this boot (its version).
static DONE: Mutex<Option<String>> = Mutex::new(None);

fn text(b: &[core::ffi::c_char]) -> String {
    unsafe { CStr::from_ptr(b.as_ptr()) }.to_string_lossy().into_owned()
}

fn label(p: *const sys::esp_partition_t) -> String {
    if p.is_null() { "-".into() } else { text(unsafe { &(*p).label }) }
}

/// The slot an update writes.
fn target() -> *const sys::esp_partition_t {
    unsafe { sys::esp_ota_get_next_update_partition(core::ptr::null()) }
}

/// Project and version of the app in `p`.
fn app_in(p: *const sys::esp_partition_t) -> Option<(String, String)> {
    if p.is_null() {
        return None;
    }
    let mut d: sys::esp_app_desc_t = unsafe { core::mem::zeroed() };
    (unsafe { sys::esp_ota_get_partition_description(p, &mut d) } == sys::ESP_OK)
        .then(|| (text(&d.project_name), text(&d.version)))
}

/// The target slot holds SONOFF's application.
pub fn target_is_stock() -> bool {
    app_in(target()).is_some_and(|(project, _)| project == crate::slots::STOCK_PROJECT)
}

fn err(code: &str, msg: impl Into<String>) -> ApiError {
    ApiError::new(code, msg)
}

fn esp_err(what: &str, e: sys::esp_err_t) -> ApiError {
    err("failed", format!("{what}: error {e}"))
}

/// `firmware/status`, also in `hub/system`.
pub fn status() -> J {
    let t = target();
    let holds = app_in(t);
    let up = UPLOAD.lock().unwrap();
    json!({
        "running": label(unsafe { sys::esp_ota_get_running_partition() }),
        "version": text(unsafe { &(*sys::esp_app_get_description()).version }),
        "target": label(t),
        "target_size": if t.is_null() { 0 } else { unsafe { (*t).size } },
        "target_holds": holds.as_ref().map(|(p, v)| format!("{p} {v}")),
        "replaces_stock": holds.as_ref().is_some_and(|(p, _)| p == crate::slots::STOCK_PROJECT),
        "state": if up.is_some() { "uploading" } else if DONE.lock().unwrap().is_some() { "ready" } else { "idle" },
        "written": up.as_ref().map(|u| u.written),
        "size": up.as_ref().map(|u| u.size),
        "ready_version": DONE.lock().unwrap().clone(),
        "chunk": CHUNK_MAX,
    })
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    let v: Option<Vec<u8>> = (0..32).map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()).collect();
    v?.try_into().ok()
}

fn abort(up: Upload) {
    unsafe { sys::esp_ota_abort(up.handle) };
}

fn begin(req: &J) -> Result<J, ApiError> {
    let size = req["size"].as_u64().ok_or_else(|| err("invalid_format", "size missing"))?;
    let expect = match req["sha256"].as_str() {
        Some(h) => Some(unhex32(h).ok_or_else(|| err("invalid_value", "sha256: 64 hex digits"))?),
        None => None,
    };
    let t = target();
    if t.is_null() {
        return Err(err("unsupported", "no other app slot to update into"));
    }
    let slot = unsafe { (*t).size } as u64;
    if size < 1024 || size > slot {
        return Err(err("invalid_value", format!("size {size}: the {} slot holds {slot} bytes", label(t))));
    }
    if target_is_stock() && req["replace_stock"] != J::Bool(true) {
        return Err(err(
            "confirm_required",
            format!(
                "This update writes over SONOFF's firmware in {}: afterwards `stock confirm` cannot return to it, only USB can \
                 (SONOFF Dongle Flasher or a backup). Send again with \"replace_stock\": true.",
                label(t)
            ),
        ));
    }
    let mut up = UPLOAD.lock().unwrap();
    if let Some(old) = up.take() {
        abort(old);
    }
    *DONE.lock().unwrap() = None;
    let mut handle: sys::esp_ota_handle_t = 0;
    match unsafe { sys::esp_ota_begin(t, sys::OTA_WITH_SEQUENTIAL_WRITES as usize, &mut handle) } {
        sys::ESP_OK => {}
        e => return Err(esp_err("esp_ota_begin", e)),
    }
    info!("firmware: update of {size} B into {} begins", label(t));
    *up = Some(Upload { handle, part: t, size: size as u32, written: 0, sha: Sha256::new(), expect });
    Ok(json!({"target": label(t), "chunk": CHUNK_MAX}))
}

fn chunk(req: &J) -> Result<J, ApiError> {
    let offset = req["offset"].as_u64().ok_or_else(|| err("invalid_format", "offset missing"))?;
    let data = req["data"].as_str().ok_or_else(|| err("invalid_format", "data missing"))?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).map_err(|e| err("invalid_value", format!("data: {e}")))?;
    let mut guard = UPLOAD.lock().unwrap();
    let up = guard.as_mut().ok_or_else(|| err("not_found", "no update in progress (firmware/begin)"))?;
    if offset != u64::from(up.written) {
        return Err(err("invalid_value", format!("offset {offset}: expected {}", up.written)));
    }
    if bytes.is_empty() || bytes.len() > CHUNK_MAX || up.written as usize + bytes.len() > up.size as usize {
        return Err(err("invalid_value", format!("chunk of {} B at {offset} of {}", bytes.len(), up.size)));
    }
    match unsafe { sys::esp_ota_write(up.handle, bytes.as_ptr().cast(), bytes.len()) } {
        sys::ESP_OK => {}
        e => {
            let up = guard.take().unwrap();
            abort(up);
            return Err(esp_err("esp_ota_write (update aborted)", e));
        }
    }
    up.sha.update(&bytes);
    up.written += bytes.len() as u32;
    Ok(json!({"written": up.written}))
}

fn end() -> Result<J, ApiError> {
    let up = UPLOAD.lock().unwrap().take().ok_or_else(|| err("not_found", "no update in progress (firmware/begin)"))?;
    if up.written != up.size {
        let msg = format!("{} of {} B written (update aborted)", up.written, up.size);
        abort(up);
        return Err(err("invalid_value", msg));
    }
    let digest: [u8; 32] = up.sha.clone().finalize().into();
    if up.expect.is_some_and(|e| e != digest) {
        abort(up);
        return Err(err("verification_failed", "SHA-256 of the upload differs (update aborted)"));
    }
    // Validates the image: header, chip, segments, appended SHA-256.
    match unsafe { sys::esp_ota_end(up.handle) } {
        sys::ESP_OK => {}
        e => return Err(esp_err("the image is not valid for this chip (esp_ota_end)", e)),
    }
    let (project, version) = app_in(up.part).unwrap_or_default();
    if !version.starts_with(IMAGE_PREFIX) {
        warn!("firmware: {project} {version} is not a {IMAGE_PREFIX} image: not selected");
        return Err(err("unsupported", format!("{project} {version} is not a {IMAGE_PREFIX} image; not selected")));
    }
    match unsafe { sys::esp_ota_set_boot_partition(up.part) } {
        sys::ESP_OK => {}
        e => return Err(esp_err("esp_ota_set_boot_partition", e)),
    }
    info!("firmware: {version} written to {} and selected for the next boot", label(up.part));
    *DONE.lock().unwrap() = Some(version.clone());
    Ok(json!({"version": version, "target": label(up.part), "reboot": "hub/reboot starts it"}))
}

/// `firmware/*` requests.
pub struct FirmwareHook;

impl hub_app::RequestHook for FirmwareHook {
    fn prefix(&self) -> &'static str {
        "firmware/"
    }
    fn feature(&self) -> Option<&'static str> {
        Some("firmware")
    }
    fn request(&mut self, ty: &str, req: &J) -> Result<J, ApiError> {
        match ty {
            "firmware/status" => Ok(status()),
            "firmware/begin" => begin(req),
            "firmware/chunk" => chunk(req),
            "firmware/end" => end(),
            "firmware/abort" => {
                if let Some(up) = UPLOAD.lock().unwrap().take() {
                    abort(up);
                }
                Ok(json!({}))
            }
            _ => Err(err("invalid_format", format!("unknown request {ty}"))),
        }
    }
}
