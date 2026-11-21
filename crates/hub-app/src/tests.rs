//! The app on the host core against the simulated NCP.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
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

const T: Duration = Duration::from_secs(10);
const DEV: u64 = 0x00158d0001a2b3c4;

fn rng() -> Rng {
    let n = Arc::new(AtomicU32::new(7));
    Arc::new(move |buf: &mut [u8]| {
        for b in buf {
            *b = (n.fetch_add(0x9E37_79B9, Ordering::Relaxed) >> 11) as u8;
        }
    })
}

struct Rig {
    ncp: Ncp,
    sim: SimNcp,
}

fn rig(store: Store) -> Rig {
    let (host, ncp_end) = pipe::pair();
    let sim = SimNcp::start(ncp_end);
    let (rx, tx) = host.split();
    let mut cfg = HubConfig::default();
    cfg.driver.lines = vec![Line::BR_DEFAULT];
    let ncp = Ncp::start(cfg, rx, tx, None, store, rng());
    assert!(ncp.wait_ready(T));
    Rig { ncp, sim }
}

fn session(app: &App) -> (u64, Receiver<String>) {
    let (tx, rx) = mpsc::channel();
    let sid = app.connect(Box::new(tx)).unwrap();
    let hello: J = serde_json::from_str(&rx.recv_timeout(T).unwrap()).unwrap();
    assert_eq!(hello["type"], "auth_required");
    app.text(sid, json!({"type": "auth", "token": "secret"}).to_string());
    let ok: J = serde_json::from_str(&rx.recv_timeout(T).unwrap()).unwrap();
    assert_eq!(ok["type"], "auth_ok", "{ok}");
    (sid, rx)
}

fn call(app: &App, sid: u64, rx: &Receiver<String>, id: u64, mut body: J) -> J {
    body["id"] = id.into();
    app.text(sid, body.to_string());
    loop {
        let f: J = serde_json::from_str(&rx.recv_timeout(T).unwrap()).unwrap();
        if f["type"] == "result" && f["id"] == id {
            return f;
        }
    }
}

fn cfg() -> AppConfig {
    AppConfig { api_token: "secret".into(), ..AppConfig::default() }
}

fn join(sim: &SimNcp) {
    let ev = EvtDeviceJoined {
        nwk: 0x1234,
        ieee: Ieee::from_u64(DEV),
        parent_nwk: 0x0000,
        join_type: 0,
        capability: 0x80,
        decision: 0,
    };
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

fn permit_events(rx: &Receiver<String>, until: impl Fn(&[u64]) -> bool) -> Vec<u64> {
    let mut seen = Vec::new();
    let t = std::time::Instant::now();
    while !until(&seen) && t.elapsed() < T {
        let Ok(f) = rx.recv_timeout(Duration::from_millis(100)) else { continue };
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
}

impl crate::Platform for TestPlatform {
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

fn ok(f: &J) -> &J {
    assert_eq!(f["success"], true, "{f}");
    &f["result"]
}

fn err_code(f: &J) -> String {
    assert_eq!(f["success"], false, "{f}");
    f["error"]["code"].as_str().unwrap().to_string()
}

fn form(app: &App, sid: u64, rx: &Receiver<String>, channel: u8) -> J {
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

    let scan = call(&app, sid, &rx, 2, json!({"type": "network/energy_scan", "duration_exp": 2}));
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
    let bad = call(&app, sid, &rx, 3, json!({"type": "network/energy_scan", "channels": [10]}));
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
    let map = call(&app, sid, &rx, 2, json!({"type": "network/topology"}));
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
fn backup_then_restore_onto_a_fresh_ncp_in_chunks() {
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
    let text = backup.to_string();
    let parts: Vec<&str> = text.as_bytes().chunks(300).map(|c| std::str::from_utf8(c).unwrap()).collect();
    let mut offset = 0;
    let mut last = J::Null;
    for (i, p) in parts.iter().enumerate() {
        last = call(&app2, sid2, &rx2, 10 + i as u64, json!({"type": "network/restore", "chunk": p, "offset": offset, "total": text.len()}));
        offset += p.len();
        if i + 1 < parts.len() {
            assert_eq!(ok(&last)["received"], offset);
        }
    }
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
    // A chunk out of order is refused.
    let oo = call(&app2, sid2, &rx2, 60, json!({"type": "network/restore", "chunk": "{}", "offset": 5, "total": 10}));
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
    let map = call(&app, sid, &rx, 1, json!({"type": "network/topology"}));
    let map = ok(&map);
    assert_eq!(map["nodes"].as_array().unwrap().len(), 1 + 8 + 48);
    assert!(map["nodes"].as_array().unwrap().iter().filter(|n| n["type"] == "router").all(|n| n["scanned"] == true));
    let d = call(&app, sid, &rx, 2, json!({"type": "network/diagnostics"}));
    let d = ok(&d);
    assert_eq!(d["counters"].as_array().unwrap().len(), 38);
    assert!(d["counters"].as_array().unwrap().iter().any(|c| c["name"] == "mac_tx_no_ack" && c["group"] == "mac"));
    app.shutdown();
}
