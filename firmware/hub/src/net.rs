//! Hub-mode network: Wi-Fi station (credentials in NVS `hub/wifi_ssid`,
//! `hub/wifi_pass`, set from the console or `hub/wifi/set`) and the
//! WebSocket JSON API on `ws://<ip>/api` (esp_http_server, see `ws`), one
//! API session per socket.
//!
//! New credentials from the API take effect at once: the station thread
//! reconnects with them, or starts if the hub had none at boot.

use std::sync::Mutex;
use std::time::Duration;

use embedded_svc::http::Method;
use embedded_svc::io::Write;
use embedded_svc::wifi::{AuthMethod, ClientConfiguration, Configuration};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::handle::RawHandle;
use esp_idf_svc::http::server::{self, EspHttpServer};
use esp_idf_svc::sys::{EspError, esp};
use esp_idf_svc::wifi::{BlockingWifi, EspWifi};
use hub_app::App;
use log::{info, warn};

use crate::mode;

/// Wi-Fi state for `hub status`.
pub static STATE: Mutex<String> = Mutex::new(String::new());

/// What the station thread needs; kept here while no credentials exist.
struct Pending {
    app: Option<App>,
    modem: Modem<'static>,
    on_up: Box<dyn Fn(bool) + Send>,
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
/// Credentials to switch to (from `hub/wifi/set`).
static SWITCH: Mutex<Option<(String, String)>> = Mutex::new(None);
/// SSID and IP address of the running station.
static LINK: Mutex<(String, String)> = Mutex::new((String::new(), String::new()));

/// Store new credentials and use them now: reconnect, or start the
/// station if the hub booted without any.
pub fn configure(ssid: &str, pass: &str) -> Result<(), String> {
    mode::set_str("wifi_ssid", Some(ssid))?;
    mode::set_str("wifi_pass", Some(pass))?;
    let pending = PENDING.lock().unwrap().take();
    match pending {
        Some(p) => spawn(p.app, p.modem, ssid.to_string(), pass.to_string(), p.on_up),
        None => *SWITCH.lock().unwrap() = Some((ssid.to_string(), pass.to_string())),
    }
    Ok(())
}

/// `hub/system` `wifi`: SSID, state, IP, and the AP's RSSI and channel while
/// connected.
pub fn info() -> serde_json::Value {
    let (ssid, ip) = LINK.lock().unwrap().clone();
    let mut v = serde_json::json!({"state": STATE.lock().unwrap().clone()});
    if !ssid.is_empty() {
        v["ssid"] = ssid.into();
    }
    if !ip.is_empty() {
        v["ip"] = ip.into();
    }
    if let Some((channel, rssi)) = ap() {
        v["channel"] = channel.into();
        v["rssi"] = rssi.into();
    }
    v
}

/// Primary channel and RSSI of the access point, while associated.
pub fn ap() -> Option<(u8, i8)> {
    let mut rec: esp_idf_svc::sys::wifi_ap_record_t = unsafe { core::mem::zeroed() };
    (unsafe { esp_idf_svc::sys::esp_wifi_sta_get_ap_info(&mut rec) } == esp_idf_svc::sys::ESP_OK).then_some((rec.primary, rec.rssi))
}

fn spawn(app: Option<App>, modem: Modem<'static>, ssid: String, pass: String, on_up: Box<dyn Fn(bool) + Send>) {
    let spawned = std::thread::Builder::new().stack_size(8192).spawn(move || {
        if let Err(e) = run(app, modem, ssid, pass, on_up) {
            set_state(format!("error: {e}"));
        }
    });
    if let Err(e) = spawned {
        set_state(format!("thread: {e}"));
    }
}

fn set_state(s: String) {
    info!("wifi: {s}");
    *STATE.lock().unwrap() = s;
}

/// Starts Wi-Fi (if configured) and the API server on a thread; without
/// an app (no PSRAM) only the wrong-image page.
pub fn start(app: Option<App>, modem: Modem<'static>, on_up: impl Fn(bool) + Send + 'static) {
    let Some(ssid) = mode::get_str("wifi_ssid") else {
        set_state("not configured (console: wifi set <ssid> <password>, or hub/wifi/set)".into());
        *PENDING.lock().unwrap() = Some(Pending { app, modem, on_up: Box::new(on_up) });
        return;
    };
    let pass = mode::get_str("wifi_pass").unwrap_or_default();
    spawn(app, modem, ssid, pass, Box::new(on_up));
}

fn client(ssid: &str, pass: &str) -> Configuration {
    Configuration::Client(ClientConfiguration {
        ssid: ssid.try_into().unwrap_or_default(),
        password: pass.try_into().unwrap_or_default(),
        auth_method: if pass.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal },
        ..Default::default()
    })
}

