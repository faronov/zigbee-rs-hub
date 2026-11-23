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

mod alloc;
#[cfg(feature = "app")]
mod catalog;
mod cpu;
mod esp;
mod mode;
mod resources;
#[cfg(feature = "mqtt")]
mod mqtt;
#[cfg(feature = "app")]
mod net;
#[cfg(feature = "app")]
mod platform;
#[cfg(feature = "app")]
mod ws;
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
#[cfg(feature = "mqtt")]
static MQTT: OnceLock<hub_mqtt::MqttService> = OnceLock::new();
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
  hub crash            panic on purpose (tests the restart after a crash)\r
  hub cpu [ms]         CPU use per task over a window (default 2000 ms)\r
  wifi                 Wi-Fi state and the API address (hub mode, app build)\r
  wifi set <ssid> [pw] store Wi-Fi credentials (hub mode: connect with them now)\r
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
    out += &unsafe {
        format!(
            "ram: internal free {} (min {}), PSRAM {} of {}\r\n",
            esp_idf_svc::sys::heap_caps_get_free_size(esp_idf_svc::sys::MALLOC_CAP_INTERNAL),
            esp_idf_svc::sys::heap_caps_get_minimum_free_size(esp_idf_svc::sys::MALLOC_CAP_INTERNAL),
            esp_idf_svc::sys::heap_caps_get_free_size(esp_idf_svc::sys::MALLOC_CAP_SPIRAM),
            esp_idf_svc::sys::heap_caps_get_total_size(esp_idf_svc::sys::MALLOC_CAP_SPIRAM)
        )
    };
    if let Some(e) = &s.last_error {
        out += &format!("last error: {e}\r\n");
    }
    #[cfg(feature = "app")]
    if !resources::app_allowed() {
        out += &format!("app: not started. {}\r\n", resources::wrong_image());
    }
    #[cfg(feature = "app")]
    if let Some(app) = APP.get() {
        let a = app.status();
        out += &format!(
            "app: {} devices ({} restored, {} imported), {} API sessions, registry writes {} errors {}, APS ok {} busy {} failed {} delivered {} not delivered {} unconfirmed {}, endpoint {}\r\n",
            a.devices,
            a.restored,
            a.imported,
            a.sessions,
            a.registry_writes,
            a.registry_errors,
            a.aps.accepted,
            a.aps.busy,
            a.aps.failed,
            a.aps.delivered,
            a.aps.not_delivered,
            a.aps.unconfirmed,
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
            s += &format!("{}\r\n", prev_stage());
            if let Some(Some(t)) = PREV_TASKS.get() {
                s += &format!("previous boot, last stack watch (free bytes): {t}\r\n");
            }
            if let Some(p) = last_panic() {
                s += &format!("previous boot panicked: {p}\r\n");
            }
            s += &alloc_failures();
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
        ["hub", "stacks"] => format!("{}\r\n", tasks_snapshot()),
        ["hub", "crash"] => panic!("console: hub crash"),
        ["hub", "bench"] => cpu::bench(),
        ["hub", "cpu", rest @ ..] => {
            let ms = rest.first().and_then(|s| s.parse().ok()).unwrap_or(2000u64).clamp(100, 30_000);
            cpu::report(Duration::from_millis(ms))
        }
        ["hub", "echo", rest @ ..] => {
            let n: u32 = rest.first().and_then(|s| s.parse().ok()).unwrap_or(10).clamp(1, 200);
            let t = std::time::Instant::now();
            let mut worst = Duration::ZERO;
            hub?.client().take_latency();
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
            format!("echo x{n}: mean {:?} worst {worst:?}; {}\r\n", t.elapsed() / n, hub?.client().take_latency())
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
        #[cfg(feature = "app")]
        ["wifi", "set", ssid, rest @ ..] if hub_mode && APP.get().is_some() => match net::configure(ssid, &rest.join(" ")) {
            Ok(()) => "stored; connecting with them now\r\n".into(),
            Err(e) => format!("error: {e}\r\n"),
        },
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
    stage(3);
    let hub = cpu::named(c"zrhp", || Hub::start(cfg, esp::UartRx, esp::UartTx, Some(Box::new(esp::H2Lines)), store, esp::rng()));
    let _ = HUB.set(hub.clone());
    stage(4);
    #[cfg(feature = "app")]
    if resources::app_allowed() {
        start_app(hub, verdict);
    } else {
        start_wrong_image(hub);
    }
}

/// No PSRAM: the hub core only, plus Wi-Fi and a page that says so.
#[cfg(feature = "app")]
fn start_wrong_image(hub: Hub) {
    match esp_idf_svc::hal::peripherals::Peripherals::take() {
        Ok(p) => net::start(None, p.modem, move |up| {
            platform::wifi_changed(up);
            if let Err(e) = hub.set_coex(up) {
                warn!("coex_enable {up}: {e}");
            }
        }),
        Err(e) => warn!("peripherals: {e}"),
    }
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

/// `verdict`: the partition table check (the OTA image store needs the
/// 8 MB production layout's inactive app slot).
#[cfg(feature = "app")]
fn start_app(hub: Hub, verdict: Verdict) {
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
    // The example definitions take ~90 KB decoded; a flash-backed catalog
    // and generic device support remain available without them.
    let builtin_catalog = resources::get().builtin_catalog;
    #[allow(unused_mut)]
    let mut app_cfg = hub_app::AppConfig {
        hub_name: format!("zigbee-rs-hub {}", env!("CARGO_PKG_VERSION")),
        api_token: token,
        stack_size: APP_STACK,
        builtin_catalog,
        platform: Some(std::sync::Arc::new(platform::S3)),
        admin_stack: ADMIN_STACK,
        ..Default::default()
    };
    // The catalog store in hubdata (banks read in place from the mapping).
    let catalog = catalog::HubData::open().map(|f| hub_app::CatalogSetup {
        flash: std::sync::Arc::new(std::sync::Mutex::new(Box::new(f) as Box<dyn hub_app::catalog::data::DataFlash>)),
        extra_keys: Vec::new(),
    });
    #[cfg(feature = "ota-store")]
    if verdict == Verdict::Ok(hub_core::layout::Layout::Production) {
        match esp::InactiveSlot::find() {
            Ok(slot) => app_cfg.ota_flash = hub_app::OtaFlash::app_slot(Box::new(slot)),
            Err(e) => warn!("OTA image store: {e}; streamed images only"),
        }
    }
    #[cfg(not(feature = "ota-store"))]
    let _ = verdict;
    stage(5);
    let app = cpu::named(c"app", || hub_app::App::start_with(hub.clone(), app_cfg, catalog));
    let _ = APP.set(app.clone());
    #[cfg(feature = "mqtt")]
    let _ = MQTT.set(mqtt::start(&app, resources::get()));
    stage(6);
    usbapi::start(app.clone());
    stage(7);
    match esp_idf_svc::hal::peripherals::Peripherals::take() {
        Ok(p) => net::start(Some(app), p.modem, move |up| {
            platform::wifi_changed(up);
            if let Err(e) = hub.set_coex(up) {
                warn!("coex_enable {up}: {e}");
            }
        }),
        Err(e) => warn!("peripherals: {e}"),
    }
}

/// Stack of the application thread (engine handlers, serde_json).
#[cfg(feature = "app")]
const APP_STACK: usize = 40 * 1024;

/// Stack of the maintenance job thread (network map, backup), allocated per
/// job from internal RAM.
#[cfg(feature = "app")]
const ADMIN_STACK: usize = 12 * 1024;

/// The last Rust panic message, kept in RTC memory across the panic reset
/// (hub-mode logs go to UART0, which the hwlab host cannot see).
#[unsafe(link_section = ".rtc_noinit")]
static mut PANIC_MSG: [u8; 256] = [0; 256];
const PANIC_MAGIC: &[u8; 4] = b"PNC1";

fn record_panics() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let text = format!("{info}");
        let n = text.len().min(250);
        let p = &raw mut PANIC_MSG;
        // SAFETY: written once by the panicking thread right before the reset.
        let buf = unsafe { &mut *p };
        buf[..4].copy_from_slice(PANIC_MAGIC);
        buf[4] = n as u8;
        buf[5..5 + n].copy_from_slice(&text.as_bytes()[..n]);
        prev(info);
    }));
}

