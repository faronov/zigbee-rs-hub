//! SONOFF's `files` partition as the hub's data region
//! (`hub_store::files`, `hub_core::layout::dongle_files`).
//!
//! **The first boot claims it** (`hub_store::files::boot_action`): before
//! the hub starts, SONOFF's blocks in use (those its LittleFS uses, found
//! by walking it read-only; every written block if it cannot be walked) are
//! copied into `stock_copy` and read back, then the region becomes the
//! hub's. A power cut at any point is safe: until the commit SONOFF's file
//! system is untouched and the next boot claims again, after it the next
//! boot finishes the claim. The attempt is kept in NVS (`zrh/files_auto`:
//! `claimed`, or `refused: <why>`), so a refusal is not retried at every
//! boot; a completed `stock confirm` clears it (a later installation is a
//! first boot again).
//!
//! **If the claim is refused** (more than 29 blocks in use, SONOFF's file
//! system in `stock_copy`, unknown content) the hub keeps its data in RAM
//! only: the device registry, names, groups, automations, settings of the
//! app and the ownership tag are lost at every restart (the Zigbee network
//! itself is in the MG24's NVM and survives), the catalog cannot be
//! installed and the Zigbee OTA image store is off. The log, the console
//! and a banner of the web UI say why. The claim can then be made by hand
//! once the cause is gone (console `files claim confirm`, API
//! `storage/claim` with `"confirm": true`, Settings › Storage): it runs on
//! its own thread and restarts the hub.
//!
//! Once claimed: the host store journal and the catalog in the `hubdata`
//! area, the Zigbee device OTA image store in `ota_store`; `radio` is
//! reserved (MG24 updates, later). Every window handed to the hub writes
//! through one [`WriteGate`]: [`close_gate`] stops them all before
//! `stock confirm` gives SONOFF's blocks back ([`restore`]).

use std::ffi::CStr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use hub_core::layout::dongle_files::{self as layout, Content};
use hub_devices::hub::ApiError;
use hub_store::files::{BootAction, Claimed, CopyState, FilesError, FilesRegion, Restored, SLOTS, ScanMethod, Status};
use hub_store::{Gated, WriteGate};
use log::{error, info, warn};
use serde_json::{Value as J, json};
use zrh_espapp::flash::Region;

use crate::settings;

const LABEL: &CStr = c"files";

/// A claim or return to stock running now.
static BUSY: Mutex<Option<&'static str>> = Mutex::new(None);
/// How the last claim ended (refusals included).
static LAST: Mutex<Option<String>> = Mutex::new(None);
/// The hub uses the region in this boot.
static IN_USE: AtomicBool = AtomicBool::new(false);
/// The gate of every window handed to the hub.
static GATE: std::sync::LazyLock<WriteGate> = std::sync::LazyLock::new(WriteGate::new);
/// NVS key of the first boot's claim.
const AUTO_KEY: &str = "files_auto";

fn region() -> Result<FilesRegion<Region>, String> {
    Region::find(LABEL, 0, layout::SIZE)
        .map(FilesRegion::new)
        .ok_or_else(|| format!("no `files` partition of {} KiB (not SONOFF's partition table)", layout::SIZE / 1024))
}

/// What the region holds.
pub fn status() -> Result<Status, String> {
    region()?.status().map_err(|e| e.to_string())
}

/// The hub keeps its data on the region in this boot.
pub fn in_use() -> bool {
    IN_USE.load(Ordering::Relaxed)
}

/// The first boot's claim: `claimed`, `refused: <why>`, `None` before it.
fn auto() -> Option<String> {
    settings::get(AUTO_KEY)
}

/// Why the hub's data are in RAM (`None`: on the region), for banners.
pub fn ram_only_reason() -> Option<String> {
    if in_use() {
        return None;
    }
    Some(match auto() {
        Some(a) if a.starts_with("refused") => format!("the first boot's claim of SONOFF's files partition was {a}"),
        _ => "SONOFF's files partition is not claimed".into(),
    })
}

/// At boot, before the hub starts (after the API token): claims the
/// region on the first boot, finishes a committed claim cut short;
/// whether the hub may use the region.
pub fn boot() -> bool {
    let st = match status() {
        Ok(s) => s,
        Err(e) => {
            warn!("files: {e}; the hub's data stay in RAM");
            return false;
        }
    };
    info!("files: {}", st.describe());
    let ready = match hub_store::files::boot_action(&st, auto().is_some()) {
        BootAction::Use => true,
        BootAction::Finish => {
            info!("files: finishing the claim (erasing the hub's areas)");
            match region().and_then(|mut r| r.claim().map_err(|e| e.to_string())) {
                Ok(_) => true,
                Err(e) => {
                    error!("files: finishing the claim: {e}");
                    false
                }
            }
        }
        BootAction::Claim => {
            info!("files: first boot: claiming SONOFF's files partition (its files are copied first)");
            let mut r = match region() {
                Ok(r) => r,
                Err(e) => {
                    warn!("files: {e}");
                    return false;
                }
            };
            match r.claim() {
                Ok(c) => {
                    let text = match c {
                        Claimed::Now { copied } => format!("claimed; {copied} blocks of SONOFF's data copied"),
                        Claimed::Already => "claimed already".into(),
                    };
                    info!("files: {text}");
                    *LAST.lock().unwrap() = Some(text);
                    remember("claimed");
                    true
                }
                // A flash error: the next boot tries again.
                Err(e @ FilesError::Io(_)) => {
                    error!("files: first boot's claim: {e}");
                    false
                }
                Err(e) => {
                    *LAST.lock().unwrap() = Some(format!("refused: {e}"));
                    remember(&format!("refused: {e}"));
                    false
                }
            }
        }
        BootAction::RamOnly => false,
    };
    IN_USE.store(ready, Ordering::Relaxed);
    if let Some(why) = ram_only_reason() {
        warn!("files: !!! the hub's data stay in RAM, lost at every restart: {why} (`files claim` on the console, Settings › Storage)");
    }
    ready
}