fn run(app: Option<App>, modem: Modem<'static>, mut ssid: String, pass: String, on_up: Box<dyn Fn(bool) + Send>) -> Result<(), EspError> {
    let sys_loop = EspSystemEventLoop::take()?;
    let mut wifi = BlockingWifi::wrap(EspWifi::new(modem, sys_loop.clone(), mode::partition())?, sys_loop)?;
    wifi.set_configuration(&client(&ssid, &pass))?;
    wifi.start()?;
    // The hub is mains powered: modem sleep (the default) holds frames to the
    // hub until the next DTIM beacon, adding 100-300 ms and lost pings.
    esp!(unsafe { esp_idf_svc::sys::esp_wifi_set_ps(esp_idf_svc::sys::wifi_ps_type_t_WIFI_PS_NONE) })?;
    let mut server_up = false;
    loop {
        if let Some((s, p)) = SWITCH.lock().unwrap().take() {
            info!("wifi: switching to {s:?}");
            let _ = wifi.disconnect();
            wifi.set_configuration(&client(&s, &p))?;
            ssid = s;
        }
        *LINK.lock().unwrap() = (ssid.clone(), String::new());
        set_state(format!("connecting to {ssid:?}"));
        match wifi.connect().and_then(|_| wifi.wait_netif_up()) {
            Ok(()) => {
                let ip = wifi.wifi().sta_netif().get_ip_info().map(|i| i.ip.to_string()).unwrap_or_default();
                set_state(format!("up, ws://{ip}/api"));
                LINK.lock().unwrap().1 = ip.clone();
                on_up(true);
                if !server_up {
                    match api_server(app.clone()) {
                        Ok(s) => {
                            // Runs for the rest of the boot.
                            std::mem::forget(s);
                            server_up = true;
                        }
                        Err(e) => warn!("api server: {e}"),
                    }
                }
                while wifi.is_connected().unwrap_or(false) && SWITCH.lock().unwrap().is_none() {
                    std::thread::sleep(Duration::from_secs(1));
                }
                on_up(false);
                set_state("disconnected".into());
            }
            Err(e) => set_state(format!("connect failed: {e}")),
        }
        let _ = wifi.disconnect();
        LINK.lock().unwrap().1.clear();
        if SWITCH.lock().unwrap().is_none() {
            std::thread::sleep(Duration::from_secs(5));
        }
    }
}

/// Closes the request's socket once the response is out: idle keep-alive
/// sockets of page loads (often never closed by the browser) would make
/// the server purge the least recently used socket — an API client's.
fn close_after(req: *mut esp_idf_svc::sys::httpd_req_t) {
    unsafe {
        let fd = esp_idf_svc::sys::httpd_req_to_sockfd(req);
        esp_idf_svc::sys::httpd_sess_trigger_close((*req).handle, fd);
    }
}

