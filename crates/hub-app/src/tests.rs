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
