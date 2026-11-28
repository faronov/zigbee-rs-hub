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

mod import;
mod ota;
mod platform;
mod push;
mod serve;
mod simdev;
mod simdev_z2m;

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
usage: hubctl (--serial PATH [--baud N] | --sim [--sim-demo] [--sim-devices | --sim-z2m-devices | --sim-ts011f] [--sim-ota] [--sim-unsupported ID,…] [--sim-crash]) [--store FILE] [--ota-store FILE] [--read-only] [--hold-trial] [-v|-vv] COMMAND
       hubctl --backend ezsp --sim [--sim-devices] [--sim-crash] [--store FILE] [--ota-store FILE] [-v|-vv] serve …
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
  serve [--listen ADDR] [--token T] [--for SECS] [--wifi-channel N]
                [--hubdata FILE] [--catalog-key HEX]
                            the hub application (device engine, registry in the
                            store) with its WebSocket JSON API and the web UI
                            (default 127.0.0.1:8099, token from HUB_TOKEN or
                            random); --wifi-channel: the Wi-Fi channel the
                            energy-scan advice avoids; the catalog store in a
                            1 MiB hubdata image (FILE, or RAM); an extra
                            trusted signing key (raw Ed25519, hex);
                            --ota-store FILE: the OTA images' area (512 KiB,
                            RAM without it)
                [--mqtt HOST[:PORT]] [--mqtt-base TOPIC] [--mqtt-discovery]
                [--mqtt-user U --mqtt-password P] [--mqtt-settings FILE]
                            the Zigbee2MQTT-compatible MQTT bridge (plain
                            MQTT over TCP): --mqtt enables it with this
                            broker; settings persist in FILE (default RAM)
                            and can be changed over the API (mqtt/config/set)
coordinator backend:
  --backend zrhp|ezsp       zrhp (default): the ZRHP host core; ezsp: the hub app
                            on the EZSP backend, with --sim its simulated
                            EmberZNet NCP (EZSP 13, a network already formed);
                            serve only, --sim-devices as below
simulated NCP:
  --sim-demo                form on channel 15 and add a small mesh (routers
                            answering Mgmt_Lqi/Mgmt_Rtg) for the network map
  --sim-devices             two On/Off plugs and a temperature sensor that
                            answer the hub (interview, reads, commands; the
                            sensor reports every 15 s), e.g. for automations;
                            serve adds the requests sim/join {device, how:
                            association|secured|tc, nwk, denied} or {partial:
                            true} (a new plug whose interview ends partial),
                            sim/announce {device, nwk}, sim/leave {device,
                            rejoin} (both backends)
  --sim-z2m-devices         instead: form on channel 15 and join four simulated
                            devices (sensor, light, plug, remote) that answer
                            the interview and commands and report; the remote
                            presses a button every 10 s (Zigbee2MQTT names,
                            for the MQTT bridge)
  --sim-ts011f              --sim-z2m-devices and a Tuya TS011F plug
                            (_TZ3000_gjnozsaz, metering) whose definition is
                            in the catalog's shards only (catalog-push)
  --sim-ota                 a device with an OTA client (0x6D6B/0x0102,
                            version 0x00010000) joins and queries
  --sim-unsupported IDS     answer these message IDs UNSUPPORTED_COMMAND, e.g.
                            0x0106,0x0108 (no energy scan / TX power, like the
                            current NCP image)
  --sim-crash               hub/system reports a crashed previous boot (reset
                            reason 4, panic, crash streak 1)
