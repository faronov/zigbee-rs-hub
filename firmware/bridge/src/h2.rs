//! H2 EN / BOOT control (S3 GPIO7 → H2 EN, S3 GPIO8 → H2 GPIO9).
//!
//! Both pins are open-drain with pull-ups: "high" releases the line to the
//! board's pull-up (EN: 10 kΩ + 1 µF; BOOT: H2 internal pull-up), so the H2
//! buttons can never fight a push-pull driver.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::Duration;

use esp_idf_svc::sys::{self, EspError, esp};
use hub_core::board;
use hub_core::lines::TargetLines;

/// Map the bridge port's DTR/RTS to EN/BOOT (esptool auto-reset).
pub static AUTO_LINES: AtomicBool = AtomicBool::new(true);

static LOCK: Mutex<()> = Mutex::new(());

pub fn init() -> Result<(), EspError> {
    unsafe {
        esp!(sys::gpio_set_level(board::H2_EN, 1))?;
        esp!(sys::gpio_set_level(board::H2_BOOT, 1))?;
        let mut cfg: sys::gpio_config_t = core::mem::zeroed();
        cfg.pin_bit_mask = (1u64 << board::H2_EN) | (1u64 << board::H2_BOOT);
        cfg.mode = sys::gpio_mode_t_GPIO_MODE_INPUT_OUTPUT_OD;
        cfg.pull_up_en = sys::gpio_pullup_t_GPIO_PULLUP_ENABLE;
        cfg.pull_down_en = sys::gpio_pulldown_t_GPIO_PULLDOWN_DISABLE;
        cfg.intr_type = sys::gpio_int_type_t_GPIO_INTR_DISABLE;
        esp!(sys::gpio_config(&cfg))
    }
}

fn set(en_high: bool, boot_high: bool) {
    unsafe {
        sys::gpio_set_level(board::H2_BOOT, boot_high as u32);
        sys::gpio_set_level(board::H2_EN, en_high as u32);
    }
}

/// Apply host modem lines (called from the TinyUSB task).
pub fn apply(t: TargetLines) {
    if !AUTO_LINES.load(Ordering::Relaxed) {
        return;
    }
    let _g = LOCK.lock().unwrap();
    set(!t.en_low, !t.boot_low);
}

/// Normal reset: BOOT released, EN pulsed low.
pub fn reset() {
    let _g = LOCK.lock().unwrap();
    set(false, true);
    sleep(Duration::from_millis(board::RESET_PULSE_MS.into()));
    set(true, true);
}

/// Reset into the H2 ROM serial loader (spec 11.5, ROM path steps 1–3).
pub fn enter_download() {
    let _g = LOCK.lock().unwrap();
    set(true, false);
    sleep(Duration::from_millis(5));
    set(false, false);
    sleep(Duration::from_millis(board::RESET_PULSE_MS.into()));
    set(true, false);
    sleep(Duration::from_millis(board::BOOT_HOLD_MS.into()));
    set(true, true);
}

/// Hold the H2 in reset (EN low) until `release()` or a reset command.
pub fn hold() {
    let _g = LOCK.lock().unwrap();
    set(false, true);
}

pub fn release() {
    let _g = LOCK.lock().unwrap();
    set(true, true);
}

/// Pin levels as seen on the pads: (EN, BOOT).
pub fn levels() -> (bool, bool) {
    unsafe { (sys::gpio_get_level(board::H2_EN) != 0, sys::gpio_get_level(board::H2_BOOT) != 0) }
}
