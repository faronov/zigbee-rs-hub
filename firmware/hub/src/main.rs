//! zigbee-rs-hub phase 1 on the ESP32-S3 of the Thread BR board.
//!
//! Two boot modes (see [`mode`]):
//!
//! - **bridge** (default): exactly the phase-0 `tbr-bridge` (CDC 0 ↔ H2
//!   UART with esptool auto-reset, console on CDC 1), so a desktop can still
//!   flash and talk to the H2 NCP;
//! - **hub**: the `hub-ncp` ZRHP host on UART1 (921 600 with RTS/CTS on the
//!   reserved lines, 460 800 fallback), the host store in `hubdata` (RAM if
//!   the partition is missing), the application (`hub-app`: device engine,
//!   registry) with its WebSocket JSON API on `ws://<ip>/api` once Wi-Fi is
//!   configured, the same API as JSON lines on CDC 0 (test path), console on
//!   CDC 1 with `hub …`, `wifi …` and `token` commands.
//!
//! USB recovery (`s3 usj`, 1200 bps touch, mount timeout, crash safe mode)
//! works the same in both modes.

mod esp;
mod mode;
#[cfg(feature = "app")]
mod net;
#[cfg(feature = "app")]
mod usbapi;

use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::Duration;

use hub_ncp::driver::CallOpts;
use hub_ncp::lifecycle::{FormRequest, Hub, HubConfig};
use hub_ncp::store::Store;
use hub_ncp::zrhp::codec::Rest;
use hub_ncp::zrhp::msg::system::SysEcho;
use hub_core::layout::Verdict;
use log::{error, info, warn};
use tbr_bridge::{console, h2, uart, usb};

use crate::mode::Mode;

static HUB: OnceLock<Hub> = OnceLock::new();
static LAYOUT_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
#[cfg(feature = "app")]
static APP: OnceLock<hub_app::App> = OnceLock::new();
static BOOT: OnceLock<mode::Boot> = OnceLock::new();

const HELP: &str = "\
hub firmware:\r
  mode                 boot mode now and persisted\r
  mode bridge|hub      persist the mode and reboot\r
  mode hub once        boot the hub once (any later reset: persisted mode)\r
  hub [status]         session, NCP, network, update state (hub mode)\r
  hub echo [n]         SYS_ECHO round trips\r
  hub form [channel]   form a network with hub-chosen parameters\r
  hub permit <s>       open joining\r
  hub fw slots         NCP A/B slots\r
  hub fw confirm       FW_CONFIRM\r
  hub ncp reset [hard] SYS_RESET, or an EN pulse\r
  hub rom              reboot into the bridge with the H2 in its ROM loader\r
                       (manual recovery: esptool on CDC 0)\r
  wifi                 Wi-Fi state and the API address (hub mode, app build)\r
  wifi set <ssid> [pw] store Wi-Fi credentials (used from the next hub boot)\r
  wifi clear           forget them\r
  token                the API token (generated at the first hub boot)\r
  catalog              trusted definitions-catalog keys (app build)\r
";

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

