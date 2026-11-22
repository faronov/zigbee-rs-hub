//! The OTA Upgrade server in the app, against the simulated NCP with a
//! simulated OTA client behind it (`SimNcp::set_aps_hook`).
// The upload helpers serve the store test only.
#![cfg_attr(not(feature = "ota-store"), allow(dead_code, unused_imports))]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use hub_ncp::lifecycle::{Hub as Ncp, HubConfig, Rng};
use hub_ncp::sim::{SIM_MAX_ASDU, SimAps, SimNcp, indication};
use hub_ncp::store::{Flash, MemFlash, Store};
use hub_ncp::transport::{Line, pipe};
use hub_ota::file::build;
use hub_ota::sim::SimClient;
use serde_json::{Value as J, json};
use zigbee_zrhp::msg::ids;
use zigbee_zrhp::msg::network::EvtDeviceJoined;
use zigbee_zrhp::types::Ieee;

use crate::{App, AppConfig};

const T: Duration = Duration::from_secs(15);
const DEV: u64 = 0x00124b0012345678;
const NWK: u16 = 0x4321;
const MFR: u16 = 0x6D6B;
const TYPE: u16 = 0x0102;

/// A flash that survives an app restart.
#[derive(Clone)]
struct Shared(Arc<Mutex<MemFlash>>);

impl Flash for Shared {
    fn sector_size(&self) -> u32 {
        self.0.lock().unwrap().sector_size()
    }
    fn sectors(&self) -> u32 {
        self.0.lock().unwrap().sectors()
    }
    fn read(&mut self, o: u32, b: &mut [u8]) -> std::io::Result<()> {
        self.0.lock().unwrap().read(o, b)
    }
    fn write(&mut self, o: u32, d: &[u8]) -> std::io::Result<()> {
        self.0.lock().unwrap().write(o, d)
    }
    fn erase(&mut self, s: u32) -> std::io::Result<()> {
        self.0.lock().unwrap().erase(s)
    }
}

fn rng() -> Rng {
    let n = Arc::new(AtomicU32::new(7));
    Arc::new(move |buf: &mut [u8]| {
        for b in buf {
            *b = (n.fetch_add(0x9E37_79B9, Ordering::Relaxed) >> 11) as u8;
        }
    })
}

fn session(app: &App) -> (u64, Receiver<String>) {
    let (tx, rx) = mpsc::channel();
    let sid = app.connect(Box::new(tx)).unwrap();
    let _hello = rx.recv_timeout(T).unwrap();
    app.text(sid, json!({"type": "auth", "token": "secret"}).to_string());
    let ok: J = serde_json::from_str(&rx.recv_timeout(T).unwrap()).unwrap();
    assert_eq!(ok["type"], "auth_ok", "{ok}");
    (sid, rx)
}

/// Sends a request; returns its result frame. Events seen meanwhile are
/// appended to `events`.
fn call(app: &App, sid: u64, rx: &Receiver<String>, events: &mut Vec<J>, id: u64, mut body: J) -> J {
    body["id"] = id.into();
    app.text(sid, body.to_string());
    loop {
        let f: J = serde_json::from_str(&rx.recv_timeout(T).unwrap()).unwrap();
        if f["type"] == "result" && f["id"] == id {
            return f;
        }
        events.push(f);
    }
}

fn wait_event(rx: &Receiver<String>, events: &mut Vec<J>, pred: impl Fn(&J) -> bool) -> J {
    if let Some(e) = events.iter().find(|e| pred(e)) {
        return e.clone();
    }
    loop {
        let f: J = serde_json::from_str(&rx.recv_timeout(T).expect("event")).unwrap();
        events.push(f.clone());
        if pred(&f) {
            return f;
        }
    }
}

/// Installs the simulated device: its OTA client answers the hub's frames.
fn device(sim: &SimNcp, client: Arc<Mutex<SimClient>>, asdus: Arc<Mutex<Vec<usize>>>) {
    sim.set_aps_hook(Box::new(move |a: &SimAps<'_>| {
        if a.dst != NWK || a.cluster != 0x0019 {
            return Vec::new();
        }
        asdus.lock().unwrap().push(a.asdu.len());
        let frames = client.lock().unwrap().on_zcl(a.asdu, 0);
        frames.into_iter().map(|f| indication(NWK, DEV, a.dst_ep, a.src_ep, 0x0104, 0x0019, &f)).collect()
    }));
}

/// The device's own Query Next Image.
fn device_query(sim: &SimNcp, client: &Arc<Mutex<SimClient>>) {
    let f = hub_ota::sim::frame(client.lock().unwrap().query());
    sim.inject_event(ids::EVT_APS_DATA_INDICATION, indication(NWK, DEV, 1, 1, 0x0104, 0x0019, &f));
}

