//! Hub-mode network: Wi-Fi station (credentials in NVS `hub/wifi_ssid`,
//! `hub/wifi_pass`, set from the console) and the WebSocket JSON API on
//! `ws://<ip>/api` (esp_http_server), one API session per socket.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use embedded_svc::http::Method;
use embedded_svc::io::Write;
use embedded_svc::wifi::{AuthMethod, ClientConfiguration, Configuration};
use embedded_svc::ws::FrameType;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::http::server::{self, EspHttpServer, ws::EspHttpWsDetachedSender};
use esp_idf_svc::sys::{EspError, ESP_ERR_INVALID_SIZE};
use esp_idf_svc::wifi::{BlockingWifi, EspWifi};
use hub_app::{App, SessionSink};
use log::{info, warn};

use crate::mode;

/// Largest API frame accepted from a client.
const MAX_FRAME: usize = 16 * 1024;

/// Wi-Fi state for `hub status`.
pub static STATE: Mutex<String> = Mutex::new(String::new());

struct WsSink(EspHttpWsDetachedSender);

impl SessionSink for WsSink {
    fn send(&mut self, text: &str) -> bool {
        !self.0.is_closed() && self.0.send(FrameType::Text(false), text.as_bytes()).is_ok()
    }
    fn alive(&self) -> bool {
        !self.0.is_closed()
    }
}

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
        EspHttpServer::new(&server::Configuration { stack_size: 8192, ..Default::default() }).map_err(|e| e.0)?;
    // The web UI: one pre-compressed page that talks to /api (crates/hub-web).
    server.fn_handler("/", Method::Get, |req| {
        if req.header("If-None-Match").is_some_and(hub_web::not_modified) {
            let headers = [("ETag", hub_web::ETAG), ("Cache-Control", "no-cache")];
            return req.into_response(304, Some("Not Modified"), &headers).map(|_| ());
        }
        req.into_response(200, None, &hub_web::HEADERS)?.write_all(hub_web::INDEX_HTML_GZ).map(|_| ())
    })?;
    let sessions: Arc<Mutex<BTreeMap<i32, u64>>> = Arc::default();
    server.ws_handler("/api", None, move |ws| {
        let mut map = sessions.lock().unwrap();
        if ws.is_new() {
            let sender = ws.create_detached_sender()?;
            if let Some(sid) = app.connect(Box::new(WsSink(sender))) {
                map.insert(ws.session(), sid);
                info!("api: socket {} session {sid}", ws.session());
            }
            return Ok(());
        }
        if ws.is_closed() {
            if let Some(sid) = map.remove(&ws.session()) {
                app.disconnect(sid);
            }
            return Ok(());
        }
        let (kind, len) = ws.recv(&mut [])?;
        if matches!(kind, FrameType::Close | FrameType::SocketClose) {
            // A browser closing the page sends a Close frame; answer it and
            // end the session (esp_http_server then closes the socket).
            let _ = ws.send(FrameType::Close, &[]);
            if let Some(sid) = map.remove(&ws.session()) {
                app.disconnect(sid);
            }
            return Ok(());
        }
        if len > MAX_FRAME {
            ws.send(FrameType::Close, &[])?;
            return Err(EspError::from_infallible::<ESP_ERR_INVALID_SIZE>());
        }
        let mut buf = vec![0u8; len];
        ws.recv(&mut buf)?;
        if kind == FrameType::Ping {
            return ws.send(FrameType::Pong, &buf);
        }
        if !matches!(kind, FrameType::Text(_)) {
            return Ok(());
        }
        while buf.last() == Some(&0) {
            buf.pop();
        }
        if let (Some(sid), Ok(text)) = (map.get(&ws.session()), String::from_utf8(buf)) {
            app.text(*sid, text);
        }
        Ok::<(), EspError>(())
    })?;
    Ok(server)
}
