//! USB OTG CDC-ACM ×2 via esp_tinyusb.
//!
//! The S3's USB-Serial-JTAG unit cannot report DTR/RTS or line coding to
//! firmware (no such registers on the S3, unlike the C6/H2), so the bridge
//! uses the OTG controller: TinyUSB delivers SET_CONTROL_LINE_STATE and
//! SET_LINE_CODING as callbacks and has no 64-byte FIFO limit.

use std::ffi::{CString, c_char, c_int};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use esp_idf_svc::sys::{self, EspError, esp};
use hub_core::lines::esptool_lines;
use log::info;

use crate::diag::{self, Site};
use crate::{h2, recovery, stats, uart};

/// Host DTR on the bridge port: a terminal has the port open.
static BRIDGE_DTR: AtomicBool = AtomicBool::new(false);

/// CDC 0 controls the H2 UART and EN/BOOT (bridge mode). The hub firmware
/// clears it in hub mode, where the hub owns UART1.
pub static BRIDGE_ENABLED: AtomicBool = AtomicBool::new(true);

/// Bumped on every DTR edge of the bridge port.
static BRIDGE_SESSION: AtomicU32 = AtomicU32::new(0);

pub fn bridge_open() -> bool {
    BRIDGE_DTR.load(Ordering::Relaxed)
}

/// Changes whenever the host opens or closes the bridge port.
pub fn bridge_session() -> u32 {
    BRIDGE_SESSION.load(Ordering::Relaxed)
}

/// Drop everything queued for the host on `itf`.
pub fn clear_tx(itf: sys::tinyusb_cdcacm_itf_t) {
    if !recovery::on_usj() {
        unsafe { sys::tud_cdc_n_write_clear(itf as u8) };
    }
}

pub const BRIDGE: sys::tinyusb_cdcacm_itf_t = sys::tinyusb_cdcacm_itf_t_TINYUSB_CDC_ACM_0;
pub const CONSOLE: sys::tinyusb_cdcacm_itf_t = sys::tinyusb_cdcacm_itf_t_TINYUSB_CDC_ACM_1;

struct RxSignal {
    pending: Mutex<[bool; 2]>,
    cv: Condvar,
}

static RX: RxSignal = RxSignal { pending: Mutex::new([false; 2]), cv: Condvar::new() };

fn mac_serial() -> String {
    let mut mac = [0u8; 6];
    unsafe { sys::esp_read_mac(mac.as_mut_ptr(), sys::esp_mac_type_t_ESP_MAC_EFUSE_FACTORY) };
    mac.iter().map(|b| format!("{b:02X}")).collect()
}

pub fn init() -> Result<(), EspError> {
    // String descriptors: language, manufacturer, product, serial, CDC interface.
    let strings: Vec<*const c_char> = [
        CString::new([0x09u8, 0x04]).unwrap(),
        CString::new("zigbee-rs-hub").unwrap(),
        CString::new("Thread BR H2 bridge").unwrap(),
        CString::new(mac_serial()).unwrap(),
        CString::new("H2 bridge").unwrap(),
    ]
    .into_iter()
    .map(|s| s.into_raw() as *const c_char)
    .collect();
    let strings = strings.leak();

    unsafe {
        let mut cfg: sys::tinyusb_config_t = core::mem::zeroed();
        cfg.string_descriptor = strings.as_mut_ptr();
        cfg.string_descriptor_count = strings.len() as c_int;
        cfg.external_phy = false;
        esp!(sys::tinyusb_driver_install(&cfg))?;

        for itf in [BRIDGE, CONSOLE] {
            let mut acm: sys::tinyusb_config_cdcacm_t = core::mem::zeroed();
            acm.usb_dev = sys::tinyusb_usbdev_t_TINYUSB_USBDEV_0;
            acm.cdc_port = itf;
            acm.callback_rx = Some(on_rx);
            acm.callback_line_state_changed = Some(on_line_state);
            acm.callback_line_coding_changed = Some(on_line_coding);
            esp!(sys::tusb_cdc_acm_init(&acm))?;
        }
    }
    recovery::set_tinyusb_up();
    info!("TinyUSB CDC-ACM x2 up (serial {})", mac_serial());
    Ok(())
}

