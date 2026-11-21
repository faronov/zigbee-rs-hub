//! Hub-mode network: Wi-Fi station (credentials in NVS `hub/wifi_ssid`,
//! `hub/wifi_pass`, set from the console, `hub/wifi/set` or the setup page)
//! and the WebSocket JSON API on `ws://<ip>/api` (esp_http_server, see `ws`),
//! one API session per socket. The setup access point (`setup`) shares the
//! Wi-Fi thread and the HTTP server.
//!
//! New credentials take effect at once: the station reconnects with them.

use std::net::Ipv4Addr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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

use crate::{mode, setup};

/// Wi-Fi state for `hub status`.
pub static STATE: Mutex<String> = Mutex::new(String::new());

/// Credentials to switch to (from `hub/wifi/set`, the console or the setup
/// page).
static SWITCH: Mutex<Option<(String, String)>> = Mutex::new(None);
/// SSID and IP address of the running station.
static LINK: Mutex<(String, String)> = Mutex::new((String::new(), String::new()));

/// Interval of the station's connection attempts.
const RETRY: Duration = Duration::from_secs(20);
/// An attempt without an address after this long has failed.
const ATTEMPT: Duration = Duration::from_secs(15);
/// Poll interval of the Wi-Fi thread (setup AP, button, DNS, mDNS).
const POLL: Duration = Duration::from_millis(100);

/// Store new credentials and use them now.
pub fn configure(ssid: &str, pass: &str) -> Result<(), String> {
    mode::set_str("wifi_ssid", Some(ssid))?;
    mode::set_str("wifi_pass", Some(pass))?;
    *SWITCH.lock().unwrap() = Some((ssid.to_string(), pass.to_string()));
    Ok(())
}

/// `hub/system` `wifi`: SSID, state, IP, and the AP's RSSI and channel while
/// connected; the setup AP while it is open.
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
    if let Some(s) = setup::info() {
        v["setup_ap"] = s;
    }
    v
}

/// Primary channel and RSSI of the access point, while associated.
pub fn ap() -> Option<(u8, i8)> {
    let mut rec: esp_idf_svc::sys::wifi_ap_record_t = unsafe { core::mem::zeroed() };
    (unsafe { esp_idf_svc::sys::esp_wifi_sta_get_ap_info(&mut rec) } == esp_idf_svc::sys::ESP_OK).then_some((rec.primary, rec.rssi))
}

fn set_state(s: String) {
    info!("wifi: {s}");
    *STATE.lock().unwrap() = s;
}

/// Starts Wi-Fi and the API server on a thread (`on_up`: the radio is in
/// use, station or setup AP, for PTA coexistence): the station if configured,
/// the setup AP when `setup::Policy` wants it. Without an app (no PSRAM,
/// wrong-image mode) only the station with stored credentials and the
/// wrong-image page: no setup AP, no Wi-Fi at all without credentials.
pub fn start(app: Option<App>, modem: Modem<'static>, on_up: impl Fn(bool) + Send + 'static) {
    let creds = mode::get_str("wifi_ssid").map(|s| (s, mode::get_str("wifi_pass").unwrap_or_default()));
    if creds.is_none() {
        if app.is_none() {
            set_state("not configured (console: wifi set <ssid> <password>, used from the next boot)".into());
            return;
        }
        set_state("not configured (setup AP, console: wifi set <ssid> <password>, or hub/wifi/set)".into());
    }
    let spawned = std::thread::Builder::new().stack_size(8192).spawn(move || {
        if let Err(e) = run(app, modem, creds, Box::new(on_up)) {
            set_state(format!("error: {e}"));
        }
    });
    if let Err(e) = spawned {
        set_state(format!("thread: {e}"));
    }
}

fn client(ssid: &str, pass: &str) -> ClientConfiguration {
    ClientConfiguration {
        ssid: ssid.try_into().unwrap_or_default(),
        password: pass.try_into().unwrap_or_default(),
        auth_method: if pass.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal },
        ..Default::default()
    }
}

/// The station's address while it is connected and has one.
fn sta_ip(wifi: &BlockingWifi<EspWifi<'static>>) -> Option<Ipv4Addr> {
    let netif = wifi.wifi().sta_netif();
    let ip = netif.get_ip_info().ok()?.ip;
    (netif.is_up().unwrap_or(false) && !ip.is_unspecified()).then_some(ip)
}