also, without a hub:
  hubctl ws-call URL --token T [--listen SECS] JSON...
                            authenticate, send requests, print the frames
  hubctl mqtt-broker [--listen ADDR] [--user U --password P]
                            a small MQTT broker for local tests (default
                            127.0.0.1:1883; not for production)
    hubctl catalog-push URL --token T [--from BASE_URL | --dir DIR] [--no-fetch]
                            upload the signed catalog to the hub: manifest,
                            then the bank and web tools it wants, then the
                            shards of devices that resolve to generic only
                            (from the hub's mirror setting, another URL or a
                            release directory)
  hubctl ota-upload URL --token T FILE [--name N] [--min-file-version V]
                 [--max-file-version V] [--replace]
                            upload a Zigbee OTA file to the hub (4 KiB chunks;
                            run again to continue an interrupted upload)
  hubctl ota-stream URL --token T FILE [--device ID]
                            serve a Zigbee OTA file on demand (no storage on the
                            hub); with --device start that device's upgrade and
                            exit when it runs the new version
  hubctl ota-make IMAGE OUT --manufacturer 0xMMMM --image-type 0xTTTT
                 --version 0xVVVVVVVV [--label TEXT] [--hw MIN-MAX]
                            wrap a firmware image into a Zigbee OTA file
  hubctl catalog-verify FILE...
                            check catalog banks (.zdb) and manifest.cose with
                            the firmware's trusted keys
  hubctl import z2m DIR|FILE... [--url URL --token T] [--out FILE] [options]
  hubctl import zha ZIGBEE_DB [--backup FILE] [--ha-storage DIR] [--url …] [options]
  hubctl import doc FILE [--url …] [options]
                            migrate from Zigbee2MQTT (its data directory or
                            database.db, coordinator_backup.json,
                            configuration.yaml/devices.yaml) or ZHA (zigbee.db,
                            the newest network backup in it or --backup, names
                            and areas from Home Assistant's .storage) without
                            pairing again (docs/migration.md): the import
                            document to stdout or --out; with --url the hub's
                            preview, with --apply the import; options --force
                            (replace the running network), --confirm (re-tag
                            another network's records), --confirm-ieee-write-once
                            (EZSP), --keep-ieee, --skip-backup, --remove-missing,
                            --reconfigure auto|always|never, --json
";

/// `serve`'s MQTT bridge settings: `--mqtt-settings FILE` (else RAM), with
/// the command-line overrides applied and stored.
fn mqtt_settings(args: &mut Args) -> Result<Arc<dyn hub_mqtt::SettingsStore>, String> {
    let store: Arc<dyn hub_mqtt::SettingsStore> = match args.value("--mqtt-settings") {
        Some(p) => Arc::new(hub_mqtt::FileStore(p.into())),
        None => Arc::new(hub_mqtt::MemoryStore::default()),
    };
    let mut o = serde_json::Map::new();
    if let Some(hp) = args.value("--mqtt") {
        let (host, port) = match hp.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse::<u16>().map_err(|_| format!("--mqtt {hp}: bad port"))?),
            None => (hp.clone(), 1883),
        };
        o.insert("enabled".into(), true.into());
        o.insert("host".into(), host.into());
        o.insert("port".into(), port.into());
    }
    if let Some(b) = args.value("--mqtt-base") {
        o.insert("base_topic".into(), b.into());
    }
    if args.flag("--mqtt-discovery") {
        o.insert("discovery".into(), true.into());
    }
    if let Some(u) = args.value("--mqtt-user") {
        o.insert("username".into(), u.into());
    }
    if let Some(p) = args.value("--mqtt-password") {
        o.insert("password".into(), p.into());
    }
    if !o.is_empty() {
        let mut c = hub_mqtt::MqttConfig::from_store(store.load().as_deref());
        c.apply(&o, true).map_err(|e| format!("--mqtt…: {e}"))?;
        store.save(&c.to_store())?;
    }
    Ok(store)
}

/// `serve`'s catalog store: the hubdata image.
fn catalog_setup(args: &mut Args) -> Result<hub_app::CatalogSetup, String> {
    use hub_app::catalog::data::{DataFlash, MemData};
    const HUBDATA: u32 = 1 << 20;
    let flash: Box<dyn DataFlash> = match args.value("--hubdata") {
        Some(p) => Box::new(MemData::file(std::path::Path::new(&p), HUBDATA).map_err(|e| format!("{p}: {e}"))?),
        None => Box::new(MemData::new(HUBDATA)),
    };
    
    let extra_keys = match args.value("--catalog-key") {
        Some(h) => {
            let b = unhex(&h).filter(|b| b.len() == 32).ok_or("--catalog-key: 64 hex digits")?;
            vec![b.try_into().unwrap()]
        }
        None => vec![],
    };
        Ok(hub_app::CatalogSetup { flash: std::sync::Arc::new(std::sync::Mutex::new(flash)), extra_keys })
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    (0..s.len() / 2).map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()).collect()
}