pub fn mounted() -> bool {
    !recovery::on_usj() && unsafe { sys::tud_mounted() }
}

unsafe extern "C" fn on_rx(itf: c_int, _ev: *mut sys::cdcacm_event_t) {
    diag::mark(Site::CbRx, 1);
    if let Ok(mut p) = RX.pending.lock() {
        p[itf as usize & 1] = true;
        RX.cv.notify_all();
    }
    diag::mark(Site::CbRx, 0);
}

unsafe extern "C" fn on_line_state(itf: c_int, ev: *mut sys::cdcacm_event_t) {
    if itf as sys::tinyusb_cdcacm_itf_t != BRIDGE {
        return;
    }
    let d = unsafe { (*ev).__bindgen_anon_1.line_state_changed_data };
    diag::mark(Site::CbLineState, 1);
    if BRIDGE_DTR.swap(d.dtr, Ordering::Relaxed) != d.dtr {
        // A new or closed session: whatever the old one left in the TX FIFO
        // is stale, and a full FIFO would hold the UART reader back.
        clear_tx(BRIDGE);
        BRIDGE_SESSION.fetch_add(1, Ordering::Relaxed);
        stats::add(&stats::DTR_FLUSHES, 1);
    }
    stats::add(&stats::LINE_STATE_EVENTS, 1);
    if BRIDGE_ENABLED.load(Ordering::Relaxed) {
        h2::apply(esptool_lines(d.dtr, d.rts));
    }
    diag::mark(Site::CbLineState, 0);
}

unsafe extern "C" fn on_line_coding(itf: c_int, ev: *mut sys::cdcacm_event_t) {
    let lc = unsafe { &*(*ev).__bindgen_anon_1.line_coding_changed_data.p_line_coding };
    let (rate, stop, parity, bits) = (lc.bit_rate, lc.stop_bits, lc.parity, lc.data_bits);
    if itf as sys::tinyusb_cdcacm_itf_t == CONSOLE {
        if rate == 1200 {
            recovery::request_touch();
        }
        return;
    }
    diag::mark(Site::CbLineCoding, 1);
    stats::add(&stats::LINE_CODING_EVENTS, 1);
    if BRIDGE_ENABLED.load(Ordering::Relaxed) {
        uart::set_line_coding(rate, stop, parity, bits);
    }
    diag::mark(Site::CbLineCoding, 0);
}

/// Wait up to `timeout_ms` for data on `itf`, then read what is available.
pub fn read_wait(itf: sys::tinyusb_cdcacm_itf_t, buf: &mut [u8], timeout_ms: u64) -> usize {
    let n = read(itf, buf);
    if n > 0 {
        return n;
    }
    let idx = itf as usize & 1;
    let mut p = RX.pending.lock().unwrap();
    if !p[idx] {
        p = RX.cv.wait_timeout(p, Duration::from_millis(timeout_ms)).unwrap().0;
    }
    p[idx] = false;
    drop(p);
    read(itf, buf)
}

fn read(itf: sys::tinyusb_cdcacm_itf_t, buf: &mut [u8]) -> usize {
    if recovery::on_usj() {
        return 0;
    }
    let mut got: usize = 0;
    let r = unsafe { sys::tinyusb_cdcacm_read(itf, buf.as_mut_ptr(), buf.len(), &mut got) };
    if r == sys::ESP_OK { got } else { 0 }
}

/// Queue `data` and flush; gives up after `timeout_ms`. Returns bytes queued.
pub fn write(itf: sys::tinyusb_cdcacm_itf_t, data: &[u8], timeout_ms: u32) -> usize {
    if recovery::on_usj() {
        return 0;
    }
    let mut done = 0;
    let mut waited = 0;
    while done < data.len() {
        let n = unsafe { sys::tinyusb_cdcacm_write_queue(itf, data[done..].as_ptr(), data.len() - done) };
        done += n;
        unsafe { sys::tinyusb_cdcacm_write_flush(itf, 0) };
        if done < data.len() {
            if waited >= timeout_ms {
                break;
            }
            unsafe { sys::vTaskDelay(1) };
            waited += 1;
        }
    }
    done
}
