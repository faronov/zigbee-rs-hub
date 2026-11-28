//! zigbee-rs-hub gateway firmware for host boards described by a
//! [`BoardProfile`]: the SONOFF Dongle Max (Dongle-M: classic ESP32 +
//! EFR32MG24 with SONOFF's EmberZNet NCP or, once the hub replaced it, the
//! zigbee-rs ZRHP NCP; Ethernet). The Thread BR board
//! keeps its own firmware (`firmware/hub`); what both need from ESP-IDF
//! for the hub application is shared (`firmware/espapp`).
//!
//! Boot order ([`run`]):
//!
//! 1. the NCP lines released ([`lines::init`]): on the Dongle-M GPIO12
//!    (MG24 bootloader entry, the ESP32's MTDI flash-voltage strap) low and
//!    GPIO15 (MG24 nRESET) high, before anything else;
//! 2. the Rust heap in PSRAM, the panic recorders, the logger with the log
//!    ring, console, settings ([`settings`], own NVS namespace, never
//!    erased), boot report: chip, memory, partitions, `files`, straps, last
//!    panic ([`esp`]);
//! 3. the NCP UART at the profile's rate ([`uart`]) and an MG24 reset into
//!    its application;
//! 4. the hub ([`hub`]): the backend chosen ([`ncp`]: the setting, or the
//!    MG24 asked: EZSP, ZRHP, or its Gecko bootloader after an update that
//!    did not finish), the application on it over the NCP UART, its data
//!    on SONOFF's `files` partition, claimed at the first boot ([`files`];
//!    RAM only if that is refused), the API token from an entropy source
//!    ([`entropy`]), the `storage/*`, `firmware/*` ([`update`]: self-update
//!    into the other app slot) and `ncp/*` requests ([`ncp`]: the MG24's
//!    firmware replaced through its Gecko bootloader), MQTT;
//! 5. network: Ethernet, Wi-Fi station as the fallback ([`net`]); the
//!    HTTP server (web UI, WebSocket API) once an interface has an address;
//! 6. LED and button ([`led`]: short press = permit join), then the
//!    console on the calling thread ([`console`]), with the way back to
//!    SONOFF's application in `ota_0` ([`slots`]).
//!
//! `restart bringup` boots once without the hub: the bring-up console
//! with the MG24 probes ([`ezsp`], [`gecko`]). Without PSRAM the hub does
//! not start either (a page says why).
//!
//! The stock partition table stays. No Bluetooth (off in sdkconfig, its
//! RAM stays with the heap) and no network access to the MG24 UART.

pub mod console;
pub mod entropy;
pub mod esp;
#[cfg(esp32)]
pub mod eth;
pub mod ezsp;
pub mod files;
pub mod gecko;
pub mod hub;
pub mod led;
pub mod lines;
pub mod ncp;
pub mod net;
pub mod platform;
pub mod settings;
pub mod slots;
pub mod uart;
pub mod update;
pub mod zrhp;

use std::sync::OnceLock;
use std::time::Duration;

use esp_idf_svc::hal::peripherals::Peripherals;
use hub_core::profile::BoardProfile;
use log::{error, info, warn};
use zrh_espapp::{alloc, cpanic};

#[global_allocator]
static HEAP: alloc::PreferPsram = alloc::PreferPsram;

/// The board as running (profile + overrides).
pub static BOARD: OnceLock<BoardProfile> = OnceLock::new();

/// The running board.
pub fn board() -> &'static BoardProfile {
    BOARD.get().expect("board set at boot")
}

/// Restart the host.
pub fn restart() -> ! {
    std::thread::sleep(Duration::from_millis(100));
    unsafe { esp_idf_svc::sys::esp_restart() }
}

const BRINGUP_ONCE: u32 = u32::from_le_bytes(*b"BRU1");
/// `restart bringup`: survives `esp_restart` (random after power-on),
/// cleared on use.
#[unsafe(link_section = ".rtc_noinit")]
static mut ONE_SHOT: u32 = 0;
static BRINGUP: OnceLock<bool> = OnceLock::new();

/// This boot runs without the hub (`restart bringup`).
pub fn bringup() -> bool {
    BRINGUP.get().copied().unwrap_or(false)
}

/// Restart once without the hub.
pub fn restart_bringup() -> ! {
    unsafe { (&raw mut ONE_SHOT).write_volatile(BRINGUP_ONCE) };
    restart()
}

