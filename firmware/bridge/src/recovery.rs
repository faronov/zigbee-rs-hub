//! Keeping the S3 reachable by hwlab (USB-Serial-JTAG).
//!
//! The S3 has one internal USB PHY. While the bridge runs it belongs to the
//! USB OTG controller (TinyUSB), so the USB-Serial-JTAG unit that hwlab uses
//! for JTAG and flash access is gone. The routing bits live in RTC_CNTL and
//! survive software and panic resets, so every path below hands the PHY back
//! to USB-Serial-JTAG explicitly:
//!
//! - console `s3 download` or a 1200 bps touch on the console port: PHY → USJ,
//!   then reboot into ROM download mode (hwlab's validated S3 mode);
//! - console `s3 usj`: PHY → USJ, the app keeps running (bridge off);
//! - USB not enumerated within [`MOUNT_TIMEOUT`] after boot: PHY → USJ;
//! - boot after a panic or watchdog reset: TinyUSB is not started at all.
//!
//! A power cycle also resets the routing to USJ.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use esp_idf_svc::sys;
use log::warn;

pub const MOUNT_TIMEOUT: Duration = Duration::from_secs(20);

const RTC_CNTL_USB_CONF_REG: u32 = 0x6000_8120;
const RTC_CNTL_SW_HW_USB_PHY_SEL: u32 = 1 << 20;
const RTC_CNTL_SW_USB_PHY_SEL: u32 = 1 << 19;
const RTC_CNTL_OPTION1_REG: u32 = 0x6000_812C;
const RTC_CNTL_FORCE_DOWNLOAD_BOOT: u32 = 1 << 0;
const USB_SERIAL_JTAG_CONF0_REG: u32 = 0x6003_8018;
const USJ_PHY_SEL: u32 = 1 << 0;
const USJ_PAD_PULL_OVERRIDE: u32 = 1 << 8;
const USJ_DP_PULLUP: u32 = 1 << 9;
const USJ_USB_PAD_ENABLE: u32 = 1 << 14;
const USB_WRAP_OTG_CONF_REG: u32 = 0x6003_9000;
const USB_WRAP_PAD_PULL_OVERRIDE: u32 = 1 << 12;
const USB_WRAP_DP_PULLUP: u32 = 1 << 13;
const USB_WRAP_USB_PAD_ENABLE: u32 = 1 << 18;

static TOUCH: AtomicBool = AtomicBool::new(false);
static ON_USJ: AtomicBool = AtomicBool::new(false);
static TINYUSB_UP: AtomicBool = AtomicBool::new(false);

pub fn set_tinyusb_up() {
    TINYUSB_UP.store(true, Ordering::SeqCst);
}

unsafe fn rmw(reg: u32, clear: u32, set: u32) {
    let p = reg as *mut u32;
    unsafe { p.write_volatile((p.read_volatile() & !clear) | set) };
}

/// Detach the OTG device, route the internal PHY to USB-Serial-JTAG and let
/// it attach.
///
/// The host must see a real disconnect (D+ pull-up off): if the pull-up
/// stays on while the PHY changes owner, the host keeps talking to the old
/// OTG address, the USJ never gets a bus reset and stays invisible (seen on
/// hardware with bridge 0.1.0).
pub fn switch_to_usj(why: &str) {
    if ON_USJ.swap(true, Ordering::SeqCst) {
        return;
    }
    warn!("USB PHY -> USB-Serial-JTAG ({why})");
    unsafe {
        if TINYUSB_UP.load(Ordering::SeqCst) {
            sys::tud_disconnect();
        }
        rmw(USB_WRAP_OTG_CONF_REG, USB_WRAP_DP_PULLUP, USB_WRAP_PAD_PULL_OVERRIDE);
        rmw(USB_SERIAL_JTAG_CONF0_REG, USJ_DP_PULLUP, USJ_PAD_PULL_OVERRIDE);
        // Detached long enough for any host to notice.
        sys::vTaskDelay(500);
        rmw(USB_WRAP_OTG_CONF_REG, USB_WRAP_USB_PAD_ENABLE | USB_WRAP_PAD_PULL_OVERRIDE, 0);
        rmw(USB_SERIAL_JTAG_CONF0_REG, USJ_PHY_SEL, USJ_USB_PAD_ENABLE);
        rmw(RTC_CNTL_USB_CONF_REG, RTC_CNTL_SW_USB_PHY_SEL, RTC_CNTL_SW_HW_USB_PHY_SEL);
        sys::vTaskDelay(20);
        // Back to the USJ reset state (DP_PULLUP = 1, no override): the USJ
        // pulls D+ up again and the host enumerates it. With DP_PULLUP left at
        // 0 the USJ stays invisible (seen on hardware with bridge 0.1.1).
        rmw(USB_SERIAL_JTAG_CONF0_REG, USJ_PAD_PULL_OVERRIDE, USJ_DP_PULLUP);
        sys::vTaskDelay(200);
    }
}

/// Reboot into the S3 ROM serial loader, enumerated as USB-Serial-JTAG.
pub fn reboot_to_rom_download() -> ! {
    switch_to_usj("ROM download reboot");
    unsafe {
        rmw(RTC_CNTL_OPTION1_REG, 0, RTC_CNTL_FORCE_DOWNLOAD_BOOT);
        sys::esp_restart()
    }
}

pub fn on_usj() -> bool {
    ON_USJ.load(Ordering::SeqCst)
}

/// Called from the TinyUSB task when the console port is set to 1200 baud.
pub fn request_touch() {
    TOUCH.store(true, Ordering::SeqCst);
}

pub fn take_touch_request() -> bool {
    TOUCH.swap(false, Ordering::SeqCst)
}

/// A reset that suggests the bridge itself is broken: stay on USJ.
pub fn crashed_last_boot() -> bool {
    let r = unsafe { sys::esp_reset_reason() };
    matches!(
        r,
        sys::esp_reset_reason_t_ESP_RST_PANIC
            | sys::esp_reset_reason_t_ESP_RST_INT_WDT
            | sys::esp_reset_reason_t_ESP_RST_TASK_WDT
            | sys::esp_reset_reason_t_ESP_RST_WDT
    )
}
