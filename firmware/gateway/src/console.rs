//! Line console on the board's console port (UART0 behind the CP2102N on
//! the Dongle-M): input read byte by byte from the ESP-IDF driver, local
//! echo, output and logs through stdout on the same driver, one command per
//! line (`help`).

use std::io::Write;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::Duration;

use esp_idf_svc::sys;
use hub_core::profile::{BoardProfile, Console};

use crate::{board, cpanic, cpu, esp, ezsp, gecko, led, lines, net, settings, slots, uart};

const HELP: &str = "\
zigbee-rs-hub gateway console (board bring-up: no coordinator backend yet):\r
  info                 firmware, chip, memory, partitions, ota_0, files, straps,\r
                       last panic\r
  board                pins of the board profile and stored overrides\r
  board set <k=v ...>  store NCP wiring overrides (tx rx rts cts reset reset_od\r
                       baud flow fallback), applied at the next boot; the\r
                       button held at boot ignores them for that boot\r
  board clear          remove the overrides\r
  net                  Ethernet / Wi-Fi state\r
  wifi set <ssid> [pw] store Wi-Fi credentials (station starts when Ethernet\r
                       has no address; used from the next boot)\r
  wifi clear           forget them\r
  led <0-100>          LED brightness in % (stored)\r
  mg24                 MG24 lines and UART\r
  mg24 reset           reset the MG24 into its application\r
  mg24 bootloader      enter the Gecko bootloader, print its banner and menu,\r
                       reset back into the application (nothing is uploaded)\r
  mg24 ezsp info       ASH reset + EZSP version, stack, EUI-64, mfg tokens,\r
                       network state (read-only)\r
  mg24 ezsp info init  also networkInit (resumes a stored network), then\r
                       reset the MG24\r
  mg24 baud <rate>     MG24 UART rate until the next boot\r
  stock                SONOFF's application in ota_0, and how to return to it\r
  stock confirm        select ota_0 (SONOFF's application) and restart into it\r
  cpu [ms]             CPU use per task\r
  atomics              lost updates of atomic adds from both cores (PSRAM and\r
                       internal RAM; Rust atomics must lose none)\r
  restart              restart the ESP32\r
";

static PORT: OnceLock<Console> = OnceLock::new();

/// Install the console driver and route stdout through it.
pub fn init(console: Console) {
    let _ = PORT.set(console);
    if console == Console::Uart0 {
        unsafe {
            sys::uart_driver_install(0, 512, 0, 0, core::ptr::null_mut(), 0);
            sys::uart_vfs_dev_use_driver(0);
        }
    }
}

fn pin(p: Option<u8>) -> String {
    p.map_or("-".into(), |p| p.to_string())
}

fn board_text(b: &BoardProfile) -> String {
    let u = &b.uart;
    let mut s = format!(
        "board {} ({}), host {:?}, NCP {:?}\r\nNCP UART{}: tx {} rx {} rts {} cts {}, {} Bd{}, fallback {}\r\n",
        b.id,
        b.name,
        b.host,
        b.ncp,
        u.port,
        u.tx,
        u.rx,
        pin(u.rts),
        pin(u.cts),
        u.line.baud,
        if u.line.flow { " rts/cts" } else { "" },
        u.fallback.map_or("-".into(), |l| l.baud.to_string()),
    );
    s += &format!("NCP control: reset {:?} {} ms, loader {:?}\r\n", b.control.reset, b.control.reset_ms, b.control.loader);
    s += &format!("ethernet {:?}\r\nled {:?}\r\nbutton {:?}, console {:?}\r\n", b.ethernet, b.led, b.button, b.console);
    s += &format!("flash {} MiB, PSRAM {} MiB, layout {:?}\r\n", b.memory.flash >> 20, b.memory.psram >> 20, b.memory.layout);
    s += &format!("overrides: {}\r\n", settings::get("board").unwrap_or_else(|| "-".into()));
    s
}

fn info_text() -> String {
    let mut s = format!("zrh-gateway {} (board bring-up)\r\n", env!("CARGO_PKG_VERSION"));
    for line in esp::boot_report(board()) {
        s += &line;
        s += "\r\n";
    }
    s += &format!("{}\r\n", esp::memory_report());
    s += &format!("last panic: {}\r\n", cpanic::last().unwrap_or_else(|| "none recorded".into()));
    s += &format!("button presses: {}\r\n", led::PRESSES.load(Ordering::Relaxed));
    s
}

fn net_text() -> String {
    let n = net::STATE.lock().unwrap().clone();
    format!("ethernet: {}\r\nwifi: {}\r\n", n.eth, n.wifi)
}

fn mg24_text() -> String {
    let (reset, boot) = lines::levels().unwrap_or((None, false));
    let level = |l: bool| if l { "high" } else { "low" };
    let b = board();
    format!(
        "MG24 nRESET GPIO{} {}, bootloader GPIO{} {}\r\nUART {} Bd, user {:?}, overruns {}, line errors {}\r\n",
        pin(b.control.reset.map(|r| r.gpio)),
        reset.map_or("-", level),
        b.control.boot().gpio,
        level(boot),
        uart::baud(),
        uart::owner(),
        uart::OVERRUNS.load(Ordering::Relaxed),
        uart::LINE_ERRORS.load(Ordering::Relaxed),
    )
}

fn stored(r: Result<(), String>, ok: &str) -> String {
    match r {
        Ok(()) => format!("{ok}\r\n"),
        Err(e) => format!("error: {e}\r\n"),
    }
}

fn command(words: &[&str]) -> String {
    match words {
        [] => String::new(),
        ["help"] => HELP.into(),
        ["info"] => info_text(),
        ["board"] => board_text(board()),
        ["board", "set", kv @ ..] => {
            let text = kv.join(" ");
            // Checked against the shipped profile, as the next boot applies it.
            let mut b = hub_core::profile::by_id(board().id).copied().unwrap_or(*board());
            match b.apply(&text) {
                Ok(()) => stored(settings::set("board", Some(&text)), "stored; applied at the next boot (`restart`)"),
                Err(e) => format!("error: {e}\r\n"),
            }
        }
        ["board", "clear"] => stored(settings::set("board", None), "overrides removed; the next boot uses the profile"),
        ["net"] | ["wifi"] | ["eth"] => net_text(),
        ["wifi", "set", ssid, rest @ ..] => {
            let pass = rest.join(" ");
            stored(
                settings::set("wifi_ssid", Some(ssid)).and_then(|_| settings::set("wifi_pass", Some(&pass))),
                "stored; used from the next boot",
            )
        }
        ["wifi", "clear"] => stored(settings::set("wifi_ssid", None).and_then(|_| settings::set("wifi_pass", None)), "Wi-Fi credentials removed"),
        ["led", pct] => match pct.parse::<u8>() {
            Ok(p) if p <= 100 => {
                led::set_brightness(p);
                stored(settings::set("led", Some(pct)), &format!("LED at {p} %"))
            }
            _ => "usage: led <0-100>\r\n".into(),
        },
        ["mg24"] => mg24_text(),
        ["mg24", "reset"] => match uart::owner() {
            Some(u) => format!("MG24 UART in use ({u:?})\r\n"),
            None => {
                lines::reset_to_app();
                "MG24 reset into its application\r\n".into()
            }
        },
        ["mg24", "bootloader"] => gecko::banner(board()),
        ["mg24", "ezsp", "info"] => ezsp::info(false),
        ["mg24", "ezsp", "info", "init"] => ezsp::info(true),
        ["mg24", "baud", rate] => match rate.parse::<u32>() {
            Ok(b) if (9_600..=3_000_000).contains(&b) => match uart::claim(uart::User::Console) {
                Ok(_c) => {
                    uart::set_baud(b);
                    format!("MG24 UART at {b} Bd until the next boot (`board set baud={b}` stores it)\r\n")
                }
                Err(u) => format!("MG24 UART in use ({u:?})\r\n"),
            },
            _ => "usage: mg24 baud <9600..3000000>\r\n".into(),
        },
        ["stock"] => slots::stock(false),
        ["stock", "confirm"] => slots::stock(true),
        ["cpu", rest @ ..] => {
            let ms = rest.first().and_then(|s| s.parse().ok()).unwrap_or(2000u64).clamp(100, 10_000);
            cpu::report(Duration::from_millis(ms))
        }
        ["atomics"] => cpu::atomics(),
        ["restart"] => {
            reply("restarting\r\n");
            crate::restart()
        }
        _ => format!("unknown command {:?} (`help`)\r\n", words.join(" ")),
    }
}

pub(crate) fn reply(s: &str) {
    let mut out = std::io::stdout();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

/// One input byte (waits up to 1 s).
fn read_byte(b: &mut [u8; 1]) -> bool {
    match PORT.get() {
        Some(Console::Uart0) => unsafe { sys::uart_read_bytes(0, b.as_mut_ptr().cast(), 1, 1000) == 1 },
        _ => {
            std::thread::sleep(Duration::from_secs(1));
            false
        }
    }
}

/// Run the console on the calling thread.
pub fn run() -> ! {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    reply("\r\nzigbee-rs-hub gateway console (`help`)\r\n> ");
    loop {
        if !read_byte(&mut b) {
            continue;
        }
        match b[0] {
            b'\r' | b'\n' => {
                reply("\r\n");
                let text = String::from_utf8_lossy(&line).into_owned();
                line.clear();
                let words: Vec<&str> = text.split_whitespace().collect();
                reply(&command(&words));
                reply("> ");
            }
            0x08 | 0x7F => {
                if line.pop().is_some() {
                    reply("\x08 \x08");
                }
            }
            c if (0x20..0x7F).contains(&c) && line.len() < 256 => {
                line.push(c);
                reply(core::str::from_utf8(&[c]).unwrap_or(""));
            }
            _ => {}
        }
    }
}
