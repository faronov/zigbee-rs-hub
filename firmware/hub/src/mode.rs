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
    match EspDefaultNvsPartition::take().and_then(|p| EspNvs::new(p, "hub", true)) {
        Ok(nvs) => *NVS.lock().unwrap() = Some(nvs),
        Err(e) => warn!("NVS unavailable ({e}); the mode cannot be persisted"),
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
