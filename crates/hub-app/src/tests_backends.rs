//! One suite, run on both coordinator backends: the app on the ZRHP host
//! core and on the EZSP backend, each with its simulated NCP and a device
//! of the engine's device simulator behind it. Join → interview → command
//! → report, groups, permit join, forming and resetting the network; on
//! EZSP also device leaves and the `NcpNv` ownership decision (design §2.5,
//! §2.7): unverified records become the hub's once the NCP's network
//! matches their tag, another network's are foreign (no device traffic)
//! until adopted or cleared, records of unknown origin stay closed until
//! the operator decides.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hub_coord::{BackendKind, NetworkIdentity, OwnerTag};
use hub_devices::testutil::sim::SimDevice;
use hub_devices::transport::{ApsDst, ApsIndication, ApsRequest, TxOptions};
use hub_devices::zcl::{TypedValue, ZclType, ZclValue};
use hub_devices::zdo::LogicalType;
use hub_ezsp::ezsp::command::{Callback, Command};
use hub_ezsp::ezsp::{
    ApsFrame, ApsOptions, DeviceUpdate, Eui64, IncomingMessageType, JoinDecision, OutgoingMessageType,
};
use hub_store::{Flash, KvStore, MemFlash, OWNER_KEY, Store, kv};
use serde_json::{Value as J, json};

use crate::tests::{T, call, cfg, ok, session};
use crate::{App, Backend};

const PLUG: u64 = 0x5A52_5300_0000_0001;
const PLUG_NWK: u16 = 0x5A01;
const SWITCH: &str = "zb:5a52530000000001:1:switch";

fn plug() -> SimDevice {
    SimDevice::new(PLUG, PLUG_NWK, LogicalType::Router, true, 0x1234)
        .endpoint(1, 0x0104, 0x0051, &[0x0000, 0x0003, 0x0006], &[])
        .basic("zigbee-rs", "SIM-PLUG", 1)
        .attr(1, 0x0006, 0x0000, TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false))))
}

/// A simulated NCP as the suite drives it.
trait SimLink: Send + Sync {
    /// Unicasts to a short address the NCP sent since the last call.
    fn requests(&self) -> Vec<ApsRequest>;
    /// A device's frame reaches the NCP.
    fn indicate(&self, i: &ApsIndication);
    /// A device joins through the trust center.
    fn join(&self, ieee: u64, nwk: u16);
    /// The trust center reports that a device left (EZSP only).
    fn leave(&self, ieee: u64, nwk: u16) {
        let _ = (ieee, nwk);
    }
    /// Joining is open on the NCP.
    fn permit_open(&self) -> bool;
    /// Groups the hub endpoint is a member of on the NCP.
    fn groups(&self) -> Vec<u16>;
}

struct Zrhp {
    sim: hub_ncp::sim::SimNcp,
}

impl SimLink for Zrhp {
    fn requests(&self) -> Vec<ApsRequest> {
        use zigbee_zrhp::codec::{Reader, Wire};
        use zigbee_zrhp::msg::data::{ApsDataRequest, ApsDst as Dst};
        let raw = self.sim.with(|s| std::mem::take(&mut s.aps));
        raw.iter()
            .filter_map(|p| {
                let r = ApsDataRequest::read(&mut Reader::new(p)).ok()?;
                let Dst::Nwk(nwk) = r.dst else { return None };
                Some(ApsRequest {
                    dst: ApsDst::Nwk(nwk),
                    dst_ep: r.dst_ep,
                    src_ep: r.src_ep,
                    profile: r.profile,
                    cluster: r.cluster,
                    payload: r.asdu.to_vec(),
                    options: TxOptions::default(),
                })
            })
            .collect()
    }

    fn indicate(&self, i: &ApsIndication) {
        let p = hub_ncp::sim::indication(i.src_nwk, i.src_ieee.unwrap_or(0), i.src_ep, i.dst_ep, i.profile, i.cluster, &i.payload);
        self.sim.inject_event(zigbee_zrhp::msg::ids::EVT_APS_DATA_INDICATION, p);
    }

