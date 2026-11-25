//! ESP-IDF glue shared by the zigbee-rs-hub firmwares: `firmware/hub`
//! (ESP32-S3 of the Thread BR board, ZRHP) and `firmware/gateway` (the
//! classic ESP32 of the SONOFF Dongle Max, EZSP). Each firmware keeps its
//! board, boot and network code; what the hub application needs from
//! ESP-IDF is here, once.
//!
//! | Module | Contents |
//! |---|---|
//! | [`alloc`] | Rust heap preferring PSRAM (the firmware declares the `#[global_allocator]`) |
//! | [`cpanic`] | the last ESP-IDF panic and the last Rust panic message in RTC memory, failed allocations |
//! | [`cpu`] | CPU use per task, thread names, atomics check between the cores |
//! | [`flash`] | a window of a data partition as `hub_store::Flash` (journal, OTA image store, `files` region) |
//! | [`resources`] | the buffer limits of this boot (`hub_core::resources`) |
//! | `catalog` | (feature `app`) a window of a data partition, mapped, as the catalog's `DataFlash` |
//! | `http` | (feature `app`) the HTTP server: web UI, web tools, `/api` |
//! | `ws` | (feature `app`) the `/api` WebSocket endpoint |
//! | `logs` | (feature `app`) the logger with the log ring for `hub/logs`, SNTP and the local time offset |
//! | `mqtt` | (feature `mqtt`) the MQTT bridge on esp-mqtt (or `mqtt-lite`: its own client) |

pub mod alloc;
#[cfg(feature = "app")]
pub mod catalog;
pub mod cpanic;
pub mod cpu;
pub mod flash;
#[cfg(feature = "app")]
pub mod http;
#[cfg(feature = "app")]
pub mod logs;
#[cfg(feature = "mqtt")]
pub mod mqtt;
pub mod resources;
#[cfg(feature = "app")]
pub mod ws;

use std::sync::Arc;

/// The hardware RNG (`esp_fill_random`).
pub fn rng() -> Arc<dyn Fn(&mut [u8]) + Send + Sync> {
    Arc::new(|buf: &mut [u8]| fill_random(buf))
}

/// Fills `buf` from the hardware RNG.
pub fn fill_random(buf: &mut [u8]) {
    unsafe { esp_idf_svc::sys::esp_fill_random(buf.as_mut_ptr().cast(), buf.len()) }
}

/// Lower-case hex.
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A new API token: 12 random bytes, hex.
pub fn new_token() -> String {
    let mut b = [0u8; 12];
    fill_random(&mut b);
    hex(&b)
}
