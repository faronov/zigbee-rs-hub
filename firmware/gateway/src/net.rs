//! Network: Ethernet (classic ESP32 EMAC + IP101 RMII PHY with the 50 MHz
//! clock in on GPIO0, Dongle-M) and/or Wi-Fi station (credentials in NVS),
//! the WebSocket JSON API on `ws://<ip>/api` and the radio-image HTTP
//! endpoints (`POST /radio/gbl`, `POST /radio/install`, `GET /radio`,
//! token in `Authorization: Bearer <token>`).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use embedded_svc::http::{Headers, Method};
use embedded_svc::io::Write;
use embedded_svc::wifi::{AuthMethod, ClientConfiguration, Configuration};
use embedded_svc::ws::FrameType;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::http::server::{self, EspHttpServer, ws::EspHttpWsDetachedSender};
use esp_idf_svc::sys::{ESP_ERR_INVALID_SIZE, EspError};
use esp_idf_svc::wifi::{BlockingWifi, EspWifi};
use hub_app::{App, SessionSink};
use log::{info, warn};

use crate::{radio, settings};

/// Largest API frame accepted from a client.
const MAX_FRAME: usize = 16 * 1024;

/// Network state for the console, the LED and the API.
#[derive(Debug, Default, Clone)]
pub struct NetState {
    /// Ethernet: `-` (none on the board), `down`, `up 192.0.2.10`.
    pub eth: String,
    /// Wi-Fi state text.
    pub wifi: String,
    /// Some interface has an address.
    pub up: bool,
    /// The API server runs.
    pub api: bool,
}

/// Current network state.
pub static STATE: Mutex<NetState> = Mutex::new(NetState { eth: String::new(), wifi: String::new(), up: false, api: false });

fn update(f: impl FnOnce(&mut NetState)) {
    let mut s = STATE.lock().unwrap();
    f(&mut s);
    let eth_up = s.eth.starts_with("up");
    let wifi_up = s.wifi.starts_with("up");
    s.up = eth_up || wifi_up;
}

struct WsSink(EspHttpWsDetachedSender);

impl SessionSink for WsSink {
    fn send(&mut self, text: &str) -> bool {
        !self.0.is_closed() && self.0.send(FrameType::Text(false), text.as_bytes()).is_ok()
    }
}

/// What the network thread needs.
pub struct NetParts {
    /// Wi-Fi radio.
    pub modem: Modem<'static>,
    /// Ethernet (Dongle-M).
    #[cfg(esp32)]
    pub eth: Option<crate::eth::EthParts>,
}

/// Start the network and the API server (own threads).
pub fn start(app: App, parts: NetParts, on_wifi: impl Fn(bool) + Send + 'static) {
    let sys_loop = match EspSystemEventLoop::take() {
        Ok(l) => l,
        Err(e) => {
            warn!("event loop: {e}");
            return;
        }
    };
    update(|s| s.eth = "-".into());
    #[cfg(esp32)]
    if let Some(eth) = parts.eth {
        crate::eth::start(eth, sys_loop.clone(), |text: String| update(|s| s.eth = text));
    }
    match settings::get("wifi_ssid") {
        Some(ssid) => {
            let pass = settings::get("wifi_pass").unwrap_or_default();
            let (modem, sl) = (parts.modem, sys_loop.clone());
            let spawned = std::thread::Builder::new().name("wifi".into()).stack_size(6144).spawn(move || {
                if let Err(e) = wifi(modem, sl, &ssid, &pass, on_wifi) {
                    update(|s| s.wifi = format!("error: {e}"));
                }
            });
            if let Err(e) = spawned {
                update(|s| s.wifi = format!("thread: {e}"));
            }
        }
        None => update(|s| s.wifi = "not configured (console: wifi set <ssid> <password>)".into()),
    }
    match api_server(app) {
        Ok(server) => {
            // Runs for the rest of the boot.
            std::mem::forget(server);
            update(|s| s.api = true);
        }
        Err(e) => warn!("api server: {e}"),
    }
}