/// The firmware's `main`.
pub fn run(profile: &'static BoardProfile) -> ! {
    esp_idf_svc::sys::link_patches();
    // First: NCP lines released (GPIO12 low on the Dongle-M).
    let early = lines::init(profile);
    console::route(profile.console);
    alloc::init();
    let _ = cpanic::last_panic();
    cpanic::record_panics();
    cpanic::count_boot();
    cpanic::record_alloc_failures();
    zrh_espapp::logs::init_logger();
    console::init(profile.console);
    let _ = BRINGUP.set(unsafe { (&raw mut ONE_SHOT).replace(0) } == BRINGUP_ONCE);
    info!("zrh-gateway {} on {}{}", env!("CARGO_PKG_VERSION"), profile.name, if bringup() { " (bring-up boot)" } else { "" });
    if let Err(e) = early {
        error!("NCP control lines: {e}");
    }
    settings::open();

    let mut board = *profile;
    if let Some(o) = settings::get("board") {
        // Escape hatch: the button held at power-up or reset boots with the
        // profile's wiring (`board clear` then removes the overrides).
        if profile.button.as_ref().is_some_and(led::held_at_boot) {
            warn!("button held at boot: board overrides {o:?} ignored for this boot (`board clear` removes them)");
        } else {
            match board.apply(&o) {
                Ok(()) => info!("board overrides applied: {o}"),
                Err(e) => error!("board overrides {o:?} ignored: {e}"),
            }
        }
    }
    let _ = BOARD.set(board);
    if board.control != profile.control
        && let Err(e) = lines::init(&board)
    {
        error!("NCP control lines: {e}");
    }
    for line in esp::boot_report(&board) {
        info!("{line}");
    }
    if let Some(p) = cpanic::last() {
        warn!("last ESP-IDF panic: {p}");
    }
    if let Some(p) = cpanic::last_panic() {
        warn!("previous boot panicked: {p}");
    }

    match uart::init(&board.uart, board.uart.line.baud) {
        Ok(()) => info!("NCP UART{}: tx {} rx {} at {} Bd, no flow control", board.uart.port, board.uart.tx, board.uart.rx, board.uart.line.baud),
        Err(e) => error!("NCP UART: {e}"),
    }
    // The hub before the network: the HTTP server starts with the first
    // address. As the stock firmware, the NCP starts from a reset into its
    // application (also leaves a bootloader entered before a host
    // restart): the hub's backend detection makes it (`ncp::boot_detect`),
    // except for a zigbee-rs NCP that runs and answers, whose log waits
    // for the backend's LOG credit; without the hub, here.
    let app_allowed = zrh_espapp::resources::app_allowed();
    if bringup() || !app_allowed {
        lines::reset_to_app();
    }
    if bringup() {
        info!("bring-up boot: the hub does not start (`restart` boots it again)");
    } else if !app_allowed {
        error!("{}", no_psram());
    } else {
        hub::start(&board);
    }
    info!("{}", esp::memory_report());

    match Peripherals::take() {
        Ok(p) => net::start(
            net::NetParts {
                modem: p.modem,
                #[cfg(esp32)]
                eth: eth::EthParts::from_profile(
                    &board.ethernet,
                    p.mac,
                    (p.pins.gpio19, p.pins.gpio21, p.pins.gpio22, p.pins.gpio25, p.pins.gpio26, p.pins.gpio27),
                    p.pins.gpio0,
                ),
            },
            move |up| on_network(up, app_allowed),
        ),
        Err(e) => error!("peripherals: {e}"),
    }
    led::start(&board);
    // Only a bootloader with app rollback puts an image on trial
    // (PENDING_VERIFY); then this confirms it once the core runs. Anything
    // else is left as is (SONOFF's bootloader very likely has no rollback:
    // docs/dongle-max.md).
    unsafe { esp_idf_svc::sys::esp_ota_mark_app_valid_cancel_rollback() };
    console::run()
}

fn no_psram() -> String {
    "PSRAM not detected: the hub application (web UI, API, MQTT) is not started; the console still runs".into()
}

/// The network came up (an interface has an address) or went down: SNTP,
/// and the HTTP server at the first time up (it listens on every
/// interface for the rest of the boot). Runs on a short-lived thread: the
/// caller is the Ethernet or Wi-Fi thread, whose stacks are small.
fn on_network(up: bool, app_allowed: bool) {
    let spawned = std::thread::Builder::new().name("net-up".into()).stack_size(8192).spawn(move || network_changed(up, app_allowed));
    if let Err(e) = spawned {
        warn!("network: {e}");
    }
}

fn network_changed(up: bool, app_allowed: bool) {
    static SERVER: std::sync::Mutex<bool> = std::sync::Mutex::new(false);
    zrh_espapp::logs::network_changed(up);
    let mut started = SERVER.lock().unwrap();
    if !up || *started {
        return;
    }
    let server = match (hub::APP.get().cloned(), app_allowed) {
        (Some(app), _) => zrh_espapp::http::api_server(app),
        (None, false) => zrh_espapp::http::wrong_image_server("psram_missing", no_psram()),
        (None, true) => return,
    };
    match server {
        Ok(s) => {
            // Runs for the rest of the boot.
            std::mem::forget(s);
            *started = true;
            info!("api: http://<address>/ (web UI), ws://<address>/api");
        }
        Err(e) => warn!("api server: {e}"),
    }
}
