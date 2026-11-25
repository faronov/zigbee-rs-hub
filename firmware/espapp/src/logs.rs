//! Logging, time and SNTP for the hub's platform (`hub_app::Platform`).
//!
//! The firmware's console may not be visible (the S3's hub-mode logs go to
//! UART0, which the lab host cannot see); [`Tee`] also keeps Rust log
//! lines up to Info in a small RAM ring for `hub/logs` (ESP-IDF's own C
//! logs are not in it), with the local time once the clock is set. SNTP
//! (`esp_sntp`) runs with the servers the app keeps, from the first time
//! the network is up; the clock survives a software restart, so the time
//! is known at boot after the first synchronisation. Nothing logs keys.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use esp_idf_svc::log::{EspIdfLogFilter, EspIdfLogger};
use esp_idf_svc::sys;
use hub_app::LogRing;
use log::{Level, Log, Metadata, Record, info};

/// Recent log lines (3 KiB, allocated with the first line).
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

/// Replaces `EspLogger::initialize_default()`.
pub fn init_logger() {
    if log::set_logger(&TEE).is_ok() {
        ESP.filter().initialize();
    }
}

/// Unix time, None before the clock is set.
pub fn unix_now() -> Option<i64> {
    let mut t: sys::time_t = 0;
    unsafe { sys::time(&mut t) };
    (i64::from(t) >= hub_app::clock::VALID_AFTER_UNIX).then_some(i64::from(t))
}

/// The local time's offset (the app keeps it; newlib's TZ code is not
/// linked).
static UTC_OFFSET_S: AtomicI32 = AtomicI32::new(0);

/// The SNTP servers and the client (started once the network is up,
/// restarted when the servers change).
type Sntp = (Vec<String>, Option<esp_idf_svc::sntp::EspSntp<'static>>);
static SNTP: Mutex<Sntp> = Mutex::new((Vec::new(), None));
static NET_UP: AtomicBool = AtomicBool::new(false);
/// SNTP has synchronised the clock since boot.
static SNTP_SYNCED: AtomicBool = AtomicBool::new(false);

fn start_sntp(s: &mut Sntp) -> Result<(), String> {
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

/// The network came up or went down: SNTP starts with the first time up.
pub fn network_changed(up: bool) {
    NET_UP.store(up, Ordering::Relaxed);
    let mut s = SNTP.lock().unwrap();
    if up
        && s.1.is_none()
        && !s.0.is_empty()
        && let Err(e) = start_sntp(&mut s)
    {
        log::warn!("{e}");
    }
}

/// `Platform::time_source`.
pub fn time_source() -> Option<&'static str> {
    if SNTP_SYNCED.load(Ordering::Relaxed) {
        return Some("sntp");
    }
    unix_now().map(|_| "rtc")
}

/// `Platform::set_utc_offset`.
pub fn set_utc_offset(offset_s: i32) {
    UTC_OFFSET_S.store(offset_s, Ordering::Relaxed);
}

/// `Platform::set_time`: use these SNTP servers (now, if the network is up).
pub fn set_time(servers: &[String]) -> Result<(), String> {
    let mut s = SNTP.lock().unwrap();
    s.0 = servers.to_vec();
    s.1 = None;
    if NET_UP.load(Ordering::Relaxed) { start_sntp(&mut s) } else { Ok(()) }
}

/// `Platform::logs`.
pub fn lines(max: usize) -> Vec<String> {
    LOGS.lines(max)
}

/// `Platform::heap`: internal RAM free and its largest block.
pub fn internal_heap() -> (usize, usize) {
    let caps = sys::MALLOC_CAP_INTERNAL;
    unsafe { (sys::heap_caps_get_free_size(caps), sys::heap_caps_get_largest_free_block(caps)) }
}

/// `Platform::data_heap_free`: internal RAM and, on modules with it, the
/// PSRAM Rust data prefer.
pub fn data_heap_free() -> usize {
    unsafe { sys::heap_caps_get_free_size(sys::MALLOC_CAP_8BIT) }
}

/// `Platform::stack_high_water` (ESP-IDF counts the stack in bytes).
pub fn stack_high_water() -> usize {
    unsafe { sys::uxTaskGetStackHighWaterMark(std::ptr::null_mut()) as usize }
}

/// `hub/system` `heap`: free, minimum free and largest block of internal
/// RAM, total PSRAM.
pub fn heap_json() -> serde_json::Value {
    let (free, min, largest, psram) = unsafe {
        (
            sys::heap_caps_get_free_size(sys::MALLOC_CAP_INTERNAL),
            sys::heap_caps_get_minimum_free_size(sys::MALLOC_CAP_INTERNAL),
            sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_INTERNAL),
            sys::heap_caps_get_total_size(sys::MALLOC_CAP_SPIRAM),
        )
    };
    serde_json::json!({"free": free, "min_free": min, "largest_block": largest, "psram": psram})
}

/// Uptime in seconds.
pub fn uptime_s() -> i64 {
    (unsafe { sys::esp_timer_get_time() }) / 1_000_000
}
