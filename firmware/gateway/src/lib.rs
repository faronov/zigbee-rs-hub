//! zigbee-rs-hub gateway firmware for host boards described by a
//! [`BoardProfile`](hub_core::profile::BoardProfile): the SONOFF Dongle-M
//! (classic ESP32 + EFR32MG24, Ethernet) and the ESP32-S3 SuperMini +
//! RF-BM-MG24B1 bench setup. The Thread BR board keeps its own firmware
//! (`firmware/hub`, with the phase-0 USB bridge).
//!
//! Boot sequence ([`run`]):
//!
//! 1. NCP control lines released ([`lines::init`]) — on the Dongle-M this
//!    drives GPIO12 (MTDI flash-voltage strap, MG24 bootloader entry) low
//!    before anything else;
//! 2. console, NVS, board overrides (`board set`), strapping/eFuse report,
//!    partition-table check (16 MB / 8 MB / 4 MB layouts);
//! 3. pending NCP install through the Gecko bootloader ([`radio`]);
//! 4. the ZRHP host (`hub-ncp`) on the NCP UART (460 800 without flow
//!    control by default, 115 200 fallback), the host store in `hubdata`;
//! 5. the application (`hub-app`) with the WebSocket JSON API over Ethernet
//!    and/or Wi-Fi, LED and button, console on the calling thread.
//!
//! No network transport for ZRHP itself (no stream server): the NCP is
//! only reachable through the hub.

pub mod console;
pub mod esp;
#[cfg(esp32)]
pub mod eth;
pub mod led;
pub mod lines;
pub mod net;
pub mod radio;
pub mod settings;
pub mod uart;

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use esp_idf_svc::hal::peripherals::Peripherals;
use hub_core::layout::{Layout, Verdict};
use hub_core::profile::BoardProfile;
use hub_ncp::lifecycle::{Hub, HubConfig};
use hub_ncp::store::{JOURNAL_BYTES, Store};
use hub_ncp::transport::{Line, NcpControl};
use log::{error, info, warn};

/// The board as running (profile + overrides).
pub static BOARD: OnceLock<BoardProfile> = OnceLock::new();
/// The hub core.
pub static HUB: OnceLock<Hub> = OnceLock::new();
/// The application.
pub static APP: OnceLock<hub_app::App> = OnceLock::new();
static HUB_ERROR: Mutex<Option<String>> = Mutex::new(None);
static LAYOUT: Mutex<String> = Mutex::new(String::new());

/// Why the hub did not start.
pub fn hub_error() -> Option<String> {
    HUB_ERROR.lock().unwrap().clone()
}

/// The partition-table verdict.
pub fn layout_text() -> String {
    LAYOUT.lock().unwrap().clone()
}

