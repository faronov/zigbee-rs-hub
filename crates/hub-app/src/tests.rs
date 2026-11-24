//! The app on the host core against the simulated NCP.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use hub_ncp::lifecycle::{Hub as Ncp, HubConfig, Rng};
use hub_ncp::sim::SimNcp;
use hub_ncp::store::{MemFlash, Store};
use hub_ncp::transport::{Line, pipe};
use serde_json::{Value as J, json};
use zigbee_zrhp::msg::ids;
use zigbee_zrhp::msg::network::EvtDeviceJoined;
use zigbee_zrhp::types::Ieee;

use crate::{App, AppConfig};

pub(crate) const T: Duration = Duration::from_secs(10);
const DEV: u64 = 0x00158d0001a2b3c4;

fn rng() -> Rng {
    let n = Arc::new(AtomicU32::new(7));
    Arc::new(move |buf: &mut [u8]| {
        for b in buf {
            *b = (n.fetch_add(0x9E37_79B9, Ordering::Relaxed) >> 11) as u8;
        }
    })
}

pub(crate) struct Rig {
    pub ncp: Ncp,
    pub sim: SimNcp,
}

pub(crate) fn rig(store: Store) -> Rig {
    rig_with(store, |_| {})
}

/// [`rig`] with `prepare` applied to the simulated NCP before the host
/// connects.
pub(crate) fn rig_with(store: Store, prepare: impl FnOnce(&SimNcp)) -> Rig {
    let (host, ncp_end) = pipe::pair();
    let sim = SimNcp::start(ncp_end);
    prepare(&sim);
    let (rx, tx) = host.split();
    let mut cfg = HubConfig::default();
    cfg.driver.lines = vec![Line::BR_DEFAULT];
    let ncp = Ncp::start(cfg, rx, tx, None, store, rng());
    assert!(ncp.wait_ready(T));
    Rig { ncp, sim }
}

pub(crate) fn session(app: &App) -> (u64, Receiver<String>) {
    let (tx, rx) = mpsc::channel();
    let sid = app.connect(Box::new(tx)).unwrap();
    let hello: J = serde_json::from_str(&rx.recv_timeout(T).unwrap()).unwrap();
    assert_eq!(hello["type"], "auth_required");
    app.text(sid, json!({"type": "auth", "token": "secret"}).to_string());
    let ok: J = serde_json::from_str(&rx.recv_timeout(T).unwrap()).unwrap();
    assert_eq!(ok["type"], "auth_ok", "{ok}");
    (sid, rx)
}

pub(crate) fn call(app: &App, sid: u64, rx: &Receiver<String>, id: u64, mut body: J) -> J {
    body["id"] = id.into();
    app.text(sid, body.to_string());
    loop {
        let f: J = serde_json::from_str(&rx.recv_timeout(T).unwrap()).unwrap();
        if f["type"] == "result" && f["id"] == id {
            return f;
        }
    }
}

pub(crate) fn cfg() -> AppConfig {
    AppConfig { api_token: "secret".into(), ..AppConfig::default() }
}

fn join(sim: &SimNcp) {
    let ev = EvtDeviceJoined { nwk: 0x1234, ieee: Ieee::from_u64(DEV), parent_nwk: 0x0000, join_type: 0, capability: 0x80, decision: 0 };
    sim.inject_event(ids::EVT_DEVICE_JOINED, hub_ncp::driver::encode_body(&ev).unwrap());
}