    fn join(&self, ieee: u64, nwk: u16) {
        use zigbee_zrhp::msg::network::EvtDeviceJoined;
        let ev = EvtDeviceJoined {
            nwk,
            ieee: zigbee_zrhp::types::Ieee::from_u64(ieee),
            parent_nwk: 0,
            join_type: 0,
            capability: 0x8E,
            decision: 0,
        };
        self.sim.inject_event(zigbee_zrhp::msg::ids::EVT_DEVICE_JOINED, hub_ncp::driver::encode_body(&ev).unwrap());
    }

    fn permit_open(&self) -> bool {
        self.sim.with(|s| {
            let last = s.trace.iter().flatten().rev().find(|(id, _)| *id == zigbee_zrhp::msg::ids::NWK_PERMIT_JOIN);
            last.is_some_and(|(_, p)| p[0] > 0)
        })
    }

    fn groups(&self) -> Vec<u16> {
        let mut g: Vec<u16> = Vec::new();
        self.sim.with(|s| {
            for (_, p) in s.log.iter().filter(|(id, _)| *id == zigbee_zrhp::msg::ids::NWK_GROUP_MEMBERSHIP) {
                let group = u16::from_le_bytes([p[1], p[2]]);
                match p[0] {
                    1 if !g.contains(&group) => g.push(group),
                    2 => g.retain(|x| *x != group),
                    3 => g.clear(),
                    _ => {}
                }
            }
        });
        g
    }
}

struct Ezsp {
    sim: hub_ezsp::sim::Sim,
    seen: Mutex<usize>,
}

impl SimLink for Ezsp {
    fn requests(&self) -> Vec<ApsRequest> {
        let mut seen = self.seen.lock().unwrap();
        self.sim.ncp(|n| {
            let sent = n.sent();
            let new = sent.get(*seen..).unwrap_or_default();
            *seen = sent.len();
            new.iter()
                .filter_map(|m| match &m.command {
                    Command::SendUnicast { kind, index_or_destination, aps, message, .. } if *kind == OutgoingMessageType::DIRECT => {
                        Some(ApsRequest {
                            dst: ApsDst::Nwk(*index_or_destination),
                            dst_ep: aps.destination_endpoint,
                            src_ep: aps.source_endpoint,
                            profile: aps.profile_id,
                            cluster: aps.cluster_id,
                            payload: message.clone(),
                            options: TxOptions::default(),
                        })
                    }
                    _ => None,
                })
                .collect()
        })
    }

    fn indicate(&self, i: &ApsIndication) {
        let aps = ApsFrame {
            profile_id: i.profile,
            cluster_id: i.cluster,
            source_endpoint: i.src_ep,
            destination_endpoint: i.dst_ep,
            options: ApsOptions::NONE,
            group_id: 0,
            sequence: 0,
        };
        self.sim.ncp(|n| {
            let cb = n.incoming_message(IncomingMessageType::UNICAST, aps, i.src_nwk, &i.payload);
            n.callback(cb);
        });
    }

    fn join(&self, ieee: u64, nwk: u16) {
        self.sim.ncp(|n| {
            n.callback(Callback::TrustCenterJoin {
                new_node_id: nwk,
                new_node_eui64: Eui64::from_u64(ieee),
                status: DeviceUpdate::STANDARD_SECURITY_UNSECURED_JOIN,
                policy_decision: JoinDecision::USE_PRECONFIGURED_KEY,
                parent_of_new_node_id: 0,
            })
        });
    }

    fn leave(&self, ieee: u64, nwk: u16) {
        self.sim.ncp(|n| {
            n.callback(Callback::TrustCenterJoin {
                new_node_id: nwk,
                new_node_eui64: Eui64::from_u64(ieee),
                status: DeviceUpdate::DEVICE_LEFT,
                policy_decision: JoinDecision::NO_ACTION,
                parent_of_new_node_id: 0,
            })
        });
    }

    fn permit_open(&self) -> bool {
        self.sim.ncp(|n| n.permit_joining() > 0)
    }

    fn groups(&self) -> Vec<u16> {
        self.sim.ncp(|n| n.multicast_table().iter().filter(|e| e.endpoint != 0).map(|e| e.multicast_id).collect())
    }
}

/// A RAM flash that outlives the store on it (an app restart).
#[derive(Clone)]
struct SharedFlash(Arc<Mutex<MemFlash>>);

impl SharedFlash {
    fn new() -> Self {
        SharedFlash(Arc::new(Mutex::new(MemFlash::new(32, 4096))))
    }