/// Restart the host.
pub fn restart() -> ! {
    std::thread::sleep(Duration::from_millis(100));
    unsafe { esp_idf_svc::sys::esp_restart() }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn open_store(verdict: Verdict) -> Option<Store> {
    match verdict {
        Verdict::Fatal(m) => {
            let e = format!("the partition table does not belong to this flash ({m:?}); flash the matching table");
            error!("hub not started: {e}");
            *HUB_ERROR.lock().unwrap() = Some(e);
            led::CONFIG_ERROR.store(true, std::sync::atomic::Ordering::Relaxed);
            None
        }
        Verdict::Foreign => Some(Store::volatile()),
        Verdict::Ok(_) => {
            // The journal starts every hub layout's `hubdata`.
            Some(match esp::Region::find(0, JOURNAL_BYTES).map(|r| Store::open(Box::new(r))) {
                Some(Ok(s)) => {
                    info!("host store: hubdata journal {:?}", s.stats());
                    s
                }
                Some(Err(e)) => {
                    warn!("host store: hubdata journal unusable ({e}); RAM only");
                    Store::volatile()
                }
                None => {
                    warn!("host store: no usable hubdata partition; RAM only");
                    Store::volatile()
                }
            })
        }
    }
}

fn start_app(hub: Hub, board: &BoardProfile, p: Peripherals) {
    let token = settings::get("api_token").unwrap_or_else(|| {
        let mut b = [0u8; 12];
        (esp::rng())(&mut b);
        let t = hex(&b);
        if let Err(e) = settings::set("api_token", Some(&t)) {
            warn!("API token not stored: {e}");
        }
        t
    });
    let mut cfg = hub_app::AppConfig::for_board(board, env!("CARGO_PKG_VERSION"));
    cfg.api_token = token;
    let app = hub_app::App::start(hub.clone(), cfg);
    let _ = APP.set(app.clone());
    let parts = net::NetParts {
        modem: p.modem,
        #[cfg(esp32)]
        eth: eth::EthParts::from_profile(
            &board.ethernet,
            p.mac,
            (p.pins.gpio19, p.pins.gpio21, p.pins.gpio22, p.pins.gpio25, p.pins.gpio26, p.pins.gpio27),
            p.pins.gpio0,
        ),
    };
    let pta = board.pta;
    net::start(app, parts, move |up| {
        if pta && let Err(e) = hub.set_coex(up) {
            warn!("coex_enable {up}: {e}");
        }
    });
}

/// The firmware's `main`.
pub fn run(profile: &'static BoardProfile) -> ! {
    esp_idf_svc::sys::link_patches();
    // First: NCP lines released (GPIO12 low on the Dongle-M).
    let early = lines::init(profile);
    esp_idf_svc::log::EspLogger::initialize_default();
    console::init(profile.console);
    info!("zigbee-rs-hub gateway {} on {}", env!("CARGO_PKG_VERSION"), profile.name);
    if let Err(e) = early {
        error!("NCP control lines: {e}");
    }
    settings::open();

    let mut board = *profile;
    if let Some(o) = settings::get("board") {
        match board.apply(&o) {
            Ok(()) => info!("board overrides applied: {o}"),
            Err(e) => error!("board overrides {o:?} ignored: {e}"),
        }
    }
    let _ = BOARD.set(board);
    let lines = match lines::init(&board) {
        Ok(l) => Some(l),
        Err(e) => {
            error!("NCP control lines: {e}");
            None
        }
    };
    esp::strapping_report(&board);
    info!("{}", esp::memory_report());

    let (_, verdict) = esp::flash_layout();
    *LAYOUT.lock().unwrap() = format!("{verdict:?}");
    let store = open_store(verdict);
    if let Some(l) = lines {
        radio::init(&board, l, verdict == Verdict::Ok(Layout::Production16));
    }

    let first = Line { baud: board.uart.line.baud, flow: board.uart.line.flow };
    if let Err(e) = uart::init(&board.uart, first) {
        error!("NCP UART: {e}");
        *HUB_ERROR.lock().unwrap() = Some(format!("NCP UART: {e}"));
    } else if let Some(store) = store {
        radio::pre_hub(first);
        let mut cfg = HubConfig::default();
        cfg.driver.host_name = format!("zigbee-rs-hub {} ({})", env!("CARGO_PKG_VERSION"), board.id);
        cfg.driver.lines = board.uart.lines().map(|l| Line { baud: l.baud, flow: l.flow }).collect();
        let control: Option<Box<dyn NcpControl>> = match (lines, board.control.reset) {
            (Some(l), Some(_)) => Some(Box::new(l)),
            _ => None,
        };
        let hub = Hub::start(cfg, uart::UartRx, uart::UartTx, control, store, esp::rng());
        let _ = HUB.set(hub.clone());
        led::set_hub(hub.clone());
        radio::start_monitor(hub.clone());
        match Peripherals::take() {
            Ok(p) => start_app(hub, &board, p),
            Err(e) => warn!("peripherals: {e}"),
        }
        info!("{}", esp::memory_report());
    }
    led::start(&board);
    console::run()
}