fn upload(app: &App, sid: u64, rx: &Receiver<String>, ev: &mut Vec<J>, file: &[u8], chunk: usize) -> J {
    let b = call(app, sid, rx, ev, 10, json!({"type": "ota/upload/begin", "size": file.len(), "name": "sensor.ota"}));
    assert_eq!(b["success"], true, "{b}");
    let up = b["result"]["upload"].clone();
    let mut at = b["result"]["offset"].as_u64().unwrap() as usize;
    let mut id = 11;
    while at < file.len() {
        let end = (at + chunk).min(file.len());
        let data = base64::engine::general_purpose::STANDARD.encode(&file[at..end]);
        let c = call(app, sid, rx, ev, id, json!({"type": "ota/upload/chunk", "upload": up, "offset": at, "data": data}));
        assert_eq!(c["result"]["written"], end, "{c}");
        at = end;
        id += 1;
    }
    let c = call(app, sid, rx, ev, 99, json!({"type": "ota/upload/commit", "upload": up}));
    assert_eq!(c["success"], true, "{c}");
    c["result"].clone()
}

#[cfg(feature = "ota-store")]
#[test]
fn upload_offer_download_and_confirm_through_the_ncp() {
    use crate::OtaFlash;
    let (host, ncp_end) = pipe::pair();
    let sim = SimNcp::start(ncp_end);
    let (rx_, tx_) = host.split();
    let mut hc = HubConfig::default();
    hc.driver.lines = vec![Line::BR_DEFAULT];
    let ncp = Ncp::start(hc, rx_, tx_, None, Store::volatile(), rng());
    assert!(ncp.wait_ready(T));
    ncp.form(hub_ncp::FormRequest::default()).unwrap();
    let area = Shared(Arc::new(Mutex::new(MemFlash::new(64, 4096))));
    let cfg = || AppConfig { api_token: "secret".into(), ota_flash: OtaFlash::new(Box::new(area.clone())), ..AppConfig::default() };
    let app = App::start(ncp.clone(), cfg());
    assert!(app.wait(T, |s| s.endpoint_registered));
    let joined = EvtDeviceJoined { nwk: NWK, ieee: Ieee::from_u64(DEV), parent_nwk: 0, join_type: 0, capability: 0x80, decision: 0 };
    sim.inject_event(ids::EVT_DEVICE_JOINED, hub_ncp::driver::encode_body(&joined).unwrap());
    assert!(app.wait(T, |s| s.devices == 1));
    let client = Arc::new(Mutex::new(SimClient::new(MFR, TYPE, 0x0001_0000)));
    let asdus = Arc::new(Mutex::new(Vec::new()));
    device(&sim, client.clone(), asdus.clone());

    let (sid, rx) = session(&app);
    let mut ev = Vec::new();
    let info = call(&app, sid, &rx, &mut ev, 1, json!({"type": "hub/info"}));
    assert_eq!(info["result"]["ota"]["capacity"], 62 * 4096, "{info}");
    let sub = call(&app, sid, &rx, &mut ev, 2, json!({"type": "subscribe", "topics": ["ota"]}));
    assert_eq!(sub["success"], true, "{sub}");
    let snap = wait_event(&rx, &mut ev, |e| e["event"]["topic"] == "snapshot");
    assert_eq!(snap["event"]["ota"]["area"]["capacity"], 62 * 4096, "{snap}");
    call(&app, sid, &rx, &mut ev, 3, json!({"type": "ota/settings/set", "blocks_per_s": 200}));

    // Upload (a resumed begin returns where it got to).
    let file = build(MFR, TYPE, 0x0001_0100, "sim sensor 1.1", &(0..6000u32).map(|i| (i * 13) as u8).collect::<Vec<_>>(), None);
    let b = call(&app, sid, &rx, &mut ev, 4, json!({"type": "ota/upload/begin", "size": file.len(), "name": "sensor.ota"}));
    let up = b["result"]["upload"].clone();
    let data = base64::engine::general_purpose::STANDARD.encode(&file[..2000]);
    call(&app, sid, &rx, &mut ev, 5, json!({"type": "ota/upload/chunk", "upload": up, "offset": 0, "data": data}));
    let bad = call(&app, sid, &rx, &mut ev, 6, json!({"type": "ota/upload/chunk", "upload": up, "offset": 0, "data": data}));
    assert_eq!(bad["error"]["code"], "invalid_value", "{bad}");
    let img = upload(&app, sid, &rx, &mut ev, &file, 1500);
    assert_eq!(img["file_version"], 0x0001_0100);
    assert_eq!(img["manufacturer_code"], MFR);
    assert_eq!(img["label"], "sim sensor 1.1");
    assert_eq!(img["devices"], json!([]), "the device's version is not known yet");
    let image_id = img["image"].clone();

    // The device queries by itself: not offered (manual), but available.
    device_query(&sim, &client);
    let e = wait_event(&rx, &mut ev, |e| e["event"]["topic"] == "ota_device" && e["event"]["device"]["current_version"] == 0x0001_0000);
    assert_eq!(e["event"]["device"]["available"]["image"], image_id, "{e}");
    assert!(e["event"]["device"]["job"].is_null());
    let list = call(&app, sid, &rx, &mut ev, 20, json!({"type": "ota/devices/list"}));
    assert_eq!(list["result"][0]["device"], "zb:00124b0012345678", "{list}");
    assert_eq!(list["result"][0]["ep"], 1);
    let imgs = call(&app, sid, &rx, &mut ev, 21, json!({"type": "ota/images/list"}));
    assert_eq!(imgs["result"][0]["devices"], json!(["zb:00124b0012345678"]), "{imgs}");

    // Start: Image Notify, query, blocks, upgrade end, the device "reboots"
    // and queries with the new version.
    let st = call(&app, sid, &rx, &mut ev, 22, json!({"type": "ota/start", "device": "zb:00124b0012345678"}));
    assert_eq!(st["success"], true, "{st}");
    let done = wait_event(&rx, &mut ev, |e| e["event"]["topic"] == "ota_device" && e["event"]["device"]["job"]["state"] == "done");
    assert_eq!(done["event"]["device"]["current_version"], 0x0001_0100);
    assert!(ev.iter().any(|e| e["event"]["device"]["job"]["state"] == "downloading"));
    let c = client.lock().unwrap().clone();
    assert_eq!((c.version, c.upgrades), (0x0001_0100, 1));
    assert_eq!(c.largest_block, 48);
    let biggest = asdus.lock().unwrap().iter().copied().max().unwrap();
    assert!(biggest <= SIM_MAX_ASDU, "{biggest}");
    // Every frame of the cluster went out (none refused as too large).
    assert_eq!(app.status().aps.failed, 0);

    // A restart keeps the images and the device's OTA state.
    app.shutdown();
    let app = App::start(ncp.clone(), cfg());
    let (sid, rx) = session(&app);
    let mut ev = Vec::new();
    let list = call(&app, sid, &rx, &mut ev, 1, json!({"type": "ota/devices/list"}));
    assert_eq!(list["result"][0]["current_version"], 0x0001_0100, "{list}");
    assert_eq!(list["result"][0]["job"]["state"], "done");
    assert!(list["result"][0]["available"].is_null());
    let imgs = call(&app, sid, &rx, &mut ev, 2, json!({"type": "ota/images/list"}));
    assert_eq!(imgs["result"][0]["image"], image_id);
    let del = call(&app, sid, &rx, &mut ev, 3, json!({"type": "ota/images/delete", "image": image_id}));
    assert_eq!(del["success"], true, "{del}");
    let status = call(&app, sid, &rx, &mut ev, 4, json!({"type": "ota/status"}));
    assert_eq!(status["result"]["area"]["used"], 0, "{status}");
    assert_eq!(status["result"]["block_max"], 48, "{status}");
    app.shutdown();
    ncp.shutdown();
    sim.stop();
}

