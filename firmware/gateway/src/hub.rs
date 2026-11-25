//! The hub application (`hub-app`) on the EZSP backend
//! (`hub_app::Backend::ezsp`) over the NCP UART: the same application,
//! web UI, WebSocket API, MQTT bridge, catalog and automations as the S3
//! hub (`firmware/hub`), with the gateway's platform
//! ([`Gateway`](crate::platform::Gateway)), its data on SONOFF's `files`
//! partition, claimed at the first boot ([`files`](crate::files), RAM only
//! if that is refused), the Zigbee device OTA image store there too, the
//! `storage/*` and `firmware/*` requests, and the button's permit join.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use hub_app::catalog::data::DataFlash;
use hub_app::{App, AppConfig, Backend, CatalogSetup, OtaFlash, SessionSink};
use hub_core::profile::BoardProfile;
use hub_ezsp::SystemClock;
use hub_ezsp::coord::EzspOptions;
use hub_store::Store;
use log::{error, info, warn};
use zrh_espapp::cpu;
use zrh_espapp::mqtt::{SETTINGS_MAX, SettingsStore};

use crate::{ezsp, files, platform, settings, update};

/// The running application.
pub static APP: OnceLock<App> = OnceLock::new();
static MQTT: OnceLock<hub_mqtt::MqttService> = OnceLock::new();

/// Stack of the application thread (engine handlers, serde_json), as on
/// the S3.
const APP_STACK: usize = 40 * 1024;
/// Stack of the maintenance job thread (network map, backup), allocated per
/// job from internal RAM.
const ADMIN_STACK: usize = 12 * 1024;
/// Permit join of a short button press.
pub const BUTTON_PERMIT_S: u32 = 180;

/// The MQTT bridge's settings in NVS `zrh/mqtt`.
struct MqttSettings;

impl SettingsStore for MqttSettings {
    fn load(&self) -> Option<String> {
        settings::get_long("mqtt", SETTINGS_MAX)
    }
    fn save(&self, json: &str) -> Result<(), String> {
        if json.len() >= SETTINGS_MAX {
            return Err(format!("settings too long ({} B)", json.len()));
        }
        settings::set("mqtt", Some(json))
    }
}

/// The API token (generated and stored at the first boot with the hub,
/// from an entropy source: `entropy`).
pub fn token() -> String {
    settings::get("api_token").unwrap_or_else(|| {
        let t = crate::entropy::token();
        if let Err(e) = settings::set("api_token", Some(&t)) {
            warn!("API token not stored: {e}");
        }
        t
    })
}

/// Starts the application (before the network: the HTTP server starts
/// when an interface has an address).
pub fn start(board: &BoardProfile) -> Option<App> {
    let port = match ezsp::BackendPort::take() {
        Ok(p) => p,
        Err(u) => {
            error!("hub not started: the MG24 UART is in use ({u:?})");
            return None;
        }
    };
    // The token first (from an entropy source, before Wi-Fi), then the
    // first boot's claim.
    let api_token = token();
    let on_flash = files::boot();
    let store = if on_flash {
        match files::journal().map(|f| Store::open(Box::new(f))) {
            Some(Ok(s)) => {
                info!("host store: files journal {:?}", s.stats());
                s
            }
            Some(Err(e)) => {
                warn!("host store: files journal unusable ({e}); RAM only");
                Store::volatile()
            }
            None => Store::volatile(),
        }
    } else {
        Store::volatile()
    };
    let r = zrh_espapp::resources::get();
    let mut cfg = AppConfig {
        hub_name: format!("zigbee-rs-hub {} ({})", env!("CARGO_PKG_VERSION"), board.name),
        api_token,
        stack_size: APP_STACK,
        // The example definitions take ~90 KB decoded (PSRAM).
        builtin_catalog: r.builtin_catalog,
        ota_streams: r.ota_streams,
        platform: Some(Arc::new(platform::Gateway)),
        admin_stack: ADMIN_STACK,
        ..Default::default()
    };
    let mut catalog = None;
    if on_flash {
        match files::ota_store() {
            Some(f) => cfg.ota_flash = OtaFlash::new(Box::new(f)),
            None => warn!("OTA image store: no ota_store area; streamed images only"),
        }
        catalog = files::catalog().map(|f| CatalogSetup {
            flash: Arc::new(Mutex::new(Box::new(f) as Box<dyn DataFlash>)),
            extra_keys: Vec::new(),
        });
    }
    let opts = EzspOptions { baud: board.uart.line.baud, flow_control: board.uart.line.flow, ..Default::default() };
    let backend = Backend::ezsp(port, SystemClock::default(), opts, store);
    let app = cpu::named(c"hub-app", || App::start_backend(backend, cfg, catalog));
    app.add_hook(Box::new(files::StorageHook));
    app.add_hook(Box::new(update::FirmwareHook));
    let _ = MQTT.set(zrh_espapp::mqtt::start(&app, r, Arc::new(MqttSettings)));
    if let Some(sid) = app.connect_trusted(Box::new(LocalSink)) {
        let _ = LOCAL.set(sid);
    }
    let _ = APP.set(app.clone());
    info!("hub: application started on the EZSP backend ({} Bd)", board.uart.line.baud);
    Some(app)
}

