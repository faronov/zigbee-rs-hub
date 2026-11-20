//! zigbee-rs-hub phase 0: ESP32-S3 USB ↔ ESP32-H2 UART bridge for the
//! Espressif Thread Border Router / Zigbee Gateway board.
//!
//! - USB CDC-ACM 0: transparent byte bridge to the H2 UART; host line coding
//!   sets the UART rate/framing, host DTR/RTS drive H2 EN/BOOT like esptool's
//!   auto-reset circuit.
//! - USB CDC-ACM 1: control console (`help`).
//!
//! The bridge is a library so that the phase-1 hub firmware (`firmware/hub`)
//! can run it as its default mode; the `tbr-bridge` binary only calls
//! [`run`].

pub mod console;
pub mod diag;
pub mod h2;
pub mod recovery;
pub mod reserved;
pub mod stats;
pub mod uart;
pub mod usb;

use std::thread;
use std::time::{Duration, Instant};

use esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration;
use log::{info, warn};

use crate::diag::Site;

pub fn spawn(name: &'static core::ffi::CStr, prio: u8, stack: usize, f: impl FnOnce() + Send + 'static) {
    ThreadSpawnConfiguration { name: Some(name), priority: prio, stack_size: stack, ..Default::default() }
        .set()
        .unwrap();
    thread::Builder::new().stack_size(stack).spawn(f).unwrap();
    ThreadSpawnConfiguration::default().set().unwrap();
}

fn uart_to_usb() {
    let mut buf = [0u8; 1024];
    loop {
        diag::mark(Site::Uart2Usb, 1);
        let n = uart::read(&mut buf, 100);
        diag::mark(Site::Uart2Usb, 2);
        if n == 0 {
            continue;
        }
        // With flow control and the host port open, wait for the host instead
        // of dropping: the UART ring then fills, RTS deasserts and the H2
        // pauses. With the port closed the data is dropped as before.
        let mut sent = 0;
        while sent < n && usb::mounted() {
            sent += usb::write(usb::BRIDGE, &buf[sent..n], 50);
            if !(uart::flow() && usb::bridge_open()) {
                break;
            }
        }
        stats::add(&stats::UART_TO_USB, sent);
        stats::add(&stats::UART_TO_USB_DROPPED, n - sent);
    }
}

fn usb_to_uart() {
    let mut buf = [0u8; 512];
    loop {
        diag::mark(Site::Usb2Uart, 1);
        let n = usb::read_wait(usb::BRIDGE, &mut buf, 100);
        if n > 0 {
            diag::mark(Site::Usb2Uart, 2);
            stats::add(&stats::USB_TO_UART, uart::write(&buf[..n]));
        }
    }
}

/// Boot after a panic or watchdog reset: hand the USB PHY to
/// USB-Serial-JTAG and stay idle (see [`recovery`]).
pub fn safe_mode_if_crashed() {
    if recovery::crashed_last_boot() {
        warn!("previous boot crashed: safe mode, USB stays on USB-Serial-JTAG, bridge off");
        recovery::switch_to_usj("safe mode");
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }
}

/// The two bridge data threads (CDC 0 ↔ H2 UART).
pub fn start_bridge_threads() {
    spawn(c"uart2usb", 20, 6144, uart_to_usb);
    spawn(c"usb2uart", 19, 6144, usb_to_uart);
}

/// Console and hang supervisor.
pub fn start_console() {
    diag::mark(Site::Console, 0);
    spawn(c"console", 5, 8192, || console::run());
    spawn(c"supervise", 22, 6144, || diag::supervise());
}

/// Wait for USB enumeration (hand the PHY back to USB-Serial-JTAG after
/// [`recovery::MOUNT_TIMEOUT`]), then idle forever.
pub fn park() -> ! {
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

/// The phase-0 bridge.
pub fn run() -> ! {
    info!("tbr-bridge {} starting", env!("CARGO_PKG_VERSION"));
    safe_mode_if_crashed();

    h2::init().expect("H2 EN/BOOT GPIO");
    uart::init(hub_core::board::ROM_BAUD).expect("H2 UART");

    usb::init().expect("TinyUSB");

    start_bridge_threads();
    start_console();
    park()
}