struct Logger(Instant, log::LevelFilter);

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata<'_>) -> bool {
        m.level() <= self.1
    }
    fn log(&self, r: &log::Record<'_>) {
        if self.enabled(r.metadata()) {
            let t = self.0.elapsed().as_secs_f64();
            eprintln!("{t:>8.3} {:5} {}", r.level(), r.args());
            if r.level() <= log::Level::Info {
                platform::LOGS.push(&format!("{t:.3} {} {}", r.level(), r.args()));
            }
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
        self.0
            .clear(serialport::ClearBuffer::Input)
            .map_err(io::Error::other)
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
        self.0
            .write_data_terminal_ready(dtr)
            .map_err(io::Error::other)?;
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
        println!(
            "frame counters: nwk {}/{} aps {}/{} stride {}",
            c.nwk_current, c.nwk_limit, c.aps_current, c.aps_limit, c.stride
        );
    }
    println!(
        "resumes {} (last {:?}), NCP restarts {}, fw {:?}, devices {}, store {} ({:?})",
        s.resumes,
        s.last_resume,
        s.ncp_restarts,
        s.fw,
        s.devices,
        if s.store_volatile {
            "volatile"
        } else {
            "persistent"
        },
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
    let rsp = hub
        .client()
        .call(&SysEcho { data: Rest(&data) }, CallOpts::default())?;
    if rsp.payload.get(1..) != Some(&data[..]) {
        return Err(hub_ncp::driver::Error::Other("echo mismatch".into()));
    }
    Ok(t.elapsed())
}

static HOLD_TRIAL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static OTA_STORE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
/// `--sim-crash`: hub/system reports a crashed previous boot.
static SIM_CRASH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `serve`'s OTA area: `--ota-store FILE`, or RAM (512 KiB).
fn ota_area() -> hub_app::OtaFlash {
    const SECTORS: u32 = 128;
    let flash: Box<dyn hub_ncp::store::Flash> = match OTA_STORE.get().cloned().flatten() {
        Some(p) => match FileFlash::open(p.as_ref(), SECTORS, SECTOR) {
            Ok(f) => Box::new(f),
            Err(e) => {
                log::warn!("{p}: {e}; OTA images in RAM");
                Box::new(hub_ncp::store::MemFlash::new(SECTORS, SECTOR))
            }
        },
        None => Box::new(hub_ncp::store::MemFlash::new(SECTORS, SECTOR)),
    };
    hub_app::OtaFlash::new(flash)
}

fn run(hub: &Hub, mut args: Args) -> Result<(), String> {
    let cmd = if args.0.is_empty() {
        "status".to_string()
    } else {
        args.0.remove(0)
    };
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
            // Whose the hub's device registry and other network records are.
            println!("registry: {:?}", hub.registry());
            if !ready {
                return Err("no session".into());
            }
        }
        "echo" => {
            let n = args.0.first().and_then(|s| s.parse().ok()).unwrap_or(10u32);
            let mut worst = Duration::ZERO;
            let mut total = Duration::ZERO;
            hub.client().take_latency();
            for i in 0..n {
                let d = echo_round(hub, i).map_err(|e| e.to_string())?;
                worst = worst.max(d);
                total += d;
            }
            println!(
                "echo x{n}: mean {:?} worst {worst:?}; {}",
                total / n.max(1),
                hub.client().take_latency()
            );
        }
        "soak" => {
            let secs: u64 = args.0.first().and_then(|s| s.parse().ok()).unwrap_or(30);
            let until = Instant::now() + Duration::from_secs(secs);
            let (mut ok, mut failed, mut interrupted, mut worst) =
                (0u64, 0u64, 0u64, Duration::ZERO);
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
            println!(
                "formed PAN 0x{:04X} EPID {} channel {}",
                n.pan_id,
                hex(&n.ext_pan_id),
                n.channel
            );
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
                println!(
                    "slot {}: state {} version 0x{:08X} sha256 {}",
                    s.slot,
                    s.state,
                    s.version,
                    hex(&s.sha256)
                );
            }
        }
        "fw-update" => {
            let path = PathBuf::from(args.0.first().ok_or("fw-update FILE")?);
            let version = args
                .value("--version")
                .and_then(|v| parse_u32(&v))
                .ok_or("--version 0xMMmmrrrr")?;
            let mut src = FileSource::open(&path).map_err(|e| e.to_string())?;
            let (size, sha256) = src.measure().map_err(|e| e.to_string())?;
            let image = FwImage {
                size,
                sha256,
                version,
                format: 1,
                flags: 0,
            };
            let t = Instant::now();
            let slot = hub.fw_update(&image, &mut src).map_err(|e| e.to_string())?;
            println!(
                "staged {size} B in slot {slot} in {:?}; trial boot",
                t.elapsed()
            );
            if HOLD_TRIAL.load(std::sync::atomic::Ordering::Relaxed) {
                let trial = hub.wait_status(Duration::from_secs(120), |s| {
                    s.slots.iter().any(|x| {
                        x.slot == slot && x.state == hub_ncp::fw::slot_state::RUNNING_TRIAL
                    })
                });
                println!("trial image running in slot {slot}, not confirmed: {trial}");
                for x in hub.status().slots {
                    println!(
                        "slot {}: state {} version 0x{:08X}",
                        x.slot, x.state, x.version
                    );
                }
                return Ok(());
            }
            let done = hub.wait_status(Duration::from_secs(120), |s| {
                !matches!(
                    s.fw,
                    hub_ncp::FwState::Activated { .. } | hub_ncp::FwState::Staging { .. }
                )
            });
            println!(
                "result: {:?}{}",
                hub.status().fw,
                if done { "" } else { " (timeout)" }
            );
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
            println!(
                "NCP reset ({}); resumed: {resumed}",
                if hard { "EN" } else { "SYS_RESET" }
            );
            let _ = hub
                .client()
                .call_empty(ids::SYS_GET_STATUS, CallOpts::default());
        }
        "serve" => serve_app(&mut args, hub_app::Backend::zrhp(hub.clone()))?,
        other => return Err(format!("unknown command {other:?}\n{USAGE}")),
    }
    Ok(())
}