    fn store(&self) -> Store {
        Store::open(Box::new(self.clone())).unwrap()
    }
}

impl Flash for SharedFlash {
    fn sector_size(&self) -> u32 {
        self.0.lock().unwrap().sector_size()
    }
    fn sectors(&self) -> u32 {
        self.0.lock().unwrap().sectors()
    }
    fn read(&mut self, offset: u32, buf: &mut [u8]) -> std::io::Result<()> {
        self.0.lock().unwrap().read(offset, buf)
    }
    fn write(&mut self, offset: u32, data: &[u8]) -> std::io::Result<()> {
        self.0.lock().unwrap().write(offset, data)
    }
    fn erase(&mut self, sector: u32) -> std::io::Result<()> {
        self.0.lock().unwrap().erase(sector)
    }
}

/// The app on one backend, its simulated NCP and the devices' answers.
struct Rig {
    app: App,
    link: Arc<dyn SimLink>,
    kv: Arc<dyn KvStore>,
    zrhp: Option<hub_ncp::Hub>,
    stop: Arc<AtomicBool>,
    pump: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Zrhp,
    Ezsp,
}

const BOTH: [Kind; 2] = [Kind::Zrhp, Kind::Ezsp];

/// The EZSP simulator's network ([`hub_ezsp::sim::SimConfig`] defaults, or
/// another EPID).
fn ezsp_sim(formed: bool, epid: u8) -> hub_ezsp::sim::SimConfig {
    let mut c = hub_ezsp::sim::SimConfig { ezsp_version: 13, network_formed: formed, ..Default::default() };
    c.network.extended_pan_id = [epid; 8];
    c
}

impl Rig {
    /// `formed`: the NCP runs a network at the start.
    fn new(kind: Kind, formed: bool) -> Rig {
        match kind {
            Kind::Zrhp => Rig::zrhp(formed),
            Kind::Ezsp => Rig::ezsp(ezsp_sim(formed, 0xDD), Store::volatile()),
        }
    }

    fn zrhp(formed: bool) -> Rig {
        let r = crate::tests::rig(Store::volatile());
        if formed {
            r.ncp.form(hub_ncp::FormRequest { channel_mask: 1 << 15, ..hub_ncp::FormRequest::default() }).unwrap();
        }
        // Every request, for the permit-join checks.
        r.sim.with(|s| s.trace = Some(Vec::new()));
        let link = Arc::new(Zrhp { sim: r.sim });
        Rig::start(Backend::zrhp(r.ncp.clone()), link, Some(r.ncp))
    }

    fn ezsp(sim_cfg: hub_ezsp::sim::SimConfig, store: Store) -> Rig {
        let (sim, transport, clock) = hub_ezsp::sim::Sim::new(sim_cfg, hub_ezsp::sim::Faults::none());
        sim.set_pace(Some(1));
        let backend = Backend::ezsp(transport, clock, hub_ezsp::coord::EzspOptions::default(), store);
        Rig::start(backend, Arc::new(Ezsp { sim, seen: Mutex::new(0) }), None)
    }

    fn start(backend: Backend, link: Arc<dyn SimLink>, zrhp: Option<hub_ncp::Hub>) -> Rig {
        let kv: Arc<dyn KvStore> = backend.kv.clone();
        let app = App::start_backend(backend, cfg(), None);
        assert!(app.wait(T, |s| s.endpoint_registered), "the first NCP session");
        Rig { app, link, kv, zrhp, stop: Arc::new(AtomicBool::new(false)), pump: None }
    }

    /// `devs` answer the hub's requests, as `answered` counts.
    fn devices(&mut self, mut devs: Vec<SimDevice>, answered: Arc<Mutex<Vec<ApsRequest>>>) {
        let (link, stop) = (self.link.clone(), self.stop.clone());
        self.pump = Some(thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                for req in link.requests() {
                    let ApsDst::Nwk(nwk) = req.dst else { continue };
                    let Some(d) = devs.iter_mut().find(|d| d.nwk == nwk) else { continue };
                    for i in d.answer(&req) {
                        link.indicate(&i);
                    }
                    answered.lock().unwrap().push(req);
                }
                thread::sleep(Duration::from_millis(5));
            }
        }));
    }

    /// The ownership tag's network.
    fn tag(&self) -> Option<NetworkIdentity> {
        kv::with(&*self.kv, |s| s.get(OWNER_KEY).and_then(OwnerTag::decode)).and_then(|t| t.identity)
    }

    fn finish(mut self) {
        self.app.shutdown();
        self.stop.store(true, Ordering::SeqCst);
        if let Some(p) = self.pump.take() {
            p.join().unwrap();
        }
        if let Some(ncp) = &self.zrhp {
            ncp.shutdown();
        }
    }
}

