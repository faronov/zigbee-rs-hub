//! Network: Ethernet first, the Wi-Fi station as the fallback.
//!
//! - Ethernet (when the board has it) starts at boot and is the default
//!   route whenever it has an address ([`eth`](crate::eth)).
//! - The Wi-Fi station (credentials from `wifi set`, NVS `zrh`) starts
//!   when Ethernet has had no address for [`FALLBACK_AFTER`] (at boot or
//!   later) and then stays up. Wi-Fi keeps its configuration in RAM only.
//!
//! No server runs in this stage.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use embedded_svc::wifi::{AuthMethod, ClientConfiguration, Configuration};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::sys::EspError;
use esp_idf_svc::wifi::{BlockingWifi, EspWifi};
use log::{info, warn};

use crate::settings;

/// Ethernet without an address this long starts the Wi-Fi station.
pub const FALLBACK_AFTER: Duration = Duration::from_secs(10);

/// Network state for the console and the LED.
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
}

impl NetState {
    /// Some interface has an address.
    pub fn up(&self) -> bool {
        self.eth_up || self.wifi_up
    }
}

/// Current network state.
pub static STATE: Mutex<NetState> = Mutex::new(NetState { eth: String::new(), wifi: String::new(), eth_up: false, wifi_up: false });

fn update(f: impl FnOnce(&mut NetState)) {
    f(&mut STATE.lock().unwrap());
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

/// Start Ethernet and the Wi-Fi fallback (own threads).
pub fn start(parts: NetParts) {
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
        crate::eth::start(eth, sys_loop.clone(), |text: String, up| {
            update(|s| {
                s.eth = text;
                s.eth_up = up;
            })
        });
    }
    let Some(ssid) = settings::get("wifi_ssid") else {
        update(|s| s.wifi = "not configured (console: wifi set <ssid> <password>)".into());
        return;
    };
    let pass = settings::get("wifi_pass").unwrap_or_default();
    let modem = parts.modem;
    let spawned = std::thread::Builder::new().name("wifi".into()).stack_size(6144).spawn(move || {
        if has_eth {
            wait_for_fallback();
        }
        if let Err(e) = wifi(modem, sys_loop, &ssid, &pass) {
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

fn wifi(modem: Modem<'static>, sys_loop: EspSystemEventLoop, ssid: &str, pass: &str) -> Result<(), EspError> {
    let mut wifi = BlockingWifi::wrap(EspWifi::new(modem, sys_loop.clone(), None)?, sys_loop)?;
    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: ssid.try_into().unwrap_or_default(),
        password: pass.try_into().unwrap_or_default(),
        auth_method: if pass.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal },
        ..Default::default()
    }))?;
    wifi.start()?;
    loop {
        update(|s| s.wifi = format!("connecting to {ssid:?}"));
        match wifi.connect().and_then(|_| wifi.wait_netif_up()) {
            Ok(()) => {
                let ip = wifi.wifi().sta_netif().get_ip_info().map(|i| i.ip.to_string()).unwrap_or_default();
                info!("wifi: up {ip}");
                update(|s| {
                    s.wifi = format!("up {ip}");
                    s.wifi_up = true;
                });
                while wifi.is_connected().unwrap_or(false) {
                    std::thread::sleep(Duration::from_secs(2));
                }
                update(|s| {
                    s.wifi = "disconnected".into();
                    s.wifi_up = false;
                });
            }
            Err(e) => update(|s| s.wifi = format!("connect failed: {e}")),
        }
        let _ = wifi.disconnect();
        std::thread::sleep(Duration::from_secs(5));
    }
}
