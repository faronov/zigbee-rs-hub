//! Network: Ethernet first, the Wi-Fi station as the fallback.
//!
//! - Ethernet (when the board has it) starts at boot and is the default
//!   route whenever it has an address ([`eth`](crate::eth)).
//! - The Wi-Fi station (credentials from `wifi set` on the console or
//!   `hub/wifi/set`, NVS `zrh`) starts when Ethernet has had no address
//!   for [`FALLBACK_AFTER`] (at boot or later) and then stays up. New
//!   credentials take effect at once: the station reconnects with them, or
//!   starts if it had none. Wi-Fi keeps its configuration in RAM only
//!   (SONOFF's NVS stays as it is).
//! - Whenever some interface has an address or none has any more, the
//!   callback given to [`start`] runs (the hub's HTTP server, SNTP).

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use embedded_svc::wifi::{AuthMethod, ClientConfiguration, Configuration};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::sys::{EspError, esp};
use esp_idf_svc::wifi::{BlockingWifi, EspWifi};
use log::{info, warn};
use serde_json::{Value as J, json};

use crate::settings;

/// Ethernet without an address this long starts the Wi-Fi station.
pub const FALLBACK_AFTER: Duration = Duration::from_secs(10);

/// Network state for the console, the LED and `hub/system`.
#[derive(Debug, Default, Clone)]
pub struct NetState {
    /// Ethernet: `-` (none on the board), `down (no link)`, `up 192.0.2.10`.
    pub eth: String,
    /// Wi-Fi state text.
    pub wifi: String,
    /// Ethernet has an address.
    pub eth_up: bool,
    /// The station has an address.
    pub wifi_up: bool,
    /// Ethernet's address.
    pub eth_ip: String,
    /// The station's SSID and address.
    pub wifi_ssid: String,
    pub wifi_ip: String,
}

impl NetState {
    /// Some interface has an address.
    pub fn up(&self) -> bool {
        self.eth_up || self.wifi_up
    }
}

/// Current network state.
pub static STATE: Mutex<NetState> = Mutex::new(NetState {
    eth: String::new(),
    wifi: String::new(),
    eth_up: false,
    wifi_up: false,
    eth_ip: String::new(),
    wifi_ssid: String::new(),
    wifi_ip: String::new(),
});

/// Runs when the network comes up (some address) or goes down (none).
type OnUp = Box<dyn Fn(bool) + Send + Sync>;
static ON_UP: OnceLock<OnUp> = OnceLock::new();

fn update(f: impl FnOnce(&mut NetState)) {
    let (before, after) = {
        let mut s = STATE.lock().unwrap();
        let before = s.up();
        f(&mut s);
        (before, s.up())
    };
    if before != after
        && let Some(cb) = ON_UP.get()
    {
        cb(after);
    }
}

/// Some interface has an address.
pub fn up() -> bool {
    STATE.lock().unwrap().up()
}

/// What the network needs.
pub struct NetParts {
    /// Wi-Fi radio.
    pub modem: Modem<'static>,
    /// Ethernet (Dongle-M).
    #[cfg(esp32)]
    pub eth: Option<crate::eth::EthParts>,
}

/// What the station needs until it starts (no credentials at boot).
struct Pending {
    modem: Modem<'static>,
    sys_loop: EspSystemEventLoop,
    has_eth: bool,
}

static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
/// Credentials to switch to (the station runs).
static SWITCH: Mutex<Option<(String, String)>> = Mutex::new(None);

/// Starts Ethernet and the Wi-Fi fallback (own threads); `on_up` runs
/// when the network comes up or goes down.
pub fn start(parts: NetParts, on_up: impl Fn(bool) + Send + Sync + 'static) {
    let _ = ON_UP.set(Box::new(on_up));
    let sys_loop = match EspSystemEventLoop::take() {
        Ok(l) => l,
        Err(e) => {
            warn!("event loop: {e}");
            return;
        }
    };
    update(|s| s.eth = "-".into());
    #[allow(unused_mut)]
    let mut has_eth = false;
    #[cfg(esp32)]
    if let Some(eth) = parts.eth {
        has_eth = true;
        crate::eth::start(eth, sys_loop.clone(), |text: String, up, ip: String| {
            update(|s| {
                s.eth = text;
                s.eth_up = up;
                s.eth_ip = ip;
            })
        });
    }
    let pending = Pending { modem: parts.modem, sys_loop, has_eth };
    match settings::get("wifi_ssid") {
        Some(ssid) => spawn(pending, ssid, settings::get("wifi_pass").unwrap_or_default()),
        None => {
            update(|s| s.wifi = "not configured (console: wifi set <ssid> <password>, or Settings in the web UI)".into());
            *PENDING.lock().unwrap() = Some(pending);
        }
    }
}

/// Stores new credentials and uses them now: the station reconnects, or
/// starts (after the Ethernet fallback wait) if it had none.
pub fn configure(ssid: &str, pass: &str) -> Result<(), String> {
    if ssid.is_empty() || ssid.len() > 32 || pass.len() > 64 {
        return Err("SSID 1–32 bytes, password up to 64".into());
    }
    settings::set("wifi_ssid", Some(ssid))?;
    settings::set("wifi_pass", Some(pass))?;
    let pending = PENDING.lock().unwrap().take();
    match pending {
        Some(p) => spawn(p, ssid.to_string(), pass.to_string()),
        None => *SWITCH.lock().unwrap() = Some((ssid.to_string(), pass.to_string())),
    }
    Ok(())
}

