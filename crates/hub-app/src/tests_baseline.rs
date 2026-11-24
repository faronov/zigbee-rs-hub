//! Coordinator-boundary baselines (`docs/coordinator-boundary.md` §4, §7
//! PR A): a scripted scenario of the app on the simulated NCP whose wire
//! trace, host store writes and content, and API answers are kept as
//! golden files ([`crate::baseline`]), and the journal format through
//! [`Store`] on its own.
//!
//! Determinism: the hub's RNG is the tests' counter ([`crate::tests::rig`]),
//! the simulated devices answer every request at once, and every step
//! waits for its effects and for a quiet link, then has the app write its
//! registry and last values, before the next one starts.
//! What still depends on the clock is normalized: [`VOLATILE`] in API JSON,
//! [`normalize_kv`] and the registry's `last_seen` in the store.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hub_devices::sim_device::SimDevice;
use hub_devices::transport::{ApsDst as EApsDst, ApsIndication, ApsRequest, TxOptions};
use hub_devices::zcl::{TypedValue, ZclType, ZclValue};
use hub_devices::zdo::LogicalType;
use hub_ncp::sim::{SimNcp, SimRemote};
use hub_store::{
    Change, DeviceRecord, EndpointRecord, FcRecord, Flash, FwPending, HostState, Journal, MemFlash,
    Meta, NetworkRecord, Store,
};
use serde_json::{Value as J, json};
use zigbee_zrhp::codec::{Bytes8, Bytes16, List, Reader, Wire};
use zigbee_zrhp::msg::data::{self as wd, ApsDataRequest, EvtApsDataIndication, indication_flags};
use zigbee_zrhp::msg::gp as wg;
use zigbee_zrhp::msg::ids;
use zigbee_zrhp::msg::network::EvtDeviceJoined;
use zigbee_zrhp::records::{GpSinkRecord, GpTranslation, GpTranslationRecord, Record};
use zigbee_zrhp::types::Ieee;

use crate::baseline::{self, Req, Step};
use crate::tests::{T, call, ok, rig_with, session};
use crate::{App, AppConfig};

/// The wire comparison of the scenario. `Exact` today; a transport that
/// reorders requests across destinations (the asynchronous pump of PR G)
/// switches to `Relaxed` ([`baseline::relaxed_steps`]).
#[derive(PartialEq)]
enum Order {
    Exact,
    #[allow(dead_code)]
    Relaxed,
}
const WIRE_ORDER: Order = Order::Exact;

/// API JSON fields that follow the clock (wall time, durations) or the
/// thread timing (event sequence, host link frame counts, how often the
/// registry coalesced its writes, bytes of last values whose times vary in
/// length).
const VOLATILE: &[&str] = &[
    "/time",
    "/time_utc",
    "/duration_ms",
    "/seq",
    "/metadata/internal/creation_time",
    "/backup/metadata/internal/creation_time",
    "/host_link",
    "/app/registry_writes",
    "/app/last_values/2",
    "/store/journal_live_sectors",
];

/// The combo light (on/off + GP proxy/sink on EP 242, as in hub-devices'
/// GP scenarios), joined and interviewed in the scenario.
const LIGHT: u64 = 0x6d6b_0000_0000_c0b0;
const LIGHT_NWK: u16 = 0x4444;
/// gpsFunctionality of a zigbee-rs combo with a Translation Table (as in
/// hub-devices' GP scenarios): it takes on-device GP bindings.
const COMBO: u64 =
    0x1F | (1 << 7) | (1 << 10) | (1 << 11) | (1 << 13) | (1 << 15) | (1 << 16) | (1 << 18);
/// A plug only in the backup: imported and interviewed after the restore.
pub(crate) const PLUG: u64 = 0x5a52_5300_0000_0001;
pub(crate) const PLUG_NWK: u16 = 0x5A01;
/// The GP switch.
const GPD: u32 = 0x00F4_0002;

fn light() -> SimDevice {
    SimDevice::new(LIGHT, LIGHT_NWK, LogicalType::Router, true, 0x6D6B)
        .endpoint(
            11,
            0x0104,
            0x0100,
            &[0x0000, 0x0003, 0x0004, 0x0005, 0x0006],
            &[],
        )
        .endpoint(242, 0xA1E0, 0x0066, &[0x0021], &[0x0021])
        .basic("zigbee-rs", "combo-light", 0x01)
        .attr(
            11,
            0x0006,
            0x0000,
            TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false))),
        )
        // gpsMaxSinkTableEntries, gpsFunctionality.
        .attr(242, 0x0021, 0x0000, TypedValue::uint(ZclType::U8, 16))
        .attr(242, 0x0021, 0x0006, TypedValue::uint(ZclType::MAP24, COMBO))
}

pub(crate) fn plug() -> SimDevice {
    SimDevice::new(PLUG, PLUG_NWK, LogicalType::Router, true, 0x1234)
        .endpoint(1, 0x0104, 0x0051, &[0x0000, 0x0003, 0x0006], &[])
        .basic("zigbee-rs", "SIM-PLUG", 1)
        .attr(
            1,
            0x0006,
            0x0000,
            TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false))),
        )
}