fn wait(what: &str, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < T * 3, "{what}");
        thread::sleep(Duration::from_millis(20));
    }
}

fn state(app: &App, sid: u64, rx: &Receiver<String>, uid: &str) -> J {
    let r = call(app, sid, rx, 50, json!({"type": "entities/states", "uids": [uid]}));
    ok(&r)[0]["state"].clone()
}

fn info(app: &App, sid: u64, rx: &Receiver<String>) -> J {
    ok(&call(app, sid, rx, 60, json!({"type": "network/info"}))).clone()
}

/// The EPID of `network/info` (big-endian hex) as wire bytes.
fn epid_of(info: &J) -> [u8; 8] {
    let h = info["ext_pan_id"].as_str().unwrap();
    let mut b = [0u8; 8];
    for (i, x) in b.iter_mut().enumerate() {
        *x = u8::from_str_radix(&h[14 - 2 * i..16 - 2 * i], 16).unwrap();
    }
    b
}

#[test]
fn join_interview_command_and_report() {
    for kind in BOTH {
        let mut rig = Rig::new(kind, true);
        let answered = Arc::new(Mutex::new(Vec::new()));
        rig.devices(vec![plug()], answered.clone());
        let (sid, rx) = session(&rig.app);
        rig.link.join(PLUG, PLUG_NWK);
        // Interviewed, resolved and read.
        wait(&format!("{kind:?}: the plug's state"), || state(&rig.app, sid, &rx, SWITCH) == json!(false));
        let dev = call(&rig.app, sid, &rx, 2, json!({"type": "devices/get", "device": "zb:5a52530000000001"}));
        assert_eq!(ok(&dev)["interview"], "complete", "{kind:?}: {dev}");
        assert_eq!(ok(&dev)["model"], "SIM-PLUG", "{kind:?}");
        // A command reaches the device.
        let c = call(&rig.app, sid, &rx, 3, json!({"type": "entities/command", "uid": SWITCH, "command": "turn_on"}));
        ok(&c);
        wait(&format!("{kind:?}: On reached the plug"), || {
            answered.lock().unwrap().iter().any(|r| r.cluster == 0x0006 && r.payload.get(2) == Some(&0x01))
        });
        // The device's report sets the state.
        let report = plug().report(1, 0x0006, None, &[(0x0000, TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(true))))]);
        rig.link.indicate(&report);
        wait(&format!("{kind:?}: reported on"), || state(&rig.app, sid, &rx, SWITCH) == json!(true));
        let report = plug().report(1, 0x0006, None, &[(0x0000, TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false))))]);
        rig.link.indicate(&report);
        wait(&format!("{kind:?}: reported off"), || state(&rig.app, sid, &rx, SWITCH) == json!(false));
        assert!(rig.app.status().aps.accepted > 0);
        if kind == Kind::Ezsp {
            // A leave: kept, unavailable with its reason, until heard again.
            rig.link.leave(PLUG, PLUG_NWK);
            let get = || ok(&call(&rig.app, sid, &rx, 5, json!({"type": "devices/get", "device": "zb:5a52530000000001"}))).clone();
            wait("the plug left", || get()["available"] == false);
            assert_eq!(get()["unavailable_reason"], "left");
            rig.link.indicate(&report);
            wait("the plug back", || get()["available"] == true);
            assert_eq!(get().get("unavailable_reason"), None);
        }
        rig.finish();
    }
}

