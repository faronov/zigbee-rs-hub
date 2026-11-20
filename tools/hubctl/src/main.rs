//! `hubctl`: the zigbee-rs-hub host core on a desktop.
//!
//! The same `hub-ncp` code the ESP32-S3 runs, talking to the real H2 NCP
//! through the phase-0 USB bridge (`tbr-bridge`, CDC 0), or to the
//! simulated NCP (`--sim`) for a fast development loop.
//!
//! ```text
//! hubctl --serial /dev/cu.usbmodem7CDFA1FE67F01 --read-only status
//! hubctl --sim form --channel 17
//! hubctl --serial … --store hubdata.bin fw-update ncp.bin --version 0x00010100
//! ```
//!
//! The bridge follows the host's line coding (921 600 by default here) and
//! does RTS/CTS towards the H2 itself; opening the port asserts DTR and
//! RTS, which leaves the H2 running (esptool auto-reset mapping).

mod serve;

use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hub_ncp::driver::{CallOpts, Message, SessionState};
use hub_ncp::fw::{FileSource, FwImage};
use hub_ncp::lifecycle::{FormRequest, Hub, HubConfig, HubStatus, ResumePolicy, Rng};
use hub_ncp::sim::SimNcp;
use hub_ncp::store::{FileFlash, JOURNAL_BYTES, SECTOR, Store};
use hub_ncp::transport::{Line, NcpControl, PortRx, PortTx, pipe};
use hub_ncp::zrhp::codec::Rest;
use hub_ncp::zrhp::msg::ids;
use hub_ncp::zrhp::msg::system::SysEcho;

const USAGE: &str = "\
usage: hubctl (--serial PATH [--baud N] | --sim) [--store FILE] [--read-only] [--hold-trial] [-v|-vv] COMMAND
commands:
  status                    session, NCP, network, slots, counters (default)
  echo [N]                  N SYS_ECHO round trips (latency)
  soak SECS                 echoes for SECS; NCP resets are resumed transparently
  form [--channel N]        form a network with hub-chosen PAN ID, EPID and key
  permit SECS               open joining (whole network)
  listen SECS               print NCP events
  fw-slots                  FW_GET_SLOTS
  fw-update FILE --version 0xMMmmrrrr
                            stream FILE into the inactive slot, trial boot, confirm
                            (--hold-trial: leave the trial image unconfirmed;
                            a reset then rolls the NCP back)
  fw-confirm                FW_CONFIRM
  reset [--hard]            SYS_RESET, or an EN pulse through the bridge's DTR/RTS
  serve [--listen ADDR] [--token T] [--for SECS]
                            the hub application (device engine, registry in the
                            store) with its WebSocket JSON API (default
                            127.0.0.1:8099, token from HUB_TOKEN or random)
also, without a hub:
  hubctl ws-call URL --token T [--listen SECS] JSON...
                            authenticate, send requests, print the frames
  hubctl catalog-verify FILE...
                            check catalog banks (.zdb) and manifest.cose with
                            the firmware's trusted keys
";

struct Logger(Instant, log::LevelFilter);

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata<'_>) -> bool {
        m.level() <= self.1
    }
    fn log(&self, r: &log::Record<'_>) {
        if self.enabled(r.metadata()) {
            eprintln!("{:>8.3} {:5} {}", self.0.elapsed().as_secs_f64(), r.level(), r.args());
        }
    }
    fn flush(&self) {}
}

struct SerialRx(Box<dyn serialport::SerialPort>);
struct SerialTx(Box<dyn serialport::SerialPort>);
struct BridgeLines(Box<dyn serialport::SerialPort>);

impl PortRx for SerialRx {
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        self.0.set_timeout(timeout).map_err(io::Error::other)?;
        match self.0.read(buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => Ok(0),
            Err(e) => Err(e),
        }
    }
    fn clear_input(&mut self) -> io::Result<()> {
        self.0.clear(serialport::ClearBuffer::Input).map_err(io::Error::other)
    }
}

