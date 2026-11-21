//! Lab access to the board's PTA links R21–R24 (ZRHP spec 11.5, hardware
//! gate H9 wiring check), bridge mode only: in hub mode the S3's
//! coexistence leader owns them.
//!
//! request (GPIO12) and priority (GPIO10) are driven by the H2 follower;
//! grant (GPIO13) and tx_line (GPIO11) are the S3's outputs. None of the four
//! is a strapping, flash, PSRAM or USB pin on the S3 (strapping: GPIO0, 3, 45,
//! 46). Grant and tx_line stay inputs until a command drives them.

use std::time::{Duration, Instant};

use esp_idf_svc::sys;
use hub_core::board::pta;

/// `request=… priority=… grant=… tx_line=…`.
pub fn levels() -> String {
    let level = |p| unsafe { sys::gpio_get_level(p) };
    format!(
        "request(GPIO{})={} priority(GPIO{})={} grant(GPIO{})={} tx_line(GPIO{})={}",
        pta::REQUEST,
        level(pta::REQUEST),
        pta::PRIORITY,
        level(pta::PRIORITY),
        pta::GRANT,
        level(pta::GRANT),
        pta::TX_LINE,
        level(pta::TX_LINE)
    )
}

/// Drive `grant` or `tx_line` (`high`, `low`) or release it (`in`: input,
/// floating; `pd`: input with pull-down). request/priority: `in`/`pd` only.
pub fn set(line: &str, mode: &str) -> Result<(), &'static str> {
    let (pin, output) = match line {
        "grant" => (pta::GRANT, true),
        "tx_line" => (pta::TX_LINE, true),
        "request" => (pta::REQUEST, false),
        "priority" => (pta::PRIORITY, false),
        _ => return Err("line: grant, tx_line, request, priority"),
    };
    unsafe {
        match mode {
            "high" | "low" if output => {
                sys::gpio_reset_pin(pin);
                sys::gpio_set_level(pin, (mode == "high") as u32);
                sys::gpio_set_direction(pin, sys::gpio_mode_t_GPIO_MODE_INPUT_OUTPUT);
                sys::gpio_set_level(pin, (mode == "high") as u32);
            }
            "in" | "pd" => {
                sys::gpio_reset_pin(pin);
                sys::gpio_set_direction(pin, sys::gpio_mode_t_GPIO_MODE_INPUT);
                sys::gpio_set_pull_mode(
                    pin,
                    if mode == "pd" { sys::gpio_pull_mode_t_GPIO_PULLDOWN_ONLY } else { sys::gpio_pull_mode_t_GPIO_FLOATING },
                );
            }
            _ => return Err("mode: high/low (grant, tx_line), in, pd"),
        }
    }
    Ok(())
}

/// Poll request and priority for `ms` (at most 5 s) and report how often
/// each was high and how many rising edges were seen: a coarse duty cycle
/// of the follower's activity (polling, not an edge capture).
pub fn sample(ms: u32) -> String {
    let window = Duration::from_millis(u64::from(ms.min(5_000)));
    let started = Instant::now();
    let (mut n, mut req_high, mut pri_high, mut req_edges, mut pri_edges) = (0u32, 0u32, 0u32, 0u32, 0u32);
    let (mut req_prev, mut pri_prev) = (0, 0);
    while started.elapsed() < window {
        let (req, pri) = unsafe { (sys::gpio_get_level(pta::REQUEST), sys::gpio_get_level(pta::PRIORITY)) };
        n += 1;
        req_high += req as u32;
        pri_high += pri as u32;
        req_edges += (req == 1 && req_prev == 0) as u32;
        pri_edges += (pri == 1 && pri_prev == 0) as u32;
        (req_prev, pri_prev) = (req, pri);
    }
    let pct = |high: u32| if n == 0 { 0.0 } else { f64::from(high) * 100.0 / f64::from(n) };
    format!(
        "{} samples in {} ms: request high {:.1}% ({} rises), priority high {:.1}% ({} rises)",
        n,
        window.as_millis(),
        pct(req_high),
        req_edges,
        pct(pri_high),
        pri_edges
    )
}