fn remember(what: &str) {
    if let Err(e) = settings::set(AUTO_KEY, Some(what)) {
        warn!("files: {AUTO_KEY} not stored: {e}");
    }
}

/// Stops every write of the hub to the region (before the return to
/// stock); returns once the writes under way have ended.
pub fn close_gate() {
    GATE.close();
}

/// The host store journal (start of the `hubdata` area), behind the gate.
pub fn journal() -> Option<Gated<Region>> {
    Region::find(LABEL, layout::HUBDATA.offset, hub_store::JOURNAL_BYTES).map(|r| Gated::new(r, GATE.clone()))
}

/// The catalog store (the whole `hubdata` area, mapped), behind the gate.
pub fn catalog() -> Option<zrh_espapp::catalog::HubData> {
    zrh_espapp::catalog::HubData::window(LABEL, layout::HUBDATA.offset, Some(layout::HUBDATA.size)).map(|c| c.with_gate(GATE.clone()))
}

/// The Zigbee device OTA image store, behind the gate.
pub fn ota_store() -> Option<Gated<Region>> {
    Region::find(LABEL, layout::OTA_STORE.offset, layout::OTA_STORE.size).map(|r| Gated::new(r, GATE.clone()))
}

fn content_name(c: Content) -> &'static str {
    match c {
        Content::LittleFs => "littlefs",
        Content::Blank => "blank",
        Content::Claimed => "claimed",
        Content::Unknown => "unknown",
    }
}

/// `files` on the console.
pub fn describe() -> String {
    let mut s = match status() {
        Ok(st) => format!(
            "files: {}\r\nthe hub's data: {}\r\n",
            st.describe(),
            match ram_only_reason() {
                None => "on the files region".into(),
                Some(why) => format!("!!! RAM only, lost at every restart: {why}"),
            }
        ),
        Err(e) => format!("files: {e}\r\n"),
    };
    if let Some(b) = *BUSY.lock().unwrap() {
        s += &format!("running: {b}\r\n");
    }
    if let Some(l) = LAST.lock().unwrap().as_ref() {
        s += &format!("last claim: {l}\r\n");
    }
    s
}

/// `files claim`: what the claim would copy, or why it is refused
/// (reads the whole region).
pub fn plan() -> String {
    let mut r = match region() {
        Ok(r) => r,
        Err(e) => return format!("{e}\r\n"),
    };
    let st = match r.status() {
        Ok(s) => s,
        Err(e) => return format!("files: {e}\r\n"),
    };
    if st.ready() {
        return "the files region is the hub's already\r\n".into();
    }
    if st.record.is_some() || matches!(st.copy, CopyState::Hub(_)) {
        return format!("{}\r\n`files claim confirm` finishes it.\r\n", st.describe());
    }
    let scan = match r.scan() {
        Ok(s) => s,
        Err(e) => return format!("files: {e}\r\n"),
    };
    let how = match (scan.method, &scan.fallback) {
        (ScanMethod::LittleFs, _) => "used by its LittleFS".to_string(),
        (ScanMethod::NonErased, Some(why)) => format!("written (the LittleFS walk failed: {why})"),
        (ScanMethod::NonErased, None) => "written".to_string(),
    };
    let mut s = format!(
        "files: {}; {} blocks {how} outside stock_copy {:?}, {} in stock_copy; {} blocks written in all\r\n",
        content_name(scan.content),
        scan.used.len(),
        scan.used,
        scan.copy_area_used.len(),
        scan.written
    );
    s += &if !matches!(scan.content, Content::LittleFs | Content::Blank) {
        "refused: neither SONOFF's LittleFS nor erased (take a USB backup first)\r\n".to_string()
    } else if !scan.copy_area_used.is_empty() {
        format!("refused: SONOFF's file system uses blocks of stock_copy {:?}\r\n", scan.copy_area_used)
    } else if scan.used.len() > SLOTS {
        format!(
            "refused: more than {SLOTS} blocks in use (reset SONOFF's settings in its web console: a staged MG24 update takes many; or take a USB backup)\r\n"
        )
    } else {
        format!(
            "`files claim confirm` copies these {} blocks into stock_copy, then makes the region the hub's and restarts; \
             `stock confirm` writes them back before it returns to SONOFF's firmware\r\n",
            scan.used.len()
        )
    };
    s
}