fn api_server(app: Option<App>) -> Result<EspHttpServer<'static>, EspError> {
    let Some(app) = app else { return wrong_image_server() };
    let r = crate::resources::get();
    let mut server =
        EspHttpServer::new(&server::Configuration {
            stack_size: 8192,
            // The web UI (page + WebSocket) and integrations at once; page
            // requests close their socket (`close_after`). With PSRAM the
            // least recently used socket is purged when they run out;
            // without it a new connection is refused instead, so Home
            // Assistant keeps its session. 7 + the server's 3 =
            // CONFIG_LWIP_MAX_SOCKETS.
            max_open_sockets: r.http_sockets,
            lru_purge_enable: r.http_lru_purge,
            // Peers that vanished without a FIN.
            keep_alive: Some(server::KeepAlive { idle_secs: 30, interval_secs: 5, probe_count: 3 }),
            ..Default::default()
        })
        .map_err(|e| e.0)?;
    // The web UI: one pre-compressed page that talks to /api (crates/hub-web).
    server.fn_handler("/", Method::Get, |mut req| {
        close_after(req.connection().handle());
        if req.header("If-None-Match").is_some_and(hub_web::not_modified) {
            let headers = [("ETag", hub_web::ETAG), ("Cache-Control", "no-cache"), ("Connection", "close")];
            return req.into_response(304, Some("Not Modified"), &headers).map(|_| ());
        }
        let headers = [hub_web::HEADERS.as_slice(), &[("Connection", "close")]].concat();
        req.into_response(200, None, &headers)?.write_all(hub_web::INDEX_HTML_GZ).map(|_| ())
    })?;
    // Web tools from hubdata (the definition compiler of the web UI).
    for (uri, name) in [("/zdefc/zdefc_wasm.js", "zdefc_wasm.js"), ("/zdefc/zdefc_wasm_bg.wasm", "zdefc_wasm_bg.wasm")] {
        let web = app.web().clone();
        server.fn_handler(uri, Method::Get, move |mut req| {
            close_after(req.connection().handle());
            let fresh = req.header("If-None-Match").map(str::to_string);
            web.serve(name, |f| match f {
                None => req.into_status_response(404).map(|_| ()),
                Some(f) if fresh.as_deref() == Some(f.etag) => {
                    req.into_response(304, None, &[("ETag", f.etag), ("Connection", "close")]).map(|_| ())
                }
                Some(f) => {
                    let headers = [
                        ("Content-Type", f.content_type),
                        ("Content-Encoding", "gzip"),
                        ("Cache-Control", "no-cache"),
                        ("ETag", f.etag),
                        ("Connection", "close"),
                    ];
                    let mut resp = req.into_response(200, None, &headers)?;
                    for c in f.data.chunks(4096) {
                        resp.write_all(c)?;
                    }
                    Ok(())
                }
            })
        })?;
    }
    crate::ws::register(&server, app)?;
    Ok(server)
}

/// The server of a hub that did not start its app: the page says why, the
/// API answers 503.
fn wrong_image_server() -> Result<EspHttpServer<'static>, EspError> {
    let mut server = EspHttpServer::new(&server::Configuration {
        stack_size: 6144,
        max_open_sockets: 2,
        ..Default::default()
    })
    .map_err(|e| e.0)?;
    server.fn_handler("/", Method::Get, |mut req| {
        close_after(req.connection().handle());
        let page = format!(
            "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\"><title>zigbee-rs hub</title>\
             <h1>zigbee-rs hub: wrong firmware image</h1><p>{}</p>",
            crate::resources::wrong_image()
        );
        let headers = [("Content-Type", "text/html; charset=utf-8"), ("Cache-Control", "no-store"), ("Connection", "close")];
        req.into_response(200, None, &headers)?.write_all(page.as_bytes()).map(|_| ())
    })?;
    server.fn_handler("/api", Method::Get, |mut req| {
        close_after(req.connection().handle());
        let body = serde_json::json!({"error": "psram_missing", "message": crate::resources::wrong_image()}).to_string();
        let headers = [("Content-Type", "application/json"), ("Connection", "close")];
        req.into_response(503, Some("Service Unavailable"), &headers)?.write_all(body.as_bytes()).map(|_| ())
    })?;
    Ok(server)
}
