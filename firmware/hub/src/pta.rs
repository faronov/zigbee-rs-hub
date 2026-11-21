//! The ESP32-S3 as the PTA (Wi-Fi / 802.15.4 coexistence) leader of the
//! Espressif Thread Border Router board (ZRHP spec 11.5, decision N7).
//!
//! The H2 NCP is the follower on four wires (`hub_core::board::pta`):
//! request and priority come in from the H2, grant and tx_line (the S3's
//! Wi-Fi TX indication) go out to it. The arbitration is the S3's Wi-Fi
//! coexistence hardware, configured with ESP-IDF's external-coexistence GPIO
//! API (`CONFIG_ESP_COEX_EXTERNAL_COEXIST_ENABLE`, which requires Bluetooth
//! off), as `esp-thread-br` does for this board.
//!
//! The leader is configured once, after `esp_wifi_start` (coexistence is
//! initialised by the Wi-Fi start), and stays on for the boot: without Wi-Fi
//! activity it grants every request. The NCP's follower (`coex_enable`) is
//! what the hub switches with the Wi-Fi state, so the follower never runs
//! against an unconfigured grant line.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use esp_idf_svc::sys::{self, EspError, esp};
use hub_core::board::pta;
use log::info;

static LEADER: AtomicBool = AtomicBool::new(false);
static WIFI_UP: AtomicBool = AtomicBool::new(false);
/// Console override (`hub coex auto|on|off`, lab H9 runs): 0 follow the
/// Wi-Fi state, 1 on whenever the leader is configured, 2 off.
static MODE: AtomicU8 = AtomicU8::new(0);

/// Whether the leader is configured (`hub status`).
pub fn leader_active() -> bool {
    LEADER.load(Ordering::Relaxed)
}

/// Configure the S3 as the 4-wire leader (idempotent). Call after the Wi-Fi
/// driver started.
pub fn enable_leader() -> Result<(), EspError> {
    if LEADER.load(Ordering::Relaxed) {
        return Ok(());
    }
    // SAFETY: plain ESP-IDF calls; the pin set is a C struct of four
    // `gpio_num_t` unions, fully written below.
    unsafe {
        esp!(sys::esp_external_coex_set_work_mode(
            sys::esp_extern_coex_work_mode_t_EXTERNAL_COEX_LEADER_ROLE
        ))?;
        let mut pins: sys::esp_external_coex_gpio_set_t = core::mem::zeroed();
        pins.__bindgen_anon_1.request = pta::REQUEST;
        pins.__bindgen_anon_2.priority = pta::PRIORITY;
        pins.__bindgen_anon_3.grant = pta::GRANT;
        pins.__bindgen_anon_4.tx_line = pta::TX_LINE;
        esp!(sys::esp_enable_extern_coex_gpio_pin(sys::external_coex_wire_t_EXTERN_COEX_WIRE_4, pins))?;
    }
    LEADER.store(true, Ordering::Relaxed);
    info!(
        "PTA leader: request GPIO{} priority GPIO{} grant GPIO{} tx_line GPIO{}",
        pta::REQUEST,
        pta::PRIORITY,
        pta::GRANT,
        pta::TX_LINE
    );
    Ok(())
}

/// Record the Wi-Fi state; the follower state the NCP should have now.
pub fn wifi_changed(up: bool) -> bool {
    WIFI_UP.store(up, Ordering::Relaxed);
    follower_wanted()
}

/// The NCP follower (`coex_enable`): on while Wi-Fi is up (or forced on)
/// and the leader drives the grant line.
pub fn follower_wanted() -> bool {
    leader_active()
        && match MODE.load(Ordering::Relaxed) {
            1 => true,
            2 => false,
            _ => WIFI_UP.load(Ordering::Relaxed),
        }
}

/// `hub coex auto|on|off`; returns the follower state to apply.
pub fn set_mode(mode: &str) -> Option<bool> {
    let value = match mode {
        "auto" => 0,
        "on" => 1,
        "off" => 2,
        _ => return None,
    };
    MODE.store(value, Ordering::Relaxed);
    Some(follower_wanted())
}

/// `hub coex` status line.
pub fn status() -> String {
    let mode = ["auto", "on", "off"][usize::from(MODE.load(Ordering::Relaxed).min(2))];
    format!(
        "PTA: leader {} mode {mode} wifi {} follower {}\r\n",
        if leader_active() { "on" } else { "off" },
        if WIFI_UP.load(Ordering::Relaxed) { "up" } else { "down" },
        if follower_wanted() { "on" } else { "off" },
    )
}
