//! zigbee-rs-hub phase 0: ESP32-S3 USB ↔ ESP32-H2 UART bridge for the
//! Espressif Thread Border Router / Zigbee Gateway board.
//!
//! - USB CDC-ACM 0: transparent byte bridge to the H2 UART; host line coding
//!   sets the UART rate/framing, host DTR/RTS drive H2 EN/BOOT like esptool's
//!   auto-reset circuit.
//! - USB CDC-ACM 1: control console (`help`).

mod console;
mod h2;
mod recovery;
mod stats;
mod uart;
mod usb;

use std::thread;
use std::time::{Duration, Instant};

use esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration;
use log::{info, warn};

fn spawn(name: &'static core::ffi::CStr, prio: u8, stack: usize, f: impl FnOnce() + Send + 'static) {
    ThreadSpawnConfiguration { name: Some(name), priority: prio, stack_size: stack, ..Default::default() }
        .set()
        .unwrap();
    thread::Builder::new().stack_size(stack).spawn(f).unwrap();
    ThreadSpawnConfiguration::default().set().unwrap();
}

fn uart_to_usb() {
    let mut buf = [0u8; 1024];
    loop {
        let n = uart::read(&mut buf, 100);
        if n == 0 {
            continue;
        }
        let sent = if usb::mounted() { usb::write(usb::BRIDGE, &buf[..n], 50) } else { 0 };
        stats::add(&stats::UART_TO_USB, sent);
        stats::add(&stats::UART_TO_USB_DROPPED, n - sent);
    }
}

fn usb_to_uart() {
    let mut buf = [0u8; 512];
    loop {
        let n = usb::read_wait(usb::BRIDGE, &mut buf, 100);
        if n > 0 {
            stats::add(&stats::USB_TO_UART, uart::write(&buf[..n]));
        }
    }
}

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    info!("tbr-bridge {} starting", env!("CARGO_PKG_VERSION"));

    if recovery::crashed_last_boot() {
        warn!("previous boot crashed: safe mode, USB stays on USB-Serial-JTAG, bridge off");
        recovery::switch_to_usj("safe mode");
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }

    h2::init().expect("H2 EN/BOOT GPIO");
    uart::init(hub_core::board::ROM_BAUD).expect("H2 UART");

    usb::init().expect("TinyUSB");

    spawn(c"uart2usb", 20, 6144, uart_to_usb);
    spawn(c"usb2uart", 19, 6144, usb_to_uart);
    spawn(c"console", 5, 8192, || console::run());

    let boot = Instant::now();
    loop {
        thread::sleep(Duration::from_millis(500));
        if usb::mounted() {
            break;
        }
        if boot.elapsed() > recovery::MOUNT_TIMEOUT {
            recovery::switch_to_usj("USB not enumerated");
            break;
        }
    }
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}
