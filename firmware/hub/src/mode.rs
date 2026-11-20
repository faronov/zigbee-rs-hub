//! Boot mode: the phase-0 bridge (default) or the hub.
//!
//! - NVS `hub/mode`: 0 bridge, 1 hub (absent = bridge).
//! - RTC no-init word (survives `esp_restart`, random after power-on):
//!   `HUB1` boots the hub once, `ROM1` boots the bridge once with the H2 in
//!   its ROM serial loader (manual NCP recovery). Both are cleared on use,
//!   so any later reset falls back to the persisted mode.
//! - A boot after a panic or watchdog reset is handled before this (bridge
//!   safe mode, USB on USB-Serial-JTAG).

use std::sync::Mutex;

use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use log::warn;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Bridge,
    Hub,
}

/// What this boot runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Boot {
    pub mode: Mode,
    /// Put the H2 into its ROM loader right after start (bridge mode).
    pub h2_rom: bool,
    /// The mode comes from a one-shot request.
    pub once: bool,
}

const HUB_ONCE: u32 = u32::from_le_bytes(*b"HUB1");
const ROM_ONCE: u32 = u32::from_le_bytes(*b"ROM1");

#[unsafe(link_section = ".rtc_noinit")]
static mut ONE_SHOT: u32 = 0;

static NVS: Mutex<Option<EspNvs<NvsDefault>>> = Mutex::new(None);
static PARTITION: std::sync::OnceLock<EspDefaultNvsPartition> = std::sync::OnceLock::new();

/// The default NVS partition (shared with Wi-Fi), once [`boot`] opened it.
#[cfg(feature = "app")]
pub fn partition() -> Option<EspDefaultNvsPartition> {
    PARTITION.get().cloned()
}

/// A string setting in NVS `hub/<key>`.
pub fn get_str(key: &str) -> Option<String> {
    let g = NVS.lock().unwrap();
    let mut buf = [0u8; 128];
    g.as_ref()?.get_str(key, &mut buf).ok().flatten().map(str::to_string)
}

/// Store (`Some`) or remove (`None`) a string setting.
pub fn set_str(key: &str, value: Option<&str>) -> Result<(), String> {
    let mut g = NVS.lock().unwrap();
    let nvs = g.as_mut().ok_or("NVS unavailable")?;
    match value {
        Some(v) => nvs.set_str(key, v).map_err(|e| e.to_string()),
        None => nvs.remove(key).map(|_| ()).map_err(|e| e.to_string()),
    }
}

fn persisted() -> Option<Mode> {
    let g = NVS.lock().unwrap();
    match g.as_ref()?.get_u8("mode") {
        Ok(Some(1)) => Some(Mode::Hub),
        Ok(_) => Some(Mode::Bridge),
        Err(e) => {
            warn!("nvs hub/mode: {e}");
            None
        }
    }
}

/// Open NVS and decide the mode of this boot.
pub fn boot() -> Boot {
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
    let shot = unsafe { core::ptr::addr_of_mut!(ONE_SHOT).replace(0) };
    match shot {
        HUB_ONCE => Boot { mode: Mode::Hub, h2_rom: false, once: true },
        ROM_ONCE => Boot { mode: Mode::Bridge, h2_rom: true, once: true },
        _ => Boot { mode: persisted().unwrap_or(Mode::Bridge), h2_rom: false, once: false },
    }
}

/// The persisted mode (`None` without NVS).
pub fn stored() -> Option<Mode> {
    persisted()
}

/// Persist `mode`.
pub fn store(mode: Mode) -> Result<(), String> {
    let mut g = NVS.lock().unwrap();
    let nvs = g.as_mut().ok_or("NVS unavailable")?;
    nvs.set_u8("mode", (mode == Mode::Hub) as u8).map_err(|e| e.to_string())
}

/// Request a one-shot boot and restart.
pub fn restart_once(hub: bool) -> ! {
    unsafe { core::ptr::addr_of_mut!(ONE_SHOT).write(if hub { HUB_ONCE } else { ROM_ONCE }) };
    restart()
}

pub fn restart() -> ! {
    std::thread::sleep(std::time::Duration::from_millis(100));
    unsafe { esp_idf_svc::sys::esp_restart() }
}
