//! The S3 side of the maintenance API ([`hub_app::Platform`]): system facts,
//! Wi-Fi credentials (NVS, applied at once by `net`), the API token in NVS,
//! reboot and boot mode, `esp_fill_random`, and the log tail of [`LOGS`].
//!
//! Hub-mode logs go to UART0, which the lab host cannot see; [`Tee`] also
//! keeps Rust log lines up to Info in a small RAM ring for `hub/logs`
//! (ESP-IDF's own C logs are not in it). Nothing logs keys.

use std::time::Duration;

use esp_idf_svc::log::{EspIdfLogFilter, EspIdfLogger};
use esp_idf_svc::sys;
use hub_app::LogRing;
use log::{Level, Log, Metadata, Record, info};
use serde_json::{Value as J, json};

use crate::mode::{self, Mode};
use crate::net;

/// Recent log lines (3 KiB of internal RAM, allocated with the first line).
pub static LOGS: LogRing = LogRing::new(3 * 1024);

static ESP: EspIdfLogger<EspIdfLogFilter> = EspIdfLogger::new(EspIdfLogFilter::new());

/// ESP-IDF's logger plus [`LOGS`].
pub struct Tee;

static TEE: Tee = Tee;

impl Log for Tee {
    fn enabled(&self, m: &Metadata<'_>) -> bool {
        ESP.enabled(m)
    }
    fn log(&self, r: &Record<'_>) {
        ESP.log(r);
        if r.level() <= Level::Info && ESP.enabled(r.metadata()) {
            let t = unsafe { sys::esp_timer_get_time() } / 1000;
            LOGS.push(&format!("{}.{:03} {} {}: {}", t / 1000, t % 1000, r.level(), r.target(), r.args()));
        }
    }
    fn flush(&self) {}
}

/// Replaces `EspLogger::initialize_default()`.
pub fn init_logger() {
    if log::set_logger(&TEE).is_ok() {
        ESP.filter().initialize();
    }
}

/// The S3 platform.
pub struct S3;

fn reboot_soon(why: &'static str) {
    let _ = std::thread::Builder::new().stack_size(3072).spawn(move || {
        info!("{why}: rebooting in 1 s");
        // The API reply goes out first.
        std::thread::sleep(Duration::from_secs(1));
        mode::restart()
    });
}

impl hub_app::Platform for S3 {
    fn system(&self) -> J {
        let (free, min, largest, psram) = unsafe {
            (
                sys::heap_caps_get_free_size(sys::MALLOC_CAP_INTERNAL),
                sys::heap_caps_get_minimum_free_size(sys::MALLOC_CAP_INTERNAL),
                sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_INTERNAL),
                sys::heap_caps_get_total_size(sys::MALLOC_CAP_SPIRAM),
            )
        };
        let boot = crate::BOOT.get().copied();
        json!({
            "version": format!("tbr-hub {}", env!("CARGO_PKG_VERSION")),
            "uptime_s": unsafe { sys::esp_timer_get_time() } / 1_000_000,
            "heap": {"free": free, "min_free": min, "largest_block": largest, "psram": psram},
            "mode": boot.map(|b| if b.once { "hub (once)" } else { "hub" }),
            "persisted_mode": match mode::stored() { Some(Mode::Hub) => "hub", Some(Mode::Bridge) => "bridge", None => "unknown" },
            "wifi": net::info(),
            "reset_reason": unsafe { sys::esp_reset_reason() },
        })
    }
    fn heap(&self) -> Option<(usize, usize)> {
        let caps = sys::MALLOC_CAP_INTERNAL;
        Some(unsafe { (sys::heap_caps_get_free_size(caps), sys::heap_caps_get_largest_free_block(caps)) })
    }
    fn stack_high_water(&self) -> Option<usize> {
        // ESP-IDF counts the stack in bytes.
        Some(unsafe { sys::uxTaskGetStackHighWaterMark(std::ptr::null_mut()) } as usize)
    }
    fn wifi_channel(&self) -> Option<u8> {
        net::ap().map(|(channel, _)| channel)
    }
    fn set_wifi(&self, ssid: &str, password: &str) -> Result<(), String> {
        net::configure(ssid, password)
    }
    fn save_token(&self, token: &str) -> Result<(), String> {
        mode::set_str("api_token", Some(token))
    }
    fn reboot(&self) -> Result<(), String> {
        reboot_soon("hub/reboot");
        Ok(())
    }
    fn set_mode(&self, m: &str) -> Result<(), String> {
        mode::store(if m == "bridge" { Mode::Bridge } else { Mode::Hub })?;
        reboot_soon("hub/mode/set");
        Ok(())
    }
    fn random(&self, buf: &mut [u8]) {
        unsafe { sys::esp_fill_random(buf.as_mut_ptr().cast(), buf.len()) }
    }
    fn logs(&self, max: usize) -> Vec<String> {
        LOGS.lines(max)
    }
    fn features(&self) -> Vec<&'static str> {
        vec!["wifi", "reboot", "mode", "logs", "token_rotate"]
    }
}
