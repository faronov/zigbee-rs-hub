//! NCP reset and serial-loader lines from the board profile.
//!
//! Every line is configured released before anything else runs
//! ([`init`]): on the Dongle-M that drives GPIO12 (MG24 bootloader entry,
//! ESP32 MTDI flash-voltage strap) low and keeps GPIO15 (MG24 RESET_N) high.
//! Open-drain lines only ever pull low and release to the target's pull-up.

use std::io;
use std::sync::Mutex;
use std::thread::sleep;
use std::time::Duration;

use esp_idf_svc::sys::{self, EspError, esp};
use hub_core::profile::{BoardProfile, NcpLoader, Output};
use hub_ncp::transport::NcpControl;

static LOCK: Mutex<()> = Mutex::new(());

fn configure(o: &Output) -> Result<(), EspError> {
    unsafe {
        // Level first, so the pin never glitches to the active level.
        esp!(sys::gpio_set_level(o.gpio.into(), o.level(false).into()))?;
        let mut cfg: sys::gpio_config_t = core::mem::zeroed();
        cfg.pin_bit_mask = 1u64 << o.gpio;
        cfg.mode = if o.open_drain {
            sys::gpio_mode_t_GPIO_MODE_INPUT_OUTPUT_OD
        } else {
            sys::gpio_mode_t_GPIO_MODE_INPUT_OUTPUT
        };
        cfg.pull_up_en = if o.open_drain && !o.active_high {
            sys::gpio_pullup_t_GPIO_PULLUP_ENABLE
        } else {
            sys::gpio_pullup_t_GPIO_PULLUP_DISABLE
        };
        cfg.pull_down_en = sys::gpio_pulldown_t_GPIO_PULLDOWN_DISABLE;
        cfg.intr_type = sys::gpio_int_type_t_GPIO_INTR_DISABLE;
        esp!(sys::gpio_config(&cfg))?;
        esp!(sys::gpio_set_level(o.gpio.into(), o.level(false).into()))
    }
}

fn drive(o: &Output, asserted: bool) {
    unsafe {
        sys::gpio_set_level(o.gpio.into(), o.level(asserted).into());
    }
}

fn ms(n: u32) {
    sleep(Duration::from_millis(n.into()));
}

/// The board's NCP lines.
#[derive(Clone, Copy)]
pub struct BoardLines {
    reset: Option<Output>,
    reset_ms: u32,
    loader: NcpLoader,
}

/// Configure every NCP control line released. Call first in `main`.
pub fn init(board: &BoardProfile) -> Result<BoardLines, EspError> {
    if let NcpLoader::EspRom { boot, .. } | NcpLoader::GeckoUart { boot, .. } = board.control.loader {
        configure(&boot)?;
    }
    if let Some(r) = &board.control.reset {
        configure(r)?;
    }
    Ok(BoardLines { reset: board.control.reset, reset_ms: board.control.reset_ms, loader: board.control.loader })
}

impl BoardLines {
    fn pulse(&self) -> io::Result<()> {
        let r = self.reset.as_ref().ok_or(io::ErrorKind::Unsupported)?;
        drive(r, true);
        ms(self.reset_ms);
        drive(r, false);
        Ok(())
    }

    /// Whether the board can enter a serial loader.
    pub fn has_loader(&self) -> bool {
        !matches!(self.loader, NcpLoader::Swd) && self.reset.is_some()
    }
}

impl NcpControl for BoardLines {
    fn reset(&mut self) -> io::Result<()> {
        let _g = LOCK.lock().unwrap();
        if let NcpLoader::EspRom { boot, .. } | NcpLoader::GeckoUart { boot, .. } = &self.loader {
            drive(boot, false);
        }
        self.pulse()
    }

    fn enter_rom_loader(&mut self) -> io::Result<()> {
        let _g = LOCK.lock().unwrap();
        match self.loader {
            NcpLoader::EspRom { boot, hold_ms } => {
                drive(&boot, true);
                self.pulse()?;
                ms(hold_ms);
                drive(&boot, false);
                Ok(())
            }
            NcpLoader::GeckoUart { boot, setup_ms, release_ms, .. } => {
                // GPIO12 on the Dongle-M is the ESP32's flash-voltage strap:
                // it is high only for this ~0.7 s window, never across an
                // ESP32 reset initiated by the firmware.
                drive(&boot, true);
                ms(setup_ms);
                let r = self.pulse();
                ms(release_ms);
                drive(&boot, false);
                r
            }
            NcpLoader::Swd => Err(io::Error::new(io::ErrorKind::Unsupported, "this board flashes the NCP over SWD")),
        }
    }
}