/// `serve`: the hub application on `backend` with its WebSocket API, the
/// web UI and the MQTT bridge.
fn serve_app(args: &mut Args, backend: hub_app::Backend) -> Result<(), String> {
    let addr = args
        .value("--listen")
        .unwrap_or_else(|| "127.0.0.1:8099".into());
    let token = args
        .value("--token")
        .or_else(|| std::env::var("HUB_TOKEN").ok())
        .unwrap_or_else(|| {
            let mut b = [0u8; 12];
            urandom()(&mut b);
            hex(&b).to_lowercase()
        });
    let until = args
        .value("--for")
        .and_then(|s| s.parse().ok())
        .map(|s: u64| Instant::now() + Duration::from_secs(s));
    let wifi_channel = args.value("--wifi-channel").and_then(|c| c.parse().ok());
    let desktop = platform::Desktop {
        started: Instant::now(),
        rng: urandom(),
        wifi_channel,
        wifi: Default::default(),
        crashed: SIM_CRASH.load(std::sync::atomic::Ordering::Relaxed),
    };
    let cfg = hub_app::AppConfig {
        hub_name: "hubctl".into(),
        api_token: token.clone(),
        platform: Some(Arc::new(desktop)),
        ..Default::default()
    };
    let catalog = catalog_setup(args)?;
    let mqtt_store = mqtt_settings(args)?;
    let app = hub_app::App::start_backend(backend, hub_app::AppConfig { ota_flash: ota_area(), ..cfg }, Some(catalog));
    let transport: hub_mqtt::TransportFactory =
        Arc::new(|| Box::new(hub_mqtt::tcp::TcpTransport::new()) as Box<dyn hub_mqtt::Transport>);
    let mqtt = hub_mqtt::MqttService::start(&app, mqtt_store, transport, hub_mqtt::ServiceConfig::default());
    if let Some(h) = simdev::SimHook::new() {
        app.add_hook(Box::new(h));
    }
    println!("token: {token}");
    let result = serve::serve(&app, &addr, until).map_err(|e| format!("{addr}: {e}"));
    mqtt.stop();
    let st = app.status();
    app.shutdown();
    println!(
        "app: {} devices ({} restored, {} imported from the NCP), registry writes {} errors {}, APS accepted {} busy {} failed {}",
        st.devices,
        st.restored,
        st.imported,
        st.registry_writes,
        st.registry_errors,
        st.aps.accepted,
        st.aps.busy,
        st.aps.failed
    );
    result
}

