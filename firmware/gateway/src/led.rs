//! Status LED and user button from the board profile.
//!
//! RGB LED (Dongle-M, active high, on/off per colour):
//!
//! | Colour | Meaning |
//! |---|---|
//! | blue, blinking | no network |
//! | yellow | network up, no NCP session |
//! | green | network up and NCP session |
//! | white, blinking | joining open (`permit join`) |
//! | magenta, fast blink | NCP firmware install running |
//! | red | configuration error (partition table) or the last NCP update failed |
//!
//! Button: a press shorter than 3 s opens joining for 180 s.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use esp_idf_svc::sys;
use hub_core::profile::{BoardProfile, Input, Led, Output};
use hub_ncp::Hub;
use log::{info, warn};

use crate::{net, radio};

/// Set when the hub could not start (partition table).
pub static CONFIG_ERROR: AtomicBool = AtomicBool::new(false);
static HUB: OnceLock<Hub> = OnceLock::new();

/// Give the LED thread the hub (once it runs).
pub fn set_hub(hub: Hub) {
    let _ = HUB.set(hub);
}

fn out(o: &Output, on: bool) {
    unsafe { sys::gpio_set_level(o.gpio.into(), o.level(on).into()) };
}

fn configure_out(o: &Output) {
    unsafe {
        let g = i32::from(o.gpio);
        sys::gpio_reset_pin(g);
        sys::gpio_set_direction(g, sys::gpio_mode_t_GPIO_MODE_OUTPUT);
    }
    out(o, false);
}

fn configure_in(i: &Input) {
    unsafe {
        let g = i32::from(i.gpio);
        sys::gpio_reset_pin(g);
        sys::gpio_set_direction(g, sys::gpio_mode_t_GPIO_MODE_INPUT);
        if i.pull_up {
            sys::gpio_set_pull_mode(g, sys::gpio_pull_mode_t_GPIO_PULLUP_ONLY);
        }
    }
}

fn pressed(i: &Input) -> bool {
    let level = unsafe { sys::gpio_get_level(i.gpio.into()) } != 0;
    level != i.active_low
}

/// (r, g, b) at tick `t` (125 ms).
fn colour(t: u32) -> (bool, bool, bool) {
    let blink = |period: u32| t % period < period / 2;
    if radio::BUSY.load(Ordering::Relaxed) {
        let on = blink(2);
        return (on, false, on);
    }
    if CONFIG_ERROR.load(Ordering::Relaxed) || radio::FAILED.load(Ordering::Relaxed) {
        return (true, false, false);
    }
    let session = HUB.get().is_some_and(|h| h.status().ncp.is_some());
    let permit = HUB.get().is_some_and(|h| h.status().permit_join_s > 0);
    let up = net::STATE.lock().unwrap().up;
    match (up, session, permit) {
        (_, true, true) => {
            let on = blink(8);
            (on, on, on)
        }
        (false, _, _) => (false, false, blink(8)),
        (true, false, _) => (true, true, false),
        (true, true, false) => (false, true, false),
    }
}

/// Start the LED/button thread if the board has either.
pub fn start(board: &BoardProfile) {
    let rgb = match board.led {
        Led::Rgb { rgb } => Some(rgb),
        Led::None => None,
    };
    let button = board.button;
    if rgb.is_none() && button.is_none() {
        return;
    }
    if let Some(rgb) = &rgb {
        rgb.iter().for_each(configure_out);
    }
    if let Some(b) = &button {
        configure_in(b);
    }
    let spawned = std::thread::Builder::new().name("led".into()).stack_size(3072).spawn(move || {
        let mut t = 0u32;
        let mut down_since: Option<Instant> = None;
        loop {
            if let Some(rgb) = &rgb {
                let (r, g, b) = colour(t);
                out(&rgb[0], r);
                out(&rgb[1], g);
                out(&rgb[2], b);
            }
            if let Some(btn) = &button {
                match (pressed(btn), down_since) {
                    (true, None) => down_since = Some(Instant::now()),
                    (false, Some(since)) => {
                        down_since = None;
                        let held = since.elapsed();
                        if held >= Duration::from_millis(40) && held < Duration::from_secs(3) {
                            match HUB.get().map(|h| h.permit_join(180)) {
                                Some(Ok(())) => info!("button: joining open for 180 s"),
                                Some(Err(e)) => warn!("button: permit join: {e}"),
                                None => warn!("button: hub not running"),
                            }
                        }
                    }
                    _ => {}
                }
            }
            t = t.wrapping_add(1);
            std::thread::sleep(Duration::from_millis(125));
        }
    });
    if let Err(e) = spawned {
        warn!("led thread: {e}");
    }
}