fn last_panic() -> Option<String> {
    static LAST: OnceLock<Option<String>> = OnceLock::new();
    LAST.get_or_init(|| {
        let p = &raw mut PANIC_MSG;
        // SAFETY: read and cleared once at the first use, before any panic of this boot.
        let buf = unsafe { &*p };
        if &buf[..4] != PANIC_MAGIC {
            return None;
        }
        let n = (buf[4] as usize).min(250);
        let text = String::from_utf8_lossy(&buf[5..5 + n]).into_owned();
        // Reported for the boot after the panic only.
        unsafe { (&mut *p)[..4].fill(0) };
        Some(text)
    })
    .clone()
}

/// Task stack and heap watch: the free stack of every task and the free
/// internal heap go to RTC memory (every 20 ms while starting, then every
/// second), so a crash in hub mode (its log is on UART0, which the lab host
/// cannot see) leaves the last picture for `info` after the next boot.
#[unsafe(link_section = ".rtc_noinit")]
static mut TASKS: [u8; 512] = [0; 512];
const TASKS_MAGIC: &[u8; 4] = b"TSK1";
static PREV_TASKS: OnceLock<Option<String>> = OnceLock::new();

fn tasks_snapshot() -> String {
    let mut s = unsafe {
        format!(
            "t={}ms internal heap free {} (min {}), PSRAM free {}",
            esp_idf_svc::sys::esp_timer_get_time() / 1000,
            esp_idf_svc::sys::heap_caps_get_free_size(esp_idf_svc::sys::MALLOC_CAP_INTERNAL),
            esp_idf_svc::sys::heap_caps_get_minimum_free_size(esp_idf_svc::sys::MALLOC_CAP_INTERNAL),
            esp_idf_svc::sys::heap_caps_get_free_size(esp_idf_svc::sys::MALLOC_CAP_SPIRAM)
        )
    };
    unsafe {
        let n = esp_idf_svc::sys::uxTaskGetNumberOfTasks() as usize;
        let mut tasks: Vec<esp_idf_svc::sys::TaskStatus_t> = vec![core::mem::zeroed(); n + 4];
        let got = esp_idf_svc::sys::uxTaskGetSystemState(tasks.as_mut_ptr(), tasks.len() as u32, core::ptr::null_mut()) as usize;
        for t in &tasks[..got] {
            let name = core::ffi::CStr::from_ptr(t.pcTaskName).to_string_lossy();
            s += &format!(" {name}:{}", t.usStackHighWaterMark);
        }
    }
    s
}