#[test]
fn groups_and_permit_join() {
    for kind in BOTH {
        let rig = Rig::new(kind, true);
        let (sid, rx) = session(&rig.app);
        let g = call(&rig.app, sid, &rx, 1, json!({"type": "groups/create", "name": "Kitchen"}));
        let group = ok(&g)["id"].as_u64().unwrap() as u16;
        wait(&format!("{kind:?}: the hub endpoint in group {group}"), || rig.link.groups().contains(&group));
        ok(&call(&rig.app, sid, &rx, 2, json!({"type": "groups/delete", "group": group})));
        wait(&format!("{kind:?}: out of group {group}"), || !rig.link.groups().contains(&group));

        ok(&call(&rig.app, sid, &rx, 3, json!({"type": "network/permit_join", "duration_s": 60})));
        wait(&format!("{kind:?}: joining open on the NCP"), || rig.link.permit_open());
        wait(&format!("{kind:?}: joining open in network/info"), || info(&rig.app, sid, &rx)["permit_join_s"].as_u64() > Some(0));
        ok(&call(&rig.app, sid, &rx, 4, json!({"type": "network/permit_join", "duration_s": 0})));
        wait(&format!("{kind:?}: joining closed"), || !rig.link.permit_open());
        rig.finish();
    }
}

#[test]
fn form_where_there_is_no_network_then_reset() {
    for kind in BOTH {
        let rig = Rig::new(kind, false);
        let (sid, rx) = session(&rig.app);
        let i = info(&rig.app, sid, &rx);
        assert_eq!((&i["ownership"], &i["state"]), (&json!("no_network"), &json!("none")), "{kind:?}: {i}");
        assert_eq!(rig.tag(), None, "{kind:?}: no network, no tag");
        let f = call(&rig.app, sid, &rx, 1, json!({"type": "network/form", "channel": 20}));
        let formed = ok(&f)["network"].clone();
        assert_eq!(formed["channel"], 20, "{kind:?}: {f}");
        wait(&format!("{kind:?}: the formed network owned"), || {
            let i = info(&rig.app, sid, &rx);
            i["state"] == "up" && i["ownership"] == "owned" && i["registry"]["state"] == "owned"
        });
        let i = info(&rig.app, sid, &rx);
        assert_eq!(i["ext_pan_id"], formed["ext_pan_id"]);
        assert_eq!(rig.tag().map(|t| t.ext_pan_id), Some(epid_of(&i)), "{kind:?}: tagged");
        // Forming again: refused, the NCP has a network.
        let again = call(&rig.app, sid, &rx, 2, json!({"type": "network/form"}));
        assert_eq!(again["error"]["code"], "invalid_value", "{kind:?}: {again}");

        // A reset replaces it, and the records follow the new network.
        let r = call(&rig.app, sid, &rx, 3, json!({"type": "network/reset", "confirm": "RESET", "channel": 25, "skip_backup": true}));
        let new = ok(&r)["network"].clone();
        assert_ne!(new["ext_pan_id"], formed["ext_pan_id"], "{kind:?}");
        wait(&format!("{kind:?}: the new network owned"), || {
            let i = info(&rig.app, sid, &rx);
            i["ext_pan_id"] == new["ext_pan_id"] && i["ownership"] == "owned" && i["registry"]["state"] == "owned"
        });
        assert_eq!(rig.tag().map(|t| t.ext_pan_id), Some(epid_of(&info(&rig.app, sid, &rx))), "{kind:?}: tag rewritten");
        // Device traffic goes to the new network.
        ok(&call(&rig.app, sid, &rx, 4, json!({"type": "network/permit_join", "duration_s": 30})));
        wait(&format!("{kind:?}: joining open"), || rig.link.permit_open());
        rig.finish();
    }
}

