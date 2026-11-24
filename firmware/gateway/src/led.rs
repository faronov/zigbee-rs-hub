//! Status LED (LEDC PWM) and user button from the board profile.
//!
//! | Colour | Meaning |
//! |---|---|
//! | magenta, fast blink | MG24 probe or bootloader banner running (UART claimed by the console) |
//! | green | network up (Ethernet or Wi-Fi has an address) |
//! | blue, blinking | no network |
//! | white flash | button pressed |
//!
//! Brightness: setting `led` (0–100 %, default [`DEFAULT_PCT`]). The
//! button is only reported (log, `info`) until a coordinator backend gives
//! it a function (permit join).

use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use esp_idf_svc::sys;
use hub_core::profile::{BoardProfile, Input, Led, Output};
use log::{info, warn};

use crate::{net, settings, uart};

/// Default brightness.
pub const DEFAULT_PCT: u8 = 30;
static PCT: AtomicU8 = AtomicU8::new(DEFAULT_PCT);
/// Short presses since boot.
pub static PRESSES: AtomicU32 = AtomicU32::new(0);

const MODE: sys::ledc_mode_t = sys::ledc_mode_t_LEDC_LOW_SPEED_MODE;
const TIMER: sys::ledc_timer_t = sys::ledc_timer_t_LEDC_TIMER_0;
const MAX_DUTY: u32 = 255;

/// Set the brightness (0–100 %).
pub fn set_brightness(pct: u8) {
    PCT.store(pct.min(100), Ordering::Relaxed);
}

/// Current brightness.
pub fn brightness() -> u8 {
    PCT.load(Ordering::Relaxed)
}

fn configure_rgb(rgb: &[Output; 3]) -> Result<(), sys::EspError> {
    unsafe {
        let mut t: sys::ledc_timer_config_t = core::mem::zeroed();
        t.speed_mode = MODE;
        t.duty_resolution = sys::ledc_timer_bit_t_LEDC_TIMER_8_BIT;
        t.timer_num = TIMER;
        t.freq_hz = 5000;
        t.clk_cfg = sys::soc_periph_ledc_clk_src_legacy_t_LEDC_AUTO_CLK;
        sys::esp!(sys::ledc_timer_config(&t))?;
        for (i, o) in rgb.iter().enumerate() {
            let mut c: sys::ledc_channel_config_t = core::mem::zeroed();
            c.gpio_num = o.gpio.into();
            c.speed_mode = MODE;
            c.channel = i as sys::ledc_channel_t;
            c.intr_type = sys::ledc_intr_type_t_LEDC_INTR_DISABLE;
            c.timer_sel = TIMER;
            c.duty = 0;
            c.flags.set_output_invert((!o.active_high).into());
            sys::esp!(sys::ledc_channel_config(&c))?;
        }
    }
    Ok(())
}

fn show(on: [bool; 3]) {
    let duty = MAX_DUTY * u32::from(brightness()) / 100;
    for (i, on) in on.into_iter().enumerate() {
        unsafe {
            sys::ledc_set_duty(MODE, i as sys::ledc_channel_t, if on { duty } else { 0 });
            sys::ledc_update_duty(MODE, i as sys::ledc_channel_t);
        }
    }
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

/// The button held down for 100 ms at boot (the escape from stored board
/// overrides).
pub(crate) fn held_at_boot(i: &Input) -> bool {
    configure_in(i);
    (0..5).all(|_| {
        let p = pressed(i);
        std::thread::sleep(Duration::from_millis(20));
        p
    })
}

/// (r, g, b) at LED tick `t` (125 ms).
fn colour(t: u32, flash: bool) -> [bool; 3] {
    let blink = |period: u32| t % period < period / 2;
    if flash {
        return [true; 3];
    }
    if uart::owner() == Some(uart::User::Console) {
        let on = blink(2);
        return [on, false, on];
    }
    if net::up() { [false, true, false] } else { [false, false, blink(8)] }
}

/// Start the LED/button thread if the board has either.
pub fn start(board: &BoardProfile) {
    if let Some(p) = settings::get("led").and_then(|s| s.parse().ok()) {
        set_brightness(p);
    }
    let rgb = match board.led {
        Led::Rgb { rgb } => match configure_rgb(&rgb) {
            Ok(()) => Some(rgb),
            Err(e) => {
                warn!("led: {e}");
                None
            }
        },
        Led::None => None,
    };
    let button = board.button;
    if rgb.is_none() && button.is_none() {
        return;
    }
    if let Some(b) = &button {
        configure_in(b);
    }
    let spawned = std::thread::Builder::new().name("led".into()).stack_size(3072).spawn(move || {
        let mut t = 0u32;
        let mut down_since: Option<Instant> = None;
        let mut flash_until = Instant::now();
        loop {
            if let Some(btn) = &button {
                match (pressed(btn), down_since) {
                    (true, None) => down_since = Some(Instant::now()),
                    (false, Some(since)) => {
                        down_since = None;
                        let held = since.elapsed();
                        if held >= Duration::from_millis(40) {
                            PRESSES.fetch_add(1, Ordering::Relaxed);
                            info!("button: pressed for {} ms", held.as_millis());
                            flash_until = Instant::now() + Duration::from_millis(250);
                        }
                    }
                    _ => {}
                }
            }
            // Button every 25 ms, LED every 125 ms.
            if rgb.is_some() && t.is_multiple_of(5) {
                show(colour(t / 5, Instant::now() < flash_until));
            }
            t = t.wrapping_add(1);
            std::thread::sleep(Duration::from_millis(25));
        }
    });
    if let Err(e) = spawned {
        warn!("led thread: {e}");
    }
}
