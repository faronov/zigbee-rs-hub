//! Test access to the board's "reserved" S3↔H2 links (R18–R20), used to
//! check which 0 Ω resistors are fitted before relying on them for RTS/CTS.
//!
//! Only S3 GPIO4/5/6 are accepted: none of them is a strapping, flash, PSRAM
//! or USB pin on the S3, and on the H2 side (GPIO5/4/12) they are inputs in
//! the ROM loader and not strapping pins (ESP32-H2 datasheet v1.3: strapping
//! pins are GPIO8, GPIO9 and GPIO25). Taking a pin over turns flow control off.

use esp_idf_svc::sys;
use hub_core::board::reserved;

use crate::uart;

pub const PINS: [i32; 3] = [reserved::R18_S3, reserved::R19_S3, reserved::R20_S3];

/// `high`, `low`: push-pull output. `in`, `pu`, `pd`: input, floating or with
/// the internal pull-up / pull-down.
pub fn set(pin: i32, mode: &str) -> Result<(), &'static str> {
    if !PINS.contains(&pin) {
        return Err("only S3 GPIO4, 5, 6 (R20, R19, R18)");
    }
    if uart::flow() {
        uart::set_flow(false);
    }
    unsafe {
        sys::gpio_reset_pin(pin);
        match mode {
            "high" | "low" => {
                sys::gpio_set_level(pin, (mode == "high") as u32);
                sys::gpio_set_direction(pin, sys::gpio_mode_t_GPIO_MODE_INPUT_OUTPUT);
                sys::gpio_set_level(pin, (mode == "high") as u32);
            }
            "in" | "pu" | "pd" => {
                sys::gpio_set_direction(pin, sys::gpio_mode_t_GPIO_MODE_INPUT);
                let pull = match mode {
                    "pu" => sys::gpio_pull_mode_t_GPIO_PULLUP_ONLY,
                    "pd" => sys::gpio_pull_mode_t_GPIO_PULLDOWN_ONLY,
                    _ => sys::gpio_pull_mode_t_GPIO_FLOATING,
                };
                sys::gpio_set_pull_mode(pin, pull);
            }
            _ => return Err("mode: high, low, in, pu, pd"),
        }
    }
    Ok(())
}

pub fn levels() -> String {
    PINS.iter()
        .map(|&p| format!("GPIO{}={}", p, unsafe { sys::gpio_get_level(p) }))
        .collect::<Vec<_>>()
        .join(" ")
}
