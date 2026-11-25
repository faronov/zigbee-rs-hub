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

use zrh_espapp::{cpanic, cpu};

use crate::{board, esp, ezsp, files, gecko, hub, led, lines, net, settings, slots, uart};

const HELP: &str = "\
zigbee-rs-hub gateway console:\r
  info                 firmware, chip, memory, partitions, ota_0, files, straps,\r
                       last panics, hub status\r
  hub                  hub status: EZSP session, Zigbee network, devices, store\r
  permit [s]           open joining for s seconds (default 180, 0 closes)\r
  token                the API token (WebSocket API, web UI, Home Assistant)\r
  files                what SONOFF's files partition holds (claim, copy)\r
  files claim          what claiming it would copy, or why it is refused\r
  files claim confirm  claim it for the hub's data (SONOFF's files are saved\r
                       first), then restart; until then the hub's data are in\r
                       RAM only\r
  board                pins of the board profile and stored overrides\r
  board set <k=v ...>  store NCP wiring overrides (tx rx rts cts reset reset_od\r
                       baud flow fallback), applied at the next boot; the\r
                       button held at boot ignores them for that boot\r
  board clear          remove the overrides\r
  net                  Ethernet / Wi-Fi state\r
  wifi set <ssid> [pw] store Wi-Fi credentials and use them now (the station\r
                       runs when Ethernet has no address for 10 s)\r
  wifi clear           forget them (from the next boot)\r
  led <0-100>          LED brightness in % (stored)\r
  mg24                 MG24 lines and UART\r
  mg24 reset           reset the MG24 into its application (bring-up boot)\r
  mg24 bootloader      enter the Gecko bootloader, print its banner and menu,\r
                       reset back into the application (bring-up boot)\r
  mg24 ezsp info [init] ASH reset + EZSP version, stack, EUI-64, mfg tokens,\r
                       network state; `init` resumes a stored network (bring-up\r
                       boot)\r
  mg24 baud <rate>     MG24 UART rate until the next boot (bring-up boot)\r
  stock                SONOFF's application in ota_0, and how to return to it\r
  stock confirm        give SONOFF's files back (if claimed), select ota_0 and\r
                       restart into SONOFF's firmware\r
  cpu [ms]             CPU use per task\r
  atomics              lost updates of atomic adds from both cores (PSRAM and\r
                       internal RAM; Rust atomics must lose none)\r
  restart              restart the ESP32\r
  restart bringup      restart once without the hub (the MG24 UART stays free\r
                       for the mg24 commands)\r
";

static PORT: OnceLock<Console> = OnceLock::new();

/// Route UART0 back to its default pins (TX GPIO1, RX GPIO3, the CP2102N).
/// SONOFF's stock bootloader, which stays in place, leaves the console off
/// those pins, so without this the application's console is silent and
/// deaf. Call before anything logs.
pub fn route(console: Console) {
    if console == Console::Uart0 {
        unsafe {
            sys::uart_set_pin(0, 1, 3, -1, -1);
        }
    }
}

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
    let mut s = format!(
        "zrh-gateway {}{}\r\n",
        env!("CARGO_PKG_VERSION"),
        if crate::bringup() { " (bring-up boot: no hub)" } else { "" }
    );
    for line in esp::boot_report(board()) {
        s += &line;
        s += "\r\n";
    }
    s += &format!("{}\r\n", esp::memory_report());
    s += &format!("last ESP-IDF panic: {}\r\n", cpanic::last().unwrap_or_else(|| "none recorded".into()));
    if let Some(p) = cpanic::last_panic() {
        s += &format!("previous boot panicked: {p}\r\n");
    }
    s += &cpanic::alloc_failures();
    s += &format!("button presses: {}\r\n", led::PRESSES.load(Ordering::Relaxed));
    s += &hub::status_text();
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
        ["wifi", "set", ssid, rest @ ..] => stored(net::configure(ssid, &rest.join(" ")), "stored; used now"),
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
        ["hub"] => hub::status_text(),
        ["permit", rest @ ..] => match rest.first().map_or(Ok(hub::BUTTON_PERMIT_S), |s| s.parse::<u32>()) {
            Ok(secs) if secs <= 254 => match hub::permit_join(secs) {
                Ok(()) => format!("permit join {secs} s requested (result in the log)\r\n"),
                Err(e) => format!("error: {e}\r\n"),
            },
            _ => "usage: permit [0-254]\r\n".into(),
        },
        ["token"] => match settings::get("api_token") {
            Some(t) => format!("API token: {t}\r\n"),
            None => "no token yet (generated when the hub starts)\r\n".into(),
        },
        ["files"] => files::describe(),
        ["files", "claim"] => files::plan(),
        ["files", "claim", "confirm"] => match files::start_claim() {
            Ok(()) => "claiming: SONOFF's files are copied first; the hub restarts when it is done (`files` shows progress)\r\n".into(),
            Err(e) => format!("error: {e}\r\n"),
        },
        ["restart", "bringup"] => {
            reply("restarting once without the hub\r\n");
            crate::restart_bringup()
        }
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
