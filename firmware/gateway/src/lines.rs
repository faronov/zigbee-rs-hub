//! NCP reset and serial-loader lines from the board profile.
//!
//! [`init`] configures every line released before anything else runs: on
//! the Dongle-M that drives GPIO12 (MG24 bootloader entry, the ESP32's MTDI
//! flash-voltage strap) low and keeps GPIO15 (MG24 nRESET) high. GPIO12 is
//! high only inside [`enter_bootloader`], never across an ESP32 reset the
//! firmware starts. Open-drain lines only ever pull low and release to the
//! target's pull-up.

use std::sync::{Mutex, OnceLock};
use std::thread::sleep;
use std::time::Duration;

use esp_idf_svc::sys::{self, EspError, esp};
use hub_core::profile::{BoardProfile, NcpControl, NcpLoader, Output};

static LINES: OnceLock<Mutex<NcpControl>> = OnceLock::new();

fn configure(o: &Output) -> Result<(), EspError> {
    unsafe {
        // Level first, so the pin never glitches to the active level.
        esp!(sys::gpio_set_level(o.gpio.into(), o.level(false).into()))?;
        let mut cfg: sys::gpio_config_t = core::mem::zeroed();
        cfg.pin_bit_mask = 1u64 << o.gpio;
        cfg.mode = if o.open_drain { sys::gpio_mode_t_GPIO_MODE_INPUT_OUTPUT_OD } else { sys::gpio_mode_t_GPIO_MODE_INPUT_OUTPUT };
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
    unsafe { sys::gpio_set_level(o.gpio.into(), o.level(asserted).into()) };
}

fn ms(n: u32) {
    sleep(Duration::from_millis(n.into()));
}

/// Configure the NCP lines released (boot line first). Call first in
/// `main`; again if overrides changed them.
pub fn init(board: &BoardProfile) -> Result<(), EspError> {
    let c = board.control;
    configure(&c.boot())?;
    if let Some(r) = &c.reset {
        configure(r)?;
    }
    match LINES.get() {
        Some(m) => *m.lock().unwrap() = c,
        None => {
            let _ = LINES.set(Mutex::new(c));
        }
    }
    Ok(())
}

fn with<R>(f: impl FnOnce(&NcpControl) -> R) -> Option<R> {
    LINES.get().map(|m| f(&m.lock().unwrap()))
}

fn pulse(c: &NcpControl) {
    if let Some(r) = &c.reset {
        drive(r, true);
        ms(c.reset_ms);
        drive(r, false);
    }
}

/// Reset the NCP into its application: loader line released, reset pulse.
pub fn reset_to_app() {
    with(|c| {
        drive(&c.boot(), false);
        ms(10);
        pulse(c);
    });
}

/// Reset the NCP into its serial loader. Gecko: boot asserted, reset
/// pulse, boot released `release_ms` after the reset release (SONOFF's
/// sequence: boot high 100 ms before a 100 ms GPIO15 pulse).
pub fn enter_bootloader() -> Result<(), &'static str> {
    with(|c| match c.loader {
        NcpLoader::GeckoUart { boot, setup_ms, release_ms, .. } => {
            drive(&boot, true);
            ms(setup_ms);
            pulse(c);
            ms(release_ms);
            drive(&boot, false);
            Ok(())
        }
        NcpLoader::EspRom { boot, hold_ms } => {
            drive(&boot, true);
            pulse(c);
            ms(hold_ms);
            drive(&boot, false);
            Ok(())
        }
    })
    .unwrap_or(Err("NCP lines not configured"))
}

/// Current levels of the reset and boot lines (`(reset, boot)`, `true` =
/// high), for the console.
pub fn levels() -> Option<(Option<bool>, bool)> {
    let level = |g: u8| unsafe { sys::gpio_get_level(g.into()) } != 0;
    with(|c| (c.reset.map(|r| level(r.gpio)), level(c.boot().gpio)))
}