/// Stops the application for `stock confirm` (it saves the registry
/// first), then closes the `files` gate: no write of the app, of a
/// maintenance job thread or of anything else holding a window reaches the
/// region afterwards. Refused while a maintenance job runs (it writes the
/// records from its own thread) or when the app does not stop.
pub fn stop() -> Result<(), String> {
    if let Some(app) = APP.get() {
        let jobs = app.status().jobs;
        if jobs > 0 {
            return Err(format!(
                "{jobs} maintenance job(s) running (network/jobs: a scan, a channel change, a network reset or restore): wait for it or cancel it"
            ));
        }
        if !app.shutdown() {
            return Err("the hub did not stop within 5 s".into());
        }
    }
    files::close_gate();
    Ok(())
}

/// Replies to the board's own requests (button, console): logged.
struct LocalSink;

impl SessionSink for LocalSink {
    fn send(&mut self, text: &str) -> bool {
        if text.contains("\"success\":false") {
            warn!("hub: {text}");
        } else if text.contains("\"type\":\"result\"") {
            info!("hub: {text}");
        }
        true
    }
}

/// The board's API session (trusted, in process).
static LOCAL: OnceLock<u64> = OnceLock::new();
static NEXT_ID: AtomicU32 = AtomicU32::new(1);

/// Opens joining for `secs` (0 closes it), as `network/permit_join`; the
/// result is logged.
pub fn permit_join(secs: u32) -> Result<(), String> {
    let (Some(app), Some(&sid)) = (APP.get(), LOCAL.get()) else {
        return Err("the hub is not running".into());
    };
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    app.text(sid, format!(r#"{{"id":{id},"type":"network/permit_join","duration_s":{secs}}}"#));
    Ok(())
}

/// `hub` on the console.
pub fn status_text() -> String {
    let Some(app) = APP.get() else {
        return "hub: not running (bring-up boot, or no PSRAM)\r\n".into();
    };
    let a = app.status();
    format!(
        "hub: EZSP session {}, Zigbee network {}, permit join {} s\r\n\
         app: {} devices ({} restored, {} imported), {} API sessions, registry writes {} errors {}, APS ok {} busy {} failed {} delivered {} not delivered {} unconfirmed {}\r\n\
         store: {}\r\n",
        if a.session_up { "up" } else { "down" },
        if a.network_up { "up" } else { "not up" },
        a.permit_join_s,
        a.devices,
        a.restored,
        a.imported,
        a.sessions,
        a.registry_writes,
        a.registry_errors,
        a.aps.accepted,
        a.aps.busy,
        a.aps.failed,
        a.aps.delivered,
        a.aps.not_delivered,
        a.aps.unconfirmed,
        files::ram_only_reason().map_or("files region".into(), |why| format!("!!! RAM only, lost at every restart: {why}")),
    )
}
