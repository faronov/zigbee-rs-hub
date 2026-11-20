//! Line console on the board's console port (UART0 behind the CP2102N on
//! the Dongle-M, USB-Serial-JTAG on the S3 SuperMini): input read byte by
//! byte from the ESP-IDF driver (the VFS would only return whole lines),
//! local echo, output and logs through stdout on the same driver, one
//! command per line (`help`).

use std::io::Write;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use esp_idf_svc::sys;
use hub_core::profile::{BoardProfile, Console};
use hub_ncp::driver::CallOpts;
use hub_ncp::lifecycle::FormRequest;
use hub_ncp::zrhp::codec::Rest;
use hub_ncp::zrhp::msg::system::SysEcho;
use log::{info, warn};

use crate::{APP, BOARD, HUB, esp, net, radio, settings, uart};

const HELP: &str = "\
zigbee-rs-hub gateway console:\r
  info                 board, memory, flash layout, straps\r
  board                NCP wiring (profile + overrides)\r
  board set <k=v ...>  store overrides (tx rx rts cts reset reset_od baud flow\r
                       fallback), applied at the next boot\r
  board clear          remove the overrides\r
  hub [status]         session, NCP, network, store\r
  hub echo [n]         SYS_ECHO round trips\r
  hub form [channel]   form a network with hub-chosen parameters\r
  hub permit <s>       open joining\r
  hub ncp reset [hard] SYS_RESET, or a reset-line pulse\r
  radio                radio banks, pending job, last result\r
  radio install [A|B]  install a bank through the Gecko bootloader (restarts)\r
  radio check <A|B>    re-check a bank's image\r
  net                  Ethernet / Wi-Fi state\r
  wifi set <ssid> [pw] store Wi-Fi credentials (used from the next boot)\r
  wifi clear           forget them\r
  token                the API token\r
  restart              restart the host\r
";

static PORT: OnceLock<Console> = OnceLock::new();