fn spawn(p: Pending, ssid: String, pass: String) {
    let spawned = std::thread::Builder::new().name("wifi".into()).stack_size(6144).spawn(move || {
        if p.has_eth {
            wait_for_fallback();
        }
        if let Err(e) = wifi(p.modem, p.sys_loop, ssid, &pass) {
            warn!("wifi: {e}");
            update(|s| {
                s.wifi = format!("error: {e}");
                s.wifi_up = false;
            });
        }
    });
    if let Err(e) = spawned {
        update(|s| s.wifi = format!("thread: {e}"));
    }
}

/// Returns once Ethernet has had no address for [`FALLBACK_AFTER`].
fn wait_for_fallback() {
    let mut down_since = Instant::now();
    loop {
        if STATE.lock().unwrap().eth_up {
            down_since = Instant::now();
            update(|s| s.wifi = "standby (Ethernet up)".into());
        } else if down_since.elapsed() >= FALLBACK_AFTER {
            info!("wifi: no Ethernet address for {} s, starting the station", FALLBACK_AFTER.as_secs());
            return;
        } else {
            update(|s| s.wifi = "standby (waiting for Ethernet)".into());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn client(ssid: &str, pass: &str) -> Configuration {
    Configuration::Client(ClientConfiguration {
        ssid: ssid.try_into().unwrap_or_default(),
        password: pass.try_into().unwrap_or_default(),
        auth_method: if pass.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal },
        ..Default::default()
    })
}

fn wifi(modem: Modem<'static>, sys_loop: EspSystemEventLoop, mut ssid: String, pass: &str) -> Result<(), EspError> {
    // The radio starts: not while `entropy::fill` has its ADC source on.
    let mut wifi = crate::entropy::radio_start(|| {
        let mut wifi = BlockingWifi::wrap(EspWifi::new(modem, sys_loop.clone(), None)?, sys_loop)?;
        wifi.set_configuration(&client(&ssid, pass))?;
        wifi.start()?;
        Ok::<_, EspError>(wifi)
    })?;
    // Mains powered: modem sleep (the default) holds frames to the hub
    // until the next DTIM beacon, adding 100-300 ms and lost pings.
    esp!(unsafe { esp_idf_svc::sys::esp_wifi_set_ps(esp_idf_svc::sys::wifi_ps_type_t_WIFI_PS_NONE) })?;
    loop {
        if let Some((s, p)) = SWITCH.lock().unwrap().take() {
            info!("wifi: switching to {s:?}");
            let _ = wifi.disconnect();
            wifi.set_configuration(&client(&s, &p))?;
            ssid = s;
        }
        update(|s| {
            s.wifi = format!("connecting to {ssid:?}");
            s.wifi_ssid = ssid.clone();
        });
        match wifi.connect().and_then(|_| wifi.wait_netif_up()) {
            Ok(()) => {
                let ip = wifi.wifi().sta_netif().get_ip_info().map(|i| i.ip.to_string()).unwrap_or_default();
                info!("wifi: up {ip}");
                update(|s| {
                    s.wifi = format!("up {ip}");
                    s.wifi_ip = ip;
                    s.wifi_up = true;
                });
                while wifi.is_connected().unwrap_or(false) && SWITCH.lock().unwrap().is_none() {
                    std::thread::sleep(Duration::from_secs(1));
                }
                update(|s| {
                    s.wifi = "disconnected".into();
                    s.wifi_ip.clear();
                    s.wifi_up = false;
                });
            }
            Err(e) => update(|s| s.wifi = format!("connect failed: {e}")),
        }
        let _ = wifi.disconnect();
        if SWITCH.lock().unwrap().is_none() {
            std::thread::sleep(Duration::from_secs(5));
        }
    }
}

/// Primary channel and RSSI of the access point, while associated.
pub fn ap() -> Option<(u8, i8)> {
    let mut rec: esp_idf_svc::sys::wifi_ap_record_t = unsafe { core::mem::zeroed() };
    (unsafe { esp_idf_svc::sys::esp_wifi_sta_get_ap_info(&mut rec) } == esp_idf_svc::sys::ESP_OK).then_some((rec.primary, rec.rssi))
}

/// `hub/system` `wifi`: state, SSID, IP, and the AP's RSSI and channel
/// while associated.
pub fn wifi_info() -> J {
    let s = STATE.lock().unwrap().clone();
    let mut v = json!({"state": s.wifi});
    if !s.wifi_ssid.is_empty() {
        v["ssid"] = s.wifi_ssid.into();
    }
    if !s.wifi_ip.is_empty() {
        v["ip"] = s.wifi_ip.into();
    }
    if s.wifi_up
        && let Some((channel, rssi)) = ap()
    {
        v["channel"] = channel.into();
        v["rssi"] = rssi.into();
    }
    v
}

/// `hub/system` `ethernet`: state and IP.
pub fn eth_info() -> J {
    let s = STATE.lock().unwrap().clone();
    let mut v = json!({"state": s.eth});
    if !s.eth_ip.is_empty() {
        v["ip"] = s.eth_ip.into();
    }
    v
}