fn load_prev_tasks() {
    // SAFETY: read at boot, before the watch thread starts writing.
    let buf = unsafe { &*(&raw const TASKS) };
    let _ = PREV_TASKS.set((&buf[..4] == TASKS_MAGIC).then(|| {
        let n = u16::from_le_bytes([buf[4], buf[5]]) as usize;
        String::from_utf8_lossy(&buf[6..6 + n.min(buf.len() - 6)]).into_owned()
    }));
}

fn watch_stacks() {
    let _ = std::thread::Builder::new().stack_size(4096).spawn(move || loop {
        let p = &raw mut TASKS;
        let text = tasks_snapshot();
        let n = text.len().min(512 - 6);
        // SAFETY: only this thread writes the buffer after start-up.
        let buf = unsafe { &mut *p };
        buf[..4].copy_from_slice(TASKS_MAGIC);
        buf[4..6].copy_from_slice(&(n as u16).to_le_bytes());
        buf[6..6 + n].copy_from_slice(&text.as_bytes()[..n]);
        // Fine-grained while starting up (where the crashes were), then 1 s.
        let up = unsafe { esp_idf_svc::sys::esp_timer_get_time() } / 1000;
        if up > HEALTHY_UPTIME_MS {
            crash_streak_reset();
        }
        std::thread::sleep(Duration::from_millis(if up < 10_000 { 20 } else { 1000 }));
    });
}