/// Journal records as programmed: `(type, payload)`.
pub(crate) type Records = Vec<(u8, Vec<u8>)>;

/// A host store region, readable after the scenario, that also logs every
/// record the journal programs.
#[derive(Clone)]
pub(crate) struct SharedMem {
    pub(crate) flash: Arc<Mutex<MemFlash>>,
    log: Arc<Mutex<Records>>,
}

impl SharedMem {
    pub(crate) fn new(sectors: u32, sector_size: u32) -> Self {
        SharedMem {
            flash: Arc::new(Mutex::new(MemFlash::new(sectors, sector_size))),
            log: Arc::default(),
        }
    }

    pub(crate) fn bytes(&self) -> Vec<u8> {
        self.flash.lock().unwrap().bytes().to_vec()
    }

    /// The records programmed since the last call.
    pub(crate) fn take_log(&self) -> Records {
        std::mem::take(&mut self.log.lock().unwrap())
    }
}

impl Flash for SharedMem {
    fn sector_size(&self) -> u32 {
        self.flash.lock().unwrap().sector_size()
    }
    fn sectors(&self) -> u32 {
        self.flash.lock().unwrap().sectors()
    }
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> std::io::Result<()> {
        self.flash.lock().unwrap().read(offset, buf)
    }
    fn write(&mut self, offset: u32, data: &[u8]) -> std::io::Result<()> {
        // A record is programmed whole with its commit byte erased (`u16
        // len | u8 type | 0xFF | payload | crc32c | pad`), then committed
        // by a one-byte write; sector headers start with the magic.
        if data.len() >= 8 && data[3] == 0xFF && &data[..4] != b"ZHJ1" {
            let len = usize::from(u16::from_le_bytes([data[0], data[1]]));
            if let Some(p) = data.get(4..4 + len) {
                self.log.lock().unwrap().push((data[2], p.to_vec()));
            }
        }
        self.flash.lock().unwrap().write(offset, data)
    }
    fn erase(&mut self, sector: u32) -> std::io::Result<()> {
        self.flash.lock().unwrap().erase(sector)
    }
}

/// One journal record as a line: a key/value part as key, part and length
/// (`verbose`: and the value), normalized ([`normalize_kv`]); a registry
/// device without `last_seen`.
fn describe(t: u8, p: &[u8], verbose: bool) -> String {
    let key = |p: &[u8]| {
        let n = usize::from(p[0]);
        (String::from_utf8_lossy(&p[1..1 + n]).into_owned(), 1 + n)
    };
    match t {
        0x14 => format!(
            "device {} {}",
            baseline::hex(&p[..8]),
            baseline::hex(&p[8..12])
        ),
        0x20 => {
            let (k, at) = key(p);
            let part = u16::from_le_bytes([p[at], p[at + 1]]);
            let parts = u16::from_le_bytes([p[at + 2], p[at + 3]]);
            let data = normalize_kv(&k, &p[at + 4..]);
            let mut line = format!("kv_put {k} {part}/{parts} {} B", data.len());
            if verbose {
                line.push(' ');
                line += &std::str::from_utf8(&data)
                    .map_or_else(|_| baseline::hex(&data), str::to_string);
            }
            line
        }
        0x21 => format!("kv_del {}", key(p).0),
        _ => {
            let name = match t {
                0x01 => "snap_begin",
                0x02 => "snap_end",
                0x10 => "meta",
                0x11 => "network",
                0x12 => "network_clear",
                0x13 => "fc",
                0x15 => "device_del",
                0x16 => "fw_pending",
                0x17 => "fw_clear",
                0x18 => "endpoint",
                0x19 => "endpoint_del",
                _ => "unknown",
            };
            format!("{name} {}", baseline::hex(p))
                .trim_end()
                .to_string()
        }
    }
}

/// A fixed platform: `hub/system` facts, Wi-Fi channel, features.
struct Platform;

impl crate::Platform for Platform {
    fn system(&self) -> J {
        json!({"version": "baseline", "uptime_s": 12, "heap": {"free": 65000, "min_free": 57000, "largest_block": 31000}})
    }
    fn wifi_channel(&self) -> Option<u8> {
        Some(6)
    }
    fn random(&self, buf: &mut [u8]) {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = 0xA0 + i as u8;
        }
    }
    fn reboot(&self) -> Result<(), String> {
        Ok(())
    }
    fn features(&self) -> Vec<&'static str> {
        vec!["logs"]
    }
}

fn inject(sim: &SimNcp, i: &ApsIndication) {
    let ev = EvtApsDataIndication {
        dst_mode: 0x02,
        dst: 0x0000,
        dst_ep: i.dst_ep,
        src_nwk: i.src_nwk,
        src_ieee: Ieee::from_u64(i.src_ieee.unwrap_or(0)),
        src_ep: i.src_ep,
        profile: i.profile,
        cluster: i.cluster,
        flags: if i.src_ieee.is_some() {
            indication_flags::SRC_IEEE_VALID
        } else {
            0
        },
        lqi: i.lqi,
        rssi: i.rssi,
        last_hop_nwk: i.src_nwk,
        aps_counter: 0,
        key_source: 0,
        rx_time_ms: 0,
        asdu: Bytes16(&i.payload),
    };
    sim.inject_event(
        ids::EVT_APS_DATA_INDICATION,
        hub_ncp::driver::encode_body(&ev).unwrap(),
    );
}