/// `--backend ezsp`: the hub application on the EZSP backend and its
/// simulated EmberZNet NCP (EZSP 13, EmberZNet 7.4.5 as on the SONOFF
/// Dongle Max), which runs a network already. Only `serve`.
fn run_ezsp(store: Store, sim: bool, sim_devices: bool, mut args: Args) -> Result<(), String> {
    use hub_ezsp::sim::{Faults, Sim, SimConfig};
    if !sim {
        return Err("--backend ezsp needs --sim: a serial EmberZNet NCP is not supported by hubctl yet".into());
    }
    if args.0.first().map(String::as_str) != Some("serve") {
        return Err(format!("--backend ezsp: only the serve command\n{USAGE}"));
    }
    args.0.remove(0);
    let cfg = SimConfig {
        ezsp_version: 13,
        stack_version: hub_ezsp::ezsp::StackVersion(0x7450),
        version_info: [0, 0, 7, 4, 5, 0, 0xAA],
        ..SimConfig::default()
    };
    let (sim, transport, clock) = Sim::new(cfg, Faults::none());
    // Virtual time follows real time (the app's ticks and timeouts are real).
    sim.set_pace(Some(1));
    if sim_devices {
        simdev::start_ezsp(&sim);
    }
    let backend = hub_app::Backend::ezsp(transport, clock, hub_ezsp::coord::EzspOptions::default(), store);
    serve_app(&mut args, backend)
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

    if args.0.first().map(String::as_str) == Some("mqtt-broker") {
        let addr = args.value("--listen").unwrap_or_else(|| "127.0.0.1:1883".into());
        let auth = args.value("--user").map(|u| (u, args.value("--password").unwrap_or_default()));
        return match hub_mqtt::broker::Broker::start(&addr, auth) {
            Ok(b) => {
                println!("mqtt-broker: listening on {}", b.addr());
                let mut seen = 0;
                loop {
                    std::thread::sleep(Duration::from_millis(200));
                    let log = b.published();
                    for (t, p, r) in &log[seen..] {
                        log::debug!("{}{t} {}", if *r { "(retained) " } else { "" }, String::from_utf8_lossy(p));
                    }
                    seen = log.len();
                }
            }
            Err(e) => {
                eprintln!("error: {addr}: {e}");
                ExitCode::FAILURE
            }
        };
    }
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
                hub_app::catalog::verify_bank(&bytes)
                    .map(|_| "bank: body hash and signature ok".to_string())
                    .map_err(|e| format!("{e:?}"))
            } else {
                hub_app::catalog::verify_manifest(&bytes)
                    .map(|m| {
                        format!(
                            "manifest {}.{}: {} banks, signature ok",
                            m.catalog_date,
                            m.catalog_serial,
                            m.banks.len()
                        )
                    })
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
        return if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
        if args.0.first().map(String::as_str) == Some("catalog-push") {
        args.0.remove(0);
        let token = args.value("--token").or_else(|| std::env::var("HUB_TOKEN").ok()).unwrap_or_default();
        let source = match (args.value("--from"), args.value("--dir")) {
            (_, Some(d)) => Some(push::Source::Dir(d.into())),
            (Some(u), None) => Some(push::Source::Url(u)),
            _ => None,
        };
        let fetch = !args.flag("--no-fetch");
        let Some(url) = (!args.0.is_empty()).then(|| args.0.remove(0)) else {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        };
        return match push::push(&url, &token, source, fetch) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }
    if args.0.first().map(String::as_str) == Some("ota-upload") {
        args.0.remove(0);
        let token = args.value("--token").or_else(|| std::env::var("HUB_TOKEN").ok()).unwrap_or_default();
        let name = args.value("--name");
        let min = args.value("--min-file-version").and_then(|v| parse_u32(&v));
        let max = args.value("--max-file-version").and_then(|v| parse_u32(&v));
        let replace = args.flag("--replace");
        let (Some(url), Some(path)) = (args.0.first().cloned(), args.0.get(1).cloned()) else {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        };
        return match ota::upload(&url, &token, &path, name, (min, max), replace) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }
    if args.0.first().map(String::as_str) == Some("ota-stream") {
        args.0.remove(0);
        let token = args.value("--token").or_else(|| std::env::var("HUB_TOKEN").ok()).unwrap_or_default();
        let device = args.value("--device");
        let (Some(url), Some(path)) = (args.0.first().cloned(), args.0.get(1).cloned()) else {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        };
        return match ota::stream(&url, &token, &path, device) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }
    if args.0.first().map(String::as_str) == Some("ota-make") {
        args.0.remove(0);
        let num = |a: &mut Args, k: &str| a.value(k).and_then(|v| parse_u32(&v));
        let (m, t, v) = (num(&mut args, "--manufacturer"), num(&mut args, "--image-type"), num(&mut args, "--version"));
        let label = args.value("--label").unwrap_or_default();
        let hw = args.value("--hw").and_then(|r| {
            let (a, b) = r.split_once('-')?;
            Some((parse_u32(a)? as u16, parse_u32(b)? as u16))
        });
        let (Some(m), Some(t), Some(v), Some(input), Some(out)) = (m, t, v, args.0.first().cloned(), args.0.get(1).cloned()) else {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        };
        let image = match std::fs::read(&input) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("{input}: {e}");
                return ExitCode::FAILURE;
            }
        };
        let f = ota::make(&image, m as u16, t as u16, v, &label, hw);
        if let Err(e) = std::fs::write(&out, &f) {
            eprintln!("{out}: {e}");
            return ExitCode::FAILURE;
        }
        println!("{out}: {} B, manufacturer 0x{m:04X} image type 0x{t:04X} file version 0x{v:08X}", f.len());
        return ExitCode::SUCCESS;
    }
    if args.0.first().map(String::as_str) == Some("import") {
        args.0.remove(0);
        return match import::main(std::mem::take(&mut args.0)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }
    if args.0.first().map(String::as_str) == Some("ws-call") {
        args.0.remove(0);
        let token = args
            .value("--token")
            .or_else(|| std::env::var("HUB_TOKEN").ok())
            .unwrap_or_default();
        let listen = Duration::from_secs(
            args.value("--listen")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
        );
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
        Some(p) => match FileFlash::open(p.as_ref(), JOURNAL_BYTES / SECTOR, SECTOR)
            .and_then(|f| Store::open(Box::new(f)))
        {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{p}: {e}");
                return ExitCode::from(2);
            }
        },
        None => Store::volatile(),
    };
    let sim = args.flag("--sim");
    let demo = args.flag("--sim-demo");
    let sim_devices = args.flag("--sim-devices");
    let sim_ts011f = args.flag("--sim-ts011f");
    let sim_z2m_devices = args.flag("--sim-z2m-devices") || sim_ts011f;
    let sim_ota = args.flag("--sim-ota");
    SIM_CRASH.store(args.flag("--sim-crash"), std::sync::atomic::Ordering::Relaxed);
    let _ = OTA_STORE.set(args.value("--ota-store"));
    match args.value("--backend").as_deref() {
        None | Some("zrhp") => {}
        Some("ezsp") => {
            if demo || sim_z2m_devices || sim_ota || args.0.iter().any(|a| a == "--sim-unsupported") {
                eprintln!("--backend ezsp: --sim-demo, --sim-z2m-devices, --sim-ota and --sim-unsupported are ZRHP simulator options");
                return ExitCode::from(2);
            }
            return match run_ezsp(store, sim, sim_devices, args) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Some(other) => {
            eprintln!("--backend {other}: zrhp or ezsp");
            return ExitCode::from(2);
        }
    }
    let serial = args.value("--serial");
    let baud = args
        .value("--baud")
        .and_then(|b| b.parse().ok())
        .unwrap_or(921_600u32);
    let mut sim_ncp = None;
    let hub = if sim {
        let (host, ncp) = pipe::pair();
        let s = SimNcp::start(ncp);
        if let Some(list) = args.value("--sim-unsupported") {
            let ids: Vec<u16> = list
                .split(',')
                .filter_map(|x| parse_u32(x.trim()))
                .map(|x| x as u16)
                .collect();
            s.with(|st| st.unsupported = ids);
        }
        if sim_ota {
            ota::sim_device(&s);
        }
        sim_ncp = Some(s);
        let (rx, tx) = host.split();
        cfg.driver.lines = vec![Line::BR_DEFAULT];
        Hub::start(cfg, rx, tx, None, store, urandom())
    } else if let Some(path) = serial {
        let port = match serialport::new(&path, baud)
            .timeout(Duration::from_millis(50))
            .open()
        {
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
        cfg.driver.lines = vec![
            Line { baud, flow: false },
            Line {
                baud: 460_800,
                flow: false,
            },
        ];
        cfg.driver.lines.dedup();
        Hub::start(
            cfg,
            SerialRx(rx),
            SerialTx(port),
            Some(Box::new(BridgeLines(lines))),
            store,
            urandom(),
        )
    } else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };
    let mut result = match sim_ncp.as_ref().filter(|_| demo) {
        Some(sim) => platform::sim_demo(&hub, sim),
        None => Ok(()),
    };
    if let Some(sim) = sim_ncp.as_ref().filter(|_| result.is_ok()) {
        if sim_z2m_devices {
            result = simdev_z2m::start(&hub, sim, sim_ts011f);
        } else if sim_devices {
            result = simdev::start(&hub, sim);
        }
    }
    let result = result.and_then(|()| run(&hub, args));
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
    let _ = hub
        .client()
        .wait_state(Duration::from_secs(1), |s| s == SessionState::Stopped);
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