/// EZSP (`NcpNv`): the records are verified against the NCP's network at
/// every start; another network's are foreign, and no device traffic
/// reaches the NCP until the operator adopts or clears them.
#[test]
fn ncp_held_networks_are_verified_against_the_records() {
    let flash = SharedFlash::new();
    let rule = json!({"type": "automations/create", "rule": {"id": "r1", "triggers": [], "actions": [{"type": "notify", "message": "x"}]}});
    let rules = |app: &App, sid, rx: &Receiver<String>| -> Vec<String> {
        let l = call(app, sid, rx, 70, json!({"type": "automations/list"}));
        ok(&l)["rules"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()).collect()
    };

    // A fresh hub: tagged at the first network, a rule stored.
    let rig = Rig::ezsp(ezsp_sim(true, 0xDD), flash.store());
    let (sid, rx) = session(&rig.app);
    wait("owned", || info(&rig.app, sid, &rx)["registry"]["state"] == "owned");
    assert_eq!(rig.tag().map(|t| t.ext_pan_id), Some([0xDD; 8]));
    ok(&call(&rig.app, sid, &rx, 1, rule.clone()));
    rig.finish();

    // The same network again: unverified at the start, then the hub's.
    let rig = Rig::ezsp(ezsp_sim(true, 0xDD), flash.store());
    let (sid, rx) = session(&rig.app);
    wait("owned again", || info(&rig.app, sid, &rx)["registry"]["state"] == "owned");
    assert_eq!(rules(&rig.app, sid, &rx), ["r1"]);
    assert_eq!(info(&rig.app, sid, &rx)["ownership"], "owned");
    rig.finish();

    // Another network (the NCP was replaced or reset elsewhere): foreign.
    let mut rig = Rig::ezsp(ezsp_sim(true, 0xEE), flash.store());
    let answered = Arc::new(Mutex::new(Vec::new()));
    rig.devices(vec![plug()], answered.clone());
    let (sid, rx) = session(&rig.app);
    wait("foreign", || info(&rig.app, sid, &rx)["registry"]["state"] == "foreign");
    let i = info(&rig.app, sid, &rx);
    assert_eq!((&i["ownership"], &i["registry"]["why"]), (&json!("mismatch"), &json!("another extended PAN ID")), "{i}");
    let pj = call(&rig.app, sid, &rx, 2, json!({"type": "network/permit_join", "duration_s": 30}));
    assert_eq!(pj["success"], false, "device traffic is held: {pj}");
    let refused = call(&rig.app, sid, &rx, 3, rule.clone());
    assert_eq!(refused["error"]["code"], "registry_read_only", "{refused}");
    // A device of the NCP's network joins: the hub sends it nothing.
    rig.link.join(PLUG, PLUG_NWK);
    thread::sleep(Duration::from_millis(500));
    assert!(answered.lock().unwrap().is_empty(), "no device traffic while foreign");
    assert!(!rig.link.permit_open());
    // Cleared: the records are the NCP network's, traffic flows.
    let c = call(&rig.app, sid, &rx, 4, json!({"type": "network/registry/clear", "confirm": true}));
    assert_eq!(ok(&c)["registry"]["state"], "owned", "{c}");
    assert_eq!(rig.tag().map(|t| t.ext_pan_id), Some([0xEE; 8]));
    assert!(rules(&rig.app, sid, &rx).is_empty());
    assert_eq!(info(&rig.app, sid, &rx)["ownership"], "owned");
    ok(&call(&rig.app, sid, &rx, 5, json!({"type": "network/permit_join", "duration_s": 30})));
    wait("joining open", || rig.link.permit_open());
    rig.link.join(PLUG, PLUG_NWK);
    wait("the plug interviewed", || state(&rig.app, sid, &rx, SWITCH) == json!(false));
    rig.finish();

    // Back on the first network: foreign now, adopted by the operator.
    let rig = Rig::ezsp(ezsp_sim(true, 0xDD), flash.store());
    let (sid, rx) = session(&rig.app);
    wait("foreign", || info(&rig.app, sid, &rx)["registry"]["state"] == "foreign");
    let a = call(&rig.app, sid, &rx, 6, json!({"type": "network/registry/adopt", "confirm": true}));
    assert_eq!(ok(&a)["registry"]["state"], "owned", "{a}");
    assert_eq!(rig.tag().map(|t| (t.ext_pan_id, t.key_id.is_some())), Some(([0xDD; 8], true)));
    assert!(rig.app.wait(T, |s| s.devices == 1), "the plug's record kept");
    rig.finish();

    // Records of another backend (a ZRHP image's tag): foreign whatever the network.
    let mut s = flash.store();
    let zrhp = OwnerTag::new(BackendKind::Zrhp, None).encode();
    s.kv_put(OWNER_KEY, zrhp.as_bytes()).unwrap();
    drop(s);
    let rig = Rig::ezsp(ezsp_sim(true, 0xDD), flash.store());
    let (sid, rx) = session(&rig.app);
    wait("foreign", || info(&rig.app, sid, &rx)["registry"]["backend"] == "zrhp");
    rig.finish();
}