/// Start-up stage of this boot and of the previous one (RTC memory).
#[unsafe(link_section = ".rtc_noinit")]
static mut STAGE: u32 = 0;
static PREV_STAGE: OnceLock<u32> = OnceLock::new();
/// Reset reason of the last crash (written by the safe-mode boot).
#[unsafe(link_section = ".rtc_noinit")]
static mut CRASH_REASON: u32 = 0;

/// Hub restarts after a crash before the safe mode takes over.
const MAX_CRASH_RESTARTS: u32 = 3;
/// Uptime after which a hub boot counts as healthy (ends a crash streak).
const HEALTHY_UPTIME_MS: i64 = 10 * 60 * 1000;
const STREAK_MAGIC: u32 = 0x4353_0000;

/// Consecutive crashed boots (RTC memory; `STREAK_MAGIC | n`).
#[unsafe(link_section = ".rtc_noinit")]
static mut CRASH_STREAK: u32 = 0;

fn crash_streak() -> u32 {
    let v = unsafe { (&raw const CRASH_STREAK).read_volatile() };
    if v >> 16 == STREAK_MAGIC >> 16 { v & 0xFFFF } else { 0 }
}

fn crash_streak_next() -> u32 {
    let n = crash_streak() + 1;
    unsafe { (&raw mut CRASH_STREAK).write_volatile(STREAK_MAGIC | n) };
    n
}

fn crash_streak_reset() {
    unsafe { (&raw mut CRASH_STREAK).write_volatile(STREAK_MAGIC) };
}

/// Free internal heap at each start-up stage (RTC memory, for the next boot).
#[unsafe(link_section = ".rtc_noinit")]
static mut STAGE_HEAP: [u32; 10] = [0; 10];
static PREV_STAGE_HEAP: OnceLock<[u32; 10]> = OnceLock::new();

fn stage(n: u32) {
    // SAFETY: plain word stores; only the main thread writes them.
    unsafe {
        (&raw mut STAGE).write_volatile(0x5354_0000 | n);
        if (n as usize) < 10 {
            let heap = esp_idf_svc::sys::heap_caps_get_free_size(esp_idf_svc::sys::MALLOC_CAP_INTERNAL) as u32;
            (*(&raw mut STAGE_HEAP))[n as usize] = heap;
        }
    }
}

/// Failed heap allocations (also ones the caller survives) in RTC memory:
/// magic, count, last size, last caps, time (ms) of the last one. Without
/// PSRAM a crash is often an exhausted heap, and an `abort()` from Rust's
/// allocation error handler leaves no panic message.
#[unsafe(link_section = ".rtc_noinit")]
static mut ALLOC_FAIL: [u32; 5] = [0; 5];
const ALLOC_MAGIC: u32 = 0x414C_4331;
static PREV_ALLOC_FAIL: OnceLock<[u32; 5]> = OnceLock::new();

unsafe extern "C" fn alloc_failed(size: usize, caps: u32, _function: *const core::ffi::c_char) {
    // SAFETY: plain word stores; a lost update between threads only miscounts.
    let a = unsafe { &mut *(&raw mut ALLOC_FAIL) };
    a[1] = a[1].wrapping_add(1);
    a[2] = size as u32;
    a[3] = caps;
    a[4] = (unsafe { esp_idf_svc::sys::esp_timer_get_time() } / 1000) as u32;
}

fn record_alloc_failures() {
    let a = unsafe { &mut *(&raw mut ALLOC_FAIL) };
    let _ = PREV_ALLOC_FAIL.set(if a[0] == ALLOC_MAGIC { *a } else { [0; 5] });
    *a = [ALLOC_MAGIC, 0, 0, 0, 0];
    unsafe { esp_idf_svc::sys::heap_caps_register_failed_alloc_callback(Some(alloc_failed)) };
}

