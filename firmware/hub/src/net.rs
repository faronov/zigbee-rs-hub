//! Hub-mode network: Wi-Fi station (credentials in NVS `hub/wifi_ssid`,
//! `hub/wifi_pass`, set from the console) and the WebSocket JSON API on
//! `ws://<ip>/api` (esp_http_server, see `ws`), one API session per socket.

use std::sync::Mutex;
use std::time::Duration;

use embedded_svc::http::Method;
use embedded_svc::io::Write;
use embedded_svc::wifi::{AuthMethod, ClientConfiguration, Configuration};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::http::server::{self, EspHttpServer};
use esp_idf_svc::sys::{EspError, esp};
use esp_idf_svc::wifi::{BlockingWifi, EspWifi};
use hub_app::App;
use log::{info, warn};

use crate::mode;

/// Wi-Fi state for `hub status`.
pub static STATE: Mutex<String> = Mutex::new(String::new());

fn set_state(s: String) {
    info!("wifi: {s}");
    *STATE.lock().unwrap() = s;
}

/// Starts Wi-Fi (if configured) and the API server on a thread.
pub fn start(app: App, modem: Modem<'static>, on_up: impl Fn(bool) + Send + 'static) {
    let Some(ssid) = mode::get_str("wifi_ssid") else {
        set_state("not configured (console: wifi set <ssid> <password>)".into());
        return;
    };
    let pass = mode::get_str("wifi_pass").unwrap_or_default();
    let spawned = std::thread::Builder::new().stack_size(8192).spawn(move || {
        if let Err(e) = run(app, modem, &ssid, &pass, on_up) {
            set_state(format!("error: {e}"));
        }
    });
    if let Err(e) = spawned {
        set_state(format!("thread: {e}"));
    }
}

fn run(app: App, modem: Modem<'static>, ssid: &str, pass: &str, on_up: impl Fn(bool)) -> Result<(), EspError> {
    let sys_loop = EspSystemEventLoop::take()?;
    let mut wifi = BlockingWifi::wrap(EspWifi::new(modem, sys_loop.clone(), mode::partition())?, sys_loop)?;
    let cfg = Configuration::Client(ClientConfiguration {
        ssid: ssid.try_into().unwrap_or_default(),
        password: pass.try_into().unwrap_or_default(),
        auth_method: if pass.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal },
        ..Default::default()
    });
    wifi.set_configuration(&cfg)?;
    wifi.start()?;
    // The hub is mains powered: modem sleep (the default) holds frames to the
    // hub until the next DTIM beacon, adding 100-300 ms and lost pings.
    esp!(unsafe { esp_idf_svc::sys::esp_wifi_set_ps(esp_idf_svc::sys::wifi_ps_type_t_WIFI_PS_NONE) })?;
    let mut server_up = false;
    loop {
        set_state(format!("connecting to {ssid:?}"));
        match wifi.connect().and_then(|_| wifi.wait_netif_up()) {
            Ok(()) => {
                let ip = wifi.wifi().sta_netif().get_ip_info().map(|i| i.ip.to_string()).unwrap_or_default();
                set_state(format!("up, ws://{ip}/api"));
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
                while wifi.is_connected().unwrap_or(false) {
                    std::thread::sleep(Duration::from_secs(2));
                }
                on_up(false);
                set_state("disconnected".into());
            }
            Err(e) => set_state(format!("connect failed: {e}")),
        }
        let _ = wifi.disconnect();
        std::thread::sleep(Duration::from_secs(5));
    }
}

fn api_server(app: App) -> Result<EspHttpServer<'static>, EspError> {
    let mut server =
        EspHttpServer::new(&server::Configuration {
            stack_size: 8192,
            // The web UI (page + WebSocket) and an integration at once; the
            // least recently used socket is purged when they run out.
            max_open_sockets: 6,
            // Peers that vanished without a FIN.
            keep_alive: Some(server::KeepAlive { idle_secs: 30, interval_secs: 5, probe_count: 3 }),
            ..Default::default()
        })
        .map_err(|e| e.0)?;
    // The web UI: one pre-compressed page that talks to /api (crates/hub-web).
    server.fn_handler("/", Method::Get, |req| {
        if req.header("If-None-Match").is_some_and(hub_web::not_modified) {
            let headers = [("ETag", hub_web::ETAG), ("Cache-Control", "no-cache")];
            return req.into_response(304, Some("Not Modified"), &headers).map(|_| ());
        }
        req.into_response(200, None, &hub_web::HEADERS)?.write_all(hub_web::INDEX_HTML_GZ).map(|_| ())
    })?;
    crate::ws::register(&server, app)?;
    Ok(server)
}