fn event<'a, T: Wire<'a>>(sim: &SimNcp, id: u16, body: &T) {
    sim.inject_event(id, hub_ncp::driver::encode_body(body).unwrap());
}

/// The simulated devices answer the hub's APS requests (and nothing else;
/// the simulated NCP's trace must be on).
pub(crate) struct Air {
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Air {
    pub(crate) fn start(sim: &SimNcp, devs: Arc<Mutex<Vec<SimDevice>>>) -> Air {
        let stop = Arc::new(AtomicBool::new(false));
        let (sim, flag) = (sim.clone(), stop.clone());
        let worker = thread::spawn(move || {
            let mut seen = sim.with(|s| s.trace.as_ref().map_or(0, Vec::len));
            while !flag.load(Ordering::SeqCst) {
                let new: Vec<(u16, Vec<u8>)> =
                    sim.with(|s| s.trace.as_ref().map_or(Vec::new(), |t| t[seen..].to_vec()));
                seen += new.len();
                for (_, raw) in new.iter().filter(|(id, _)| *id == ids::APS_DATA_REQUEST) {
                    let r = ApsDataRequest::read(&mut Reader::new(raw)).unwrap();
                    let mut devs = devs.lock().unwrap();
                    let Some(d) = devs.iter_mut().find(|d| match r.dst {
                        wd::ApsDst::Nwk(n) => d.nwk == n,
                        wd::ApsDst::Ieee(i) => d.ieee == i.to_u64(),
                        _ => false,
                    }) else {
                        continue;
                    };
                    let req = ApsRequest {
                        dst: EApsDst::Nwk(d.nwk),
                        dst_ep: r.dst_ep,
                        src_ep: r.src_ep,
                        profile: r.profile,
                        cluster: r.cluster,
                        payload: r.asdu.to_vec(),
                        options: TxOptions::default(),
                    };
                    for i in d.answer(&req) {
                        inject(&sim, &i);
                    }
                }
                thread::sleep(Duration::from_millis(2));
            }
        });
        Air {
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for Air {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

fn until(what: &str, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < T, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Cuts the trace and the host store's records into steps.
struct Recorder {
    sim: SimNcp,
    mem: SharedMem,
    mark: usize,
    steps: Vec<Step>,
    /// Records committed per step, described and sorted.
    writes: Vec<(String, Vec<String>)>,
}

/// No new request for this long ends a step.
const QUIET: Duration = Duration::from_millis(300);

impl Recorder {
    fn trace(&self, from: usize) -> Vec<(u16, Vec<u8>)> {
        self.sim
            .with(|s| s.trace.as_ref().expect("trace on")[from..].to_vec())
    }

    fn len(&self) -> usize {
        self.sim.with(|s| s.trace.as_ref().map_or(0, Vec::len))
    }

    /// Requests of `id` since the step began.
    fn count(&self, id: u16) -> usize {
        self.trace(self.mark)
            .iter()
            .filter(|(m, _)| *m == id)
            .count()
    }

    fn wait_for(&self, id: u16, n: usize) {
        until(&format!("{n}× {}", baseline::msg_name(id)), || {
            self.count(id) >= n
        });
    }

    /// Ends a step once the link is quiet: its requests, and the records
    /// the host store committed, after the app (`api`, if running) wrote
    /// its registry and last values.
    fn end(&mut self, name: &str, api: Option<&mut Api>) {
        let (mut n, mut since) = (self.len(), Instant::now());
        while since.elapsed() < QUIET {
            thread::sleep(Duration::from_millis(20));
            let m = self.len();
            if m != n {
                (n, since) = (m, Instant::now());
            }
        }
        if let Some(api) = api {
            api.flush();
        }
        let reqs: Vec<Req> = self.trace(self.mark)[..n - self.mark]
            .iter()
            .map(|(msg_id, payload)| Req {
                msg_id: *msg_id,
                payload: payload.clone(),
            })
            .collect();
        self.mark = n;
        self.steps.push(Step {
            name: name.into(),
            reqs,
        });
        let mut writes: Vec<String> = self
            .mem
            .take_log()
            .iter()
            .map(|(t, p)| describe(*t, p, false))
            .collect();
        writes.sort();
        self.writes.push((name.into(), writes));
    }
}

fn render_writes(steps: &[(String, Vec<String>)]) -> String {
    let mut s = String::from(
        "# Records the host store committed per scenario step (SharedMem's log of the journal's record writes),\n\
         # sorted within a step (threads interleave). The app writes its registry and last values at the end of\n\
         # every step (hub/reboot). kv_put: key, part/parts, length; device: without last_seen.\n",
    );
    for (name, writes) in steps {
        s += &format!("@ {name}\n");
        for w in writes {
            s += w;
            s.push('\n');
        }
    }
    s
}

fn config() -> AppConfig {
    AppConfig {
        api_token: "secret".into(),
        builtin_catalog: false,
        platform: Some(Arc::new(Platform)),
        // The endpoint with OTA Upgrade (the default with `ota`) in every
        // build: its clusters are a build feature, like `hub/info.ota`.
        endpoint: crate::app::hub_endpoint(true),
        ..AppConfig::default()
    }
}

/// What follows the clock in the host store's JSON values, zeroed: in
/// `st/`, every number of at least 10^9 (Unix times in s or ms: `c`, `r`,
/// `seen`, the `last_seen` entity); in `bkp/last`, `creation_time`.
fn normalize_kv(key: &str, value: &[u8]) -> Vec<u8> {
    if key.starts_with("st/") {
        mask_times(value)
    } else if key == "bkp/last" {
        mask_json(value, "creation_time")
    } else {
        value.to_vec()
    }
}

/// JSON numbers of at least 10^9 outside strings → `0`.
fn mask_times(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let (mut i, mut in_str, mut esc) = (0, false, false);
    while i < text.len() {
        let c = text[i];
        if in_str {
            (in_str, esc) = (esc || c != b'"', !esc && c == b'\\');
        } else if c == b'"' {
            in_str = true;
        } else if c.is_ascii_digit() || c == b'-' {
            let end = text[i..]
                .iter()
                .position(|x| !(x.is_ascii_digit() || b".eE+-".contains(x)))
                .map_or(text.len(), |n| i + n);
            let n: f64 = std::str::from_utf8(&text[i..end])
                .unwrap()
                .parse()
                .unwrap_or(0.0);
            if n.abs() >= 1e9 {
                out.push(b'0');
            } else {
                out.extend_from_slice(&text[i..end]);
            }
            i = end;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// `"key":"…"` → `"key":0` in a JSON text, the rest as written.
fn mask_json(text: &[u8], key: &str) -> Vec<u8> {
    let mut out = String::from_utf8_lossy(text).into_owned();
    let pat = format!("\"{key}\":\"");
    let mut from = 0;
    while let Some(i) = out[from..].find(&pat) {
        let start = from + i + pat.len() - 1;
        let end = out[start + 1..]
            .find('"')
            .map_or(out.len(), |e| start + e + 2);
        out.replace_range(start..end, "0");
        from = start + 1;
    }
    out.into_bytes()
}

/// The store state with what follows the clock zeroed.
fn normalized(state: &HostState) -> HostState {
    let mut s = state.clone();
    for d in s.devices.values_mut() {
        d.last_seen = 0;
    }
    for (k, v) in s.kv.iter_mut() {
        *v = normalize_kv(k, v);
    }
    s
}

/// `state` written through [`Store`] into an empty 32 × 4 KiB journal
/// (typed records with `commit`, key/value records with `kv_put`): the
/// records it programmed and the image.
fn store_golden(state: &HostState) -> (String, String) {
    let mem = SharedMem::new(32, 4096);
    let mut store = Store::open(Box::new(mem.clone())).unwrap();
    for c in state
        .snapshot()
        .into_iter()
        .filter(|c| !matches!(c, Change::KvPut { .. }))
    {
        store.commit(c).unwrap();
    }
    for (k, v) in &state.kv {
        store.kv_put(k, v).unwrap();
    }
    assert_eq!(store.state(), state);
    let mut listing = String::from(
        "# The scenario's host store, normalized (registry last_seen 0, normalize_kv), as Store writes it.\n",
    );
    for (t, p) in mem.take_log() {
        listing += &describe(t, &p, true);
        listing.push('\n');
    }
    (listing, baseline::hexdump(&mem.bytes()))
}

fn gpd_frame(sim: &SimNcp, command_id: u8, counter: u32, result: u8, payload: &[u8]) {
    let ev = wg::EvtGpFrame {
        origin: 2,
        result,
        gpd_id: wg::GpdId::SrcId(GPD),
        device_id: 0x02,
        command_id,
        frame_counter: counter,
        security_level: 0,
        key_type: 0,
        flags: 0,
        proxy_nwk: LIGHT_NWK,
        gpp_gpd_link: 0xC8,
        rssi: -60,
        lqi: 180,
        payload: Bytes8(payload),
    };
    event(sim, ids::EVT_GP_FRAME, &ev);
}

/// An authenticated API session.
struct Api {
    app: App,
    sid: u64,
    rx: std::sync::mpsc::Receiver<String>,
    id: u64,
}

impl Api {
    fn start(r: &crate::tests::Rig) -> Api {
        let app = App::start(r.ncp.clone(), config());
        assert!(app.wait(T, |s| s.endpoint_registered));
        let (sid, rx) = session(&app);
        Api {
            app,
            sid,
            rx,
            id: 0,
        }
    }

    fn call(&mut self, body: J) -> J {
        self.id += 1;
        call(&self.app, self.sid, &self.rx, self.id, body)
    }

    /// A successful result.
    fn ok(&mut self, body: J) -> J {
        let f = self.call(body);
        ok(&f).clone()
    }

    /// The final `job_result` of a background job.
    fn job(&mut self, body: J) -> J {
        let job = self.ok(body)["job"].as_u64().unwrap();
        loop {
            let f: J = serde_json::from_str(&self.rx.recv_timeout(T).unwrap()).unwrap();
            if f["type"] == "job_result" && f["job"] == job && f["partial"] != true {
                return f["result"].clone();
            }
        }
    }

    /// The registry and last values written now, as before a reboot (the
    /// test platform's reboot does nothing).
    fn flush(&mut self) {
        assert_eq!(self.ok(json!({"type": "hub/reboot"}))["rebooting"], true);
    }

    fn interviewed(&mut self, device: &str) -> bool {
        self.call(json!({"type": "devices/get", "device": device}))["result"]["interview"]
            == "complete"
    }
}

/// The ZCL TSN of the last remote GP request (the hub sends the next one
/// only after the answer to this one).
fn gp_tsn(sim: &SimNcp) -> u8 {
    sim.with(|s| s.gp_tsn)
}

/// The scenario's host store (normalized) as goldens `<name>.txt` and
/// `<name>.hex`; the raw journal of the scenario reopens to the same state.
fn store_goldens(
    name: &str,
    mem: &SharedMem,
    state: &HostState,
    goldens: &mut Vec<Result<(), String>>,
) {
    let raw = Journal::open(mem.flash.lock().unwrap().clone()).unwrap();
    assert_eq!(
        raw.state(),
        state,
        "the scenario's journal reopens to the store state"
    );
    let (listing, image) = store_golden(&normalized(state));
    goldens.push(baseline::check(&format!("{name}.txt"), &listing));
    goldens.push(baseline::check(&format!("{name}.hex"), &image));
}

#[test]
fn coordinator_boundary_scenario() {
    let mem = SharedMem::new(32, 4096);
    let r = rig_with(Store::open(Box::new(mem.clone())).unwrap(), |sim| {
        sim.with(|s| s.trace = Some(Vec::new()))
    });
    let mut rec = Recorder {
        sim: r.sim.clone(),
        mem: mem.clone(),
        mark: 0,
        steps: Vec::new(),
        writes: Vec::new(),
    };
    let mut goldens = Vec::new();
    rec.end("boot: session and resume without a network", None);

    let devs = Arc::new(Mutex::new(vec![light(), plug()]));
    let _air = Air::start(&r.sim, devs.clone());
    let mut api = Api::start(&r);
    rec.wait_for(ids::TBL_READ, 1);
    rec.end(
        "app start: endpoint registration, NCP device import",
        Some(&mut api),
    );

    // Form (no network yet: no backup, no leave).
    api.ok(json!({"type": "network/reset", "confirm": "RESET", "channel": 15, "pan_id": 0x1A2B, "ext_pan_id": "00124b0001020304"}));
    rec.wait_for(ids::TBL_READ, 1);
    rec.end("form: network/reset on an empty NCP", Some(&mut api));

    // Resume after an NCP restart: NWK_INIT floors, endpoint replay.
    let resumes = r.ncp.status().resumes;
    r.sim.reboot();
    assert!(r.ncp.wait_status(T, |s| s.resumes > resumes));
    rec.wait_for(ids::TBL_READ, 1);
    rec.end("resume: NCP restart, replay, import", Some(&mut api));

    api.ok(json!({"type": "network/permit_join", "duration_s": 60}));
    rec.end("permit join: open 60 s", Some(&mut api));

    // Join and interview.
    let ev = EvtDeviceJoined {
        nwk: LIGHT_NWK,
        ieee: Ieee::from_u64(LIGHT),
        parent_nwk: 0,
        join_type: 0,
        capability: 0x8E,
        decision: 0,
    };
    event(&r.sim, ids::EVT_DEVICE_JOINED, &ev);
    let light_id = format!("zb:{LIGHT:016x}");
    until("the interview of the light", || api.interviewed(&light_id));
    rec.end("join: device joined, interview, configure", Some(&mut api));
    // Closed before any later resume could replay a remainder that
    // depends on the clock.
    api.ok(json!({"type": "network/permit_join", "duration_s": 0}));
    rec.end("permit join: close", Some(&mut api));

    // Groups: the hub endpoint and the light's endpoint 11.
    let group = api.ok(json!({"type": "groups/create", "name": "Kitchen"}))["id"]
        .as_u64()
        .unwrap();
    api.ok(
        json!({"type": "groups/add_member", "group": group, "device": light_id, "endpoint": 11}),
    );
    until("the light in the group", || {
        devs.lock().unwrap()[0].groups.contains(&(11, group as u16))
    });
    rec.end("group: create, add member", Some(&mut api));
    api.ok(json!({"type": "groups/delete", "group": group}));
    until("the light out of the group", || {
        devs.lock().unwrap()[0].groups.is_empty()
    });
    rec.end("group: delete", Some(&mut api));

    // Green Power: the sink settings, then commissioning.
    api.ok(json!({"type": "gp/config", "security_level": 0}));
    api.ok(json!({"type": "gp/commissioning/open", "duration_s": 60, "involve_proxies": true}));
    rec.wait_for(ids::GP_SINK_COMMISSIONING, 1);
    event(
        &r.sim,
        ids::EVT_GP_COMMISSIONING,
        &wg::EvtGpCommissioning {
            state: 1,
            window_s: 60,
            involve_proxies: 1,
            exit_reason: 0,
        },
    );
    // Commissioning command: device 0x02 (on/off switch), MAC sequence
    // numbers, application information: manufacturer 0x6D6B, model
    // 0xF400, commands 0x20 0x21 0x22.
    gpd_frame(
        &r.sim,
        0xE0,
        1,
        2,
        &[
            0x02, 0x05, 0x07, 0x6B, 0x6D, 0x00, 0xF4, 0x03, 0x20, 0x21, 0x22,
        ],
    );
    let sink = GpSinkRecord {
        gpd_id: wg::GpdId::SrcId(GPD),
        device_id: 0x02,
        communication_mode: 2,
        group: 0x0B84,
        assigned_alias: 0xFFFF,
        flags: 0,
        security_level: 0,
        key_type: 0,
        key: None,
        frame_counter: 1,
        groupcast_radius: 0,
    };
    let done = wg::EvtGpCommissioned {
        gpd_id: wg::GpdId::SrcId(GPD),
        device_id: 0x02,
        flags: 0,
        record: Record::GpSink(sink),
    };
    event(&r.sim, ids::EVT_GP_COMMISSIONED, &done);
    let gpd_id = format!("gp:{GPD:08x}");
    until("the GP switch", || {
        api.call(json!({"type": "gp/devices/list"}))["result"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
    });
    api.ok(json!({"type": "gp/commissioning/close"}));
    rec.end("gp: sink config, commissioning", Some(&mut api));

    // Remote pairing: a binding the light executes (ZRHP remote GP
    // provisioning; the answers are the light's, through the NCP).
    let binding = api.ok(json!({"type": "gp/bindings/create", "gpd": gpd_id, "trigger": {"command_id": 0x22},
        "target": {"device": light_id, "endpoint": 11},
        "action": {"cluster": 6, "command": 2, "payload": {"mode": "fixed", "bytes": []}}, "executor": "device"}))["id"]
    .clone();
    rec.wait_for(ids::GP_REMOTE_SINK_TABLE_REQUEST, 1);
    let st = wg::EvtGpRemoteSinkTable {
        src_nwk: LIGHT_NWK,
        zcl_tsn: gp_tsn(&r.sim),
        status: 0x8B,
        total: 0,
        start_index: 0,
        flags: 0,
        records: List::new(&[]),
    };
    event(&r.sim, ids::EVT_GP_REMOTE_SINK_TABLE, &st);
    rec.wait_for(ids::GP_PAIRING_CONFIGURATION, 1);
    let status = |command_id| wg::EvtGpRemoteStatus {
        src_nwk: LIGHT_NWK,
        zcl_tsn: gp_tsn(&r.sim),
        command_id,
        zcl_status: 0,
    };
    event(&r.sim, ids::EVT_GP_REMOTE_STATUS, &status(0x09));
    rec.wait_for(ids::GP_TRANSLATION_UPDATE, 1);
    event(&r.sim, ids::EVT_GP_REMOTE_STATUS, &status(0x07));
    rec.wait_for(ids::GP_TRANSLATION_REQUEST, 1);
    let tr = GpTranslationRecord {
        gpd_id: wg::GpdId::SrcId(GPD),
        translation: GpTranslation {
            gpd_command: 0x22,
            endpoint: 11,
            profile: 0x0104,
            cluster: 6,
            zcl_command: 2,
            payload_len: 0,
            payload: &[],
        },
    };
    let recs = [Record::GpTranslation(tr)];
    let table = wg::EvtGpRemoteTranslationTable {
        src_nwk: LIGHT_NWK,
        zcl_tsn: gp_tsn(&r.sim),
        status: 0,
        total: 1,
        start_index: 0,
        flags: 0,
        records: List::new(&recs),
    };
    event(&r.sim, ids::EVT_GP_REMOTE_TRANSLATION_TABLE, &table);
    until("the binding active on the light", || {
        let l = api.call(json!({"type": "gp/bindings/list", "gpd": gpd_id}));
        l["result"].as_array().is_some_and(|a| {
            a.iter()
                .any(|x| x["id"] == binding && x["state"] == "active")
        })
    });
    rec.end("gp: remote pairing (binding on the light)", Some(&mut api));

    // An app restart: the registry and last values are written at the
    // stop, restored at the start.
    api.app.shutdown();
    rec.end("app stop", None);
    store_goldens("store_mid", &mem, &r.ncp.store_state(), &mut goldens);
    let mut api = Api::start(&r);
    assert!(
        api.app.wait(T, |s| s.restored == 2),
        "{:?}",
        api.app.status()
    );
    rec.wait_for(ids::TBL_READ, 1);
    rec.end(
        "app restart: registry restored, NCP device import",
        Some(&mut api),
    );

    // Network map: coordinator tables, the light answering over the air.
    let own = r
        .ncp
        .client()
        .session()
        .unwrap()
        .info
        .as_ref()
        .unwrap()
        .ieee_active;
    r.sim.with(|s| {
        s.neighbors = Some(vec![(LIGHT.to_le_bytes(), LIGHT_NWK, 1, 2)]);
        s.children = Some(Vec::new());
        s.routes = vec![(LIGHT_NWK, LIGHT_NWK, 0)];
        s.remotes = vec![SimRemote {
            nwk: LIGHT_NWK,
            ieee: LIGHT.to_le_bytes(),
            neighbors: vec![(own, 0, 0, 2, 220)],
            routes: vec![(0, 0, 0)],
        }];
    });
    let map = api.job(json!({"type": "network/topology"}));
    goldens.push(baseline::check(
        "api/topology.json",
        &baseline::json(&map, VOLATILE),
    ));
    rec.end("topology: network map", Some(&mut api));
    r.sim.with(|s| {
        (s.neighbors, s.children, s.routes, s.remotes) = (None, None, Vec::new(), Vec::new())
    });

    api.ok(json!({"type": "devices/remove", "device": light_id}));
    rec.wait_for(ids::NWK_REMOVE_DEVICE, 1);
    rec.end("remove: the light", Some(&mut api));

    // Read-only answers, once the engine counted the confirms of all its
    // APS requests since the restart (the map's ZDO requests are the
    // admin's).
    let restart = rec
        .steps
        .iter()
        .position(|s| s.name.starts_with("app restart"))
        .unwrap();
    let aps = rec.steps[restart..]
        .iter()
        .filter(|s| !s.name.starts_with("topology"))
        .flat_map(|s| &s.reqs)
        .filter(|q| q.msg_id == ids::APS_DATA_REQUEST)
        .count() as u64;
    assert!(
        api.app.wait(T, |s| s.aps.accepted == aps),
        "{aps}: {:?}",
        api.app.status().aps
    );
    for (ty, name) in [
        ("network/info", "network_info"),
        ("hub/info", "hub_info"),
        ("hub/system", "hub_system"),
        ("network/diagnostics", "diagnostics"),
    ] {
        let mut v = api.ok(json!({"type": ty}));
        if ty == "hub/info" {
            // The build's feature (a workspace build with hubctl has it).
            assert_eq!(v["advanced"], cfg!(feature = "advanced"), "{v}");
            v["advanced"] = "<feature>".into();
            // Zigbee OTA (feature `ota`, the 8 MB images) likewise.
            assert_eq!(v.get("ota").is_some(), cfg!(feature = "ota"), "{v}");
            #[cfg(feature = "ota")]
            {
                let store = cfg!(feature = "ota-store");
                assert_eq!(v["ota"]["store"], store, "{v}");
                assert_eq!(v["ota"]["stream"], true, "{v}");
                assert_eq!(v["ota"]["max_streams"], 4, "{v}");
                assert_eq!(v["ota"].get("capacity").is_some(), store, "{v}");
            }
            v["ota"] = "<feature>".into();
        }
        goldens.push(baseline::check(
            &format!("api/{name}.json"),
            &baseline::json(&v, VOLATILE),
        ));
    }
    rec.end(
        "api: network/info, hub/info, hub/system, network/diagnostics",
        Some(&mut api),
    );

    // Backup, with a device in the NCP's table (the plug).
    r.sim.with(|s| {
        s.table_devices = vec![(PLUG.to_le_bytes(), PLUG_NWK, None)];
        s.link_keys = vec![(PLUG.to_le_bytes(), [0x42; 16])];
    });
    let backup = api.ok(json!({"type": "network/backup"}));
    goldens.push(baseline::check(
        "api/backup_ocb.json",
        &baseline::json(&backup, VOLATILE),
    ));
    rec.end("backup: export", Some(&mut api));

    // Reset onto another channel: automatic backup, leave, form.
    r.sim.with(|s| s.table_devices.clear());
    let reset = api.ok(json!({"type": "network/reset", "confirm": "RESET", "channels": [25], "pan_id": 0x2B3C, "ext_pan_id": "00124b00aabbccdd"}));
    goldens.push(baseline::check(
        "api/reset.json",
        &baseline::json(&reset, VOLATILE),
    ));
    rec.wait_for(ids::TBL_READ, 1);
    rec.end(
        "reset: backup, leave, form, NCP device import",
        Some(&mut api),
    );

    // Restore the backup: the plug comes back, is imported and interviewed.
    let report = api.ok(
        json!({"type": "network/restore", "backup": backup, "force": true, "skip_backup": true}),
    );
    goldens.push(baseline::check(
        "api/restore.json",
        &baseline::json(&report, VOLATILE),
    ));
    let stream = r.sim.state().restored.expect("a committed restore stream");
    goldens.push(baseline::check(
        "restore_stream.hex",
        &baseline::hexdump(&stream),
    ));
    let plug_id = format!("zb:{PLUG:016x}");
    until("the interview of the plug", || api.interviewed(&plug_id));
    rec.end(
        "restore: backup onto the NCP, import, interview",
        Some(&mut api),
    );

    api.app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
    rec.end("shutdown", None);
    store_goldens("store_end", &mem, &r.ncp.store_state(), &mut goldens);

    goldens.push(baseline::check(
        "store_writes.txt",
        &render_writes(&rec.writes),
    ));
    let wire = baseline::check("wire_trace.txt", &baseline::render_trace(&rec.steps));
    if wire.is_err() && WIRE_ORDER == Order::Relaxed {
        let want = baseline::parse_trace(
            &std::fs::read_to_string(baseline::path("wire_trace.txt")).unwrap_or_default(),
        );
        let problems = baseline::relaxed_steps(&want, &rec.steps);
        goldens.push(if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("\n"))
        });
    } else {
        goldens.push(wire);
    }
    baseline::assert_all(goldens);
}

/// A fixed sequence through [`Store`], as raw `hubdata` bytes: every
/// record type (`commit`), changes skipped and written (`commit_if_changed`),
/// key/value records (`kv_put`: an identical put, a value of three parts, an
/// empty one; `kv_delete`, also of a missing key), three compactions and the
/// wrap of the ring. The on-flash format and the store's write rules, byte
/// for byte.
#[test]
fn journal_format_golden() {
    let mem = SharedMem::new(6, 4096);
    let mut store = Store::open(Box::new(mem.clone())).unwrap();
    let meta = Meta {
        ncp_ieee: [0x02, 0x01, 0, 0, 0, 0, 0x51, 0x5A],
        table_epoch: 0xE0E0_0001,
        host_db_epoch: 3,
        host_max_rev: 9,
        last_ncp_cookie: 0x5100_0001,
    };
    let net = NetworkRecord {
        pan_id: 0x1A2B,
        ext_pan_id: [4, 3, 2, 1, 0, 0x4B, 0x12, 0],
        channel: 15,
        channel_mask: 1 << 15,
        network_key: [0x11; 16],
        key_seq: 0,
        nwk_update_id: 1,
        tx_power: -3,
        nwk_manager: 0,
    };
    let ep = EndpointRecord {
        ep: 1,
        profile: 0x0104,
        device_id: 5,
        version: 0,
        in_clusters: vec![0, 3, 0x0A, 0x19],
        out_clusters: vec![0, 3, 4, 5, 6, 8],
    };
    let dev = |i: u8| DeviceRecord {
        ieee: [i; 8],
        nwk: 0x1000 + u16::from(i),
        capability: 0x8E,
        flags: i & 1,
        last_seen: u32::from(i) * 60,
    };
    let fc = |limit: u32, generation: u32| FcRecord {
        key_seq: 0,
        limit,
        generation,
    };
    for c in [
        Change::Meta(meta),
        Change::Network(net),
        Change::Fc(1, fc(0x400, 1)),
        Change::Fc(2, fc(0x800, 1)),
        Change::Endpoint(ep),
        Change::FwPending(FwPending {
            version: 0x0001_0002,
            sha256: [0x5A; 32],
            slot: 1,
            activated: false,
        }),
        Change::FwClear,
        Change::Device(dev(1)),
        Change::Device(dev(2)),
        Change::DeviceDel([1; 8]),
        Change::EndpointDel(9),
        Change::NetworkClear,
        Change::Network(net),
    ] {
        store.commit(c).unwrap();
    }
    // Unchanged: nothing written.
    store.commit_if_changed(Change::Network(net)).unwrap();
    store.commit_if_changed(Change::Device(dev(2))).unwrap();
    store.commit_if_changed(Change::DeviceDel([1; 8])).unwrap();
    store
        .commit_if_changed(Change::Device(DeviceRecord {
            nwk: 0x2222,
            ..dev(2)
        }))
        .unwrap();
    let big: Vec<u8> = (0..4100u32).map(|i| (i * 7 + 3) as u8).collect();
    let small = br#"{"v":1,"id":"zb:0000000000000002"}"#;
    store.kv_put("dev/zb:0000000000000002", small).unwrap();
    store.kv_put("dev/zb:0000000000000002", small).unwrap();
    store.kv_put("udef/big", &big).unwrap();
    store.kv_put("udef/big", &big).unwrap();
    store.kv_put("cfg/empty", b"").unwrap();
    store.kv_put("cfg/tz", b"UTC0").unwrap();
    store.kv_delete("dev/zb:0000000000000002").unwrap();
    store.kv_delete("dev/zb:0000000000000002").unwrap();
    store.kv_delete("cfg/missing").unwrap();
    // Appends until the third compaction (the ring has wrapped by then).
    let mut round = 0u32;
    while store.stats().generation < 3 {
        store
            .commit(Change::Fc(1, fc(0x400 * (round + 2), round + 2)))
            .unwrap();
        store
            .kv_put(
                "st/zb:0000000000000002",
                format!("{{\"lqi\":{round}}}").as_bytes(),
            )
            .unwrap();
        round += 1;
        assert!(round < 10_000);
    }
    let state = store.state().clone();
    drop(store);
    let reopened = Journal::open(mem.flash.lock().unwrap().clone()).unwrap();
    assert_eq!(reopened.state(), &state, "reopens to the same state");
    baseline::assert_all(vec![baseline::check(
        "journal_format.hex",
        &baseline::hexdump(&mem.bytes()),
    )]);
}