#[test]
fn api_session_endpoint_and_interview_start() {
    let r = rig(Store::volatile());
    let app = App::start(r.ncp.clone(), cfg());
    assert!(app.wait(T, |s| s.endpoint_registered));
    assert_eq!(r.sim.state().endpoints, vec![1]);
    let (sid, rx) = session(&app);
    let list = call(&app, sid, &rx, 1, json!({"type": "devices/list"}));
    assert_eq!(list["success"], true, "{list}");
    assert_eq!(list["result"].as_array().map(Vec::len), Some(0), "{list}");
    // The hub identity for clients (the HA integration's unique ID).
    let info = call(&app, sid, &rx, 9, json!({"type": "hub/info"}));
    assert_eq!(info["result"]["coordinator_ieee"], "020100000000515a", "{info}");
    assert_eq!(info["result"]["api_version"], hub_devices::api::messages::API_VERSION);
    assert_eq!(info["result"]["devices"], 0);
    // A join reaches the engine, which starts the interview through the NCP.
    let sub = call(&app, sid, &rx, 2, json!({"type": "subscribe", "topics": ["devices"]}));
    assert_eq!(sub["success"], true);
    join(&r.sim);
    let added = loop {
        let f: J = serde_json::from_str(&rx.recv_timeout(T).unwrap()).unwrap();
        if f["type"] == "event" && f["event"]["topic"] == "device_added" {
            break f;
        }
    };
    assert_eq!(added["event"]["device"]["id"], "zb:00158d0001a2b3c4");
    let node_desc = |aps: &Vec<Vec<u8>>| {
        aps.iter().any(|p| {
            let req: zigbee_zrhp::msg::data::ApsDataRequest =
                zigbee_zrhp::codec::Wire::read(&mut zigbee_zrhp::codec::Reader::new(p)).unwrap();
            req.profile == 0 && req.cluster == 0x0002
        })
    };
    let t = std::time::Instant::now();
    while !node_desc(&r.sim.state().aps) && t.elapsed() < T {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(node_desc(&r.sim.state().aps), "Node_Desc_req sent through APS_DATA_REQUEST");
    assert!(app.wait(T, |s| s.aps.accepted > 0));
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

/// `EVT_APS_DATA_CONFIRM` outcomes: delivered ones counted, a failure
/// reaches the engine at once (the interview moves on instead of waiting
/// for its response timeout).
#[test]
fn mqtt_requests_without_the_bridge_answer_unsupported() {
    let r = rig(Store::volatile());
    let app = App::start(r.ncp.clone(), cfg());
    let (sid, rx) = session(&app);
    let st = call(&app, sid, &rx, 1, json!({"type": "mqtt/status"}));
    assert_eq!(st["success"], false, "{st}");
    assert_eq!(st["error"]["code"], "unsupported", "{st}");
    let other = call(&app, sid, &rx, 2, json!({"type": "nonexistent/request"}));
    assert_eq!(other["error"]["code"], "invalid_format", "{other}");
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

#[test]
fn aps_confirms_are_counted_and_failures_reach_the_engine() {
    let r = rig(Store::volatile());
    r.sim.with(|s| s.undeliverable.push((0x1234, 0xA8)));
    let app = App::start(r.ncp.clone(), cfg());
    assert!(app.wait(T, |s| s.endpoint_registered));
    join(&r.sim);
    // Node_Desc_req (and its immediate retry) fail; the interview goes on
    // to the next steps long before the 8 s response timeout.
    assert!(app.wait(Duration::from_secs(4), |s| s.aps.not_delivered >= 3), "{:?}", app.status().aps);
    assert_eq!(app.status().aps.delivered, 0);
    // Reachable again: a re-interview's requests are confirmed delivered.
    r.sim.with(|s| s.undeliverable.clear());
    let (sid, rx) = session(&app);
    let re = call(&app, sid, &rx, 1, json!({"type": "devices/reinterview", "device": "zb:00158d0001a2b3c4"}));
    assert_eq!(re["success"], true, "{re}");
    assert!(app.wait(T, |s| s.aps.delivered > 0));
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

#[test]
fn registry_survives_an_app_restart() {
    let flash: Box<dyn hub_ncp::store::Flash> = Box::new(MemFlash::new(32, 4096));
    let r = rig(Store::open(flash).unwrap());
    let app = App::start(r.ncp.clone(), cfg());
    assert!(app.wait(T, |s| s.endpoint_registered));
    join(&r.sim);
    assert!(app.wait(T, |s| s.devices == 1));
    let (sid, rx) = session(&app);
    let upd = call(&app, sid, &rx, 1, json!({"type": "devices/update", "device": "zb:00158d0001a2b3c4", "name": "Kitchen"}));
    assert_eq!(upd["success"], true, "{upd}");
    // A user definition with its source text for the editor.
    let golden = format!("{}/../zdefc/tests/golden/philips-sml001-motion.cbor", env!("CARGO_MANIFEST_DIR"));
    let cbor = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, std::fs::read(golden).unwrap());
    let text = "zdef: 1\nid: philips/sml001\n# my comment\n";
    let up = call(&app, sid, &rx, 2, json!({"type": "definitions/upload", "cbor": cbor, "source": text, "source_format": "yaml"}));
    assert_eq!(up["success"], true, "{up}");
    // Written within the flush delay, not only at the 30 s sweep.
    let t = std::time::Instant::now();
    while r.ncp.with_store(|s| s.kv_get("usrc/philips/sml001").is_none()) && t.elapsed() < T {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(t.elapsed() < Duration::from_secs(5), "user definition written after {:?}", t.elapsed());
    app.shutdown();
    let stored = r.ncp.with_store(|s| s.kv_get("dev/zb:00158d0001a2b3c4").map(<[u8]>::to_vec));
    let snap = crate::registry::decode(&stored.expect("device persisted")).unwrap();
    assert_eq!(snap.user_name.as_deref(), Some("Kitchen"));

    // A new app on the same store restores the device.
    let app = App::start(r.ncp.clone(), cfg());
    assert!(app.wait(T, |s| s.restored == 1 && s.devices == 1));
    let (sid, rx) = session(&app);
    let dev = call(&app, sid, &rx, 1, json!({"type": "devices/get", "device": "zb:00158d0001a2b3c4"}));
    assert_eq!(dev["result"]["name"], "Kitchen", "{dev}");
    let g = call(&app, sid, &rx, 3, json!({"type": "definitions/get", "definition": "philips/sml001"}));
    assert_eq!((g["result"]["source_text"].as_str(), g["result"]["source_format"].as_str()), (Some(text), Some("yaml")), "{g}");
    // Removal deletes the record.
    let rm = call(&app, sid, &rx, 2, json!({"type": "devices/remove", "device": "zb:00158d0001a2b3c4", "force": true}));
    assert_eq!(rm["success"], true, "{rm}");
    app.shutdown();
    assert!(r.ncp.with_store(|s| s.kv_get("dev/zb:00158d0001a2b3c4").is_none()));
    r.ncp.shutdown();
    r.sim.stop();
}

#[test]
fn devices_joined_before_the_hub_are_imported_from_the_ncp() {
    let r = rig(Store::volatile());
    // Joined before this hub adopted the network: only the NCP's DEVICE
    // table knows it (DIAG_GET_CHILDREN/NEIGHBORS unsupported, as on the
    // master NCP image).
    r.sim.with(|s| s.table_devices = vec![(DEV.to_le_bytes(), 0x93B2, Some(0x80))]);
    let app = App::start(r.ncp.clone(), cfg());
    assert!(app.wait(T, |s| s.imported == 1 && s.devices == 1), "{:?}", app.status());
    let (sid, rx) = session(&app);
    let dev = call(&app, sid, &rx, 1, json!({"type": "devices/get", "device": "zb:00158d0001a2b3c4"}));
    assert_eq!(dev["success"], true, "{dev}");
    let node_desc_to_child = |aps: &Vec<Vec<u8>>| {
        aps.iter().any(|p| {
            let req: zigbee_zrhp::msg::data::ApsDataRequest =
                zigbee_zrhp::codec::Wire::read(&mut zigbee_zrhp::codec::Reader::new(p)).unwrap();
            req.profile == 0 && req.cluster == 0x0002 && p.windows(2).any(|w| w == 0x93B2u16.to_le_bytes())
        })
    };
    let t = std::time::Instant::now();
    while !node_desc_to_child(&r.sim.state().aps) && t.elapsed() < T {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(node_desc_to_child(&r.sim.state().aps), "the imported device is interviewed");
    // An NCP reboot (new session) does not import it twice.
    r.sim.reboot();
    assert!(r.ncp.wait_status(T, |s| s.resumes >= 2));
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(app.status().imported, 1);
    assert_eq!(app.status().devices, 1);
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

/// A sink that, like esp_http_server's detached sender, can only deliver
/// once the connecting thread is free again.
struct NeedsCallerFree(Arc<std::sync::atomic::AtomicBool>, mpsc::Sender<bool>);

impl crate::SessionSink for NeedsCallerFree {
    fn send(&mut self, _text: &str) -> bool {
        let t = std::time::Instant::now();
        while !self.0.load(Ordering::SeqCst) && t.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let free = self.0.load(Ordering::SeqCst);
        let _ = self.1.send(free);
        free
    }
}

#[test]
fn connect_returns_before_the_hello_is_sent() {
    let r = rig(Store::volatile());
    let app = App::start(r.ncp.clone(), cfg());
    let returned = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    let sid = app.connect(Box::new(NeedsCallerFree(returned.clone(), tx)));
    returned.store(true, Ordering::SeqCst);
    assert!(sid.is_some());
    assert_eq!(rx.recv_timeout(T), Ok(true), "the hello waited for connect() to return");
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

#[test]
fn network_info_and_permit_join_events() {
    let r = rig(Store::volatile());
    r.ncp.form(hub_ncp::FormRequest::default()).unwrap();
    let app = App::start(r.ncp.clone(), cfg());
    assert!(app.wait(T, |s| s.endpoint_registered));
    let (sid, rx) = session(&app);
    let info = call(&app, sid, &rx, 1, json!({"type": "network/info"}));
    assert_eq!(info["success"], true, "{info}");
    let n = &info["result"];
    assert_eq!(n["state"], "up");
    assert_eq!(n["ownership"], "owned");
    assert_eq!(n["pan_id"], r.sim.state().pan_id);
    assert_eq!(n["channel"], r.sim.state().channel);
    assert!(n["ncp"]["fw"].is_string() && n["ncp"]["ieee"].as_str().map(str::len) == Some(16), "{n}");
    assert_eq!(n["permit_join_s"], 0);

    let sub = call(&app, sid, &rx, 2, json!({"type": "subscribe", "topics": ["network"]}));
    assert_eq!(sub["success"], true);
    let pj = call(&app, sid, &rx, 3, json!({"type": "network/permit_join", "duration_s": 60}));
    assert_eq!(pj["success"], true, "{pj}");
    let ev = loop {
        let f: J = serde_json::from_str(&rx.recv_timeout(T).unwrap()).unwrap();
        if f["type"] == "event" && f["event"]["topic"] == "permit_join" {
            break f;
        }
    };
    assert_eq!(ev["event"]["remaining_s"], 60);
    let info = call(&app, sid, &rx, 4, json!({"type": "network/info"}));
    let left = info["result"]["permit_join_s"].as_u64().unwrap();
    assert!((58..=60).contains(&left), "{left}");
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

#[test]
fn refused_control_requests_answer_as_before() {
    let r = rig(Store::volatile());
    r.ncp.form(hub_ncp::FormRequest::default()).unwrap();
    r.sim.with(|s| s.unsupported.extend([ids::GP_SINK_CONFIG, ids::NWK_PERMIT_JOIN]));
    let app = App::start(r.ncp.clone(), cfg());
    assert!(app.wait(T, |s| s.endpoint_registered));
    let (sid, rx) = session(&app);
    // The result follows the NCP's answer, worded as it always was.
    let c = call(&app, sid, &rx, 1, json!({"type": "gp/config", "security_level": 1}));
    assert_eq!(c["error"], json!({"code": "busy", "message": "ZRHP status 0x04"}), "{c}");
    let c = call(&app, sid, &rx, 2, json!({"type": "gp/config/get"}));
    assert_eq!(c["result"]["security_level"], 0, "{c}");
    // An automation's request action has its result in place.
    let rule = json!({"id": "pj", "actions": [{"type": "request", "request": {"type": "network/permit_join", "duration_s": 30}}]});
    let c = call(&app, sid, &rx, 3, json!({"type": "automations/create", "rule": rule}));
    assert_eq!(c["success"], true, "{c}");
    let c = call(&app, sid, &rx, 4, json!({"type": "automations/run", "automation": "pj"}));
    let a = &c["result"]["trace"]["actions"][0];
    assert_eq!(a["result"], "error", "{c}");
    assert_eq!(a["error"], json!({"code": "busy", "message": "ZRHP status 0x04"}), "{c}");
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

/// The NCP never answers `GP_SINK_CONFIG`: meanwhile the app answers API
/// requests at once, and the GP request ends as it always did, without an
/// answer after the 10 s call timeout.
#[test]
fn api_answers_while_the_ncp_stalls_a_control_request() {
    let r = rig(Store::volatile());
    r.ncp.form(hub_ncp::FormRequest::default()).unwrap();
    let app = App::start(r.ncp.clone(), cfg());
    assert!(app.wait(T, |s| s.endpoint_registered));
    let (sid, rx) = session(&app);
    r.sim.with(|s| s.stall = Some(ids::GP_SINK_CONFIG));
    let t0 = std::time::Instant::now();
    app.text(sid, json!({"type": "gp/config", "security_level": 1, "id": 1}).to_string());
    while !r.sim.state().requests.iter().any(|(id, _)| *id == ids::GP_SINK_CONFIG) {
        assert!(t0.elapsed() < T);
        std::thread::sleep(Duration::from_millis(5));
    }
    for (id, ty) in [(2, "devices/list"), (3, "network/info"), (4, "groups/list"), (5, "gp/config/get")] {
        let t = std::time::Instant::now();
        let f = call(&app, sid, &rx, id, json!({"type": ty}));
        assert_eq!(f["success"], true, "{f}");
        assert!(t.elapsed() < Duration::from_millis(500), "{ty} answered after {:?}", t.elapsed());
    }
    assert!(t0.elapsed() < Duration::from_secs(5));
    let gp = loop {
        let f: J = serde_json::from_str(&rx.recv_timeout(Duration::from_secs(15)).unwrap()).unwrap();
        if f["type"] == "result" && f["id"] == 1 {
            break f;
        }
    };
    assert!(t0.elapsed() >= Duration::from_secs(10), "{:?}", t0.elapsed());
    assert_eq!(gp["error"], json!({"code": "busy", "message": "NCP link down"}), "{gp}");
    let c = call(&app, sid, &rx, 6, json!({"type": "gp/config/get"}));
    assert_eq!(c["result"]["security_level"], 0, "{c}");
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

#[test]
fn automation_group_delete_is_done_before_the_next_action() {
    let r = rig(Store::volatile());
    r.ncp.form(hub_ncp::FormRequest::default()).unwrap();
    let app = App::start(r.ncp.clone(), cfg());
    assert!(app.wait(T, |s| s.endpoint_registered));
    let (sid, rx) = session(&app);
    let c = call(&app, sid, &rx, 1, json!({"type": "groups/create", "name": "a", "group": 5}));
    assert_eq!(c["result"]["hub_member"], true, "{c}");
    // The ID is free again for the next action.
    let rule = json!({"id": "re", "actions": [
        {"type": "request", "request": {"type": "groups/delete", "group": 5}},
        {"type": "request", "request": {"type": "groups/create", "name": "b", "group": 5}},
    ]});
    let c = call(&app, sid, &rx, 2, json!({"type": "automations/create", "rule": rule}));
    assert_eq!(c["success"], true, "{c}");
    let c = call(&app, sid, &rx, 3, json!({"type": "automations/run", "automation": "re"}));
    let t = &c["result"]["trace"];
    assert_eq!(t["actions"][0]["result"], "ok", "{c}");
    assert_eq!(t["actions"][1]["result"], "ok", "{c}");
    let c = call(&app, sid, &rx, 4, json!({"type": "groups/list"}));
    let groups = c["result"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "{c}");
    assert_eq!((groups[0]["id"].as_u64(), groups[0]["name"].as_str()), (Some(5), Some("b")), "{c}");
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

fn permit_events(rx: &Receiver<String>, until: impl Fn(&[u64]) -> bool) -> Vec<u64> {
    let mut seen = Vec::new();
    let t = std::time::Instant::now();
    while !until(&seen) && t.elapsed() < T {
        let Ok(f) = rx.recv_timeout(Duration::from_millis(100)) else {
            continue;
        };
        let f: J = serde_json::from_str(&f).unwrap();
        if f["type"] == "event" && f["event"]["topic"] == "permit_join" {
            seen.push(f["event"]["remaining_s"].as_u64().unwrap());
        }
    }
    seen
}

#[test]
fn permit_join_events_without_ncp_reports() {
    let r = rig(Store::volatile());
    // Like the zigbee-rs NCP image: no EVT_PERMIT_JOIN_STATE at all.
    r.sim.with(|s| s.silent_permit_join = true);
    r.ncp.form(hub_ncp::FormRequest::default()).unwrap();
    let app = App::start(r.ncp.clone(), cfg());
    assert!(app.wait(T, |s| s.endpoint_registered));
    let (sid, rx) = session(&app);
    call(&app, sid, &rx, 1, json!({"type": "subscribe", "topics": ["network"]}));
    // Opened from the API: one event, then one at expiry (no countdown).
    let pj = call(&app, sid, &rx, 2, json!({"type": "network/permit_join", "duration_s": 2}));
    assert_eq!(pj["success"], true, "{pj}");
    assert_eq!(permit_events(&rx, |e| e.last() == Some(&0)), [2, 0]);
    // Re-opened while open.
    call(&app, sid, &rx, 3, json!({"type": "network/permit_join", "duration_s": 30}));
    assert_eq!(permit_events(&rx, |e| !e.is_empty()), [30]);
    call(&app, sid, &rx, 4, json!({"type": "network/permit_join", "duration_s": 60}));
    assert_eq!(permit_events(&rx, |e| !e.is_empty()), [60]);
    // Closed by the NCP (its event, e.g. another commissioning tool).
    let close = zigbee_zrhp::msg::network::EvtPermitJoinState { remaining_s: 0 };
    r.sim.inject_event(ids::EVT_PERMIT_JOIN_STATE, hub_ncp::driver::encode_body(&close).unwrap());
    assert_eq!(permit_events(&rx, |e| !e.is_empty()), [0]);
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

/// A WebSocket-like sink that takes messages in pieces; `busy` stands for
/// bytes the socket has not sent yet.
struct PieceSink {
    tx: mpsc::Sender<(String, Option<(bool, bool)>)>,
    busy: Arc<std::sync::atomic::AtomicUsize>,
    held: usize,
    closed: Arc<AtomicBool>,
}

impl crate::SessionSink for PieceSink {
    fn send(&mut self, text: &str) -> bool {
        self.tx.send((text.to_string(), None)).is_ok()
    }
    fn pieces(&self) -> Option<crate::Pieces> {
        Some(crate::Pieces { size: 16, backlog: 0, held: self.held })
    }
    fn backlog(&self) -> usize {
        self.busy.load(Ordering::SeqCst)
    }
    fn send_piece(&mut self, text: String, first: bool, last: bool) -> bool {
        self.tx.send((text, Some((first, last)))).is_ok()
    }
    fn close(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

#[test]
fn snapshot_waits_for_the_sink_and_later_frames_wait_for_the_snapshot() {
    let r = rig(Store::volatile());
    let app = App::start(r.ncp.clone(), cfg());
    let (tx, rx) = mpsc::channel();
    let busy = Arc::new(std::sync::atomic::AtomicUsize::new(1));
    let sid = app.connect(Box::new(PieceSink { tx, busy: busy.clone(), held: 64 * 1024, closed: Arc::default() })).unwrap();
    let next = || rx.recv_timeout(T).unwrap();
    assert!(next().0.contains("auth_required"));
    app.text(sid, json!({"type": "auth", "token": "secret"}).to_string());
    assert!(next().0.contains("auth_ok"));
    app.text(sid, json!({"id": 1, "type": "subscribe", "topics": ["devices", "entities", "network"]}).to_string());
    let ok: J = serde_json::from_str(&next().0).unwrap();
    assert_eq!((ok["id"].clone(), ok["success"].clone()), (json!(1), json!(true)));
    app.text(sid, json!({"id": 2, "type": "hub/info"}).to_string());
    // The socket is busy: neither a piece nor the later result goes.
    assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    busy.store(0, Ordering::SeqCst);
    let mut text = String::new();
    let mut pieces = 0;
    loop {
        let (piece, flags) = next();
        let (first, last) = flags.expect("a whole frame between the pieces of the snapshot");
        assert_eq!(first, pieces == 0);
        text.push_str(&piece);
        pieces += 1;
        if last {
            break;
        }
    }
    assert!(pieces > 2, "{pieces} pieces");
    let snap: J = serde_json::from_str(&text).unwrap();
    assert_eq!(snap["event"]["topic"], "snapshot");
    let (info, flags) = next();
    assert!(flags.is_none());
    assert_eq!(serde_json::from_str::<J>(&info).unwrap()["id"], 2);
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

#[test]
fn too_much_held_behind_a_stuck_snapshot_drops_the_session() {
    let r = rig(Store::volatile());
    let app = App::start(r.ncp.clone(), cfg());
    let (tx, rx) = mpsc::channel();
    let busy = Arc::new(std::sync::atomic::AtomicUsize::new(1));
    let closed = Arc::new(AtomicBool::new(false));
    let sid = app.connect(Box::new(PieceSink { tx, busy, held: 32, closed: closed.clone() })).unwrap();
    app.text(sid, json!({"type": "auth", "token": "secret"}).to_string());
    app.text(sid, json!({"id": 1, "type": "subscribe", "topics": ["devices"]}).to_string());
    assert!(app.wait(T, |s| s.sessions == 1));
    app.text(sid, json!({"id": 2, "type": "hub/info"}).to_string());
    assert!(app.wait(T, |s| s.sessions == 0), "{:?}", app.status());
    assert!(closed.load(Ordering::SeqCst), "the connection of a dropped session stays open");
    drop(rx);
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

/// A connection that goes away without a close notification.
struct Vanishing(Arc<std::sync::atomic::AtomicBool>);

impl crate::SessionSink for Vanishing {
    fn send(&mut self, _text: &str) -> bool {
        true
    }
    fn alive(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[test]
fn silently_closed_connections_lose_their_session() {
    let r = rig(Store::volatile());
    let app = App::start(r.ncp.clone(), cfg());
    let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
    assert!(app.connect(Box::new(Vanishing(alive.clone()))).is_some());
    assert!(app.wait(T, |s| s.sessions == 1));
    alive.store(false, Ordering::SeqCst);
    assert!(app.wait(T, |s| s.sessions == 0), "{:?}", app.status());
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

/// A sink that, like esp_http_server's detached sender, needs the
/// transport's thread (here: a mutex the "server thread" holds).
struct NeedsServerThread(Arc<std::sync::Mutex<()>>);

impl crate::SessionSink for NeedsServerThread {
    fn send(&mut self, _text: &str) -> bool {
        drop(self.0.lock().unwrap());
        true
    }
}

#[test]
fn connect_never_waits_for_the_app_thread() {
    let r = rig(Store::volatile());
    let app = App::start(r.ncp.clone(), cfg());
    let server = Arc::new(std::sync::Mutex::new(()));
    let (done_tx, done_rx) = mpsc::channel();
    let (a, s) = (app.clone(), server.clone());
    std::thread::spawn(move || {
        // The server thread is busy in its handler (holds the lock) while
        // the app thread blocks sending the first session's hello to it.
        let _busy = s.lock().unwrap();
        let first = a.connect(Box::new(NeedsServerThread(s.clone())));
        std::thread::sleep(Duration::from_millis(100));
        let second = a.connect(Box::new(NeedsServerThread(s.clone())));
        done_tx.send((first, second)).unwrap();
    });
    let (first, second) = done_rx.recv_timeout(T).expect("connect() blocked on the app thread");
    assert!(first.is_some() && second.is_some() && first != second);
    assert!(app.wait(T, |st| st.sessions == 2));
    app.shutdown();
    r.ncp.shutdown();
    r.sim.stop();
}

// ── Maintenance API (admin) ─────────────────────────────────────────────

#[derive(Default)]
struct TestPlatform {
    token: std::sync::Mutex<Option<String>>,
    wifi: std::sync::Mutex<Option<(String, String)>>,
    reboots: AtomicU32,
    ring: Option<crate::LogRing>,
    /// Free heap the platform reports (None: unknown).
    heap: std::sync::Mutex<Option<(usize, usize)>>,
}

impl crate::Platform for TestPlatform {
    fn heap(&self) -> Option<(usize, usize)> {
        *self.heap.lock().unwrap()
    }
    fn system(&self) -> J {
        json!({"version": "test", "uptime_s": 12, "heap": {"free": 65000, "min_free": 57000}})
    }
    fn wifi_channel(&self) -> Option<u8> {
        Some(6)
    }
    fn set_wifi(&self, ssid: &str, password: &str) -> Result<(), String> {
        *self.wifi.lock().unwrap() = Some((ssid.into(), password.into()));
        Ok(())
    }
    fn save_token(&self, token: &str) -> Result<(), String> {
        *self.token.lock().unwrap() = Some(token.into());
        Ok(())
    }
    fn reboot(&self) -> Result<(), String> {
        self.reboots.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn random(&self, buf: &mut [u8]) {
        for (i, b) in buf.iter_mut().enumerate() {
            *b = 0xA0 + i as u8;
        }
    }
    fn logs(&self, max: usize) -> Vec<String> {
        self.ring.as_ref().map(|r| r.lines(max)).unwrap_or_default()
    }
    fn features(&self) -> Vec<&'static str> {
        vec!["wifi", "reboot", "logs", "token_rotate"]
    }
}

fn admin_app(r: &Rig, platform: Option<Arc<TestPlatform>>) -> App {
    let mut c = cfg();
    c.platform = platform.map(|p| p as Arc<dyn crate::Platform>);
    let app = App::start(r.ncp.clone(), c);
    assert!(app.wait(T, |s| s.endpoint_registered));
    app
}

/// A background job: the start answer, then its final `job_result`
/// (shaped like a result frame for `ok`/`err_code`); `progress` collects
/// the `job_progress` events and partial results seen meanwhile.
fn job_with(app: &App, sid: u64, rx: &Receiver<String>, id: u64, body: J, progress: &mut Vec<J>) -> J {
    let start = call(app, sid, rx, id, body);
    let Some(job) = start["result"]["job"].as_u64() else {
        return start;
    };
    loop {
        let f: J = serde_json::from_str(&rx.recv_timeout(T).unwrap()).unwrap();
        if f["type"] == "job_result" && f["job"] == job {
            if f["partial"] == true {
                progress.push(f);
                continue;
            }
            return match f.get("error") {
                Some(e) => json!({"success": false, "error": e}),
                None => json!({"success": true, "result": f["result"]}),
            };
        }
        if f["type"] == "event" && f["event"]["job"] == job {
            progress.push(f["event"].clone());
        }
    }
}

fn job(app: &App, sid: u64, rx: &Receiver<String>, id: u64, body: J) -> J {
    job_with(app, sid, rx, id, body, &mut Vec::new())
}

pub(crate) fn ok(f: &J) -> &J {
    assert_eq!(f["success"], true, "{f}");
    &f["result"]
}

fn err_code(f: &J) -> String {
    assert_eq!(f["success"], false, "{f}");
    f["error"]["code"].as_str().unwrap().to_string()
}

pub(crate) fn form(app: &App, sid: u64, rx: &Receiver<String>, channel: u8) -> J {
    let f = call(app, sid, rx, 900, json!({"type": "network/reset", "confirm": "RESET", "channel": channel}));
    ok(&f).clone()
}

#[test]
fn maintenance_requests_need_an_authenticated_session() {
    let r = rig(Store::volatile());
    let app = admin_app(&r, None);
    let (tx, rx) = mpsc::channel();
    let sid = app.connect(Box::new(tx)).unwrap();
    let _hello = rx.recv_timeout(T).unwrap();
    app.text(sid, json!({"id": 1, "type": "network/reset", "confirm": "RESET"}).to_string());
    let f: J = serde_json::from_str(&rx.recv_timeout(T).unwrap()).unwrap();
    assert_eq!(f["type"], "auth_invalid");
    std::thread::sleep(Duration::from_millis(100));
    assert!(!r.sim.state().requests.iter().any(|(id, _)| *id == ids::NWK_FORM));
}

#[test]
fn energy_scan_channel_change_tx_power_and_diagnostics() {
    let r = rig(Store::volatile());
    let app = admin_app(&r, Some(Arc::new(TestPlatform::default())));
    let (sid, rx) = session(&app);
    let formed = form(&app, sid, &rx, 15);
    assert_eq!(formed["network"]["channel"], 15);
    assert_eq!(formed["backup"], J::Null, "no network before: nothing to back up");
    let info = call(&app, sid, &rx, 1, json!({"type": "hub/info"}));
    let maint = ok(&info)["maintenance"].as_array().unwrap().clone();
    for f in ["energy_scan", "tx_power", "backup", "restore", "topology", "wifi"] {
        assert!(maint.contains(&json!(f)), "{f} in {maint:?}");
    }

    let scan = job(&app, sid, &rx, 2, json!({"type": "network/energy_scan", "duration_exp": 2}));
    let scan = ok(&scan);
    assert_eq!(scan["channels"].as_array().unwrap().len(), 16);
    assert_eq!(scan["wifi_channel"], 6);
    assert_eq!(scan["current"], 15);
    let overlap: Vec<u64> = scan["channels"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["wifi_overlap"] == true)
        .map(|c| c["channel"].as_u64().unwrap())
        .collect();
    assert_eq!(overlap, vec![16, 17, 18, 19]);
    // The sim's quietest channel, a Light Link primary away from Wi-Fi 6.
    assert_eq!(scan["recommended"], 25, "{scan}");
    // Active scan: the beacons around; our own network is marked.
    let own = r.ncp.status().network.unwrap();
    r.sim.with(|s| {
        s.beacons = vec![
            (20, 0x6A0B, [0xA0; 8], 0, 90, -84, true),
            (15, own.pan_id, own.ext_pan_id, 0x1234, 200, -50, false),
            (25, 0x1111, [1; 8], 0, 30, -95, false),
            (20, 0x6A0B, [0xA0; 8], 0x5555, 120, -70, true),
        ];
    });
    let a = job(&app, sid, &rx, 30, json!({"type": "network/active_scan", "channels": [15, 20]}));
    let nets = ok(&a)["networks"].as_array().unwrap().clone();
    assert_eq!(nets.len(), 2, "channel 25 not scanned");
    assert_eq!((nets[0]["pan_id"].as_u64(), nets[0]["permit_join"].as_bool(), nets[0]["own"].as_bool()), (Some(0x6A0B), Some(true), Some(false)));
    assert_eq!(nets[0]["ext_pan_id"], "a0a0a0a0a0a0a0a0");
    // Two routers of that network beaconed: one entry, the better link.
    assert_eq!((nets[0]["beacons"].as_u64(), nets[0]["src"].as_u64(), nets[0]["lqi"].as_u64()), (Some(2), Some(0x5555), Some(120)));
    assert_eq!(nets[1]["own"], true);
    r.sim.with(|s| s.unsupported.push(ids::NWK_ACTIVE_SCAN));
    let a = job(&app, sid, &rx, 31, json!({"type": "network/active_scan"}));
    assert_eq!(err_code(&a), "unsupported");
    let bad = job(&app, sid, &rx, 3, json!({"type": "network/energy_scan", "channels": [10]}));
    assert_eq!(err_code(&bad), "invalid_value");

    let sub = call(&app, sid, &rx, 4, json!({"type": "subscribe", "topics": ["network"]}));
    ok(&sub);
    app.text(sid, json!({"id": 5, "type": "network/change_channel", "channel": 20}).to_string());
    let (mut moved, mut ev) = (J::Null, J::Null);
    while moved.is_null() || ev.is_null() {
        let f: J = serde_json::from_str(&rx.recv_timeout(T).unwrap()).unwrap();
        if f["type"] == "result" && f["id"] == 5 {
            moved = f;
        } else if f["type"] == "event" && f["event"]["topic"] == "network_updated" {
            ev = f;
        }
    }
    assert_eq!(ok(&moved)["channel"], 20);
    assert_eq!(ok(&moved)["nwk_update_id"], 1);
    assert_eq!(ev["event"]["reason"], "channel_changed");
    let net = call(&app, sid, &rx, 6, json!({"type": "network/info"}));
    assert_eq!(ok(&net)["channel"], 20);

    let tx = call(&app, sid, &rx, 7, json!({"type": "network/set_tx_power", "dbm": 25}));
    assert_eq!(ok(&tx)["applied_dbm"], 20);
    assert_eq!(r.ncp.stored_tx_power(), Some(25));
    let net = call(&app, sid, &rx, 70, json!({"type": "network/info"}));
    assert_eq!((ok(&net)["tx_power"].as_i64(), ok(&net)["tx_power_requested"].as_i64()), (Some(20), Some(25)));

    let d = call(&app, sid, &rx, 8, json!({"type": "network/diagnostics"}));
    let d = ok(&d);
    assert_eq!(d["ncp"]["uptime_s"], 3600);
    assert!(d["counters"].as_array().unwrap().iter().any(|c| c["name"] == "tx_frames"), "{d}");
    assert_eq!(d["pta"]["capable"], false);
    assert!(d["host_link"].as_array().unwrap().iter().any(|c| c["name"] == "tx_frames"));
    let d = call(&app, sid, &rx, 9, json!({"type": "network/diagnostics", "reset": true, "groups": 1}));
    assert!(ok(&d)["counters"].as_array().unwrap().iter().all(|c| c["value"] == 0));

    // An NCP image without NWK_SET_TX_POWER.
    r.sim.with(|s| s.unsupported.push(ids::NWK_SET_TX_POWER));
    let tx = call(&app, sid, &rx, 10, json!({"type": "network/set_tx_power", "dbm": 3}));
    assert_eq!(err_code(&tx), "unsupported");
    app.shutdown();
}

#[test]
fn network_map_from_coordinator_tables_and_router_answers() {
    use hub_ncp::sim::SimRemote;
    let r = rig(Store::volatile());
    let app = admin_app(&r, None);
    let (sid, rx) = session(&app);
    form(&app, sid, &rx, 15);
    let (r1, r2, e1, e2) = ([0x11; 8], [0x22; 8], [0xE1; 8], [0xE2; 8]);
    let own = r.ncp.client().session().unwrap().info.as_ref().unwrap().ieee_active;
    r.sim.with(|s| {
        s.neighbors = Some(vec![(r1, 0x1111, 1, 2), (e1, 0x3333, 2, 1)]);
        s.children = Some(vec![(e1, 0x3333, 0x80)]);
        s.routes = vec![(0x2222, 0x1111, 0)];
        s.remotes = vec![
            SimRemote {
                nwk: 0x1111,
                ieee: r1,
                neighbors: vec![(own, 0x0000, 0, 2, 220), (r2, 0x2222, 1, 2, 150), (e2, 0x4444, 2, 1, 90), ([0x99; 8], 0x9999, 2, 4, 10)],
                routes: vec![(0x0000, 0x0000, 0), (0x2222, 0x2222, 0)],
            },
            SimRemote { nwk: 0x2222, ieee: r2, neighbors: vec![(r1, 0x1111, 1, 2, 140)], routes: vec![] },
        ];
    });
    let sub = call(&app, sid, &rx, 1, json!({"type": "subscribe", "topics": ["network"]}));
    ok(&sub);
    let map = job(&app, sid, &rx, 2, json!({"type": "network/topology"}));
    let map = ok(&map).clone();
    let nodes = map["nodes"].as_array().unwrap();
    let nwks: Vec<u64> = nodes.iter().map(|n| n["nwk"].as_u64().unwrap()).collect();
    assert_eq!(nwks, vec![0, 0x1111, 0x2222, 0x3333, 0x4444], "the previous child is left out: {map}");
    let node = |nwk: u64| nodes.iter().find(|n| n["nwk"] == nwk).unwrap();
    assert_eq!(node(0)["type"], "coordinator");
    assert_eq!(node(0x2222)["type"], "router");
    assert_eq!(node(0x2222)["scanned"], true, "found through 0x1111 and asked too");
    assert_eq!(node(0x4444)["ieee"], "e2e2e2e2e2e2e2e2");
    let links = map["links"].as_array().unwrap();
    assert!(links.iter().any(|l| l["source"] == 0x1111 && l["target"] == 0x2222 && l["lqi"] == 150));
    assert!(links.iter().any(|l| l["source"] == 0 && l["target"] == 0x3333 && l["relationship"] == "child"));
    let routes = map["routes"].as_array().unwrap();
    assert!(routes.iter().any(|x| x["source"] == 0 && x["dst"] == 0x2222 && x["next_hop"] == 0x1111));
    assert!(routes.iter().any(|x| x["source"] == 0x1111 && x["dst"] == 0x2222));
    // The cached map without a new scan.
    let aps = r.sim.state().aps.len();
    let cached = call(&app, sid, &rx, 3, json!({"type": "network/topology", "refresh": false}));
    assert_eq!(ok(&cached)["nodes"], map["nodes"]);
    assert_eq!(r.sim.state().aps.len(), aps, "no requests for the cached map");
    app.shutdown();
}

#[test]
fn backup_then_restore_onto_a_fresh_ncp_in_batches() {
    let r = rig(Store::volatile());
    let app = admin_app(&r, None);
    let (sid, rx) = session(&app);
    form(&app, sid, &rx, 17);
    let dev = [0xC4, 0xB3, 0xA2, 0x01, 0x00, 0x8D, 0x15, 0x00];
    r.sim.with(|s| {
        s.table_devices = vec![(dev, 0x1234, Some(0x80))];
        s.link_keys = vec![(dev, [0x42; 16])];
    });
    let b = call(&app, sid, &rx, 1, json!({"type": "network/backup"}));
    let backup = ok(&b).clone();
    assert_eq!(backup["metadata"]["format"], "zigpy/open-coordinator-backup");
    assert_eq!(backup["channel"], 17);
    let key: String = r.sim.state().key.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(backup["network_key"]["key"], key);
    assert_eq!(backup["devices"][0]["ieee_address"], "00158d0001a2b3c4");
    assert_eq!(backup["devices"][0]["link_key"]["key"], "42".repeat(16));
    let last = call(&app, sid, &rx, 2, json!({"type": "network/backup/last"}));
    assert_eq!(ok(&last)["backup"], backup, "kept in the host store");
    // Restoring over a running network needs `force`.
    let refused = call(&app, sid, &rx, 3, json!({"type": "network/restore", "backup": backup}));
    assert_eq!(err_code(&refused), "invalid_value");
    app.shutdown();

    let r2 = rig(Store::volatile());
    let app2 = admin_app(&r2, None);
    let (sid2, rx2) = session(&app2);
    // The backup, then its devices in batches (one here).
    let mut head = backup.clone();
    let devices = head["devices"].take();
    head["devices"] = json!([]);
    let staged = call(&app2, sid2, &rx2, 10, json!({"type": "network/restore", "backup": head, "more": true}));
    assert_eq!(ok(&staged)["staged_devices"], 0);
    let last = call(&app2, sid2, &rx2, 11, json!({"type": "network/restore", "devices": devices}));
    let rep = ok(&last);
    assert_eq!(rep["channel"], 17);
    assert_eq!(rep["children"], 1);
    assert_eq!(rep["link_keys_on_ncp"], 1);
    assert_eq!(rep["previous_network_backed_up"], false);
    let s1 = r.sim.state();
    let s2 = r2.sim.state();
    assert_eq!((s2.pan_id, s2.ext_pan_id, s2.key, s2.channel, s2.network_state), (s1.pan_id, s1.ext_pan_id, s1.key, 17, 2));
    assert!(s2.nwk_limit >= s1.nwk_limit + 10_000);
    let net = call(&app2, sid2, &rx2, 50, json!({"type": "network/info"}));
    assert_eq!(ok(&net)["ownership"], "owned");
    // The restored device is imported and interviewed.
    assert!(app2.wait(T, |s| s.imported == 1), "{:?}", app2.status());
    // A batch without a staged backup is refused.
    let oo = call(&app2, sid2, &rx2, 60, json!({"type": "network/restore", "devices": []}));
    assert_eq!(err_code(&oo), "invalid_value");
    app2.shutdown();
}

#[test]
fn reset_backs_up_leaves_and_forms_a_new_network() {
    let r = rig(Store::volatile());
    let app = admin_app(&r, None);
    let (sid, rx) = session(&app);
    let first = form(&app, sid, &rx, 15);
    let wrong = call(&app, sid, &rx, 1, json!({"type": "network/reset", "confirm": "reset"}));
    assert_eq!(err_code(&wrong), "invalid_value");
    let f = call(
        &app,
        sid,
        &rx,
        2,
        json!({"type": "network/reset", "confirm": "RESET", "channels": [25], "pan_id": 0x1A2B, "ext_pan_id": "00112233445566ff", "tx_power": 4}),
    );
    let res = ok(&f);
    assert_eq!(res["network"]["channel"], 25);
    assert_eq!(res["network"]["pan_id"], 0x1A2B);
    assert_eq!(res["network"]["ext_pan_id"], "00112233445566ff");
    assert_eq!(res["backup"]["pan_id"], format!("{:04x}", first["network"]["pan_id"].as_u64().unwrap()), "the old network was backed up");
    assert!(r.sim.state().log.iter().any(|(id, _)| *id == ids::NWK_LEAVE_RESET));
    assert_eq!(r.sim.state().tx_power, 4);
    assert_eq!(r.ncp.store_state().network.unwrap().pan_id, 0x1A2B);
    app.shutdown();
}

#[test]
fn hub_settings_through_the_platform() {
    let r = rig(Store::volatile());
    let ring = crate::LogRing::new(1024);
    ring.push("INFO hub: started");
    let p = Arc::new(TestPlatform { ring: Some(ring), ..TestPlatform::default() });
    let app = admin_app(&r, Some(p.clone()));
    let (sid, rx) = session(&app);
    let sys = call(&app, sid, &rx, 1, json!({"type": "hub/system"}));
    let sys = ok(&sys);
    assert_eq!(sys["heap"]["free"], 65000);
    assert_eq!(sys["app"]["sessions"], 1);
    assert_eq!(sys["ncp"]["fw"], "sim-ncp 0.1.0");
    let logs = call(&app, sid, &rx, 2, json!({"type": "hub/logs", "lines": 5}));
    assert_eq!(ok(&logs)["lines"], json!(["INFO hub: started"]));
    let w = call(&app, sid, &rx, 3, json!({"type": "hub/wifi/set", "ssid": "home", "password": "short"}));
    assert_eq!(err_code(&w), "invalid_value");
    let w = call(&app, sid, &rx, 4, json!({"type": "hub/wifi/set", "ssid": "home", "password": "longenough"}));
    ok(&w);
    assert_eq!(p.wifi.lock().unwrap().clone(), Some(("home".into(), "longenough".into())));
    let t = call(&app, sid, &rx, 5, json!({"type": "hub/token/rotate"}));
    let token = ok(&t)["token"].as_str().unwrap().to_string();
    assert_eq!(token, "a0a1a2a3a4a5a6a7a8a9aaab");
    assert_eq!(p.token.lock().unwrap().as_deref(), Some(token.as_str()));
    // A new connection needs the new token; this one stays.
    let (tx2, rx2) = mpsc::channel();
    let s2 = app.connect(Box::new(tx2)).unwrap();
    let _ = rx2.recv_timeout(T).unwrap();
    app.text(s2, json!({"type": "auth", "token": "secret"}).to_string());
    let f: J = serde_json::from_str(&rx2.recv_timeout(T).unwrap()).unwrap();
    assert_eq!(f["type"], "auth_invalid");
    let (tx3, rx3) = mpsc::channel();
    let s3 = app.connect(Box::new(tx3)).unwrap();
    let _ = rx3.recv_timeout(T).unwrap();
    app.text(s3, json!({"type": "auth", "token": token}).to_string());
    let f: J = serde_json::from_str(&rx3.recv_timeout(T).unwrap()).unwrap();
    assert_eq!(f["type"], "auth_ok");
    let still = call(&app, sid, &rx, 6, json!({"type": "hub/reboot"}));
    assert_eq!(ok(&still)["rebooting"], true);
    assert_eq!(p.reboots.load(Ordering::SeqCst), 1);
    let m = call(&app, sid, &rx, 7, json!({"type": "hub/mode/set", "mode": "bridge"}));
    assert_eq!(err_code(&m), "failed", "the test platform has no boot modes");
    app.shutdown();
}

#[test]
fn hub_settings_without_a_platform_are_unsupported() {
    let r = rig(Store::volatile());
    let app = admin_app(&r, None);
    let (sid, rx) = session(&app);
    let f = call(&app, sid, &rx, 1, json!({"type": "hub/token/rotate"}));
    assert_eq!(err_code(&f), "unsupported");
    let f = call(&app, sid, &rx, 2, json!({"type": "hub/system"}));
    assert!(ok(&f)["app"].is_object());
    app.shutdown();
}

#[test]
fn cached_network_map_is_null_before_the_first_scan() {
    let r = rig(Store::volatile());
    let app = admin_app(&r, None);
    let (sid, rx) = session(&app);
    let f = call(&app, sid, &rx, 1, json!({"type": "network/topology", "refresh": false}));
    assert_eq!(ok(&f), &J::Null);
    assert!(r.sim.state().aps.iter().all(|p| p.get(5..7) != Some(&[0x31, 0x00])), "no scan started");
    app.shutdown();
}

#[test]
fn large_network_map_and_full_diagnostics_are_valid_frames() {
    use hub_ncp::sim::SimRemote;
    let r = rig(Store::volatile());
    let app = admin_app(&r, None);
    let (sid, rx) = session(&app);
    form(&app, sid, &rx, 15);
    // 8 routers in a chain, each with 6 end devices.
    let ieee = |n: u16| { let mut b = [0x77; 8]; b[..2].copy_from_slice(&n.to_le_bytes()); b };
    r.sim.with(|s| {
        s.neighbors = Some(vec![(ieee(0x100), 0x100, 1, 2)]);
        s.remotes = (0..8u16)
            .map(|i| {
                let nwk = 0x100 + i;
                let mut nb: Vec<_> = (0..6u16).map(|e| (ieee(0x1000 + i * 16 + e), 0x1000 + i * 16 + e, 2, 1, 100)).collect();
                if i < 7 {
                    nb.push((ieee(nwk + 1), nwk + 1, 1, 2, 150));
                }
                SimRemote { nwk, ieee: ieee(nwk), neighbors: nb, routes: vec![(0, 0, 0)] }
            })
            .collect();
        s.counters = (0x0101u16..0x0119).chain(0x0201..0x0209).chain(0x0301..0x0307).map(|i| (i, 7)).collect();
    });
    let map = job(&app, sid, &rx, 1, json!({"type": "network/topology"}));
    let map = ok(&map);
    assert_eq!(map["nodes"].as_array().unwrap().len(), 1 + 8 + 48);
    assert!(map["nodes"].as_array().unwrap().iter().filter(|n| n["type"] == "router").all(|n| n["scanned"] == true));
    let d = call(&app, sid, &rx, 2, json!({"type": "network/diagnostics"}));
    let d = ok(&d);
    assert_eq!(d["counters"].as_array().unwrap().len(), 38);
    assert!(d["counters"].as_array().unwrap().iter().any(|c| c["name"] == "mac_tx_no_ack" && c["group"] == "mac"));
    app.shutdown();
}

#[test]
fn restore_of_many_devices_in_batches() {
    let r = rig(Store::volatile());
    let app = admin_app(&r, None);
    let (sid, rx) = session(&app);
    let devices: Vec<J> = (0..45u16)
        .map(
            |i| json!({"ieee_address": format!("00158d00{:08x}", 0x1000 + u32::from(i)), "nwk_address": format!("{:04x}", 0x2000 + i), "is_child": i % 3 == 0}),
        )
        .collect();
    let backup = json!({
        "metadata": {"format": "zigpy/open-coordinator-backup", "version": 1},
        "coordinator_ieee": "020100000000515a", "pan_id": "1234", "extended_pan_id": "00112233aabbccdd",
        "channel": 20, "channel_mask": [20], "security_level": 5,
        "network_key": {"key": "0102030405060708090a0b0c0d0e0f10", "sequence_number": 0, "frame_counter": 5000},
        "devices": devices[..20],
    });
    let f = call(&app, sid, &rx, 1, json!({"type": "network/restore", "backup": backup, "more": true}));
    assert_eq!(ok(&f)["staged_devices"], 20);
    let f = call(&app, sid, &rx, 2, json!({"type": "network/restore", "devices": devices[20..40], "more": true}));
    assert_eq!(ok(&f)["staged_devices"], 40);
    let f = call(&app, sid, &rx, 3, json!({"type": "network/restore", "devices": devices[40..]}));
    assert_eq!(ok(&f)["devices"], 45);
    assert_eq!(ok(&f)["children"], 15);
    let st = r.sim.state();
    assert_eq!((st.pan_id, st.channel, st.table_devices.len()), (0x1234, 20, 45));
    app.shutdown();
}

#[test]
fn scans_are_background_jobs_that_leave_the_api_usable() {
    let r = rig(Store::volatile());
    let app = admin_app(&r, Some(Arc::new(TestPlatform::default())));
    let (sid, rx) = session(&app);
    form(&app, sid, &rx, 15);
    ok(&call(&app, sid, &rx, 1, json!({"type": "subscribe", "topics": ["network"]})));
    // Each pass takes 0.8 s: long enough to look at the job meanwhile.
    r.sim.with(|s| s.scan_time = Duration::from_millis(800));
    let start = call(&app, sid, &rx, 2, json!({"type": "network/energy_scan", "passes": 3}));
    let job = ok(&start)["job"].as_u64().unwrap();
    assert_eq!(ok(&start)["passes"], 3);

    // The rest of the API answers; only another radio job is busy.
    let info = call(&app, sid, &rx, 3, json!({"type": "network/info"}));
    ok(&info);
    let d = call(&app, sid, &rx, 4, json!({"type": "network/diagnostics"}));
    ok(&d);
    let again = call(&app, sid, &rx, 5, json!({"type": "network/active_scan"}));
    assert_eq!(err_code(&again), "busy");
    // A second client (a reconnecting UI) finds the running job.
    let (sid2, rx2) = session(&app);
    let jobs = call(&app, sid2, &rx2, 1, json!({"type": "network/jobs"}));
    let running = ok(&jobs)["running"].as_array().unwrap().clone();
    assert_eq!((running.len(), running[0]["job"].as_u64(), running[0]["kind"].as_str()), (1, Some(job), Some("energy_scan")));
    assert_eq!(running[0]["passes"], 3);

    // Progress events (once a second) and a partial result after pass 1;
    // then cancel: the scan stops after the current pass.
    let mut progress = Vec::new();
    let mut partial = None;
    while partial.is_none() {
        let f: J = serde_json::from_str(&rx.recv_timeout(T).unwrap()).unwrap();
        if f["type"] == "job_result" && f["partial"] == true {
            partial = Some(f);
        } else if f["type"] == "event" && f["event"]["topic"] == "job_progress" {
            progress.push(f["event"].clone());
        }
    }
    let partial = partial.unwrap();
    assert_eq!((partial["job"].as_u64(), partial["result"]["passes"].as_u64()), (Some(job), Some(1)));
    assert!(progress.iter().all(|p| p["job"] == job && p["progress"].as_f64().unwrap() <= 1.0), "{progress:?}");
    let c = call(&app, sid, &rx, 6, json!({"type": "network/job/cancel", "job": job}));
    assert_eq!(ok(&c)["cancelling"], true);
    let (mut result, mut done) = (None, None);
    while result.is_none() || done.is_none() {
        let f: J = serde_json::from_str(&rx.recv_timeout(T).unwrap()).unwrap();
        if f["type"] == "job_result" && f["partial"] == false {
            result = Some(f);
        } else if f["type"] == "event" && f["event"]["topic"] == "job_done" {
            done = Some(f["event"].clone());
        }
    }
    let (result, done) = (result.unwrap(), done.unwrap());
    assert_eq!(done["cancelled"], true);
    assert_eq!(result["result"]["cancelled"], true);
    assert!(result["result"]["passes"].as_u64().unwrap() < 3, "{result}");
    assert_eq!(result["result"]["channels"].as_array().unwrap().len(), 16);
    // Kept for clients that come later; the job is gone from the board.
    let kept = call(&app, sid2, &rx2, 2, json!({"type": "network/job/result", "kind": "energy_scan"}));
    assert_eq!((ok(&kept)["job"].as_u64(), ok(&kept)["result"]["cancelled"].as_bool()), (Some(job), Some(true)));
    let jobs = call(&app, sid2, &rx2, 3, json!({"type": "network/jobs"}));
    assert_eq!(ok(&jobs)["running"], json!([]));
    let gone = call(&app, sid, &rx, 7, json!({"type": "network/job/cancel", "job": job}));
    assert_eq!(err_code(&gone), "not_found");

    // A failing job reports to its session and as `job_failed`.
    r.sim.with(|s| {
        s.scan_time = Duration::from_millis(20);
        s.unsupported.push(ids::NWK_ACTIVE_SCAN);
    });
    let a = self::job(&app, sid, &rx, 8, json!({"type": "network/active_scan"}));
    assert_eq!(err_code(&a), "unsupported");
}

#[test]
fn jobs_run_on_the_worker_and_only_a_second_one_needs_heap() {
    let r = rig(Store::volatile());
    let p = Arc::new(TestPlatform::default());
    let app = admin_app(&r, Some(p.clone()));
    let (sid, rx) = session(&app);
    form(&app, sid, &rx, 15);
    // Fragmented heap: no block for a job stack. The worker's stack
    // exists already, so jobs one at a time still run, again and again.
    *p.heap.lock().unwrap() = Some((30 * 1024, 8 * 1024));
    for i in 1..=5 {
        ok(&call(&app, sid, &rx, i, json!({"type": "network/diagnostics"})));
    }
    // A second job while the worker scans needs a thread of its own.
    r.sim.with(|s| s.scan_time = Duration::from_millis(1500));
    let s = call(&app, sid, &rx, 10, json!({"type": "network/energy_scan"}));
    ok(&s);
    let d = call(&app, sid, &rx, 11, json!({"type": "network/diagnostics"}));
    assert_eq!(err_code(&d), "busy");
    assert!(d["error"]["message"].as_str().unwrap().contains("low memory (30 KB free)"), "{d}");
    *p.heap.lock().unwrap() = Some((60 * 1024, 31 * 1024));
    ok(&call(&app, sid, &rx, 12, json!({"type": "network/diagnostics"})));
    // Requests that need no job thread are not affected.
    *p.heap.lock().unwrap() = Some((4 * 1024, 4 * 1024));
    ok(&call(&app, sid, &rx, 13, json!({"type": "network/jobs"})));
}

#[test]
fn time_zone_and_servers_are_set_kept_and_reported() {
    let store = Store::volatile();
    let r = rig(store);
    let app = admin_app(&r, Some(Arc::new(TestPlatform::default())));
    let (sid, rx) = session(&app);
    let t = call(&app, sid, &rx, 1, json!({"type": "hub/time"}));
    let t = ok(&t);
    assert_eq!((t["time_synced"].as_bool(), t["tz"].as_str(), t["servers"][0].as_str()), (Some(true), Some("UTC0"), Some("pool.ntp.org")));
    let set = call(&app, sid, &rx, 2, json!({"type": "hub/time/set", "tz": "EET-2EEST,M3.5.0/3,M10.5.0/4", "servers": ["time.cloudflare.com", "pool.ntp.org"]}));
    let set = ok(&set).clone();
    assert!([7200, 10_800].contains(&set["tz_offset_s"].as_i64().unwrap()), "{set}");
    assert_eq!(set["servers"], json!(["time.cloudflare.com", "pool.ntp.org"]));
    assert!(set["dst"]["start"].as_i64().unwrap() < set["dst"]["end"].as_i64().unwrap());
    assert_eq!(set["local"].as_str().unwrap().len(), 19);
    let bad = call(&app, sid, &rx, 3, json!({"type": "hub/time/set", "tz": "Europe/Kyiv"}));
    assert_eq!(err_code(&bad), "invalid_value");
    let bad = call(&app, sid, &rx, 4, json!({"type": "hub/time/set", "servers": []}));
    assert_eq!(err_code(&bad), "invalid_value");
    // Kept in the host store; network/info carries the wall clock.
    let kept = r.ncp.with_store(|s| s.kv_get(crate::admin::TZ_KEY).map(<[u8]>::to_vec));
    assert_eq!(kept.as_deref(), Some(&b"EET-2EEST,M3.5.0/3,M10.5.0/4"[..]));
    std::thread::sleep(Duration::from_millis(1100));
    let n = call(&app, sid, &rx, 5, json!({"type": "network/info"}));
    let n = ok(&n);
    assert_eq!(n["time_synced"], true);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
    assert!((n["time_utc"].as_f64().unwrap() - now).abs() < 5.0, "{n}");
}

#[test]
fn last_values_are_kept_over_a_restart() {
    use zigbee_zrhp::codec::Bytes16;
    use zigbee_zrhp::msg::data::{EvtApsDataIndication, indication_flags};
    let flash: Box<dyn hub_ncp::store::Flash> = Box::new(MemFlash::new(32, 4096));
    let r = rig(Store::open(flash).unwrap());
    let app = App::start(r.ncp.clone(), cfg());
    assert!(app.wait(T, |s| s.endpoint_registered));
    join(&r.sim);
    assert!(app.wait(T, |s| s.devices == 1));
    // A frame from the device: its LQI and last_seen.
    let ind = EvtApsDataIndication {
        dst_mode: 0x02,
        dst: 0,
        dst_ep: 1,
        src_nwk: 0x1234,
        src_ieee: Ieee::from_u64(DEV),
        src_ep: 1,
        profile: 0x0104,
        cluster: 0x0402,
        flags: indication_flags::SRC_IEEE_VALID,
        lqi: 177,
        rssi: -61,
        last_hop_nwk: 0x1234,
        aps_counter: 1,
        key_source: 0,
        rx_time_ms: 5,
        asdu: Bytes16(&[0x18, 0x01, 0x0A, 0x00, 0x00, 0x29, 0x59, 0x08]),
    };
    r.sim.inject_event(ids::EVT_APS_DATA_INDICATION, hub_ncp::driver::encode_body(&ind).unwrap());
    let (sid, rx) = session(&app);
    let t = std::time::Instant::now();
    while call(&app, sid, &rx, 1, json!({"type": "devices/get", "device": "zb:00158d0001a2b3c4"}))["result"]["lqi"] != 177 {
        assert!(t.elapsed() < T, "no LQI");
        std::thread::sleep(Duration::from_millis(50));
    }
    // A clean stop writes them at once (else every 10 min).
    app.shutdown();
    let kept = r.ncp.with_store(|s| s.kv_get("st/zb:00158d0001a2b3c4").map(<[u8]>::to_vec)).expect("kept");
    let kept: J = serde_json::from_slice(&kept).unwrap();
    assert_eq!((kept["lqi"].as_u64(), kept["rssi"].as_i64()), (Some(177), Some(-61)));
    assert!(kept["seen"].as_i64().unwrap() > 1_700_000_000_000, "Unix ms: {kept}");

    let app = App::start(r.ncp.clone(), cfg());
    assert!(app.wait(T, |s| s.restored == 1 && s.devices == 1));
    let (sid, rx) = session(&app);
    let dev = call(&app, sid, &rx, 1, json!({"type": "devices/get", "device": "zb:00158d0001a2b3c4"}));
    assert_eq!(dev["result"]["lqi"], 177, "{dev}");
    let sys = call(&app, sid, &rx, 2, json!({"type": "hub/system"}));
    assert!(sys["result"]["app"]["last_values"][1].is_u64(), "{sys}");
    app.shutdown();
}
