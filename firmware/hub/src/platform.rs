//! The S3 side of the maintenance API ([`hub_app::Platform`]): system facts,
//! Wi-Fi credentials (NVS, applied at once by `net`), the API token in NVS,
//! reboot and boot mode, `esp_fill_random`, and the log tail of
//! `zrh_espapp::logs::LOGS` (logger, SNTP and time: `zrh_espapp::logs`).

use std::time::Duration;

use esp_idf_svc::sys;
use log::info;
use serde_json::{Value as J, json};
use zrh_espapp::logs;

use crate::mode::{self, Mode};
use crate::net;

pub use zrh_espapp::logs::{init_logger, network_changed as wifi_changed};

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
        let boot = crate::BOOT.get().copied();
        let r = crate::resources::get();
        #[allow(unused_mut)]
        let mut v = json!({
            "version": format!("tbr-hub {}", env!("CARGO_PKG_VERSION")),
            "uptime_s": logs::uptime_s(),
            "heap": logs::heap_json(),
            "mode": boot.map(|b| if b.once { "hub (once)" } else { "hub" }),
            "persisted_mode": match mode::stored() { Some(Mode::Hub) => "hub", Some(Mode::Bridge) => "bridge", None => "unknown" },
            "wifi": net::info(),
            "reset_reason": unsafe { sys::esp_reset_reason() },
            // The previous boot's crash: the Rust panic message (this boot
            // only), the last ESP-IDF panic record (until a power-on) and
            // the crashed boots in a row (0 after 10 min up).
            "crash": {
                "crashed": tbr_bridge::recovery::crashed_last_boot(),
                "streak": crate::crash_streak(),
                "panic": zrh_espapp::cpanic::last_panic(),
                "exception": zrh_espapp::cpanic::last(),
            },
            "build": {"features": crate::resources::features(), "psram": crate::resources::psram_mode()},
            "memory_mode": r.memory_mode(),
            "limits": {
                "http_sockets": r.http_sockets,
                "ws_client_bytes": r.ws_client_bytes,
                "ws_total_bytes": r.ws_total_bytes,
            },
        });
        #[cfg(feature = "ota")]
        {
            v["limits"]["ota_streams"] = r.ota_streams.into();
        }
        v
    }
    fn heap(&self) -> Option<(usize, usize)> {
        Some(logs::internal_heap())
    }
    fn data_heap_free(&self) -> Option<usize> {
        Some(logs::data_heap_free())
    }
    fn stack_high_water(&self) -> Option<usize> {
        Some(logs::stack_high_water())
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
        zrh_espapp::fill_random(buf)
    }
    fn logs(&self, max: usize) -> Vec<String> {
        logs::lines(max)
    }
    fn features(&self) -> Vec<&'static str> {
        vec!["wifi", "reboot", "mode", "logs", "token_rotate", "time"]
    }
    fn time_source(&self) -> Option<&'static str> {
        logs::time_source()
    }
    fn set_utc_offset(&self, offset_s: i32) {
        logs::set_utc_offset(offset_s)
    }
    fn set_time(&self, servers: &[String]) -> Result<(), String> {
        logs::set_time(servers)
    }
}