fn run(app: Option<App>, modem: Modem<'static>, mut creds: Option<(String, String)>, on_up: Box<dyn Fn(bool) + Send>) -> Result<(), EspError> {
    let sys_loop = EspSystemEventLoop::take()?;
    let mut wifi = BlockingWifi::wrap(EspWifi::new(modem, sys_loop.clone(), mode::partition())?, sys_loop)?;
    setup::prepare(wifi.wifi());
    let mut conf = creds.as_ref().map(|(s, p)| client(s, p)).unwrap_or_default();
    wifi.set_configuration(&Configuration::Client(conf.clone()))?;
    wifi.start()?;
    // The hub is mains powered: modem sleep (the default) holds frames to the
    // hub until the next DTIM beacon, adding 100-300 ms and lost pings.
    esp!(unsafe { esp_idf_svc::sys::esp_wifi_set_ps(esp_idf_svc::sys::wifi_ps_type_t_WIFI_PS_NONE) })?;
    let app_runs = app.is_some();
    // Runs for the rest of the boot; serves the setup page too.
    match api_server(app) {
        Ok(s) => std::mem::forget(s),
        Err(e) => warn!("api server: {e}"),
    }
    let mut portal = setup::Portal::new(app_runs);
    let mut up = false;
    let mut radio = false;
    let mut attempt: Option<Instant> = None;
    let mut reported = false;
    loop {
        if let Some((s, p)) = SWITCH.lock().unwrap().take() {
            info!("wifi: switching to {s:?}");
            let _ = wifi.wifi_mut().disconnect();
            conf = client(&s, &p);
            wifi.set_configuration(&setup::configuration(conf.clone(), setup::is_open()))?;
            creds = Some((s, p));
            attempt = None;
        }
        let ip = sta_ip(&wifi);
        if ip.is_some() != up {
            up = ip.is_some();
            crate::platform::wifi_changed(up);
            match ip {
                Some(ip) => {
                    set_state(format!("up, ws://{ip}/api"));
                    LINK.lock().unwrap().1 = ip.to_string();
                }
                None => {
                    set_state("disconnected".into());
                    LINK.lock().unwrap().1.clear();
                }
            }
        }
        let (ap_open, clients) = portal.poll(&mut wifi, &conf, ip);
        // PTA coexistence while the radio is in use (station or setup AP).
        if (up || ap_open) != radio {
            radio = up || ap_open;
            on_up(radio);
        }
        if let (Some((ssid, _)), false) = (&creds, up) {
            // Attempts scan all channels and take the AP off its channel:
            // none while someone is on the setup page, except right after
            // new credentials.
            let due = attempt.is_none_or(|t| t.elapsed() >= RETRY && !(ap_open && clients > 0));
            if due {
                *LINK.lock().unwrap() = (ssid.clone(), String::new());
                set_state(format!("connecting to {ssid:?}"));
                // Associated but without an address (DHCP): start over.
                if wifi.wifi().driver().is_sta_connected().unwrap_or(false) {
                    let _ = wifi.wifi_mut().disconnect();
                }
                if let Err(e) = wifi.wifi_mut().connect() {
                    warn!("wifi: connect: {e}");
                }
                attempt = Some(Instant::now());
                reported = false;
            } else if !reported && attempt.is_some_and(|t| t.elapsed() >= ATTEMPT) {
                set_state(format!("could not connect to {ssid:?} (password? in range?); retrying"));
                reported = true;
            }
        }
        std::thread::sleep(POLL);
    }
}

/// Closes the request's socket once the response is out: idle keep-alive
/// sockets of page loads (often never closed by the browser) would make
/// the server purge the least recently used socket — an API client's.
pub fn close_after(req: *mut esp_idf_svc::sys::httpd_req_t) {
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
            // The web UI (page + WebSocket) and integrations at once, and the
            // setup page while the AP is open; page requests close their
            // socket (`close_after`). With PSRAM the least recently used
            // socket is purged when they run out; without it a new
            // connection is refused instead, so Home Assistant keeps its
            // session. The whole socket budget: `Resources::sockets`.
            max_open_sockets: r.http_sockets,
            lru_purge_enable: r.http_lru_purge,
            // Peers that vanished without a FIN.
            keep_alive: Some(server::KeepAlive { idle_secs: 30, interval_secs: 5, probe_count: 3 }),
            // The setup AP's catch-all (`/*`, captive portal probes).
            uri_match_wildcard: true,
            ..Default::default()
        })
        .map_err(|e| e.0)?;
    // The web UI: one pre-compressed page that talks to /api (crates/hub-web).
    server.fn_handler("/", Method::Get, |mut req| {
        close_after(req.connection().handle());
        if let Some(to) = setup::ap_redirect(req.connection().handle()) {
            return req.into_response(302, None, &[("Location", to.as_str()), ("Connection", "close")]).map(|_| ());
        }
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
    setup::register(&server)?;
    Ok(server)
}

/// The server of a hub that did not start its app: the page says why, the
/// API answers 503.
fn wrong_image_server() -> Result<EspHttpServer<'static>, EspError> {
    let mut server = EspHttpServer::new(&server::Configuration {
        stack_size: 6144,
        max_open_sockets: hub_core::resources::WRONG_IMAGE_HTTP_SOCKETS,
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