#[test]
fn without_an_area_queries_are_answered_and_uploads_refused() {
    let (host, ncp_end) = pipe::pair();
    let sim = SimNcp::start(ncp_end);
    let (rx_, tx_) = host.split();
    let mut hc = HubConfig::default();
    hc.driver.lines = vec![Line::BR_DEFAULT];
    let ncp = Ncp::start(hc, rx_, tx_, None, Store::volatile(), rng());
    assert!(ncp.wait_ready(T));
    let app = App::start(ncp.clone(), AppConfig { api_token: "secret".into(), ..AppConfig::default() });
    assert!(app.wait(T, |s| s.endpoint_registered));
    let joined = EvtDeviceJoined { nwk: NWK, ieee: Ieee::from_u64(DEV), parent_nwk: 0, join_type: 0, capability: 0x80, decision: 0 };
    sim.inject_event(ids::EVT_DEVICE_JOINED, hub_ncp::driver::encode_body(&joined).unwrap());
    assert!(app.wait(T, |s| s.devices == 1));
    let client = Arc::new(Mutex::new(SimClient::new(MFR, TYPE, 7)));
    device(&sim, client.clone(), Arc::new(Mutex::new(Vec::new())));
    let (sid, rx) = session(&app);
    let mut ev = Vec::new();
    let b = call(&app, sid, &rx, &mut ev, 1, json!({"type": "ota/upload/begin", "size": 1000}));
    assert_eq!(b["error"]["code"], "unsupported", "{b}");
    device_query(&sim, &client);
    let t = std::time::Instant::now();
    while client.lock().unwrap().state != hub_ota::sim::ClientState::Idle && t.elapsed() < T {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(client.lock().unwrap().state, hub_ota::sim::ClientState::Idle, "NO_IMAGE_AVAILABLE received");
    let list = call(&app, sid, &rx, &mut ev, 2, json!({"type": "ota/devices/list"}));
    assert_eq!(list["result"][0]["current_version"], 7, "{list}");
    app.shutdown();
    ncp.shutdown();
    sim.stop();
}

/// The 4 MB way: no store, the client (this test's API session) streams
/// the image on demand.
#[test]
fn a_streamed_image_reaches_the_device_through_the_ncp() {
    let (host, ncp_end) = pipe::pair();
    let sim = SimNcp::start(ncp_end);
    let (rx_, tx_) = host.split();
    let mut hc = HubConfig::default();
    hc.driver.lines = vec![Line::BR_DEFAULT];
    let ncp = Ncp::start(hc, rx_, tx_, None, Store::volatile(), rng());
    assert!(ncp.wait_ready(T));
    ncp.form(hub_ncp::FormRequest::default()).unwrap();
    let app = App::start(ncp.clone(), AppConfig { api_token: "secret".into(), ..AppConfig::default() });
    assert!(app.wait(T, |s| s.endpoint_registered));
    let joined = EvtDeviceJoined { nwk: NWK, ieee: Ieee::from_u64(DEV), parent_nwk: 0, join_type: 0, capability: 0x80, decision: 0 };
    sim.inject_event(ids::EVT_DEVICE_JOINED, hub_ncp::driver::encode_body(&joined).unwrap());
    assert!(app.wait(T, |s| s.devices == 1));
    let client = Arc::new(Mutex::new(SimClient::new(MFR, TYPE, 0x0001_0000)));
    device(&sim, client.clone(), Arc::new(Mutex::new(Vec::new())));
    // The device's WAIT_FOR_DATA delays pass.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ticker = {
        let (client, sim, stop) = (client.clone(), sim.clone(), stop.clone());
        let t0 = std::time::Instant::now();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
                let now = t0.elapsed().as_millis() as u64 + 1_000_000;
                for f in client.lock().unwrap().tick_zcl(now) {
                    sim.inject_event(ids::EVT_APS_DATA_INDICATION, indication(NWK, DEV, 1, 1, 0x0104, 0x0019, &f));
                }
            }
        })
    };
    client.lock().unwrap().version = 0x0001_0000;

    let (sid, rx) = session(&app);
    let mut ev = Vec::new();
    call(&app, sid, &rx, &mut ev, 1, json!({"type": "subscribe", "topics": ["ota"]}));
    let up = call(&app, sid, &rx, &mut ev, 2, json!({"type": "ota/upload/begin", "size": 100}));
    assert_eq!(up["error"]["code"], "unsupported", "{up}");
    device_query(&sim, &client);
    wait_event(&rx, &mut ev, |e| e["event"]["device"]["current_version"] == 0x0001_0000);

    let file = build(MFR, TYPE, 0x0001_0200, "streamed 1.2", &(0..9000u32).map(|i| (i * 31) as u8).collect::<Vec<_>>(), None);
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let offer = call(&app, sid, &rx, &mut ev, 3, json!({"type": "ota/stream/offer", "size": file.len(), "header": b64(&file[..256]), "name": "s.ota"}));
    assert_eq!(offer["success"], true, "{offer}");
    let session_id = offer["result"]["session"].clone();
    assert_eq!(offer["result"]["devices"], json!(["zb:00124b0012345678"]));
    let st = call(&app, sid, &rx, &mut ev, 4, json!({"type": "ota/start", "device": "zb:00124b0012345678", "image": session_id}));
    assert_eq!(st["success"], true, "{st}");
    // Serve block_needed until the device runs the new version.
    let mut id = 10;
    let mut served = 0;
    let t = std::time::Instant::now();
    loop {
        assert!(t.elapsed() < Duration::from_secs(30), "no progress");
        let f: J = serde_json::from_str(&rx.recv_timeout(T).unwrap()).unwrap();
        if f["type"] == "ota/block_needed" {
            assert_eq!(f["session"], session_id);
            let (off, len) = (f["offset"].as_u64().unwrap() as usize, f["len"].as_u64().unwrap() as usize);
            id += 1;
            app.text(sid, json!({"id": id, "type": "ota/block_data", "session": session_id, "offset": off, "data": b64(&file[off..off + len])}).to_string());
            served += len;
        } else if f["type"] == "result" {
            assert_eq!(f["success"], true, "{f}");
        } else if f["event"]["topic"] == "ota_device" && f["event"]["device"]["job"]["state"] == "done" {
            break;
        }
    }
    assert_eq!(client.lock().unwrap().version, 0x0001_0200);
    assert!(served <= file.len() + 4096, "{served}");
    stop.store(true, Ordering::Relaxed);
    ticker.join().ok();
    app.shutdown();
    ncp.shutdown();
    sim.stop();
}
