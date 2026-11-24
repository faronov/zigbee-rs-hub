//! Settings in the NVS namespace `zrh` of the default `nvs` partition.
//!
//! On the Dongle-M that partition belongs to the stock firmware too (its
//! Wi-Fi and web settings): the firmware only adds and changes keys of its
//! own namespace, never initialises the partition over existing data
//! (`nvs_flash_init` without the erase-and-retry fallback) and keeps
//! everything else out of NVS (Wi-Fi without NVS storage, no PHY
//! calibration data).
//!
//! | Key | Content |
//! |---|---|
//! | `wifi_ssid`, `wifi_pass` | Wi-Fi station credentials |
//! | `board` | NCP wiring overrides (`BoardProfile::apply` syntax) |
//! | `led` | LED brightness, 0–100 % |

use std::sync::Mutex;

use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use log::warn;

const NAMESPACE: &str = "zrh";

static NVS: Mutex<Option<EspNvs<NvsDefault>>> = Mutex::new(None);

/// Open NVS (once, at boot).
pub fn open() {
    // `false`: no erase when the partition is full or has a newer format.
    match EspDefaultNvsPartition::take_with(false) {
        Ok(p) => match EspNvs::new(p, NAMESPACE, true) {
            Ok(nvs) => *NVS.lock().unwrap() = Some(nvs),
            Err(e) => warn!("NVS namespace {NAMESPACE} unavailable ({e}); settings are not persisted"),
        },
        Err(e) => warn!("NVS unavailable ({e}); settings are not persisted"),
    }
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