impl PortTx for SerialTx {
    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        io::Write::write_all(&mut self.0, data)
    }
    fn set_line(&mut self, line: Line) -> io::Result<()> {
        // The bridge applies the rate to the H2 UART; RTS/CTS is the bridge's.
        if self.0.baud_rate().ok() != Some(line.baud) {
            self.0.set_baud_rate(line.baud).map_err(io::Error::other)?;
        }
        Ok(())
    }
}

impl BridgeLines {
    fn set(&mut self, dtr: bool, rts: bool) -> io::Result<()> {
        self.0.write_data_terminal_ready(dtr).map_err(io::Error::other)?;
        self.0.write_request_to_send(rts).map_err(io::Error::other)
    }
}

impl NcpControl for BridgeLines {
    fn reset(&mut self) -> io::Result<()> {
        // EN low = RTS && !DTR; both asserted = run.
        self.set(false, true)?;
        std::thread::sleep(Duration::from_millis(100));
        self.set(true, true)
    }
    fn enter_rom_loader(&mut self) -> io::Result<()> {
        // esptool classic reset: EN low, then BOOT low while EN rises.
        self.set(false, true)?;
        std::thread::sleep(Duration::from_millis(100));
        self.set(true, false)?;
        std::thread::sleep(Duration::from_millis(50));
        self.set(false, false)
    }
}

fn urandom() -> Rng {
    Arc::new(|buf: &mut [u8]| {
        let mut f = std::fs::File::open("/dev/urandom").expect("/dev/urandom");
        f.read_exact(buf).expect("/dev/urandom");
    })
}

struct Args(Vec<String>);