fn alloc_failures() -> String {
    let line = |what: &str, a: [u32; 5]| {
        if a[1] == 0 {
            String::new()
        } else {
            format!("{what}: {} failed allocations, last {} bytes (caps 0x{:x}) at t={}ms\r\n", a[1], a[2], a[3], a[4])
        }
    };
    let now = unsafe { (&raw const ALLOC_FAIL).read_volatile() };
    line("previous boot", PREV_ALLOC_FAIL.get().copied().unwrap_or_default()) + &line("this boot", now)
}

fn prev_stage() -> String {
    let s = *PREV_STAGE.get().unwrap_or(&0);
    let reason = unsafe { esp_idf_svc::sys::esp_reset_reason() };
    let crash = unsafe { (&raw const CRASH_REASON).read_volatile() };
    let crash = if crash >> 16 == 0x4352 {
        format!("; last crash: reset reason {}, crash streak {}", crash & 0xFFFF, crash_streak())
    } else {
        String::new()
    };
    if s >> 16 == 0x5354 {
        let heap: String = PREV_STAGE_HEAP
            .get()
            .map(|h| h.iter().enumerate().filter(|(_, v)| **v != 0).map(|(i, v)| format!(" {i}:{v}")).collect())
            .unwrap_or_default();
        format!("previous boot reached start-up stage {}; free internal heap by stage{heap}; reset reason {reason}{crash}", s & 0xFFFF)
    } else {
        format!("reset reason {reason}{crash}")
    }
}

#[global_allocator]
static HEAP: alloc::PreferPsram = alloc::PreferPsram;

fn main() {
    esp_idf_svc::sys::link_patches();
    alloc::init();
    #[cfg(feature = "app")]
    platform::init_logger();
    #[cfg(not(feature = "app"))]
    esp_idf_svc::log::EspLogger::initialize_default();
    let _ = PREV_STAGE.set(unsafe { (&raw const STAGE).read_volatile() });
    let _ = PREV_STAGE_HEAP.set(unsafe { (&raw const STAGE_HEAP).read_volatile() });
    load_prev_tasks();
    let _ = last_panic();
    record_panics();
    record_alloc_failures();
    info!("tbr-hub {} starting", env!("CARGO_PKG_VERSION"));
    resources::log();
    let crashed = tbr_bridge::recovery::crashed_last_boot();
    if crashed {
        unsafe { (&raw mut CRASH_REASON).write_volatile(0x4352_0000 | esp_idf_svc::sys::esp_reset_reason()) };
    }
    let boot = mode::boot();
    // A persisted hub restarts after a crash; only a crash loop falls back
    // to the safe mode (which never returns, so the crashed boot's stage
    // survives it).
    if !crashed {
        crash_streak_reset();
    } else if boot.mode == Mode::Hub && crash_streak_next() <= MAX_CRASH_RESTARTS {
        warn!("previous boot crashed: restarting the hub ({} of {MAX_CRASH_RESTARTS})", crash_streak());
    } else {
        tbr_bridge::safe_mode_if_crashed();
    }
    unsafe { (&raw mut STAGE_HEAP).write_volatile([0; 10]) };
    stage(1);

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
            // The host link has RTS/CTS and credits; small rings also leave
            // room for the internal-RAM fallback.
            uart::init_with_rings(hub_ncp::transport::Line::BR_DEFAULT.baud, 4096, 2048).expect("H2 UART");
            usb::init().expect("TinyUSB");
            stage(2);
            watch_stacks();
            tbr_bridge::start_console();
            start_hub();
        }
    }
    stage(9);
    tbr_bridge::wait_usb_mount();
    // Returning ends the main task: ESP-IDF frees its stack
    // (CONFIG_ESP_MAIN_TASK_STACK_SIZE of internal RAM); the threads keep running.
}