/// Starts the claim on its own thread; the hub restarts when it is done.
pub fn start_claim() -> Result<(), String> {
    let st = status()?;
    if st.ready() {
        return Err("the files region is the hub's already".into());
    }
    {
        let mut busy = BUSY.lock().unwrap();
        if let Some(b) = *busy {
            return Err(format!("{b} is running"));
        }
        *busy = Some("claim");
    }
    let spawned = std::thread::Builder::new().name("files-claim".into()).stack_size(8192).spawn(|| {
        info!("files: claiming");
        let result = region().and_then(|mut r| r.claim().map_err(|e| e.to_string()));
        let text = match &result {
            Ok(Claimed::Now { copied }) => format!("claimed; {copied} blocks of SONOFF's data copied"),
            Ok(Claimed::Already) => "claimed already".into(),
            Err(e) => format!("refused: {e}"),
        };
        info!("files: {text}");
        *LAST.lock().unwrap() = Some(text);
        if result.is_ok() {
            remember("claimed");
            info!("files: restarting to use the region");
            std::thread::sleep(Duration::from_secs(2));
            crate::restart();
        }
        *BUSY.lock().unwrap() = None;
    });
    if let Err(e) = spawned {
        *BUSY.lock().unwrap() = None;
        return Err(format!("thread: {e}"));
    }
    Ok(())
}

/// What `stock confirm` does with the region, or why it cannot.
pub fn restore_plan() -> Result<Option<String>, String> {
    let st = status()?;
    Ok(match (st.record, &st.copy) {
        (None, CopyState::None | CopyState::Other) => None,
        (Some(_), CopyState::Hub(ix)) if ix.done => Some(format!(
            "SONOFF's data ({} blocks) are written back to `files` first; the hub's data on it (devices, catalog, OTA images) are erased",
            ix.blocks.len()
        )),
        (Some(_), _) => {
            return Err("the hub owns the files region but its copy of SONOFF's data is incomplete: the way back is USB (SONOFF Dongle Flasher or a backup)".into());
        }
        (None, CopyState::Hub(_)) => Some("an interrupted claim or return to stock is finished first".into()),
    })
}

/// Gives SONOFF's data back (`stock confirm`); the hub has stopped (its
/// writes are gated off here too).
pub fn restore() -> Result<Restored, String> {
    {
        let mut busy = BUSY.lock().unwrap();
        if let Some(b) = *busy {
            return Err(format!("{b} is running"));
        }
        *busy = Some("return to stock");
    }
    IN_USE.store(false, Ordering::Relaxed);
    // No write of the hub's reaches the region from here on.
    close_gate();
    let r = region().and_then(|mut r| r.restore().map_err(|e| e.to_string()));
    *BUSY.lock().unwrap() = None;
    r
}

/// The dongle returns to SONOFF's firmware (`stock confirm` selected it):
/// a later installation of the hub is a first boot again.
pub fn forget_first_boot() {
    if let Err(e) = settings::set(AUTO_KEY, None) {
        warn!("files: {AUTO_KEY} not cleared: {e}");
    }
}

/// `storage/status`.
pub fn status_json() -> J {
    let mut v = match status() {
        Ok(st) => json!({
            "content": content_name(st.content),
            "claimed": st.record.is_some(),
            "state": st.record.map(|r| r.state.as_str()),
            "copy": match &st.copy {
                CopyState::Hub(ix) => json!({"blocks": ix.blocks.len(), "complete": ix.done}),
                CopyState::Other | CopyState::None => J::Null,
            },
            "text": st.describe(),
        }),
        Err(e) => json!({"error": e}),
    };
    v["in_use"] = in_use().into();
    v["ram_only"] = ram_only_reason().into();
    v["first_boot_claim"] = auto().into();
    v["max_blocks"] = SLOTS.into();
    v["busy"] = (*BUSY.lock().unwrap()).into();
    v["last"] = LAST.lock().unwrap().clone().into();
    v
}

/// `storage/*` requests (maintenance feature `storage`).
pub struct StorageHook;

impl hub_app::RequestHook for StorageHook {
    fn prefix(&self) -> &'static str {
        "storage/"
    }
    fn feature(&self) -> Option<&'static str> {
        Some("storage")
    }
    fn request(&mut self, ty: &str, req: &J) -> Result<J, ApiError> {
        match ty {
            "storage/status" => Ok(status_json()),
            "storage/claim" if req["confirm"] != J::Bool(true) => Err(ApiError::new(
                "confirm_required",
                "Claiming the files partition saves SONOFF's files into it, then makes it the hub's data region and restarts the hub; \
                 data kept in RAM until now are lost. Send again with \"confirm\": true.",
            )),
            "storage/claim" => start_claim().map(|()| json!({"state": "claiming"})).map_err(|e| ApiError::new("refused", e)),
            _ => Err(ApiError::new("invalid_format", format!("unknown request {ty}"))),
        }
    }
}