/// EZSP: a store without the ownership tag is never taken silently. A ZRHP
/// image's stored network makes its records another backend's (tagged as
/// ZRHP's migration does); records of unknown origin stay hidden and
/// unwritable, with no device traffic, until the operator adopts them.
#[test]
fn untagged_records_wait_for_the_operator() {
    use hub_store::{Change, Meta, NetworkRecord};
    let flash = SharedFlash::new();
    let rule = json!({"type": "automations/create", "rule": {"id": "r1", "triggers": [], "actions": [{"type": "notify", "message": "x"}]}});
    let rules = |app: &App, sid, rx: &Receiver<String>| -> usize {
        let l = call(app, sid, rx, 70, json!({"type": "automations/list"}));
        ok(&l)["rules"].as_array().unwrap().len()
    };
    let rig = Rig::ezsp(ezsp_sim(true, 0xDD), flash.store());
    let (sid, rx) = session(&rig.app);
    wait("owned", || info(&rig.app, sid, &rx)["registry"]["state"] == "owned");
    ok(&call(&rig.app, sid, &rx, 1, rule.clone()));
    rig.finish();
    let untag = || {
        let mut s = flash.store();
        s.kv_delete(OWNER_KEY).unwrap();
        s
    };

    // (b) No tag, no stored network: unknown origin.
    drop(untag());
    let rig = Rig::ezsp(ezsp_sim(true, 0xDD), flash.store());
    let (sid, rx) = session(&rig.app);
    let i = info(&rig.app, sid, &rx);
    assert_eq!(i["registry"]["state"], "unverified", "{i}");
    thread::sleep(Duration::from_millis(300));
    assert_eq!(info(&rig.app, sid, &rx)["registry"]["state"], "unverified", "the NCP's network decides nothing");
    assert_eq!(rules(&rig.app, sid, &rx), 0, "hidden");
    assert_eq!(call(&rig.app, sid, &rx, 2, rule.clone())["error"]["code"], "registry_read_only");
    let pj = call(&rig.app, sid, &rx, 3, json!({"type": "network/permit_join", "duration_s": 30}));
    assert_eq!(pj["success"], false, "no device traffic: {pj}");
    assert!(rig.tag().is_none(), "nothing written");
    let a = call(&rig.app, sid, &rx, 4, json!({"type": "network/registry/adopt", "confirm": true}));
    assert_eq!(ok(&a)["registry"]["state"], "owned", "{a}");
    assert_eq!(rules(&rig.app, sid, &rx), 1, "loaded once adopted");
    assert_eq!(rig.tag().map(|t| t.ext_pan_id), Some([0xDD; 8]));
    ok(&call(&rig.app, sid, &rx, 5, json!({"type": "network/permit_join", "duration_s": 30})));
    rig.finish();

    // (a) No tag, a ZRHP image's stored network: that image's records.
    let mut s = untag();
    s.commit(Change::Meta(Meta { ncp_ieee: [0x5A, 0x51, 0, 0, 0, 0, 1, 2], ..Meta::default() })).unwrap();
    let n = NetworkRecord {
        pan_id: 0x1A2B,
        ext_pan_id: [0xDD; 8],
        channel: 15,
        channel_mask: 1 << 15,
        network_key: [0x42; 16],
        key_seq: 0,
        nwk_update_id: 0,
        tx_power: 8,
        nwk_manager: 0,
    };
    s.commit(Change::Network(n)).unwrap();
    drop(s);
    let rig = Rig::ezsp(ezsp_sim(true, 0xDD), flash.store());
    let (sid, rx) = session(&rig.app);
    let i = info(&rig.app, sid, &rx);
    assert_eq!((&i["registry"]["state"], &i["registry"]["backend"]), (&json!("foreign"), &json!("zrhp")), "{i}");
    assert_eq!(kv::with(&*rig.kv, |s| s.get(OWNER_KEY).and_then(OwnerTag::decode)).unwrap().backend, "zrhp");
    assert_eq!(rules(&rig.app, sid, &rx), 0);
    let a = call(&rig.app, sid, &rx, 6, json!({"type": "network/registry/adopt", "confirm": true}));
    assert_eq!(ok(&a)["registry"]["state"], "owned", "{a}");
    assert_eq!(rules(&rig.app, sid, &rx), 1);
    assert_eq!(info(&rig.app, sid, &rx)["ownership"], "owned");
    rig.finish();
}
