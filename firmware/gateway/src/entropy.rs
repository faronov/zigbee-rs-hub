//! Random bytes with an entropy source, for the API token.
//!
//! The classic ESP32's RNG (`esp_fill_random`) gives true random numbers
//! only while the radio (Wi-Fi, Bluetooth) runs or the SAR-ADC entropy
//! source is on (`bootloader_random_enable`); the bootloader turns that
//! source off before the application starts, and the classic ESP32 has no
//! other one (ESP-IDF Programming Guide, "Random Number Generation"). A
//! dongle on Ethernet never starts its radio. So [`fill`] reads the RNG as
//! it is while the Wi-Fi station runs, and otherwise with the SAR-ADC
//! source on around the read. That source must not run together with the
//! radio, the ADC or I2S: the Wi-Fi start holds the same lock
//! ([`radio_start`]); the firmware uses neither the ADC nor I2S.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use esp_idf_svc::sys;

static LOCK: Mutex<()> = Mutex::new(());
/// The Wi-Fi driver runs (the RNG is a true RNG from then on).
static RADIO: AtomicBool = AtomicBool::new(false);

/// Fills `buf` with random bytes from an entropy source.
pub fn fill(buf: &mut [u8]) {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    if RADIO.load(Ordering::Acquire) {
        unsafe { sys::esp_fill_random(buf.as_mut_ptr().cast(), buf.len()) };
        return;
    }
    unsafe {
        sys::bootloader_random_enable();
        sys::esp_fill_random(buf.as_mut_ptr().cast(), buf.len());
        sys::bootloader_random_disable();
    }
}

/// A new API token: 12 random bytes, lower-case hex.
pub fn token() -> String {
    let mut b = [0u8; 12];
    fill(&mut b);
    zrh_espapp::hex(&b)
}

/// Runs the Wi-Fi start `f` (driver init and start) with [`fill`]'s
/// SAR-ADC source off; once it succeeded the RNG reads the radio.
pub fn radio_start<T, E>(f: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let r = f();
    if r.is_ok() {
        RADIO.store(true, Ordering::Release);
    }
    r
}