fn status_text(hub: &Hub) -> String {
    let s = hub.status();
    let mut out = format!("hub: session {:?}, ownership {:?}\r\n", s.session, s.ownership);
    if let Some(n) = &s.ncp {
        out += &format!(
            "ncp: fw {:?} chip {} caps 0x{:X} epoch {} line {} {}\r\n",
            n.negotiated.fw,
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
    for slot in &s.slots {
        out += &format!("slot {}: state {} version 0x{:08X}\r\n", slot.slot, slot.state, slot.version);
    }
    let c = hub.client().counters();
    out += &format!(
        "resumes {} NCP restarts {} fw {:?} devices {} store {} link tx {} retx {} rx {} bad {}\r\n",
        s.resumes,
        s.ncp_restarts,
        s.fw,
        s.devices,
        if s.store_volatile { "RAM" } else { "hubdata" },
        c.tx_frames,
        c.tx_retransmissions,
        c.rx_frames,
        c.rx_bad_frame
    );
    if let Some(e) = &s.last_error {
        out += &format!("last error: {e}\r\n");
    }
    #[cfg(feature = "app")]
    if let Some(app) = APP.get() {
        let a = app.status();
        out += &format!(
            "app: {} devices ({} restored), {} API sessions, registry writes {} errors {}, APS ok {} busy {} failed {}, endpoint {}\r\n",
            a.devices,
            a.restored,
            a.sessions,
            a.registry_writes,
            a.registry_errors,
            a.aps.accepted,
            a.aps.busy,
            a.aps.failed,
            if a.endpoint_registered { "registered" } else { "pending" }
        );
    }
    #[cfg(feature = "app")]
    {
        out += &format!("wifi: {}\r\n", net::STATE.lock().unwrap());
    }
    out
}

/// Long operations run off the console thread (the supervisor restarts the
/// S3 when the console loop stalls for 15 s).
fn background(name: &'static str, f: impl FnOnce(&Hub) -> Result<String, String> + Send + 'static) -> String {
    let Some(hub) = HUB.get() else {
        return "error: not in hub mode\r\n".into();
    };
    let hub = hub.clone();
    let spawned = std::thread::Builder::new().stack_size(8192).spawn(move || match f(&hub) {
        Ok(msg) => info!("{name}: {msg}"),
        Err(e) => warn!("{name}: {e}"),
    });
    match spawned {
        Ok(_) => format!("{name} started; see `hub status` and the UART0 log\r\n"),
        Err(e) => format!("error: {e}\r\n"),
    }
}

fn not_hub() -> String {
    "not in hub mode (`mode hub once`)\r\n".into()
}

fn extension(words: &[&str]) -> Option<String> {
    let boot = *BOOT.get()?;
    let hub_mode = boot.mode == Mode::Hub;
    let hub = HUB.get();
    Some(match words {
        ["info"] => {
            let mut s = console::info();
            s += &format!(
                "tbr-hub {}: mode {:?}{} (persisted {:?})\r\n",
                env!("CARGO_PKG_VERSION"),
                boot.mode,
                if boot.once { " once" } else { "" },
                mode::stored()
            );
            if let Some(hub) = hub {
                s += &status_text(hub);
            }
            s
        }
        ["mode"] => {
            format!("mode {:?}{}, persisted {:?}\r\n", boot.mode, if boot.once { " (once)" } else { "" }, mode::stored())
        }
        ["mode", "hub", "once"] => {
            console_reply("rebooting into hub mode once\r\n");
            mode::restart_once(true)
        }
        ["mode", m @ ("bridge" | "hub")] => {
            let m = if *m == "hub" { Mode::Hub } else { Mode::Bridge };
            match mode::store(m) {
                Ok(()) => {
                    console_reply(&format!("mode {m:?} stored, rebooting\r\n"));
                    mode::restart()
                }
                Err(e) => format!("error: {e}\r\n"),
            }
        }
        ["hub", ..] if hub.is_none() => match LAYOUT_ERROR.lock().unwrap().as_ref() {
            Some(e) => format!("hub not started: partition table does not belong to this flash ({e})\r\n"),
            None => not_hub(),
        },
        ["hub"] | ["hub", "status"] => status_text(hub?),
        ["hub", "echo", rest @ ..] => {
            let n: u32 = rest.first().and_then(|s| s.parse().ok()).unwrap_or(10).clamp(1, 200);
            let t = std::time::Instant::now();
            let mut worst = Duration::ZERO;
            for i in 0..n {
                let data = [i as u8; 32];
                let s = std::time::Instant::now();
                let opts = CallOpts::timeout(Duration::from_secs(2));
                match hub?.client().call(&SysEcho { data: Rest(&data) }, opts) {
                    Ok(rsp) if rsp.payload.get(1..) == Some(&data[..]) => worst = worst.max(s.elapsed()),
                    Ok(_) => return Some("echo mismatch\r\n".into()),
                    Err(e) => return Some(format!("echo: {e}\r\n")),
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
        ["hub", "permit", secs] => {
            let secs: u8 = secs.parse().unwrap_or(60);
            match hub?.permit_join(secs) {
                Ok(()) => format!("joining open for {secs} s\r\n"),
                Err(e) => format!("error: {e}\r\n"),
            }
        }
        ["hub", "fw", "slots"] => match hub?.fw_slots() {
            Ok(slots) => slots
                .iter()
                .map(|s| format!("slot {}: state {} version 0x{:08X} sha256 {}\r\n", s.slot, s.state, s.version, hex(&s.sha256)))
                .collect(),
            Err(e) => format!("error: {e}\r\n"),
        },
        ["hub", "fw", "confirm"] => match hub?.fw_confirm() {
            Ok(()) => "confirmed\r\n".into(),
            Err(e) => format!("error: {e}\r\n"),
        },
        ["hub", "ncp", "reset", rest @ ..] => match hub?.reset_ncp(rest.first() == Some(&"hard")) {
            Ok(()) => "NCP reset; the hub resumes it\r\n".into(),
            Err(e) => format!("error: {e}\r\n"),
        },
        ["hub", "rom"] => {
            console_reply("rebooting into the bridge with the H2 in its ROM loader\r\n");
            hub?.shutdown();
            mode::restart_once(false)
        }
        #[cfg(feature = "app")]
        ["wifi"] => format!("wifi: {}\r\n", net::STATE.lock().unwrap()),
        #[cfg(feature = "app")]
        ["catalog"] => catalog_keys(),
        ["wifi", "set", ssid, rest @ ..] => {
            let pass = rest.join(" ");
            match mode::set_str("wifi_ssid", Some(ssid)).and_then(|_| mode::set_str("wifi_pass", Some(&pass))) {
                Ok(()) => "stored; used from the next hub boot\r\n".into(),
                Err(e) => format!("error: {e}\r\n"),
            }
        }
        ["wifi", "clear"] => match mode::set_str("wifi_ssid", None).and_then(|_| mode::set_str("wifi_pass", None)) {
            Ok(()) => "Wi-Fi credentials removed\r\n".into(),
            Err(e) => format!("error: {e}\r\n"),
        },
        ["token"] => match mode::get_str("api_token") {
            Some(t) => format!("API token: {t}\r\n"),
            None => "no token yet (generated at the first hub boot)\r\n".into(),
        },
        // The hub owns UART1 and EN/BOOT in hub mode.
        ["baud", ..] | ["flow", ..] | ["lines", ..] | ["h2", ..] | ["gpio", ..] if hub_mode => {
            "error: the hub owns the H2 UART and EN/BOOT in hub mode (`hub rom`, `mode bridge`)\r\n".into()
        }
        _ => return None,
    })
}

fn console_reply(s: &str) {
    usb::write(usb::CONSOLE, s.as_bytes(), 50);
}

fn start_hub() {
    let (_, verdict) = esp::flash_layout();
    let store = match verdict {
        Verdict::Fatal(m) => {
            error!("hub not started: the partition table does not belong to this flash ({m:?}); flash the matching table");
            *LAYOUT_ERROR.lock().unwrap() = Some(format!("{m:?}"));
            return;
        }
        Verdict::Foreign => Store::volatile(),
        Verdict::Ok(_) => match esp::Partition::find().map(|p| Store::open(Box::new(p))) {
            Some(Ok(s)) => {
                info!("host store: hubdata journal {:?}", s.stats());
                s
            }
            Some(Err(e)) => {
                warn!("host store: hubdata journal unusable ({e}); RAM only");
                Store::volatile()
            }
            None => {
                warn!("host store: no usable hubdata partition; RAM only");
                Store::volatile()
            }
        },
    };
    let mut cfg = HubConfig::default();
    cfg.driver.host_name = format!("zigbee-rs-hub {}", env!("CARGO_PKG_VERSION"));
    let hub = Hub::start(cfg, esp::UartRx, esp::UartTx, Some(Box::new(esp::H2Lines)), store, esp::rng());
    let _ = HUB.set(hub.clone());
    #[cfg(feature = "app")]
    start_app(hub);
}

/// The catalog verifier's key slots.
#[cfg(feature = "app")]
fn catalog_keys() -> String {
    hub_app::catalog::KEY_SLOTS
        .iter()
        .enumerate()
        .map(|(i, k)| match k {
            Some(k) => format!("catalog key slot {i}: kid {} ed25519 {}\r\n", hex(&hub_app::catalog::key_ids()[i].1), hex(k)),
            None => format!("catalog key slot {i}: reserved (rotation)\r\n"),
        })
        .collect()
}

#[cfg(feature = "app")]
fn start_app(hub: Hub) {
    for line in catalog_keys().lines() {
        info!("{}", line.trim_end());
    }
    let token = mode::get_str("api_token").unwrap_or_else(|| {
        let mut b = [0u8; 12];
        (esp::rng())(&mut b);
        let t = hex(&b).to_lowercase();
        if let Err(e) = mode::set_str("api_token", Some(&t)) {
            warn!("API token not stored: {e}");
        }
        t
    });
    let app_cfg = hub_app::AppConfig {
        hub_name: format!("zigbee-rs-hub {}", env!("CARGO_PKG_VERSION")),
        api_token: token,
        stack_size: APP_STACK,
        ..Default::default()
    };
    let app = hub_app::App::start(hub.clone(), app_cfg);
    let _ = APP.set(app.clone());
    usbapi::start(app.clone());
    match esp_idf_svc::hal::peripherals::Peripherals::take() {
        Ok(p) => net::start(app, p.modem, move |up| {
            if let Err(e) = hub.set_coex(up) {
                warn!("coex_enable {up}: {e}");
            }
        }),
        Err(e) => warn!("peripherals: {e}"),
    }
}

/// Stack of the application thread (engine handlers, serde_json).
#[cfg(feature = "app")]
const APP_STACK: usize = 64 * 1024;

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    info!("tbr-hub {} starting", env!("CARGO_PKG_VERSION"));
    tbr_bridge::safe_mode_if_crashed();

    let boot = mode::boot();
    let _ = BOOT.set(boot);
    info!("boot mode {boot:?}");
    console::set_extension(extension, HELP);
    h2::init().expect("H2 EN/BOOT GPIO");

    match boot.mode {
        Mode::Bridge => {
            uart::init(hub_core::board::ROM_BAUD).expect("H2 UART");
            usb::init().expect("TinyUSB");
            tbr_bridge::start_bridge_threads();
            tbr_bridge::start_console();
            if boot.h2_rom {
                uart::flush_input();
                h2::enter_download();
                info!("H2 in ROM download mode (115200 8N1): flash it with esptool on CDC 0");
            }
        }
        Mode::Hub => {
            usb::BRIDGE_ENABLED.store(false, Ordering::SeqCst);
            h2::AUTO_LINES.store(false, Ordering::SeqCst);
            uart::init(hub_ncp::transport::Line::BR_DEFAULT.baud).expect("H2 UART");
            usb::init().expect("TinyUSB");
            tbr_bridge::start_console();
            start_hub();
        }
    }
    tbr_bridge::park()
}
