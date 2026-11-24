//! zigbee-rs-hub gateway firmware for host boards described by a
//! [`BoardProfile`]: the SONOFF Dongle Max (Dongle-M: classic ESP32 +
//! EFR32MG24, Ethernet). The Thread BR board keeps its own firmware
//! (`firmware/hub`).
//!
//! **Board bring-up stage.** No coordinator backend and no hub application
//! run yet (the EZSP backend and the backend selection come later). What
//! runs, in boot order ([`run`]):
//!
//! 1. the NCP lines released ([`lines::init`]): on the Dongle-M GPIO12
//!    (MG24 bootloader entry, the ESP32's MTDI flash-voltage strap) low and
//!    GPIO15 (MG24 nRESET) high, before anything else;
//! 2. the Rust heap in PSRAM and the panic recorder (the sources of
//!    `firmware/hub/src/alloc.rs` and `cpanic.rs`, shared), console,
//!    settings ([`settings`], own NVS namespace, never erased), boot report:
//!    chip, memory, partitions, straps, last panic ([`esp`]);
//! 3. the NCP UART at the profile's rate ([`uart`]) and an MG24 reset into
//!    its application;
//! 4. network: Ethernet, Wi-Fi station as the fallback ([`net`]);
//! 5. LED and button ([`led`]), then the console on the calling thread
//!    ([`console`]): MG24 reset, Gecko bootloader banner ([`gecko`]) and the
//!    read-only EZSP probe ([`ezsp`]).
//!
//! No Bluetooth (off in sdkconfig, its RAM stays with the heap) and no
//! network access to the MG24 UART.

#[path = "../../hub/src/alloc.rs"]
mod alloc;
pub mod console;
#[path = "../../hub/src/cpanic.rs"]
mod cpanic;
// `report` and `atomics` are used; `named` and `bench` belong to firmware/hub.
#[allow(dead_code)]
#[path = "../../hub/src/cpu.rs"]
mod cpu;
pub mod esp;
#[cfg(esp32)]
pub mod eth;
pub mod ezsp;
pub mod gecko;
pub mod led;
pub mod lines;
pub mod net;
pub mod settings;
pub mod uart;

use std::sync::OnceLock;
use std::time::Duration;

use esp_idf_svc::hal::peripherals::Peripherals;
use hub_core::profile::BoardProfile;
use log::{error, info, warn};

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

/// The firmware's `main`.
pub fn run(profile: &'static BoardProfile) -> ! {
    esp_idf_svc::sys::link_patches();
    // First: NCP lines released (GPIO12 low on the Dongle-M).
    let early = lines::init(profile);
    alloc::init();
    cpanic::count_boot();
    esp_idf_svc::log::EspLogger::initialize_default();
    console::init(profile.console);
    info!("zrh-gateway {} on {}", env!("CARGO_PKG_VERSION"), profile.name);
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
    if board.control != profile.control
        && let Err(e) = lines::init(&board)
    {
        error!("NCP control lines: {e}");
    }
    for line in esp::boot_report(&board) {
        info!("{line}");
    }
    if let Some(p) = cpanic::last() {
        warn!("last panic: {p}");
    }

    match uart::init(&board.uart, board.uart.line.baud) {
        Ok(()) => info!("NCP UART{}: tx {} rx {} at {} Bd, no flow control", board.uart.port, board.uart.tx, board.uart.rx, board.uart.line.baud),
        Err(e) => error!("NCP UART: {e}"),
    }
    // As the stock firmware: the NCP starts from a reset into its
    // application (also leaves a bootloader entered before a host restart).
    lines::reset_to_app();

    match Peripherals::take() {
        Ok(p) => net::start(net::NetParts {
            modem: p.modem,
            #[cfg(esp32)]
            eth: eth::EthParts::from_profile(
                &board.ethernet,
                p.mac,
                (p.pins.gpio19, p.pins.gpio21, p.pins.gpio22, p.pins.gpio25, p.pins.gpio26, p.pins.gpio27),
                p.pins.gpio0,
            ),
        }),
        Err(e) => error!("peripherals: {e}"),
    }
    led::start(&board);
    info!("{}", esp::memory_report());
    // A self-updated image boots PENDING_VERIFY: it got this far, keep it
    // (no effect on an image that is not on trial).
    unsafe { esp_idf_svc::sys::esp_ota_mark_app_valid_cancel_rollback() };
    console::run()
}