impl Args {
    fn flag(&mut self, name: &str) -> bool {
        match self.0.iter().position(|a| a == name) {
            Some(i) => {
                self.0.remove(i);
                true
            }
            None => false,
        }
    }
    fn value(&mut self, name: &str) -> Option<String> {
        let i = self.0.iter().position(|a| a == name)?;
        self.0.remove(i);
        (i < self.0.len()).then(|| self.0.remove(i))
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

fn parse_u32(s: &str) -> Option<u32> {
    match s.strip_prefix("0x") {
        Some(h) => u32::from_str_radix(h, 16).ok(),
        None => s.parse().ok(),
    }
}

fn print_status(s: &HubStatus) {
    println!("session: {:?}", s.session);
    if let Some(n) = &s.ncp {
        let g = &n.negotiated;
        println!(
            "ncp: fw {:?} v{}.{} chip {} caps 0x{:X} epoch {} cookie 0x{:08X} line {} {}",
            g.fw,
            g.major,
            g.minor,
            g.chip,
            g.ncp_caps,
            n.link.epoch,
            n.link.peer_cookie,
            n.line.baud,
            if n.line.flow { "rts/cts" } else { "no flow" }
        );
        println!("  capacities {:?}", g.capacities);
        if let Some(i) = &n.info {
            println!(
                "  ieee {} flash {} KiB transport {} uart {} flow {} board {:?} product {:?}",
                hex(&i.ieee_active),
                i.flash_size / 1024,
                i.transport,
                i.uart_baud,
                i.uart_flow,
                i.board,
                i.product
            );
        }
        if let Some(r) = &n.ready {
            println!(
                "  ready: reset_reason {} network_state {} table_epoch 0x{:08X} fc limits {}/{}",
                r.reset_reason, r.network_state, r.table_epoch, r.nwk_fc_limit, r.aps_fc_limit
            );
        }
    }
    if let Some(n) = &s.network {
        println!(
            "network: state {} PAN 0x{:04X} EPID {} channel {} short 0x{:04X} update {} key_seq {}",
            n.state,
            n.pan_id,
            hex(&n.ext_pan_id),
            n.channel,
            n.short,
            n.nwk_update_id,
            n.key_seq
        );
    }
    println!("ownership: {:?}", s.ownership);
    for slot in &s.slots {
        println!(
            "slot {}: state {} version 0x{:08X} sha256 {}…",
            slot.slot,
            slot.state,
            slot.version,
            hex(&slot.sha256[..6])
        );
    }
    if let Some(c) = &s.counters {
        println!("frame counters: nwk {}/{} aps {}/{} stride {}", c.nwk_current, c.nwk_limit, c.aps_current, c.aps_limit, c.stride);
    }
    println!(
        "resumes {} (last {:?}), NCP restarts {}, fw {:?}, devices {}, store {} ({:?})",
        s.resumes,
        s.last_resume,
        s.ncp_restarts,
        s.fw,
        s.devices,
        if s.store_volatile { "volatile" } else { "persistent" },
        s.journal
    );
    if let Some(e) = &s.last_error {
        println!("last error: {e}");
    }
}

fn print_event(m: &Message) {
    println!("event 0x{:04X} seq {} {}", m.msg_id, m.tid, hex(&m.payload));
}

fn echo_round(hub: &Hub, i: u32) -> Result<Duration, hub_ncp::driver::Error> {
    let data = [(i & 0xFF) as u8; 32];
    let t = Instant::now();
    let rsp = hub.client().call(&SysEcho { data: Rest(&data) }, CallOpts::default())?;
    if rsp.payload.get(1..) != Some(&data[..]) {
        return Err(hub_ncp::driver::Error::Other("echo mismatch".into()));
    }
    Ok(t.elapsed())
}

static HOLD_TRIAL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn run(hub: &Hub, mut args: Args) -> Result<(), String> {
    let cmd = if args.0.is_empty() { "status".to_string() } else { args.0.remove(0) };
    let ready = hub.wait_ready(Duration::from_secs(20));
    if !ready && cmd != "status" {
        return Err(format!("no session: {:?}", hub.status().session));
    }
    match cmd.as_str() {
        "status" => {
            if ready {
                let _ = hub.fw_slots();
            }
            print_status(&hub.status());
            if !ready {
                return Err("no session".into());
            }
        }
        "echo" => {
            let n = args.0.first().and_then(|s| s.parse().ok()).unwrap_or(10u32);
            let mut worst = Duration::ZERO;
            let mut total = Duration::ZERO;
            for i in 0..n {
                let d = echo_round(hub, i).map_err(|e| e.to_string())?;
                worst = worst.max(d);
                total += d;
            }
            println!("echo x{n}: mean {:?} worst {worst:?}", total / n.max(1));
        }
        "soak" => {
            let secs: u64 = args.0.first().and_then(|s| s.parse().ok()).unwrap_or(30);
            let until = Instant::now() + Duration::from_secs(secs);
            let (mut ok, mut failed, mut interrupted, mut worst) = (0u64, 0u64, 0u64, Duration::ZERO);
            let mut i = 0;
            while Instant::now() < until {
                match echo_round(hub, i) {
                    Ok(d) => {
                        ok += 1;
                        worst = worst.max(d);
                    }
                    // In flight when the NCP rebooted: expected, not a failure.
                    Err(hub_ncp::driver::Error::LinkReset) => interrupted += 1,
                    Err(e) => {
                        failed += 1;
                        log::warn!("echo: {e}");
                    }
                }
                i += 1;
            }
            let s = hub.status();
            let c = hub.client().counters();
            println!(
                "soak {secs} s: {ok} ok, {failed} failed, {interrupted} interrupted by NCP resets, worst {worst:?}; resumes {} NCP restarts {}; host link retx {} bad {} dup {}",
                s.resumes, s.ncp_restarts, c.tx_retransmissions, c.rx_bad_frame, c.rx_duplicate
            );
            if failed > 0 {
                return Err("echo failures".into());
            }
        }
        "form" => {
            let mut req = FormRequest::default();
            if let Some(ch) = args.value("--channel").and_then(|c| c.parse::<u32>().ok()) {
                req.channel_mask = 1 << ch;
            }
            let n = hub.form(req).map_err(|e| e.to_string())?;
            println!("formed PAN 0x{:04X} EPID {} channel {}", n.pan_id, hex(&n.ext_pan_id), n.channel);
        }
        "permit" => {
            let secs = args.0.first().and_then(|s| s.parse().ok()).unwrap_or(60u8);
            hub.permit_join(secs).map_err(|e| e.to_string())?;
            println!("joining open for {secs} s");
        }
        "listen" => {
            let secs: u64 = args.0.first().and_then(|s| s.parse().ok()).unwrap_or(30);
            let events = hub.subscribe();
            let until = Instant::now() + Duration::from_secs(secs);
            while let Some(left) = until.checked_duration_since(Instant::now()) {
                if let Ok(m) = events.recv_timeout(left) {
                    print_event(&m);
                }
            }
        }
        "fw-slots" => {
            for s in hub.fw_slots().map_err(|e| e.to_string())? {
                println!("slot {}: state {} version 0x{:08X} sha256 {}", s.slot, s.state, s.version, hex(&s.sha256));
            }
        }
        "fw-update" => {
            let path = PathBuf::from(args.0.first().ok_or("fw-update FILE")?);
            let version = args.value("--version").and_then(|v| parse_u32(&v)).ok_or("--version 0xMMmmrrrr")?;
            let mut src = FileSource::open(&path).map_err(|e| e.to_string())?;
            let (size, sha256) = src.measure().map_err(|e| e.to_string())?;
            let image = FwImage { size, sha256, version, format: 1, flags: 0 };
            let t = Instant::now();
            let slot = hub.fw_update(&image, &mut src).map_err(|e| e.to_string())?;
            println!("staged {size} B in slot {slot} in {:?}; trial boot", t.elapsed());
            if HOLD_TRIAL.load(std::sync::atomic::Ordering::Relaxed) {
                let trial = hub.wait_status(Duration::from_secs(120), |s| {
                    s.slots.iter().any(|x| x.slot == slot && x.state == hub_ncp::fw::slot_state::RUNNING_TRIAL)
                });
                println!("trial image running in slot {slot}, not confirmed: {trial}");
                for x in hub.status().slots {
                    println!("slot {}: state {} version 0x{:08X}", x.slot, x.state, x.version);
                }
                return Ok(());
            }
            let done = hub.wait_status(Duration::from_secs(120), |s| {
                !matches!(s.fw, hub_ncp::FwState::Activated { .. } | hub_ncp::FwState::Staging { .. })
            });
            println!("result: {:?}{}", hub.status().fw, if done { "" } else { " (timeout)" });
        }
        "fw-confirm" => {
            hub.fw_confirm().map_err(|e| e.to_string())?;
            println!("confirmed");
        }
        "reset" => {
            let hard = args.flag("--hard");
            let before = hub.status().resumes;
            hub.reset_ncp(hard).map_err(|e| e.to_string())?;
            let resumed = hub.wait_status(Duration::from_secs(20), |s| s.resumes > before);
            println!("NCP reset ({}); resumed: {resumed}", if hard { "EN" } else { "SYS_RESET" });
            let _ = hub.client().call_empty(ids::SYS_GET_STATUS, CallOpts::default());
        }
        "serve" => {
            let addr = args.value("--listen").unwrap_or_else(|| "127.0.0.1:8099".into());
            let token = args.value("--token").or_else(|| std::env::var("HUB_TOKEN").ok()).unwrap_or_else(|| {
                let mut b = [0u8; 12];
                urandom()(&mut b);
                hex(&b).to_lowercase()
            });
            let until = args.value("--for").and_then(|s| s.parse().ok()).map(|s: u64| Instant::now() + Duration::from_secs(s));
            let cfg = hub_app::AppConfig { hub_name: "hubctl".into(), api_token: token.clone(), ..Default::default() };
            let app = hub_app::App::start(hub.clone(), cfg);
            println!("token: {token}");
            let result = serve::serve(&app, &addr, until).map_err(|e| format!("{addr}: {e}"));
            let st = app.status();
            app.shutdown();
            println!(
                "app: {} devices ({} restored, {} imported from the NCP), registry writes {} errors {}, APS accepted {} busy {} failed {}",
                st.devices, st.restored, st.imported, st.registry_writes, st.registry_errors, st.aps.accepted, st.aps.busy, st.aps.failed
            );
            result?;
        }
        other => return Err(format!("unknown command {other:?}\n{USAGE}")),
    }
    Ok(())
}

fn main() -> ExitCode {
    let mut args = Args(std::env::args().skip(1).collect());
    if args.flag("-h") || args.flag("--help") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let level = if args.flag("-vv") {
        log::LevelFilter::Trace
    } else if args.flag("-v") {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    };
    let _ = log::set_logger(Box::leak(Box::new(Logger(Instant::now(), level))));
    log::set_max_level(level);

    if args.0.first().map(String::as_str) == Some("catalog-verify") {
        let mut ok = true;
        for (slot, kid) in hub_app::catalog::key_ids() {
            println!("trusted key slot {slot}: kid {}", hex(&kid).to_lowercase());
        }
        for path in &args.0[1..] {
            let bytes = match std::fs::read(path) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("{path}: {e}");
                    ok = false;
                    continue;
                }
            };
            let result = if bytes.starts_with(b"ZDB1") {
                hub_app::catalog::verify_bank(&bytes).map(|_| "bank: body hash and signature ok".to_string()).map_err(|e| format!("{e:?}"))
            } else {
                hub_app::catalog::verify_manifest(&bytes)
                    .map(|m| format!("manifest {}.{}: {} banks, signature ok", m.catalog_date, m.catalog_serial, m.banks.len()))
                    .map_err(|e| format!("{e:?}"))
            };
            match result {
                Ok(m) => println!("{path}: {m}"),
                Err(e) => {
                    println!("{path}: REJECTED: {e}");
                    ok = false;
                }
            }
        }
        return if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE };
    }
    if args.0.first().map(String::as_str) == Some("ws-call") {
        args.0.remove(0);
        let token = args.value("--token").or_else(|| std::env::var("HUB_TOKEN").ok()).unwrap_or_default();
        let listen = Duration::from_secs(args.value("--listen").and_then(|s| s.parse().ok()).unwrap_or(0));
        let Some(url) = (!args.0.is_empty()).then(|| args.0.remove(0)) else {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        };
        return match serve::ws_call(&url, &token, &args.0, listen) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }

    let mut cfg = HubConfig::default();
    cfg.driver.host_name = "hubctl".into();
    if args.flag("--read-only") {
        cfg.resume = ResumePolicy::read_only();
    }
    cfg.resume.hold_trial = args.flag("--hold-trial");
    HOLD_TRIAL.store(cfg.resume.hold_trial, std::sync::atomic::Ordering::Relaxed);
    let store = match args.value("--store") {
        Some(p) => match FileFlash::open(p.as_ref(), JOURNAL_BYTES / SECTOR, SECTOR).and_then(|f| Store::open(Box::new(f))) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{p}: {e}");
                return ExitCode::from(2);
            }
        },
        None => Store::volatile(),
    };
    let sim = args.flag("--sim");
    let serial = args.value("--serial");
    let baud = args.value("--baud").and_then(|b| b.parse().ok()).unwrap_or(921_600u32);
    let mut sim_ncp = None;
    let hub = if sim {
        let (host, ncp) = pipe::pair();
        sim_ncp = Some(SimNcp::start(ncp));
        let (rx, tx) = host.split();
        cfg.driver.lines = vec![Line::BR_DEFAULT];
        Hub::start(cfg, rx, tx, None, store, urandom())
    } else if let Some(path) = serial {
        let port = match serialport::new(&path, baud).timeout(Duration::from_millis(50)).open() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{path}: {e}");
                return ExitCode::from(2);
            }
        };
        let mut port = port;
        let _ = port.write_data_terminal_ready(true);
        let _ = port.write_request_to_send(true);
        let (Ok(rx), Ok(lines)) = (port.try_clone(), port.try_clone()) else {
            eprintln!("{path}: cannot clone the port");
            return ExitCode::from(2);
        };
        // The desktop side sets the rate only; the bridge owns RTS/CTS.
        cfg.driver.lines = vec![Line { baud, flow: false }, Line { baud: 460_800, flow: false }];
        cfg.driver.lines.dedup();
        Hub::start(cfg, SerialRx(rx), SerialTx(port), Some(Box::new(BridgeLines(lines))), store, urandom())
    } else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };
    let result = run(&hub, args);
    let c = hub.client().counters();
    log::info!(
        "host link: tx {} retx {} rx {} bad {} dup {} rto {}",
        c.tx_frames,
        c.tx_retransmissions,
        c.rx_frames,
        c.rx_bad_frame,
        c.rx_duplicate,
        c.tx_rto_expiries
    );
    hub.shutdown();
    let _ = hub.client().wait_state(Duration::from_secs(1), |s| s == SessionState::Stopped);
    if let Some(s) = sim_ncp {
        s.stop();
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
