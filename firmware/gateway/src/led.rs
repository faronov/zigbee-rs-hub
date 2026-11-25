//! Status LED (LEDC PWM) and user button from the board profile.
//!
//! | Colour | Meaning |
//! |---|---|
//! | white flash | button pressed (white steady: held for 10 s or more) |
//! | magenta, fast blink | MG24 probe or bootloader banner running (bring-up boot) |
//! | yellow, blinking | joining open (permit join) |
//! | red, blinking | the MG24 does not answer (no EZSP session for [`ERROR_AFTER`]), or the hub did not start |
//! | blue, blinking | no network (Ethernet and Wi-Fi without an address) |
//! | cyan | network up, no Zigbee network (none formed yet) |
//! | green | network and Zigbee network up (bring-up boot: network up) |
//!
//! Brightness: setting `led` (0–100 %, default [`DEFAULT_PCT`]).
//!
//! The button: a **short press** (under 3 s) opens joining for
//! [`BUTTON_PERMIT_S`](crate::hub::BUTTON_PERMIT_S) s, as the web UI's
//! "Permit join". A press of 3 s or more does nothing; held for 10 s or
//! more the LED turns white and nothing happens either (no factory reset
//! from the button: nothing destructive without the console or the web
//! UI). Held while the dongle powers up or resets, it ignores the stored
//! board overrides for that boot.

use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use hub_app::AppStatus;

use esp_idf_svc::sys;
use hub_core::profile::{BoardProfile, Input, Led, Output};
use log::{info, warn};

use crate::{net, settings, uart};

/// Default brightness.
pub const DEFAULT_PCT: u8 = 30;
static PCT: AtomicU8 = AtomicU8::new(DEFAULT_PCT);
/// Short presses since boot.
pub static PRESSES: AtomicU32 = AtomicU32::new(0);
/// No EZSP session this long: the LED shows an error.
pub const ERROR_AFTER: Duration = Duration::from_secs(30);
/// Longest short press.
const SHORT_PRESS: Duration = Duration::from_secs(3);
/// A long press (no function).
const LONG_PRESS: Duration = Duration::from_secs(10);

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
    // gpio_config, not gpio_reset_pin: the latter always enables the pull-up,
    // which input-only pads (ESP32 GPIO34–39, the Dongle-M button) lack.
    let cfg = sys::gpio_config_t {
        pin_bit_mask: 1u64 << i.gpio,
        mode: sys::gpio_mode_t_GPIO_MODE_INPUT,
        pull_up_en: if i.pull_up { sys::gpio_pullup_t_GPIO_PULLUP_ENABLE } else { sys::gpio_pullup_t_GPIO_PULLUP_DISABLE },
        pull_down_en: sys::gpio_pulldown_t_GPIO_PULLDOWN_DISABLE,
        intr_type: sys::gpio_int_type_t_GPIO_INTR_DISABLE,
        ..Default::default()
    };
    unsafe {
        sys::gpio_config(&cfg);
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

/// What the LED shows besides the network.
#[derive(Clone, Copy)]
struct Hub {
    /// The hub's status, `None` in a bring-up boot.
    status: Option<(bool, bool, u32)>,
    /// The EZSP session has been down for [`ERROR_AFTER`].
    stuck: bool,
}

fn hub_now(session_since: &mut Instant) -> Hub {
    let status = crate::hub::APP.get().map(|a| {
        let AppStatus { session_up, network_up, permit_join_s, .. } = a.status();
        (session_up, network_up, permit_join_s)
    });
    if status.is_none_or(|(session, _, _)| session) {
        *session_since = Instant::now();
    }
    // The hub did not start (no PSRAM, the UART taken): an error too.
    let missing = status.is_none() && !crate::bringup();
    Hub { status, stuck: missing || session_since.elapsed() >= ERROR_AFTER }
}

/// (r, g, b) at LED tick `t` (125 ms).
fn colour(t: u32, flash: bool, hub: Hub) -> [bool; 3] {
    let blink = |period: u32| t % period < period / 2;
    if flash {
        return [true; 3];
    }
    if uart::owner() == Some(uart::User::Console) {
        let on = blink(2);
        return [on, false, on];
    }
    if hub.status.is_some_and(|(_, _, permit)| permit > 0) {
        let on = blink(8);
        return [on, on, false];
    }
    if hub.stuck {
        return [blink(8), false, false];
    }
    match (net::up(), hub.status) {
        (false, _) => [false, false, blink(8)],
        (true, Some((_, false, _))) => [false, true, true],
        (true, _) => [false, true, false],
    }
}

fn short_press() {
    PRESSES.fetch_add(1, Ordering::Relaxed);
    if crate::hub::APP.get().is_none() {
        return;
    }
    match crate::hub::permit_join(crate::hub::BUTTON_PERMIT_S) {
        Ok(()) => info!("button: permit join for {} s", crate::hub::BUTTON_PERMIT_S),
        Err(e) => warn!("button: permit join: {e}"),
    }
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
    let spawned = std::thread::Builder::new().name("led".into()).stack_size(4096).spawn(move || {
        let mut t = 0u32;
        let mut down_since: Option<Instant> = None;
        let mut flash_until = Instant::now();
        let mut session_since = Instant::now();
        loop {
            let mut held_long = false;
            if let Some(btn) = &button {
                match (pressed(btn), down_since) {
                    (true, None) => down_since = Some(Instant::now()),
                    (true, Some(since)) => held_long = since.elapsed() >= LONG_PRESS,
                    (false, Some(since)) => {
                        down_since = None;
                        let held = since.elapsed();
                        if held >= LONG_PRESS {
                            info!("button: held for {} s: no function", held.as_secs());
                        } else if held >= SHORT_PRESS {
                            info!("button: pressed for {} ms: no function (short press: permit join)", held.as_millis());
                        } else if held >= Duration::from_millis(40) {
                            info!("button: pressed for {} ms", held.as_millis());
                            flash_until = Instant::now() + Duration::from_millis(250);
                            short_press();
                        }
                    }
                    _ => {}
                }
            }
            // Button every 25 ms, LED every 125 ms.
            if rgb.is_some() && t.is_multiple_of(5) {
                let hub = hub_now(&mut session_since);
                show(colour(t / 5, held_long || Instant::now() < flash_until, hub));
            }
            t = t.wrapping_add(1);
            std::thread::sleep(Duration::from_millis(25));
        }
    });
    if let Err(e) = spawned {
        warn!("led thread: {e}");
    }
}