fn wifi(modem: Modem<'static>, sys_loop: EspSystemEventLoop, ssid: &str, pass: &str, on_up: impl Fn(bool)) -> Result<(), EspError> {
    let mut wifi = BlockingWifi::wrap(EspWifi::new(modem, sys_loop.clone(), settings::partition())?, sys_loop)?;
    let cfg = Configuration::Client(ClientConfiguration {
        ssid: ssid.try_into().unwrap_or_default(),
        password: pass.try_into().unwrap_or_default(),
        auth_method: if pass.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal },
        ..Default::default()
    });
    wifi.set_configuration(&cfg)?;
    wifi.start()?;
    loop {
        update(|s| s.wifi = format!("connecting to {ssid:?}"));
        match wifi.connect().and_then(|_| wifi.wait_netif_up()) {
            Ok(()) => {
                let ip = wifi.wifi().sta_netif().get_ip_info().map(|i| i.ip.to_string()).unwrap_or_default();
                info!("wifi: up, ws://{ip}/api");
                update(|s| s.wifi = format!("up {ip}"));
                on_up(true);
                while wifi.is_connected().unwrap_or(false) {
                    std::thread::sleep(Duration::from_secs(2));
                }
                on_up(false);
                update(|s| s.wifi = "disconnected".into());
            }
            Err(e) => update(|s| s.wifi = format!("connect failed: {e}")),
        }
        let _ = wifi.disconnect();
        std::thread::sleep(Duration::from_secs(5));
    }
}

fn authorized(header: Option<&str>) -> bool {
    let Some(token) = settings::get("api_token") else { return false };
    header.and_then(|h| h.strip_prefix("Bearer ")).is_some_and(|t| t.trim() == token)
}

fn api_server(app: App) -> Result<EspHttpServer<'static>, EspError> {
    let mut server = EspHttpServer::new(&server::Configuration {
        stack_size: 8192,
        max_uri_handlers: 8,
        ..Default::default()
    })
    .map_err(|e| e.0)?;
    server.fn_handler("/", Method::Get, |req| {
        req.into_ok_response()?
            .write_all(b"zigbee-rs-hub: WebSocket JSON API at /api, radio images at /radio\n")
            .map(|_| ())
    })?;
    server.fn_handler("/radio", Method::Get, |req| {
        let text = radio::status_text();
        req.into_ok_response()?.write_all(text.as_bytes()).map(|_| ())
    })?;
    server.fn_handler("/radio/gbl", Method::Post, |mut req| {
        if !authorized(req.header("Authorization")) {
            return req.into_status_response(401)?.write_all(b"token required\n").map(|_| ());
        }
        let Some(len) = req.content_len() else {
            return req.into_status_response(411)?.write_all(b"Content-Length required\n").map(|_| ());
        };
        let result = radio::upload(len as u32, |buf| req.read(buf).map_err(|e| std::io::Error::other(format!("{e:?}"))));
        match result {
            Ok(msg) => req.into_ok_response()?.write_all(format!("{msg}\n").as_bytes()).map(|_| ()),
            Err(e) => req.into_status_response(400)?.write_all(format!("{e}\n").as_bytes()).map(|_| ()),
        }
    })?;
    server.fn_handler("/radio/install", Method::Post, |req| {
        if !authorized(req.header("Authorization")) {
            return req.into_status_response(401)?.write_all(b"token required\n").map(|_| ());
        }
        let bank = match req.uri().split_once("bank=").map(|(_, b)| b) {
            Some(b) if b.starts_with('A') => Some(0),
            Some(b) if b.starts_with('B') => Some(1),
            _ => None,
        };
        match radio::schedule(bank) {
            Ok(msg) => {
                req.into_ok_response()?.write_all(format!("{msg}\n").as_bytes())?;
                std::thread::spawn(|| {
                    std::thread::sleep(Duration::from_millis(500));
                    crate::restart();
                });
                Ok(())
            }
            Err(e) => req.into_status_response(400)?.write_all(format!("{e}\n").as_bytes()).map(|_| ()),
        }
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
        let (_, len) = ws.recv(&mut [])?;
        if len > MAX_FRAME {
            ws.send(FrameType::Close, &[])?;
            return Err(EspError::from_infallible::<ESP_ERR_INVALID_SIZE>());
        }
        let mut buf = vec![0u8; len];
        ws.recv(&mut buf)?;
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
