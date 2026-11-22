//! The S3 side of the maintenance API ([`hub_app::Platform`]): system facts,
//! Wi-Fi credentials (NVS, applied at once by `net`), the API token in NVS,
//! reboot and boot mode, `esp_fill_random`, and the log tail of [`LOGS`].
//!
//! Hub-mode logs go to UART0, which the lab host cannot see; [`Tee`] also
//! keeps Rust log lines up to Info in a small RAM ring for `hub/logs`
//! (ESP-IDF's own C logs are not in it), with the local time once the
//! clock is set. SNTP (`esp_sntp`) runs with the servers the app keeps;
//! the S3's clock survives a software restart, so the time is known at
//! boot after the first synchronisation. Nothing logs keys.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
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
            match unix_now() {
                // "MM-DD HH:MM:SS", local.
                Some(t) => {
                    let c = hub_app::clock::civil(t, UTC_OFFSET_S.load(Ordering::Relaxed));
                    LOGS.push(&format!("{} {} {}: {}", &c[5..], r.level(), r.target(), r.args()))
                }
                None => {
                    let t = unsafe { sys::esp_timer_get_time() } / 1000;
                    LOGS.push(&format!("{}.{:03} {} {}: {}", t / 1000, t % 1000, r.level(), r.target(), r.args()))
                }
            }
        }
    }
    fn flush(&self) {}
}

/// Unix time, None before the clock is set.
fn unix_now() -> Option<i64> {
    let mut t: sys::time_t = 0;
    unsafe { sys::time(&mut t) };
    (i64::from(t) >= hub_app::clock::VALID_AFTER_UNIX).then_some(i64::from(t))
}

/// The local time's offset (the app keeps it; newlib's TZ code is not
/// linked).
static UTC_OFFSET_S: core::sync::atomic::AtomicI32 = core::sync::atomic::AtomicI32::new(0);

/// The SNTP servers and the client (started once Wi-Fi is up, restarted
/// when the servers change).
static SNTP: Mutex<(Vec<String>, Option<esp_idf_svc::sntp::EspSntp<'static>>)> = Mutex::new((Vec::new(), None));
static WIFI_UP: AtomicBool = AtomicBool::new(false);
/// SNTP has synchronised the clock since boot.
static SNTP_SYNCED: AtomicBool = AtomicBool::new(false);

fn start_sntp(s: &mut (Vec<String>, Option<esp_idf_svc::sntp::EspSntp<'static>>)) -> Result<(), String> {
    let mut conf = esp_idf_svc::sntp::SntpConf::default();
    for (slot, name) in conf.servers.iter_mut().zip(s.0.iter().map(String::as_str).chain(std::iter::repeat(""))) {
        *slot = name;
    }
    let client = esp_idf_svc::sntp::EspSntp::new_with_callback(&conf, |_| {
        if !SNTP_SYNCED.swap(true, Ordering::Relaxed) {
            info!("time: synchronised by SNTP");
        }
    })
    .map_err(|e| format!("SNTP: {e}"))?;
    s.1 = Some(client);
    Ok(())
}

/// Wi-Fi came up or went down: SNTP starts with the first connection.
pub fn wifi_changed(up: bool) {
    WIFI_UP.store(up, Ordering::Relaxed);
    let mut s = SNTP.lock().unwrap();
    if up && s.1.is_none() && !s.0.is_empty()
        && let Err(e) = start_sntp(&mut s)
    {
        log::warn!("{e}");
    }
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
        vec!["wifi", "reboot", "mode", "logs", "token_rotate", "time"]
    }
    fn time_source(&self) -> Option<&'static str> {
        if SNTP_SYNCED.load(Ordering::Relaxed) {
            return Some("sntp");
        }
        unix_now().map(|_| "rtc")
    }
    fn set_utc_offset(&self, offset_s: i32) {
        UTC_OFFSET_S.store(offset_s, Ordering::Relaxed);
    }
    fn set_time(&self, servers: &[String]) -> Result<(), String> {
        let mut s = SNTP.lock().unwrap();
        s.0 = servers.to_vec();
        s.1 = None;
        if WIFI_UP.load(Ordering::Relaxed) { start_sntp(&mut s) } else { Ok(()) }
    }
}