/// Install the console driver and route stdout through it.
pub fn init(console: Console) {
    let _ = PORT.set(console);
    unsafe {
        match console {
            Console::Uart0 => {
                sys::uart_driver_install(0, 512, 0, 0, core::ptr::null_mut(), 0);
                sys::uart_vfs_dev_use_driver(0);
            }
            #[cfg(not(esp32))]
            Console::UsbSerialJtag => {
                let mut cfg: sys::usb_serial_jtag_driver_config_t = core::mem::zeroed();
                cfg.rx_buffer_size = 512;
                cfg.tx_buffer_size = 1024;
                sys::usb_serial_jtag_driver_install(&mut cfg);
                sys::esp_vfs_usb_serial_jtag_use_driver();
            }
            _ => {}
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

fn board_text(b: &BoardProfile) -> String {
    let u = &b.uart;
    let pin = |p: Option<u8>| p.map_or("-".into(), |p| p.to_string());
    let mut s = format!(
        "board {} ({}), host {:?}, NCP {:?}\r\nNCP UART{}: tx {} rx {} rts {} cts {}, line {} {}, fallback {}\r\n",
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
        if u.line.flow { "rts/cts" } else { "no flow" },
        u.fallback.map_or("-".into(), |l| l.baud.to_string()),
    );
    s += &format!("NCP control: reset {:?} {} ms, loader {:?}\r\n", b.control.reset, b.control.reset_ms, b.control.loader);
    s += &format!("ethernet {:?}\r\nled {:?} button {:?} console {:?}\r\n", b.ethernet, b.led, b.button, b.console);
    s += &format!(
        "flash {} MiB, PSRAM {} MiB, layout {:?}\r\n",
        b.memory.flash >> 20,
        b.memory.psram >> 20,
        b.memory.layout
    );
    s += &format!("overrides: {}\r\n", settings::get("board").unwrap_or_else(|| "-".into()));
    s
}

fn status_text() -> String {
    let Some(hub) = HUB.get() else {
        return format!("hub not running: {}\r\n", crate::hub_error().unwrap_or_else(|| "starting".into()));
    };
    let s = hub.status();
    let mut out = format!("hub: session {:?}, ownership {:?}\r\n", s.session, s.ownership);
    if let Some(n) = &s.ncp {
        out += &format!(
            "ncp: fw {:?} build {} chip {} caps 0x{:X} epoch {} line {} {}\r\n",
            n.negotiated.fw,
            hex(&n.negotiated.build_id),
            n.negotiated.chip,
            n.negotiated.ncp_caps,
            n.link.epoch,
            n.line.baud,
            if n.line.flow { "rts/cts" } else { "no flow" }
        );
        if let Some(i) = &n.info {
            out += &format!(
                "ncp: ieee {} flash {} KiB uart {} flow {} board {:?}\r\n",
                hex(&i.ieee_active),
                i.flash_size / 1024,
                i.uart_baud,
                i.uart_flow,
                i.board
            );
        }
    }
    if let Some(n) = &s.network {
        out += &format!("network: state {} PAN 0x{:04X} channel {}\r\n", n.state, n.pan_id, n.channel);
    }
    let c = hub.client().counters();
    out += &format!(
        "resumes {} NCP restarts {} devices {} store {} link tx {} retx {} rx {} bad {} uart overruns {} errors {}\r\n",
        s.resumes,
        s.ncp_restarts,
        s.devices,
        if s.store_volatile { "RAM" } else { "hubdata" },
        c.tx_frames,
        c.tx_retransmissions,
        c.rx_frames,
        c.rx_bad_frame,
        uart::OVERRUNS.load(std::sync::atomic::Ordering::Relaxed),
        uart::LINE_ERRORS.load(std::sync::atomic::Ordering::Relaxed),
    );
    if let Some(e) = &s.last_error {
        out += &format!("last error: {e}\r\n");
    }
    if let Some(app) = APP.get() {
        let a = app.status();
        out += &format!(
            "app: {} devices ({} restored), {} API sessions, registry writes {} errors {}\r\n",
            a.devices, a.restored, a.sessions, a.registry_writes, a.registry_errors
        );
    }
    out
}

fn net_text() -> String {
    let n = net::STATE.lock().unwrap().clone();
    format!("ethernet: {}\r\nwifi: {}\r\napi: {}\r\n", n.eth, n.wifi, if n.api { "ws://<ip>/api" } else { "down" })
}

fn background(name: &'static str, f: impl FnOnce(&hub_ncp::Hub) -> Result<String, String> + Send + 'static) -> String {
    let Some(hub) = HUB.get().cloned() else {
        return "hub not running\r\n".into();
    };
    match std::thread::Builder::new().stack_size(8192).spawn(move || match f(&hub) {
        Ok(msg) => info!("{name}: {msg}"),
        Err(e) => warn!("{name}: {e}"),
    }) {
        Ok(_) => format!("{name} started; see `hub status` and the log\r\n"),
        Err(e) => format!("error: {e}\r\n"),
    }
}

fn bank(s: &str) -> Option<u8> {
    match s {
        "A" | "a" => Some(0),
        "B" | "b" => Some(1),
        _ => None,
    }
}

fn command(words: &[&str]) -> String {
    let hub = HUB.get();
    match words {
        [] => String::new(),
        ["help"] => HELP.into(),
        ["info"] => {
            let mut s = format!("zigbee-rs-hub gateway {}\r\n", env!("CARGO_PKG_VERSION"));
            if let Some(b) = BOARD.get() {
                s += &board_text(b);
                for line in esp::strapping_report(b) {
                    s += &format!("strap: {line}\r\n");
                }
            }
            s += &format!("{}\r\n", esp::memory_report());
            s += &format!("layout: {}\r\n", crate::layout_text());
            s
        }
        ["board"] => BOARD.get().map(board_text).unwrap_or_default(),
        ["board", "set", kv @ ..] => {
            let text = kv.join(" ");
            let mut b = *BOARD.get().expect("board");
            // Check against the shipped profile, as the next boot applies it.
            if let Some(base) = hub_core::profile::by_id(b.id) {
                b = *base;
            }
            match b.apply(&text) {
                Ok(()) => match settings::set("board", Some(&text)) {
                    Ok(()) => "stored; applied at the next boot (`restart`)\r\n".into(),
                    Err(e) => format!("error: {e}\r\n"),
                },
                Err(e) => format!("error: {e}\r\n"),
            }
        }
        ["board", "clear"] => match settings::set("board", None) {
            Ok(()) => "overrides removed; next boot uses the profile\r\n".into(),
            Err(e) => format!("error: {e}\r\n"),
        },
        ["hub"] | ["hub", "status"] => status_text(),
        ["hub", "echo", rest @ ..] => {
            let Some(hub) = hub else { return "hub not running\r\n".into() };
            let n: u32 = rest.first().and_then(|s| s.parse().ok()).unwrap_or(10).clamp(1, 200);
            let t = Instant::now();
            let mut worst = Duration::ZERO;
            for i in 0..n {
                let data = [i as u8; 32];
                let s = Instant::now();
                match hub.client().call(&SysEcho { data: Rest(&data) }, CallOpts::timeout(Duration::from_secs(2))) {
                    Ok(rsp) if rsp.payload.get(1..) == Some(&data[..]) => worst = worst.max(s.elapsed()),
                    Ok(_) => return "echo mismatch\r\n".into(),
                    Err(e) => return format!("echo: {e}\r\n"),
                }
            }
            format!("echo x{n}: mean {:?} worst {worst:?}\r\n", t.elapsed() / n)
        }
        ["hub", "form", rest @ ..] => {
            let mut req = FormRequest::default();
            if let Some(ch) = rest.first().and_then(|c| c.parse::<u32>().ok()).filter(|c| (11..=26).contains(c)) {
                req.channel_mask = 1 << ch;
            }
            background("form", move |hub| {
                hub.form(req).map(|n| format!("PAN 0x{:04X} channel {}", n.pan_id, n.channel)).map_err(|e| e.to_string())
            })
        }
        ["hub", "permit", secs] => match hub.map(|h| h.permit_join(secs.parse().unwrap_or(60))) {
            Some(Ok(())) => format!("joining open for {secs} s\r\n"),
            Some(Err(e)) => format!("error: {e}\r\n"),
            None => "hub not running\r\n".into(),
        },
        ["hub", "ncp", "reset", rest @ ..] => match hub.map(|h| h.reset_ncp(rest.first() == Some(&"hard"))) {
            Some(Ok(())) => "NCP reset; the hub resumes it\r\n".into(),
            Some(Err(e)) => format!("error: {e}\r\n"),
            None => "hub not running\r\n".into(),
        },
        ["radio"] | ["radio", "status"] => radio::status_text(),
        ["radio", "install", rest @ ..] => {
            let b = rest.first().and_then(|s| bank(s));
            if rest.first().is_some() && b.is_none() {
                return "usage: radio install [A|B]\r\n".into();
            }
            match radio::schedule(b) {
                Ok(msg) => {
                    reply(&format!("{msg}\r\n"));
                    if let Some(h) = hub {
                        h.shutdown();
                    }
                    crate::restart()
                }
                Err(e) => format!("error: {e}\r\n"),
            }
        }
        ["radio", "check", b] => match bank(b) {
            Some(i) => radio::check(i),
            None => "usage: radio check <A|B>\r\n".into(),
        },
        ["net"] | ["wifi"] | ["eth"] => net_text(),
        ["wifi", "set", ssid, rest @ ..] => {
            let pass = rest.join(" ");
            match settings::set("wifi_ssid", Some(ssid)).and_then(|_| settings::set("wifi_pass", Some(&pass))) {
                Ok(()) => "stored; used from the next boot\r\n".into(),
                Err(e) => format!("error: {e}\r\n"),
            }
        }
        ["wifi", "clear"] => match settings::set("wifi_ssid", None).and_then(|_| settings::set("wifi_pass", None)) {
            Ok(()) => "Wi-Fi credentials removed\r\n".into(),
            Err(e) => format!("error: {e}\r\n"),
        },
        ["token"] => match settings::get("api_token") {
            Some(t) => format!("API token: {t}\r\n"),
            None => "no token yet\r\n".into(),
        },
        ["restart"] => {
            reply("restarting\r\n");
            if let Some(h) = hub {
                h.shutdown();
            }
            crate::restart()
        }
        _ => format!("unknown command {:?} (`help`)\r\n", words.join(" ")),
    }
}

fn reply(s: &str) {
    let mut out = std::io::stdout();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

/// One input byte (waits up to 1 s).
fn read_byte(b: &mut [u8; 1]) -> bool {
    let n = unsafe {
        match PORT.get() {
            Some(Console::Uart0) => sys::uart_read_bytes(0, b.as_mut_ptr().cast(), 1, 1000),
            #[cfg(not(esp32))]
            Some(Console::UsbSerialJtag) => sys::usb_serial_jtag_read_bytes(b.as_mut_ptr().cast(), 1, 1000),
            _ => {
                std::thread::sleep(Duration::from_secs(1));
                0
            }
        }
    };
    n == 1
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
