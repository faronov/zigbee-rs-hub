//! The gateway's side of the maintenance API ([`hub_app::Platform`]):
//! system facts (Ethernet and Wi-Fi, the `files` region, the app slots),
//! Wi-Fi credentials (NVS `zrh`, applied at once by [`net`](crate::net)),
//! the API token in NVS, reboot, the hardware RNG, the log tail, time
//! (`zrh_espapp::logs`).

use std::time::Duration;

use esp_idf_svc::sys;
use log::info;
use serde_json::{Value as J, json};
use zrh_espapp::logs;

use crate::{board, files, net, settings, update};

/// Cargo features of the image's hub (`hub/system` `build`).
pub const FEATURES: &str = "ezsp,advanced,ota-store,mqtt";

/// The gateway platform.
pub struct Gateway;

fn reboot_soon(why: &'static str) {
    let _ = std::thread::Builder::new().stack_size(3072).spawn(move || {
        info!("{why}: rebooting in 1 s");
        // The API reply goes out first.
        std::thread::sleep(Duration::from_secs(1));
        crate::restart()
    });
}

impl hub_app::Platform for Gateway {
    fn system(&self) -> J {
        let r = zrh_espapp::resources::get();
        let desc = unsafe { &*sys::esp_app_get_description() };
        let version = unsafe { std::ffi::CStr::from_ptr(desc.version.as_ptr()) }.to_string_lossy().into_owned();
        json!({
            "version": version,
            "board": board().name,
            "uptime_s": logs::uptime_s(),
            "heap": logs::heap_json(),
            "wifi": net::wifi_info(),
            "ethernet": net::eth_info(),
            "reset_reason": unsafe { sys::esp_reset_reason() },
            "build": {"features": FEATURES, "psram": "quad"},
            "memory_mode": r.memory_mode(),
            "limits": {
                "http_sockets": r.http_sockets,
                "ws_client_bytes": r.ws_client_bytes,
                "ws_total_bytes": r.ws_total_bytes,
                "ota_streams": r.ota_streams,
            },
            "storage": files::status_json(),
            "firmware": update::status(),
        })
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
        settings::set("api_token", Some(token))
    }
    fn reboot(&self) -> Result<(), String> {
        reboot_soon("hub/reboot");
        Ok(())
    }
    fn random(&self, buf: &mut [u8]) {
        zrh_espapp::fill_random(buf)
    }
    fn logs(&self, max: usize) -> Vec<String> {
        logs::lines(max)
    }
    fn features(&self) -> Vec<&'static str> {
        vec!["wifi", "reboot", "logs", "token_rotate", "time"]
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
