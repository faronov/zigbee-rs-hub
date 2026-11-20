//! Settings in NVS namespace `hub` (shared default partition with Wi-Fi).
//!
//! | Key | Content |
//! |---|---|
//! | `wifi_ssid`, `wifi_pass` | Wi-Fi station credentials |
//! | `api_token` | WebSocket/HTTP API token (generated at the first boot) |
//! | `board` | NCP wiring overrides (`BoardProfile::apply` syntax) |
//! | `radio_job` | pending radio update step (`gecko::job::Job` text) |
//! | `radio_last` | outcome of the last radio update (text) |

use std::sync::{Mutex, OnceLock};

use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use log::warn;

static NVS: Mutex<Option<EspNvs<NvsDefault>>> = Mutex::new(None);
static PARTITION: OnceLock<EspDefaultNvsPartition> = OnceLock::new();

/// Open NVS (once, at boot).
pub fn open() {
    match EspDefaultNvsPartition::take() {
        Ok(p) => {
            let _ = PARTITION.set(p.clone());
            match EspNvs::new(p, "hub", true) {
                Ok(nvs) => *NVS.lock().unwrap() = Some(nvs),
                Err(e) => warn!("NVS hub namespace unavailable ({e}); settings cannot be persisted"),
            }
        }
        Err(e) => warn!("NVS unavailable ({e}); settings cannot be persisted"),
    }
}

/// The default NVS partition (for Wi-Fi).
pub fn partition() -> Option<EspDefaultNvsPartition> {
    PARTITION.get().cloned()
}

/// A string setting.
pub fn get(key: &str) -> Option<String> {
    let g = NVS.lock().unwrap();
    let mut buf = [0u8; 256];
    g.as_ref()?.get_str(key, &mut buf).ok().flatten().map(str::to_string)
}

/// Store (`Some`) or remove (`None`) a string setting.
pub fn set(key: &str, value: Option<&str>) -> Result<(), String> {
    let mut g = NVS.lock().unwrap();
    let nvs = g.as_mut().ok_or("NVS unavailable")?;
    match value {
        Some(v) => nvs.set_str(key, v).map_err(|e| e.to_string()),
        None => nvs.remove(key).map(|_| ()).map_err(|e| e.to_string()),
    }
}
