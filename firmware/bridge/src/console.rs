//! Control console on the second CDC-ACM port.
//!
//! Line-oriented text commands; nothing here ever touches the bridge data
//! stream on CDC 0. Setting this port to 1200 baud ("1200 bps touch") reboots
//! the S3 into its ROM download mode on USB-Serial-JTAG (hwlab recovery).

use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::{h2, recovery, stats, uart, usb};

const HELP: &str = "\
commands:\r
  info                 bridge state, UART rate/framing, counters\r
  h2 reset             pulse H2 EN (normal boot)\r
  h2 boot              reset the H2 into its ROM serial loader (BOOT low)\r
  h2 run               alias of `h2 reset`\r
  h2 hold | h2 release hold the H2 in reset / let it run\r
  baud <rate>          set the H2 UART rate (also follows CDC0 line coding)\r
  lines on|off         map CDC0 DTR/RTS to H2 EN/BOOT (esptool auto-reset)\r
  s3 download          reboot the S3 into ROM download mode on USB-Serial-JTAG\r
  s3 usj               hand the USB PHY back to USB-Serial-JTAG (app keeps running)\r
  s3 reset             reboot the S3 (bridge restarts)\r
";

pub fn run() -> ! {
    let mut line = Vec::<u8>::with_capacity(128);
    let mut buf = [0u8; 64];
    loop {
        if recovery::take_touch_request() {
            reply("1200 bps touch: rebooting the S3 into ROM download mode\r\n");
            std::thread::sleep(Duration::from_millis(50));
            recovery::reboot_to_rom_download();
        }
        let n = usb::read_wait(usb::CONSOLE, &mut buf, 100);
        for &c in &buf[..n] {
            match c {
                b'\r' | b'\n' => {
                    if !line.is_empty() {
                        reply("\r\n");
                        let cmd = String::from_utf8_lossy(&line).trim().to_string();
                        line.clear();
                        execute(&cmd);
                    }
                    reply("> ");
                }
                0x08 | 0x7f => {
                    if line.pop().is_some() {
                        reply("\x08 \x08");
                    }
                }
                c if c.is_ascii_graphic() || c == b' ' => {
                    if line.len() < 120 {
                        line.push(c);
                        usb::write(usb::CONSOLE, &[c], 20);
                    }
                }
                _ => {}
            }
        }
    }
}

fn reply(s: &str) {
    usb::write(usb::CONSOLE, s.as_bytes(), 50);
}

fn execute(cmd: &str) {
    let words: Vec<&str> = cmd.split_whitespace().collect();
    match words.as_slice() {
        ["help"] | ["?"] => reply(HELP),
        ["info"] => reply(&info()),
        ["h2", "reset"] | ["h2", "run"] => {
            h2::reset();
            reply("ok: H2 reset\r\n");
        }
        ["h2", "boot"] => {
            uart::flush_input();
            h2::enter_download();
            reply("ok: H2 in ROM download mode (115200 8N1 until changed)\r\n");
        }
        ["h2", "hold"] => {
            h2::hold();
            reply("ok: H2 held in reset\r\n");
        }
        ["h2", "release"] => {
            h2::release();
            reply("ok: H2 released\r\n");
        }
        ["baud", rate] => match rate.parse::<u32>() {
            Ok(r) if (300..=5_000_000).contains(&r) => {
                uart::set_baud(r);
                reply(&format!("ok: H2 UART {} {}\r\n", r, uart::framing()));
            }
            _ => reply("error: baud 300..5000000\r\n"),
        },
        ["lines", v @ ("on" | "off")] => {
            h2::AUTO_LINES.store(*v == "on", Ordering::Relaxed);
            reply(&format!("ok: DTR/RTS auto-reset {}\r\n", v));
        }
        ["s3", "download"] => {
            reply("rebooting the S3 into ROM download mode (USB-Serial-JTAG)\r\n");
            std::thread::sleep(Duration::from_millis(50));
            recovery::reboot_to_rom_download();
        }
        ["s3", "usj"] => {
            reply("handing the USB PHY to USB-Serial-JTAG; this port disappears\r\n");
            std::thread::sleep(Duration::from_millis(50));
            recovery::switch_to_usj("console request");
        }
        ["s3", "reset"] => {
            reply("rebooting the S3\r\n");
            std::thread::sleep(Duration::from_millis(50));
            unsafe { esp_idf_svc::sys::esp_restart() };
        }
        _ => reply("unknown command (help)\r\n"),
    }
}

fn info() -> String {
    let (en, boot) = h2::levels();
    let s = stats::snapshot();
    format!(
        "tbr-bridge {} (zigbee-rs-hub phase 0)\r\n\
         uart: {} {} (S3 TX GPIO18 -> H2 GPIO23, RX GPIO17 <- H2 GPIO24)\r\n\
         h2: EN={} BOOT={} auto-reset={}\r\n\
         uart->usb: {} B, dropped {} B; usb->uart: {} B\r\n\
         line-state events: {}, line-coding events: {}\r\n",
        env!("CARGO_PKG_VERSION"),
        uart::BAUD.load(Ordering::Relaxed),
        uart::framing(),
        if en { "high" } else { "LOW" },
        if boot { "high" } else { "LOW" },
        if h2::AUTO_LINES.load(Ordering::Relaxed) { "on" } else { "off" },
        s.uart_to_usb,
        s.uart_to_usb_dropped,
        s.usb_to_uart,
        s.line_state_events,
        s.line_coding_events,
    )
}
