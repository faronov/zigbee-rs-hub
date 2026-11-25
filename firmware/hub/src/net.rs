//! Hub-mode network: Wi-Fi station (credentials in NVS `hub/wifi_ssid`,
//! `hub/wifi_pass`, set from the console or `hub/wifi/set`) and the
//! WebSocket JSON API on `ws://<ip>/api` (esp_http_server,
//! `zrh_espapp::http`), one API session per socket.
//!
//! New credentials from the API take effect at once: the station thread
//! reconnects with them, or starts if the hub had none at boot.

use std::sync::Mutex;
use std::time::Duration;

use embedded_svc::wifi::{AuthMethod, ClientConfiguration, Configuration};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::http::server::EspHttpServer;
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

fn api_server(app: Option<App>) -> Result<EspHttpServer<'static>, EspError> {
    match app {
        Some(app) => zrh_espapp::http::api_server(app),
        None => zrh_espapp::http::wrong_image_server("psram_missing", crate::resources::wrong_image()),
    }
}
