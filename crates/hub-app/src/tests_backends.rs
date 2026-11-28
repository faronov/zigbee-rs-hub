//! One suite, run on both coordinator backends: the app on the ZRHP host
//! core and on the EZSP backend, each with its simulated NCP and a device
//! of the engine's device simulator behind it. Join → interview → command
//! → report, groups, permit join, forming and resetting the network,
//! device leaves and the network log; on EZSP also the `NcpNv` ownership
//! decision (design §2.5,
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
    fn join(&self, ieee: u64, nwk: u16) {
        self.join_as(ieee, nwk, 0, false);
    }
    /// A join (`how` as ZRHP's `join_type`: 0 association, 1 secured
    /// rejoin, 2 trust center rejoin) the trust center admits or refuses.
    fn join_as(&self, ieee: u64, nwk: u16, how: u8, denied: bool);
    /// The coordinator reports that a device left.
    fn leave(&self, ieee: u64, nwk: u16);
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
        let mut p = hub_ncp::sim::indication(i.src_nwk, i.src_ieee.unwrap_or(0), i.src_ep, i.dst_ep, i.profile, i.cluster, &i.payload);
        if let Some(g) = i.group {
            // The NCP's APS drops a groupcast to a group without the hub endpoint.
            if !self.groups().contains(&g) {
                return;
            }
            p[0] = 0x01;
            p[1..3].copy_from_slice(&g.to_le_bytes());
        }
        self.sim.inject_event(zigbee_zrhp::msg::ids::EVT_APS_DATA_INDICATION, p);
    }

    fn join_as(&self, ieee: u64, nwk: u16, how: u8, denied: bool) {
        use zigbee_zrhp::msg::network::EvtDeviceJoined;
        let ev = EvtDeviceJoined {
            nwk,
            ieee: zigbee_zrhp::types::Ieee::from_u64(ieee),
            parent_nwk: 0,
            join_type: how,
            capability: 0x8E,
            decision: denied.into(),
        };
        self.sim.inject_event(zigbee_zrhp::msg::ids::EVT_DEVICE_JOINED, hub_ncp::driver::encode_body(&ev).unwrap());
    }

    fn leave(&self, ieee: u64, nwk: u16) {
        use zigbee_zrhp::msg::network::EvtDeviceLeft;
        let ev = EvtDeviceLeft { nwk, ieee: zigbee_zrhp::types::Ieee::from_u64(ieee), flags: 0, reason: 1 };
        self.sim.inject_event(zigbee_zrhp::msg::ids::EVT_DEVICE_LEFT, hub_ncp::driver::encode_body(&ev).unwrap());
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
        let (kind, group_id) = match i.group {
            // The NCP drops a multicast to a group its multicast table lacks.
            Some(g) if !self.groups().contains(&g) => return,
            Some(g) => (IncomingMessageType::MULTICAST, g),
            None => (IncomingMessageType::UNICAST, 0),
        };
        let aps = ApsFrame {
            profile_id: i.profile,
            cluster_id: i.cluster,
            source_endpoint: i.src_ep,
            destination_endpoint: i.dst_ep,
            options: ApsOptions::NONE,
            group_id,
            sequence: 0,
        };
        self.sim.ncp(|n| {
            let cb = n.incoming_message(kind, aps, i.src_nwk, &i.payload);
            n.callback(cb);
        });
    }

    fn join_as(&self, ieee: u64, nwk: u16, how: u8, denied: bool) {
        let status = match how {
            1 => DeviceUpdate::STANDARD_SECURITY_SECURED_REJOIN,
            2 => DeviceUpdate::STANDARD_SECURITY_UNSECURED_REJOIN,
            _ => DeviceUpdate::STANDARD_SECURITY_UNSECURED_JOIN,
        };
        let policy_decision = if denied { JoinDecision::DENY_JOIN } else { JoinDecision::USE_PRECONFIGURED_KEY };
        self.sim.ncp(|n| {
            n.callback(Callback::TrustCenterJoin {
                new_node_id: nwk,
                new_node_eui64: Eui64::from_u64(ieee),
                status,
                policy_decision,
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
    /// The simulated ZRHP NCP.
    zsim: Option<hub_ncp::sim::SimNcp>,
    ezsp: Option<hub_ezsp::sim::Sim>,
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
        Rig::zrhp_with(Store::volatile(), formed)
    }

    fn zrhp_with(store: Store, formed: bool) -> Rig {
        let r = crate::tests::rig(store);
        if formed {
            r.ncp.form(hub_ncp::FormRequest { channel_mask: 1 << 15, ..hub_ncp::FormRequest::default() }).unwrap();
        }
        // Every request, for the permit-join checks.
        r.sim.with(|s| s.trace = Some(Vec::new()));
        let zsim = r.sim.clone();
        let link = Arc::new(Zrhp { sim: r.sim });
        let mut rig = Rig::start(Backend::zrhp(r.ncp.clone()), link, Some(r.ncp));
        rig.zsim = Some(zsim);
        rig
    }

    fn ezsp(sim_cfg: hub_ezsp::sim::SimConfig, store: Store) -> Rig {
        Rig::ezsp_with(sim_cfg, store, hub_ezsp::coord::EzspOptions::default())
    }

    fn ezsp_with(sim_cfg: hub_ezsp::sim::SimConfig, store: Store, opts: hub_ezsp::coord::EzspOptions) -> Rig {
        let (sim, transport, clock) = hub_ezsp::sim::Sim::new(sim_cfg, hub_ezsp::sim::Faults::none());
        sim.set_pace(Some(1));
        let backend = Backend::ezsp(transport, clock, opts, store);
        let mut rig = Rig::start(backend, Arc::new(Ezsp { sim: sim.clone(), seen: Mutex::new(0) }), None);
        rig.ezsp = Some(sim);
        rig
    }

    fn start(backend: Backend, link: Arc<dyn SimLink>, zrhp: Option<hub_ncp::Hub>) -> Rig {
        let kv: Arc<dyn KvStore> = backend.kv.clone();
        let app = App::start_backend(backend, cfg(), None);
        assert!(app.wait(T, |s| s.endpoint_registered), "the first NCP session");
        Rig { app, link, kv, zrhp, zsim: None, ezsp: None, stop: Arc::new(AtomicBool::new(false)), pump: None }
    }

    /// ZRHP: a new app on the same host core and simulated NCP (a hub
    /// restart that keeps the NCP's state).
    fn restart_zrhp(mut self) -> Rig {
        let (ncp, zsim, link) = (self.zrhp.take().unwrap(), self.zsim.take(), self.link.clone());
        crate::tests::stop(&self.app);
        self.finish();
        let mut rig = Rig::start(Backend::zrhp(ncp.clone()), link, Some(ncp));
        rig.zsim = zsim;
        rig
    }

    /// `devs` answer the hub's requests, as `answered` counts.
    fn devices(&mut self, devs: Vec<SimDevice>, answered: Arc<Mutex<Vec<ApsRequest>>>) {
        self.devices_shared(Arc::new(Mutex::new(devs)), answered);
    }

    /// [`Rig::devices`] that the test changes meanwhile.
    fn devices_shared(&mut self, devs: Arc<Mutex<Vec<SimDevice>>>, answered: Arc<Mutex<Vec<ApsRequest>>>) {
        let (link, stop) = (self.link.clone(), self.stop.clone());
        self.pump = Some(thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                for req in link.requests() {
                    let ApsDst::Nwk(nwk) = req.dst else { continue };
                    let inds = {
                        let mut devs = devs.lock().unwrap();
                        let Some(d) = devs.iter_mut().find(|d| d.nwk == nwk) else { continue };
                        d.answer(&req)
                    };
                    for i in inds {
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
        // A leave: kept, unavailable with its reason, until heard again.
        rig.link.leave(PLUG, PLUG_NWK);
        let get = || ok(&call(&rig.app, sid, &rx, 5, json!({"type": "devices/get", "device": "zb:5a52530000000001"}))).clone();
        wait(&format!("{kind:?}: the plug left"), || get()["available"] == false);
        assert_eq!(get()["unavailable_reason"], "left");
        rig.link.indicate(&report);
        wait(&format!("{kind:?}: the plug back"), || get()["available"] == true);
        assert_eq!(get().get("unavailable_reason"), None);
        rig.finish();
    }
}

/// The network log (API 1.4) on both backends: an admitted join, a join
/// refused while joining is closed, a secured rejoin under a new address,
/// a Device_annce and a leave come as `device_network` events and in
/// `network/events`; the device record counts them and says why and since
/// when the device is unavailable.
#[test]
fn joins_refusals_rejoins_announcements_and_leaves_are_logged() {
    const STRANGER: u64 = 0x5A52_5300_0000_00FF;
    for kind in BOTH {
        let mut rig = Rig::new(kind, true);
        let answered = Arc::new(Mutex::new(Vec::new()));
        let devs = Arc::new(Mutex::new(vec![plug()]));
        rig.devices_shared(devs.clone(), answered);
        let (sid, rx) = session(&rig.app);
        let (esid, erx) = session(&rig.app);
        ok(&call(&rig.app, esid, &erx, 1, json!({"type": "subscribe", "topics": ["network"]})));
        let get = || ok(&call(&rig.app, sid, &rx, 2, json!({"type": "devices/get", "device": "zb:5a52530000000001"}))).clone();
        let log = || ok(&call(&rig.app, sid, &rx, 3, json!({"type": "network/events"}))).as_array().unwrap().clone();

        rig.link.join(PLUG, PLUG_NWK);
        wait(&format!("{kind:?}: the plug's state"), || state(&rig.app, sid, &rx, SWITCH) == json!(false));
        rig.link.join_as(STRANGER, 0x5AFF, 0, true);
        devs.lock().unwrap()[0].nwk = 0x5A11;
        rig.link.join_as(PLUG, 0x5A11, 1, false);
        let annce = hub_devices::zdo::device_annce(1, 0x5A11, PLUG, 0x8E);
        rig.link.indicate(&plug().ind(0, 0, 0x0013, annce));
        // The plug, under its new address.
        let mut p = plug();
        p.nwk = 0x5A11;
        wait(&format!("{kind:?}: announced"), || log().len() == 4);
        rig.link.leave(PLUG, 0x5A11);
        wait(&format!("{kind:?}: five entries"), || log().len() == 5);

        let l = log();
        let pick = |e: &J| (e["event"].clone(), e["decision"].clone(), e["how"].clone(), e["reason"].clone(), e["device"].clone());
        let dev = json!("zb:5a52530000000001");
        let none = J::Null;
        assert_eq!(
            l.iter().map(pick).collect::<Vec<_>>(),
            [
                (json!("joined"), json!("admitted"), json!("association"), none.clone(), dev.clone()),
                (json!("joined"), json!("denied"), json!("association"), json!("permit_closed"), json!("zb:5a525300000000ff")),
                (json!("rejoined"), json!("admitted"), json!("secured"), none.clone(), dev.clone()),
                (json!("announced"), none.clone(), none.clone(), none.clone(), dev.clone()),
                (json!("left"), none.clone(), none.clone(), json!("self"), dev.clone()),
            ],
            "{kind:?}: {l:?}"
        );
        assert_eq!((l[2]["nwk"].as_u64(), l[2]["prev_nwk"].as_u64()), (Some(0x5A11), Some(u64::from(PLUG_NWK))), "{kind:?}");
        assert!(l.iter().all(|e| e["time"].as_f64().is_some() && e["topic"] == "device_network"), "{kind:?}");
        // Each as an event of topic `network`, in the same order.
        let mut seen = Vec::new();
        while seen.len() < 5 {
            let f: J = serde_json::from_str(&erx.recv_timeout(T).unwrap()).unwrap();
            if f["event"]["topic"] == "device_network" {
                seen.push(f["event"]["event"].clone());
            }
        }
        assert_eq!(seen, ["joined", "joined", "rejoined", "announced", "left"], "{kind:?}");

        let d = get();
        assert_eq!((&d["rejoins"], &d["announces"], &d["leaves"]), (&json!(1), &json!(1), &json!(1)), "{kind:?}: {d}");
        assert_eq!((&d["logical_type"], &d["rx_on_when_idle"]), (&json!("router"), &json!(true)), "{kind:?}: {d}");
        assert_eq!((&d["available"], &d["unavailable_reason"]), (&json!(false), &json!("left")), "{kind:?}: {d}");
        assert!(d["unavailable_since"].as_f64().is_some(), "{kind:?}: {d}");
        rig.link.indicate(&p.report(1, 0x0006, None, &[(0x0000, TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(true))))]));
        wait(&format!("{kind:?}: back"), || get()["available"] == true);
        assert_eq!(get().get("unavailable_since"), None, "{kind:?}");
        rig.finish();
    }
}

/// A child the ZRHP NCP dropped after a timeout (`EVT_DEVICE_LEFT` reason
/// 4, flags 0: a router child not heard directly for a minute, which
/// routes through others; an end device past its End Device Timeout) has
/// not left: the network log says `aged_out`, the device stays available
/// under the usual availability rules. A leave is still one.
#[test]
fn a_child_that_aged_out_of_the_ncp_has_not_left() {
    use zigbee_zrhp::msg::network::EvtDeviceLeft;
    let mut rig = Rig::zrhp(true);
    let answered = Arc::new(Mutex::new(Vec::new()));
    rig.devices(vec![plug()], answered);
    let (sid, rx) = session(&rig.app);
    let get = || ok(&call(&rig.app, sid, &rx, 2, json!({"type": "devices/get", "device": "zb:5a52530000000001"}))).clone();
    let log = || ok(&call(&rig.app, sid, &rx, 3, json!({"type": "network/events"}))).as_array().unwrap().clone();
    rig.link.join(PLUG, PLUG_NWK);
    wait("the plug's state", || state(&rig.app, sid, &rx, SWITCH) == json!(false));

    let ev = EvtDeviceLeft { nwk: PLUG_NWK, ieee: zigbee_zrhp::types::Ieee::from_u64(PLUG), flags: 0, reason: 4 };
    rig.zsim.as_ref().unwrap().inject_event(zigbee_zrhp::msg::ids::EVT_DEVICE_LEFT, hub_ncp::driver::encode_body(&ev).unwrap());
    wait("aged out", || log().iter().any(|e| e["event"] == "aged_out"));
    let e = log().into_iter().find(|e| e["event"] == "aged_out").unwrap();
    assert_eq!((&e["device"], e.get("reason"), e.get("rejoin")), (&json!("zb:5a52530000000001"), None, None), "{e}");
    let d = get();
    assert_eq!((&d["available"], d.get("unavailable_reason"), d.get("leaves")), (&json!(true), None, None), "{d}");

    rig.link.leave(PLUG, PLUG_NWK);
    wait("the plug left", || get()["unavailable_reason"] == "left");
    rig.finish();
}

/// A device whose interview ended partial (its Simple descriptor failed)
/// is interviewed on from there when it announces itself: the Device_annce
/// reaches the engine through either NCP.
#[test]
fn a_partial_interview_resumes_on_the_devices_announcement() {
    use hub_devices::zdo::cluster::{NODE_DESC_REQ, SIMPLE_DESC_REQ};
    for kind in BOTH {
        let mut rig = Rig::new(kind, true);
        let answered = Arc::new(Mutex::new(Vec::new()));
        let mut p = plug();
        p.zdo_failing.insert(SIMPLE_DESC_REQ);
        let devs = Arc::new(Mutex::new(vec![p]));
        rig.devices_shared(devs.clone(), answered.clone());
        let (sid, rx) = session(&rig.app);
        let interview =
            || call(&rig.app, sid, &rx, 2, json!({"type": "devices/get", "device": "zb:5a52530000000001"}))["result"]["interview"].clone();
        rig.link.join(PLUG, PLUG_NWK);
        wait(&format!("{kind:?}: interview partial"), || interview() == "partial");
        // What failed, and since when (API 1.4).
        let dev = ok(&call(&rig.app, sid, &rx, 3, json!({"type": "devices/get", "device": "zb:5a52530000000001"}))).clone();
        assert_eq!(dev["interview_failed"], json!(["simple_descriptor_0"]), "{kind:?}: {dev}");
        assert!(dev["interview_since"].as_f64().is_some(), "{kind:?}: {dev}");
        let diag = ok(&call(&rig.app, sid, &rx, 4, json!({"type": "devices/diagnostics", "device": "zb:5a52530000000001"}))).clone();
        assert_eq!((&diag["interview_failed"], &diag["interview_step"]), (&json!(["simple_descriptor_0"]), &json!("done")), "{kind:?}");

        devs.lock().unwrap()[0].zdo_failing.clear();
        answered.lock().unwrap().clear();
        let annce = hub_devices::zdo::device_annce(1, PLUG_NWK, PLUG, 0x8E);
        rig.link.indicate(&plug().ind(0, 0, 0x0013, annce));
        wait(&format!("{kind:?}: the plug's state"), || state(&rig.app, sid, &rx, SWITCH) == json!(false));
        assert_eq!(interview(), "complete", "{kind:?}");
        let dev = ok(&call(&rig.app, sid, &rx, 5, json!({"type": "devices/get", "device": "zb:5a52530000000001"}))).clone();
        assert_eq!((dev.get("interview_failed"), dev.get("interview_since")), (None, None), "{kind:?}: {dev}");
        let zdo: Vec<u16> = answered.lock().unwrap().iter().filter(|r| r.profile == 0).map(|r| r.cluster).collect();
        assert!(zdo.contains(&SIMPLE_DESC_REQ) && !zdo.contains(&NODE_DESC_REQ), "{kind:?}: {zdo:04x?}");

        // A requested interview: the record says so at once.
        let (esid, erx) = session(&rig.app);
        ok(&call(&rig.app, esid, &erx, 1, json!({"type": "subscribe", "topics": ["devices"]})));
        ok(&call(&rig.app, sid, &rx, 6, json!({"type": "devices/reinterview", "device": "zb:5a52530000000001"})));
        loop {
            let f: J = serde_json::from_str(&erx.recv_timeout(T).unwrap()).unwrap();
            if f["event"]["topic"] == "device_updated" && f["event"]["device"]["interview"] == "in_progress" {
                break;
            }
        }
        rig.finish();
    }
}

/// A sleepy sensor's Device_annce, which the ZRHP NCP forwards as an APS
/// indication (ZDO 0x0013), counts as the device awake: what was queued
/// for it after a hub restart (a binding table read) goes out at once, as
/// on any other frame from it. Before, it waited for the next frame (here:
/// the answer to the identity re-read, which this sensor does not give;
/// on hardware, within an hour of the last read, nothing).
#[test]
fn a_sleepy_devices_announcement_sends_what_waits_for_it() {
    const SENSOR: u64 = 0x5A52_5300_0000_0002;
    const SENSOR_NWK: u16 = 0x5A02;
    const ID: &str = "zb:5a52530000000002";
    let sensor = || {
        SimDevice::new(SENSOR, SENSOR_NWK, LogicalType::EndDevice, false, 0x1234)
            .endpoint(1, 0x0104, 0x0302, &[0x0000, 0x0001, 0x0402], &[])
            .basic("zigbee-rs", "SIM-TH", 3)
            .attr(1, 0x0402, 0x0000, TypedValue::int(ZclType::I16, 2137))
    };
    let mut rig = Rig::zrhp(true);
    let answered = Arc::new(Mutex::new(Vec::new()));
    let devs = Arc::new(Mutex::new(vec![sensor()]));
    rig.devices_shared(devs.clone(), answered.clone());
    let (sid, rx) = session(&rig.app);
    rig.link.join(SENSOR, SENSOR_NWK);
    wait("interviewed", || state(&rig.app, sid, &rx, &format!("{ID}:1:temperature")) == json!(21.37));

    // Restarted: the sensor sleeps, a read of its binding table waits. It
    // will not answer the identity re-read its announcement brings (an
    // answer would be a frame from it).
    devs.lock().unwrap()[0].silent_attrs.insert((0x0000, 0x0006));
    let mut rig = rig.restart_zrhp();
    rig.devices_shared(devs.clone(), answered.clone());
    let (sid, rx) = session(&rig.app);
    wait("restored", || rig.app.status().restored == 1);
    answered.lock().unwrap().clear();
    let reading = || ok(&call(&rig.app, sid, &rx, 4, json!({"type": "bindings/list", "device": ID})))[0]["reading"].clone();
    let read = call(&rig.app, sid, &rx, 3, json!({"type": "bindings/read", "device": ID}));
    // Said at once (API 1.4).
    assert_eq!(ok(&read)["queued"], true, "{read}");
    thread::sleep(Duration::from_millis(300));
    assert_eq!(reading(), json!(true), "queued while it sleeps");
    assert!(!answered.lock().unwrap().iter().any(|r| r.profile == 0 && r.cluster == 0x0033));

    let annce = hub_devices::zdo::device_annce(1, SENSOR_NWK, SENSOR, 0x80);
    rig.link.indicate(&sensor().ind(0, 0, 0x0013, annce));
    wait("the binding table read", || reading() == json!(false));
    let list = call(&rig.app, sid, &rx, 5, json!({"type": "bindings/list", "device": ID}));
    assert_eq!(ok(&list)[0].get("queued"), None, "{list}");
    assert!(answered.lock().unwrap().iter().any(|r| r.profile == 0 && r.cluster == 0x0033));
    rig.finish();
}

/// A remote's button press (On/Off Toggle, client to server), to the hub
/// endpoint or to `group`.
fn press(d: &SimDevice, group: Option<u16>) -> ApsIndication {
    let mut i = d.ind(1, 0x0104, 0x0006, vec![0x11, 0x42, 0x02]);
    if group.is_some() {
        i.group = group;
        i.src_ieee = None;
    }
    i
}

/// The next `entity_event` of `uid` (its `event_type`) on a session
/// subscribed to `events`.
fn next_event(rx: &Receiver<String>, uid: &str) -> String {
    let t = Instant::now();
    loop {
        assert!(t.elapsed() < T * 3, "no event of {uid}");
        let Ok(f) = rx.recv_timeout(T) else { continue };
        let f: J = serde_json::from_str(&f).unwrap();
        if f["event"]["topic"] == "entity_event" && f["event"]["uid"] == uid {
            return f["event"]["event_type"].as_str().unwrap_or_default().to_string();
        }
    }
}

/// `NWK_GROUP_MEMBERSHIP` adds of the hub endpoint to `group` the ZRHP NCP got.
fn zrhp_joins(rig: &Rig, group: u16) -> usize {
    rig.zsim.as_ref().unwrap().with(|s| {
        s.log.iter().filter(|(id, p)| *id == zigbee_zrhp::msg::ids::NWK_GROUP_MEMBERSHIP && p[0] == 1 && p[1..3] == group.to_le_bytes()).count()
    })
}

/// Remotes whose presses reach the hub only when configure bound them, on
/// either NCP: a SONOFF SNZB-01's On/Off and a TRADFRI remote's on
/// firmware 2.3.080 are bound to the hub endpoint (and listed with their
/// bindings); one on 2.3.014 (zhc `ikeaConfigureRemote`: a group only) is
/// bound to group 901, which the hub endpoint joins on the NCP, so its
/// groupcasts fire its events (the NCP drops a groupcast to a group the
/// hub endpoint is not in). A restarted hub joins the groups again.
#[test]
fn remotes_are_bound_so_that_their_presses_reach_the_hub() {
    use hub_devices::zdo::BindDst;
    let ikea = |ieee: u64, nwk: u16, sw: &str| {
        SimDevice::new(ieee, nwk, LogicalType::EndDevice, false, 0x117C)
            .endpoint(1, 0x0104, 0x0820, &[0x0000, 0x0001, 0x0003, 0x1000], &[0x0003, 0x0004, 0x0005, 0x0006, 0x0008, 0x0019])
            .basic("IKEA of Sweden", "TRADFRI remote control", 3)
            .attr(1, 0x0000, 0x4000, TypedValue::new(ZclType::CHAR_STR, ZclValue::String(sw.into())))
    };
    let snzb = SimDevice::new(0x5A52_5300_0000_0011, 0x5A11, LogicalType::EndDevice, false, 0)
        .endpoint(1, 0x0104, 0x0000, &[0x0000, 0x0003, 0x0001], &[0x0006, 0x0003])
        .basic("eWeLink", "WB01", 3);
    let old = ikea(0x5A52_5300_0000_0012, 0x5A12, "2.3.014");
    let new = ikea(0x5A52_5300_0000_0013, 0x5A13, "2.3.080");
    for kind in BOTH {
        let flash = SharedFlash::new();
        let mut rig = match kind {
            Kind::Zrhp => Rig::zrhp(true),
            Kind::Ezsp => Rig::ezsp(ezsp_sim(true, 0xDD), flash.store()),
        };
        let devs = Arc::new(Mutex::new(vec![snzb.clone(), old.clone(), new.clone()]));
        rig.devices_shared(devs.clone(), Arc::new(Mutex::new(Vec::new())));
        let (sid, rx) = session(&rig.app);
        let (esid, erx) = session(&rig.app);
        ok(&call(&rig.app, esid, &erx, 1, json!({"type": "subscribe", "topics": ["events"]})));
        let bound = |ieee: u64, f: &dyn Fn(&BindDst) -> bool| {
            devs.lock().unwrap().iter().find(|d| d.ieee == ieee).unwrap().bind_table.iter().any(|b| b.0 == 1 && b.1 == 6 && f(&b.2))
        };
        let to_hub = |d: &BindDst| matches!(d, BindDst::Ieee(_, 1));
        for d in [&snzb, &new] {
            rig.link.join(d.ieee, d.nwk);
            let id = format!("zb:{:016x}", d.ieee);
            let hub = json!({"endpoint": 1, "cluster": 6, "target": {"hub": true, "endpoint": 1}});
            wait(&format!("{kind:?}: {id} bound to the hub, listed"), || {
                ok(&call(&rig.app, sid, &rx, 2, json!({"type": "bindings/list", "device": id})))[0]["table"][0] == hub
            });
            assert!(bound(d.ieee, &to_hub), "{kind:?}");
            rig.link.indicate(&press(d, None));
            assert_eq!(next_event(&erx, &format!("{id}:1:action")), "toggle", "{kind:?}");
        }
        assert!(!rig.link.groups().contains(&901), "{kind:?}");

        rig.link.join(old.ieee, old.nwk);
        wait(&format!("{kind:?}: bound to group 901"), || bound(old.ieee, &|d| *d == BindDst::Group(901)));
        let groups = rig.link.groups();
        assert!(groups.contains(&0) && groups.contains(&901), "{kind:?}: {groups:?}");
        assert!(!bound(old.ieee, &to_hub), "{kind:?}: no binds to the hub on group-only firmware");
        rig.link.indicate(&press(&old, Some(901)));
        assert_eq!(next_event(&erx, "zb:5a52530000000012:1:action"), "toggle", "{kind:?}");

        // Restarted: the ZRHP NCP is asked again, the EZSP NCP's multicast
        // table (a new one) gets both groups.
        let rig = match kind {
            Kind::Zrhp => {
                let n = zrhp_joins(&rig, 901);
                let rig = rig.restart_zrhp();
                wait("ZRHP: group 901 joined again", || zrhp_joins(&rig, 901) > n);
                rig
            }
            Kind::Ezsp => {
                rig.finish();
                let rig = Rig::ezsp(ezsp_sim(true, 0xDD), flash.store());
                wait("EZSP: groups 0 and 901 joined again", || {
                    let g = rig.link.groups();
                    g.contains(&0) && g.contains(&901)
                });
                rig
            }
        };
        rig.finish();
    }
}

#[test]
fn groups_and_permit_join() {
    for kind in BOTH {
        let rig = Rig::new(kind, true);
        let (sid, rx) = session(&rig.app);
        // What a status LED shows.
        wait(&format!("{kind:?}: session and network up in the app status"), || {
            let s = rig.app.status();
            s.session_up && s.network_up && s.permit_join_s == 0
        });
        let g = call(&rig.app, sid, &rx, 1, json!({"type": "groups/create", "name": "Kitchen"}));
        let group = ok(&g)["id"].as_u64().unwrap() as u16;
        wait(&format!("{kind:?}: the hub endpoint in group {group}"), || rig.link.groups().contains(&group));
        ok(&call(&rig.app, sid, &rx, 2, json!({"type": "groups/delete", "group": group})));
        wait(&format!("{kind:?}: out of group {group}"), || !rig.link.groups().contains(&group));

        ok(&call(&rig.app, sid, &rx, 3, json!({"type": "network/permit_join", "duration_s": 60})));
        wait(&format!("{kind:?}: joining open on the NCP"), || rig.link.permit_open());
        wait(&format!("{kind:?}: joining open in network/info"), || info(&rig.app, sid, &rx)["permit_join_s"].as_u64() > Some(0));
        wait(&format!("{kind:?}: joining open in the app status"), || rig.app.status().permit_join_s > 0);
        ok(&call(&rig.app, sid, &rx, 4, json!({"type": "network/permit_join", "duration_s": 0})));
        wait(&format!("{kind:?}: joining closed"), || !rig.link.permit_open());
        wait(&format!("{kind:?}: joining closed in the app status"), || rig.app.status().permit_join_s == 0);
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

/// `network/form` takes the records, so only the hub's own (or none):
/// foreign or unverified records need adopt/clear or a `network/reset`
/// ("RESET"). A blank EmberZNet NCP over a ZRHP image's records is exactly
/// a ZRHP → EZSP migration; a blank ZRHP NCP over an EZSP image's records
/// the reverse.
#[test]
fn form_needs_records_that_are_the_hubs() {
    use hub_store::{Change, Meta, NetworkRecord};
    let refused = |rig: &Rig, what: &str| {
        let (sid, rx) = session(&rig.app);
        let before = kv::with(&*rig.kv, |s| s.get(OWNER_KEY).map(<[u8]>::to_vec));
        let f = call(&rig.app, sid, &rx, 1, json!({"type": "network/form"}));
        assert_eq!(f["error"]["code"], "registry_read_only", "{what}: {f}");
        thread::sleep(Duration::from_millis(200));
        assert_eq!(info(&rig.app, sid, &rx)["state"], "none", "{what}: nothing formed");
        assert_eq!(kv::with(&*rig.kv, |s| s.get(OWNER_KEY).map(<[u8]>::to_vec)), before, "{what}: tag kept");
        (sid, rx)
    };

    // EZSP over a ZRHP image's store (its stored network, untagged).
    let mut s = Store::volatile();
    s.commit(Change::Meta(Meta { ncp_ieee: [0x5A, 0x51, 0, 0, 0, 0, 1, 2], ..Meta::default() })).unwrap();
    let n = NetworkRecord {
        pan_id: 0x1A2B,
        ext_pan_id: [0x31; 8],
        channel: 15,
        channel_mask: 1 << 15,
        network_key: [0x42; 16],
        key_seq: 0,
        nwk_update_id: 0,
        tx_power: 8,
        nwk_manager: 0,
    };
    s.commit(Change::Network(n)).unwrap();
    s.kv_put("dev/zb:1", b"{}").unwrap();
    let rig = Rig::ezsp(ezsp_sim(false, 0xDD), s);
    let (sid, rx) = refused(&rig, "ZRHP records, blank EZSP NCP");
    assert_eq!(info(&rig.app, sid, &rx)["registry"]["backend"], "zrhp");
    // An explicit reset takes them.
    let r = call(&rig.app, sid, &rx, 2, json!({"type": "network/reset", "confirm": "RESET", "skip_backup": true}));
    ok(&r);
    wait("owned after the reset", || info(&rig.app, sid, &rx)["registry"]["state"] == "owned");
    rig.finish();

    // EZSP over records of unknown origin.
    let mut s = Store::volatile();
    s.kv_put("auto/r1", b"{}").unwrap();
    let rig = Rig::ezsp(ezsp_sim(false, 0xDD), s);
    refused(&rig, "untagged records, blank EZSP NCP");
    rig.finish();

    // EZSP: the NCP loses the hub's network mid-run (erased, swapped).
    let rig = Rig::ezsp(ezsp_sim(true, 0xDD), Store::volatile());
    let (sid, rx) = session(&rig.app);
    wait("owned", || info(&rig.app, sid, &rx)["ownership"] == "owned");
    rig.ezsp.as_ref().unwrap().ncp(|n| {
        n.config_mut().network_formed = false;
        n.spontaneous_reset(hub_ezsp::ResetCode::WATCHDOG);
    });
    wait("no network", || info(&rig.app, sid, &rx)["ownership"] == "no_network");
    let (sid, rx) = refused(&rig, "the hub's network, lost by the NCP");
    assert_eq!(rig.tag().map(|t| t.ext_pan_id), Some([0xDD; 8]));
    ok(&call(&rig.app, sid, &rx, 2, json!({"type": "network/reset", "confirm": "RESET", "skip_backup": true})));
    rig.finish();

    // ZRHP over an EZSP image's records.
    let mut s = Store::volatile();
    s.kv_put(OWNER_KEY, OwnerTag::new(BackendKind::Ezsp, None).encode().as_bytes()).unwrap();
    s.kv_put("dev/zb:1", b"{}").unwrap();
    let rig = Rig::zrhp_with(s, false);
    refused(&rig, "EZSP records, blank ZRHP NCP");
    rig.finish();
}

/// EZSP first setup, interrupted: records the hub made before its first
/// network (a rule) and the tag of a form that failed are the hub's own
/// after a restart, so `network/form` still takes them.
#[test]
fn first_setup_survives_restarts() {
    use hub_ezsp::coord::EzspOptions;
    use hub_ezsp::ezsp::command::id;
    let flash = SharedFlash::new();
    let rules = |app: &App, sid, rx: &Receiver<String>| -> usize {
        let l = call(app, sid, rx, 70, json!({"type": "automations/list"}));
        ok(&l)["rules"].as_array().unwrap().len()
    };
    // A blank NCP: a rule before the first network.
    let rig = Rig::ezsp(ezsp_sim(false, 0xDD), flash.store());
    let (sid, rx) = session(&rig.app);
    let rule = json!({"type": "automations/create", "rule": {"id": "r1", "triggers": [], "actions": [{"type": "notify", "message": "x"}]}});
    ok(&call(&rig.app, sid, &rx, 1, rule));
    assert_eq!(rig.tag(), None, "no network yet");
    rig.finish();

    // Restarted: the rule is the hub's; a form the NCP never answers fails.
    let host = hub_ezsp::HostConfig { command_timeout: Duration::from_millis(500), ..Default::default() };
    let rig = Rig::ezsp_with(ezsp_sim(false, 0xDD), flash.store(), EzspOptions { host, ..Default::default() });
    let (sid, rx) = session(&rig.app);
    let i = info(&rig.app, sid, &rx);
    assert_eq!(i["registry"]["state"], "owned", "{i}");
    assert_eq!(rules(&rig.app, sid, &rx), 1);
    rig.ezsp.as_ref().unwrap().ncp(|n| n.stall(Some(id::FORM_NETWORK)));
    let f = call(&rig.app, sid, &rx, 2, json!({"type": "network/form"}));
    assert_eq!(f["success"], false, "{f}");
    assert_eq!(rig.tag().map(|t| (t.channel, t.key_id)), Some((0, None)), "the network being formed");
    rig.finish();

    // Restarted again, the NCP still blank: the hub forms its network.
    let rig = Rig::ezsp(ezsp_sim(false, 0xDD), flash.store());
    let (sid, rx) = session(&rig.app);
    assert_eq!(info(&rig.app, sid, &rx)["registry"]["state"], "owned");
    let f = call(&rig.app, sid, &rx, 3, json!({"type": "network/form"}));
    let formed = ok(&f)["network"].clone();
    wait("the formed network owned", || {
        let i = info(&rig.app, sid, &rx);
        i["ext_pan_id"] == formed["ext_pan_id"] && i["ownership"] == "owned" && i["registry"]["state"] == "owned"
    });
    assert_eq!(rules(&rig.app, sid, &rx), 1);
    rig.finish();
}

/// EZSP: right after the app starts (a hub restart, Home Assistant
/// reconnecting at once), device traffic waits for the first session's
/// ownership decision instead of being refused, for a few seconds; when
/// the NCP does not come up within them the request is refused, not left
/// hanging.
#[test]
fn device_traffic_waits_for_the_first_sessions_decision() {
    let flash = SharedFlash::new();
    let rig = Rig::ezsp(ezsp_sim(true, 0xDD), flash.store());
    let (sid, rx) = session(&rig.app);
    wait("owned", || info(&rig.app, sid, &rx)["registry"]["state"] == "owned");
    rig.finish();
    let start = |boot_ms| {
        let mut c = ezsp_sim(true, 0xDD);
        c.boot_ms = boot_ms;
        let (sim, transport, clock) = hub_ezsp::sim::Sim::new(c, hub_ezsp::sim::Faults::none());
        sim.set_pace(Some(1));
        let backend = Backend::ezsp(transport, clock, hub_ezsp::coord::EzspOptions::default(), flash.store());
        (App::start_backend(backend, cfg(), None), Ezsp { sim, seen: Mutex::new(0) })
    };
    let permit_join = json!({"type": "network/permit_join", "duration_s": 30});

    // The NCP boots for a second: the request waits for the decision.
    let (app, ncp) = start(1000);
    let (sid, rx) = session(&app);
    let i = info(&app, sid, &rx);
    assert_ne!(i["session"], "ready", "before the first session: {i}");
    ok(&call(&app, sid, &rx, 1, permit_join.clone()));
    assert_eq!(info(&app, sid, &rx)["registry"]["state"], "owned");
    assert!(ncp.permit_open(), "sent once the network was the hub's");
    app.shutdown();

    // No NCP session within the hold: refused.
    let (app, ncp) = start(60_000);
    let (sid, rx) = session(&app);
    let t = Instant::now();
    let r = call(&app, sid, &rx, 2, permit_join);
    assert_eq!(r["error"]["code"], "busy", "{r}");
    assert!(t.elapsed() >= Duration::from_secs(2), "held first: {:?}", t.elapsed());
    assert!(!ncp.permit_open());
    app.shutdown();
}

/// EZSP: a request queued behind a control request on the NCP does not go
/// out after an NCP reset into another network (whose ownership is decided
/// first; here it is foreign): the backend shares the records' traffic
/// flag.
#[test]
fn queued_requests_do_not_reach_the_network_after_an_ncp_reset() {
    use hub_ezsp::ezsp::command::id;
    let mut rig = Rig::ezsp(ezsp_sim(true, 0xDD), Store::volatile());
    let answered = Arc::new(Mutex::new(Vec::new()));
    rig.devices(vec![plug()], answered.clone());
    let (sid, rx) = session(&rig.app);
    rig.link.join(PLUG, PLUG_NWK);
    wait("the plug's state", || state(&rig.app, sid, &rx, SWITCH) == json!(false));
    let sim = rig.ezsp.clone().unwrap();
    let on_off = |cmd: u8| {
        sim.ncp(|n| {
            n.sent().iter().filter(|m| {
                matches!(&m.command, Command::SendUnicast { index_or_destination, aps, message, .. }
                    if *index_or_destination == PLUG_NWK && aps.cluster_id == 6
                        && message.first().is_some_and(|fc| fc & 0x03 == 0x01) && message.get(2) == Some(&cmd))
            }).count()
        })
    };
    // A group membership the NCP does not answer (a barrier), and a
    // command behind it; the results come with the outcomes.
    sim.ncp(|n| n.stall(Some(id::SET_MULTICAST_TABLE_ENTRY)));
    let text = |v: J| rig.app.text(sid, v.to_string());
    text(json!({"id": 1, "type": "groups/create", "name": "Kitchen"}));
    wait("the barrier on the NCP", || sim.ncp(|n| n.commands().contains(&id::SET_MULTICAST_TABLE_ENTRY)));
    text(json!({"id": 2, "type": "entities/command", "uid": SWITCH, "command": "turn_off"}));
    thread::sleep(Duration::from_millis(300));
    assert_eq!(on_off(0), 0, "Off waits behind the barrier");
    // The NCP restarts into another network.
    sim.ncp(|n| {
        n.stall(None);
        n.config_mut().network.extended_pan_id = [0xEE; 8];
        n.spontaneous_reset(hub_ezsp::ResetCode::WATCHDOG);
    });
    wait("foreign", || info(&rig.app, sid, &rx)["registry"]["state"] == "foreign");
    thread::sleep(Duration::from_millis(500));
    assert_eq!(on_off(0), 0, "the queued Off did not go to the other network");
    rig.finish();
}

/// EZSP: the hub endpoint is a member of no more groups than the NCP's
/// multicast table took (`network/info` `capacity`); groups without it
/// are not limited.
#[test]
fn the_hub_endpoint_joins_as_many_groups_as_the_ncp_table_holds() {
    use hub_ezsp::ezsp::ConfigId;
    // An NCP image whose memory holds no more than 2 multicast entries.
    let mut sim = ezsp_sim(true, 0xDD);
    sim.config_values = vec![(ConfigId::MULTICAST_TABLE_SIZE, 2)];
    sim.multicast_table_size = 2;
    sim.table_memory = Some(0);
    let rig = Rig::ezsp(sim, Store::volatile());
    let (sid, rx) = session(&rig.app);
    wait("session and network up", || rig.app.status().network_up);
    assert_eq!(info(&rig.app, sid, &rx)["capacity"]["hub_groups"], 2);
    for (i, name) in ["A", "B"].into_iter().enumerate() {
        ok(&call(&rig.app, sid, &rx, i as u64 + 1, json!({"type": "groups/create", "name": name})));
    }
    let c = call(&rig.app, sid, &rx, 3, json!({"type": "groups/create", "name": "C"}));
    assert_eq!(c["error"]["code"], "no_space", "{c}");
    ok(&call(&rig.app, sid, &rx, 4, json!({"type": "groups/create", "name": "C", "hub_member": false})));
    wait("the hub endpoint in two groups", || rig.link.groups().len() == 2);
    rig.finish();
}

/// EZSP: SONOFF's EmberZNet 7.4.5 NCP appends a byte to
/// `incomingMessageHandler` of a device that joined in its session; the
/// device's responses still reach the engine and its interview completes
/// (it ended `partial`, with nothing learned, while such frames were
/// refused).
#[test]
fn an_ncp_that_appends_bytes_to_incoming_messages_still_interviews() {
    let mut sim = ezsp_sim(true, 0xDD);
    sim.incoming_trailer = vec![0x02];
    let mut rig = Rig::ezsp(sim, Store::volatile());
    let answered = Arc::new(Mutex::new(Vec::new()));
    rig.devices(vec![plug()], answered);
    let (sid, rx) = session(&rig.app);
    rig.link.join(PLUG, PLUG_NWK);
    wait("the plug's state", || state(&rig.app, sid, &rx, SWITCH) == json!(false));
    let dev = call(&rig.app, sid, &rx, 2, json!({"type": "devices/get", "device": "zb:5a52530000000001"}));
    assert_eq!(ok(&dev)["interview"], "complete", "{dev}");
    assert_eq!(ok(&dev)["model"], "SIM-PLUG");
    rig.finish();
}

/// The factory IEEE of the dongle a network moves to.
const NEW_DONGLE: u64 = 0x000B_57FF_FE65_4321;

/// EZSP: a backup of the hub's network (bellows' and Z2M's form) restores
/// onto a new dongle. The coordinator IEEE goes to the new NCP's one-time
/// token only with `confirm_ieee_write_once`; refused, nothing changes.
/// Restored, the frame counters are the backup's plus the margin, the
/// records are tagged with the restored network and device traffic flows.
#[test]
fn ezsp_backup_restores_onto_a_new_dongle() {
    let mut src = ezsp_sim(true, 0xDD);
    src.network_key = [0x6E; 16];
    src.nwk_frame_counter = 50_000;
    src.network.nwk_update_id = 2;
    let rig = Rig::ezsp(src, Store::volatile());
    let (sid, rx) = session(&rig.app);
    wait("owned", || info(&rig.app, sid, &rx)["ownership"] == "owned");
    let b = call(&rig.app, sid, &rx, 1, json!({"type": "network/backup"}));
    let backup = ok(&b).clone();
    assert_eq!(backup["metadata"]["internal"]["ezspVersion"], 13, "{backup}");
    assert_eq!(backup["coordinator_ieee"], "000b57fffe123456");
    assert_eq!(backup["network_key"]["key"], "6e".repeat(16));
    assert_eq!(backup["network_key"]["frame_counter"], 50_000);
    assert_eq!(backup["stack_specific"]["ezsp"]["hashed_tclk"], "3c".repeat(16));
    assert_eq!(backup["metadata"]["internal"]["network"]["tc_link_key"]["key"], "5a6967426565416c6c69616e63653039");
    rig.finish();

    let mut dst = ezsp_sim(false, 0xDD);
    dst.eui64 = Eui64::from_u64(NEW_DONGLE);
    let rig = Rig::ezsp(dst, Store::volatile());
    let sim = rig.ezsp.clone().unwrap();
    let (sid, rx) = session(&rig.app);
    let coordinator = |rig: &Rig| ok(&call(&rig.app, sid, &rx, 70, json!({"type": "hub/info"})))["coordinator_ieee"].clone();
    assert_eq!(coordinator(&rig), "000b57fffe654321", "the new dongle's own IEEE");
    let r = call(&rig.app, sid, &rx, 2, json!({"type": "network/restore", "backup": backup}));
    assert_eq!(r["error"]["code"], "invalid_value", "{r}");
    let why = r["error"]["message"].as_str().unwrap();
    assert!(why.contains("confirm_ieee_write_once") && why.contains("only once"), "{why}");
    assert!(sim.ncp(|n| !n.config().network_formed && n.config().custom_eui64.is_none()), "nothing written");
    assert_eq!(rig.tag(), None, "the records untouched");

    let r = call(&rig.app, sid, &rx, 3, json!({"type": "network/restore", "backup": backup, "confirm_ieee_write_once": true}));
    let rep = ok(&r);
    assert_eq!((&rep["pan_id"], &rep["channel"]), (&json!(0x1A62), &json!(15)), "{rep}");
    assert_eq!(rep["nwk_fc_start"], 65_536, "the backup's counter plus 10 000 on the NCP's 4096 boundary, then 4096 more (the second reset)");
    assert!(rep["notes"].as_array().unwrap().iter().any(|n| n["note"] == "ieee_written_once"), "{rep}");
    let n = sim.ncp(|n| n.config().clone());
    assert_eq!(n.custom_eui64, Some(Eui64::from_u64(0x000B_57FF_FE12_3456)));
    assert_eq!((n.network_key, n.nwk_frame_counter, n.tc_link_key), ([0x6E; 16], 65_536, [0x3C; 16]));
    wait("the restored network owned", || {
        let i = info(&rig.app, sid, &rx);
        i["ownership"] == "owned" && i["registry"]["state"] == "owned"
    });
    let t = rig.tag().unwrap();
    assert_eq!((t.ext_pan_id, t.coordinator_ieee), ([0xDD; 8], 0x000B_57FF_FE12_3456u64.to_le_bytes()), "tag rewritten");
    // Binds and the IAS CIE address name the coordinator the devices know.
    assert_eq!(coordinator(&rig), "000b57fffe123456", "the restored IEEE in hub/info");
    assert!(t.key_id.is_some());
    ok(&call(&rig.app, sid, &rx, 4, json!({"type": "network/permit_join", "duration_s": 30})));
    wait("joining open", || rig.link.permit_open());
    rig.finish();
}

/// EZSP: an older backup restored over its own running network through the
/// API (the app leaves the network before the backend restores it): the
/// frame counters start from the network's newer ones, not the backup's.
#[test]
fn ezsp_an_older_backup_of_the_running_network_keeps_its_newer_counters() {
    let mut src = ezsp_sim(true, 0xDD);
    src.nwk_frame_counter = 50_000;
    let rig = Rig::ezsp(src, Store::volatile());
    let sim = rig.ezsp.clone().unwrap();
    let (sid, rx) = session(&rig.app);
    wait("owned", || info(&rig.app, sid, &rx)["ownership"] == "owned");
    let backup = ok(&call(&rig.app, sid, &rx, 1, json!({"type": "network/backup"}))).clone();
    sim.ncp(|n| n.config_mut().nwk_frame_counter = 120_000);
    let r = call(&rig.app, sid, &rx, 2, json!({"type": "network/restore", "backup": backup, "force": true}));
    let rep = ok(&r);
    assert_eq!(rep["nwk_fc_start"], 135_168, "{rep}");
    assert!(rep["notes"].as_array().unwrap().iter().any(|n| n["note"] == "floor_raised"), "{rep}");
    assert_eq!(sim.ncp(|n| n.config().nwk_frame_counter), 135_168);
    rig.finish();
}

/// EZSP (`NcpNv`): a restore over a network the NCP stores needs `force`,
/// over records of another network `confirm` (they are re-tagged);
/// refused, neither the NCP nor the records change. Confirmed, the records
/// are tagged with the restored network.
#[test]
fn ezsp_restore_over_another_network_needs_force() {
    // The backup of network DD, by a coordinator with the sims' IEEE.
    let src = Rig::ezsp(ezsp_sim(true, 0xDD), Store::volatile());
    let (sid, rx) = session(&src.app);
    wait("owned", || info(&src.app, sid, &rx)["ownership"] == "owned");
    let backup = ok(&call(&src.app, sid, &rx, 1, json!({"type": "network/backup"}))).clone();
    src.finish();

    // The NCP runs network EE, the records are its.
    let flash = SharedFlash::new();
    let rig = Rig::ezsp(ezsp_sim(true, 0xEE), flash.store());
    let sim = rig.ezsp.clone().unwrap();
    let (sid, rx) = session(&rig.app);
    wait("owned", || info(&rig.app, sid, &rx)["ownership"] == "owned");
    let r = call(&rig.app, sid, &rx, 2, json!({"type": "network/restore", "backup": backup}));
    assert!(r["error"]["message"].as_str().unwrap().contains("set force "), "{r}");
    // The NCP's network may go, but the records are network EE's: a
    // distinct refusal, its own confirmation.
    let r = call(&rig.app, sid, &rx, 3, json!({"type": "network/restore", "backup": backup, "force": true}));
    assert_eq!(r["error"]["code"], "registry_read_only", "{r}");
    assert!(r["error"]["message"].as_str().unwrap().contains("set confirm "), "{r}");
    assert_eq!(rig.tag().map(|t| t.ext_pan_id), Some([0xEE; 8]));
    assert_eq!(sim.ncp(|n| n.config().network.extended_pan_id), [0xEE; 8]);
    let r = call(&rig.app, sid, &rx, 3, json!({"type": "network/restore", "backup": backup, "force": true, "confirm": true}));
    assert_eq!(ok(&r)["previous_network_backed_up"], true, "{r}");
    assert_eq!(r["result"]["previous_backup"]["ext_pan_id"], "eeeeeeeeeeeeeeee", "the replaced network's backup, summarised");
    wait("the restored network owned", || {
        let i = info(&rig.app, sid, &rx);
        epid_of(&i) == [0xDD; 8] && i["ownership"] == "owned" && i["registry"]["state"] == "owned"
    });
    assert_eq!(rig.tag().map(|t| t.ext_pan_id), Some([0xDD; 8]));
    rig.finish();

    // The NCP lost its network, the records are still network EE's.
    let mut s = flash.store();
    let ee = NetworkIdentity {
        ext_pan_id: [0xEE; 8],
        key_id: None,
        pan_id: 0x1A62,
        channel: 15,
        key_seq: 0,
        coordinator_ieee: [0; 8],
    };
    s.kv_put(OWNER_KEY, OwnerTag::new(BackendKind::Ezsp, Some(ee)).encode().as_bytes()).unwrap();
    drop(s);
    let rig = Rig::ezsp(ezsp_sim(false, 0xEE), flash.store());
    let (sid, rx) = session(&rig.app);
    assert_eq!(info(&rig.app, sid, &rx)["ownership"], "no_network");
    let r = call(&rig.app, sid, &rx, 4, json!({"type": "network/restore", "backup": backup}));
    let why = r["error"]["message"].as_str().unwrap_or_default();
    assert_eq!(r["error"]["code"], "registry_read_only", "{r}");
    assert!(why.contains("re-tags") && why.contains("set confirm ") && !why.contains("force"), "{r}");
    assert_eq!(rig.tag().map(|t| t.ext_pan_id), Some([0xEE; 8]), "the tag kept");
    ok(&call(&rig.app, sid, &rx, 5, json!({"type": "network/restore", "backup": backup, "confirm": true})));
    wait("the restored network owned", || info(&rig.app, sid, &rx)["registry"]["state"] == "owned");
    assert_eq!(rig.tag().map(|t| t.ext_pan_id), Some([0xDD; 8]));
    rig.finish();
}

/// EZSP: the network a restore replaces is backed up first; when the
/// records are another backend's (foreign, closed) it is not the last
/// backup, but it is kept as the replaced network's (hub-scoped), and the
/// answer only summarises it: no key leaves with it.
#[test]
fn ezsp_restore_over_foreign_records_returns_the_replaced_networks_backup() {
    let src = Rig::ezsp(ezsp_sim(true, 0xDD), Store::volatile());
    let (sid, rx) = session(&src.app);
    wait("owned", || info(&src.app, sid, &rx)["ownership"] == "owned");
    let backup = ok(&call(&src.app, sid, &rx, 1, json!({"type": "network/backup"}))).clone();
    src.finish();

    let mut s = Store::volatile();
    s.kv_put(OWNER_KEY, OwnerTag::new(BackendKind::Zrhp, None).encode().as_bytes()).unwrap();
    let mut ee = ezsp_sim(true, 0xEE);
    ee.network_key = [0x5E; 16];
    let rig = Rig::ezsp(ee, s);
    let (sid, rx) = session(&rig.app);
    wait("foreign", || info(&rig.app, sid, &rx)["registry"]["state"] == "foreign");
    let r = call(&rig.app, sid, &rx, 2, json!({"type": "network/restore", "backup": backup, "force": true, "confirm": true}));
    let rep = ok(&r);
    assert_eq!(rep["previous_network_backed_up"], true, "{rep}");
    let prev = &rep["previous_backup"];
    assert_eq!((prev["ext_pan_id"].as_str(), prev["kept"].as_str()), (Some("eeeeeeeeeeeeeeee"), Some("network/backup/replaced")), "{rep}");
    assert_eq!((prev["pan_id"].as_u64(), prev["channel"].as_u64()), (Some(0x1A62), Some(15)), "{rep}");
    no_keys(&r, [0x5E; 16]);
    wait("the restored network owned", || info(&rig.app, sid, &rx)["registry"]["state"] == "owned");
    let last = call(&rig.app, sid, &rx, 3, json!({"type": "network/backup/last"}));
    assert_ne!(ok(&last)["backup"]["extended_pan_id"], "eeeeeeeeeeeeeeee", "the closed records kept nothing of it");
    let kept = call(&rig.app, sid, &rx, 4, json!({"type": "network/backup/replaced", "ext_pan_id": prev["ext_pan_id"]}));
    assert_eq!(ok(&kept)["backup"]["network_key"]["key"], "5e".repeat(16), "kept, whatever the gate");
    rig.finish();
}

/// No key in an answer: not the network key `key`, nor any `"key"` field.
fn no_keys(frame: &J, key: [u8; 16]) {
    let t = frame.to_string();
    let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
    assert!(!t.contains(&hex) && !t.contains("\"key\""), "keys in an answer: {t}");
}

/// The network of EE over another backend's (foreign, closed) records, and
/// the backup of network DD (the sims' IEEE).
fn foreign_ee(max_tx_power: Option<i8>) -> (Rig, J) {
    let src = Rig::ezsp(ezsp_sim(true, 0xDD), Store::volatile());
    let (sid, rx) = session(&src.app);
    wait("owned", || info(&src.app, sid, &rx)["ownership"] == "owned");
    let backup = ok(&call(&src.app, sid, &rx, 1, json!({"type": "network/backup"}))).clone();
    src.finish();
    let mut s = Store::volatile();
    s.kv_put(OWNER_KEY, OwnerTag::new(BackendKind::Zrhp, None).encode().as_bytes()).unwrap();
    let mut ee = ezsp_sim(true, 0xEE);
    ee.network_key = [0x5E; 16];
    ee.max_tx_power = max_tx_power;
    let rig = Rig::ezsp(ee, s);
    (rig, backup)
}

/// A restore or reset that fails after leaving the network (the NCP forms
/// at no TX power) over closed records: the replaced network's backup is in
/// the error frame and kept (`network/backup/replaced`), for another
/// session too.
#[test]
fn ezsp_a_replacement_that_fails_after_the_leave_keeps_the_replaced_network() {
    for what in ["restore", "reset"] {
        let (rig, backup) = foreign_ee(Some(-30));
        let sim = rig.ezsp.clone().unwrap();
        let (sid, rx) = session(&rig.app);
        wait("foreign", || info(&rig.app, sid, &rx)["registry"]["state"] == "foreign");
        let req = if what == "restore" {
            json!({"type": "network/restore", "backup": backup, "force": true, "confirm": true})
        } else {
            json!({"type": "network/reset", "confirm": "RESET", "channel": 15})
        };
        let r = call(&rig.app, sid, &rx, 2, req);
        assert_eq!(r["success"], false, "{what}: {r}");
        assert_eq!(r["previous_backup"]["ext_pan_id"], "eeeeeeeeeeeeeeee", "{what}: summarised in the error frame");
        assert_eq!(r["previous_backup"]["kept"], "network/backup/replaced", "{what}");
        no_keys(&r, [0x5E; 16]);
        assert!(sim.ncp(|n| !n.config().network_formed), "{what}: the network was left");
        let (sid2, rx2) = session(&rig.app);
        let kept = call(&rig.app, sid2, &rx2, 3, json!({"type": "network/backup/replaced"}));
        assert_eq!(ok(&kept)["backup"]["extended_pan_id"], "eeeeeeeeeeeeeeee", "{what}: kept");
        assert_eq!(ok(&kept)["backup"]["network_key"]["key"], "5e".repeat(16), "{what}: kept");
        rig.finish();
    }
}

/// The session that started a restore is gone (a page reload) before the
/// answer: the replaced network's backup is not lost with it.
#[test]
fn ezsp_the_replaced_networks_backup_survives_the_requesting_session() {
    let (rig, backup) = foreign_ee(None);
    let (sid, rx) = session(&rig.app);
    wait("foreign", || info(&rig.app, sid, &rx)["registry"]["state"] == "foreign");
    let req = json!({"id": 2, "type": "network/restore", "backup": backup, "force": true, "confirm": true});
    rig.app.text(sid, req.to_string());
    rig.app.disconnect(sid);
    drop(rx);
    let (sid, rx) = session(&rig.app);
    wait("the restored network owned", || {
        let i = info(&rig.app, sid, &rx);
        i["ext_pan_id"] == "dddddddddddddddd" && i["registry"]["state"] == "owned"
    });
    let kept = call(&rig.app, sid, &rx, 3, json!({"type": "network/backup/replaced"}));
    assert_eq!(ok(&kept)["backup"]["extended_pan_id"], "eeeeeeeeeeeeeeee", "{kept}");
    assert_eq!(ok(&kept)["backup"]["network_key"]["key"], "5e".repeat(16));
    rig.finish();
}

/// Network DD restored over EE (foreign records), then restored again
/// while the NCP runs DD (the retry after a restore that failed late, the
/// NCP back with DD): the half-restored DD is not another network to
/// keep, and EE's kept backup stays.
#[test]
fn ezsp_a_retry_over_the_restored_network_keeps_the_replaced_one() {
    let (rig, backup) = foreign_ee(None);
    let (sid, rx) = session(&rig.app);
    wait("foreign", || info(&rig.app, sid, &rx)["registry"]["state"] == "foreign");
    let first = call(&rig.app, sid, &rx, 2, json!({"type": "network/restore", "backup": backup, "force": true, "confirm": true}));
    assert_eq!(ok(&first)["previous_network_backed_up"], true, "{first}");
    wait("the restored network owned", || info(&rig.app, sid, &rx)["registry"]["state"] == "owned");
    let again = call(&rig.app, sid, &rx, 3, json!({"type": "network/restore", "backup": backup, "force": true}));
    assert_eq!(ok(&again)["previous_backup"]["ext_pan_id"], "dddddddddddddddd", "{again}");
    assert_eq!(ok(&again)["previous_backup"]["kept"], "network/backup/last", "{again}");
    assert_eq!(ok(&again)["previous_network_backed_up"], true, "as the last backup: {again}");
    wait("the restored network owned", || info(&rig.app, sid, &rx)["registry"]["state"] == "owned");
    let kept = call(&rig.app, sid, &rx, 4, json!({"type": "network/backup/replaced"}));
    assert_eq!(ok(&kept)["replaced"], json!(["eeeeeeeeeeeeeeee"]), "{kept}");
    assert_eq!(ok(&kept)["backup"]["network_key"]["key"], "5e".repeat(16));
    rig.finish();
}

/// An automatic backup without the network key (the NCP's key export
/// locked) neither replaces a kept one nor lets the network go: refused,
/// unless `skip_backup`.
#[test]
fn ezsp_an_incomplete_automatic_backup_is_refused() {
    let (rig, backup) = foreign_ee(None);
    let sim = rig.ezsp.clone().unwrap();
    let (sid, rx) = session(&rig.app);
    wait("foreign", || info(&rig.app, sid, &rx)["registry"]["state"] == "foreign");
    ok(&call(&rig.app, sid, &rx, 2, json!({"type": "network/restore", "backup": backup, "force": true, "confirm": true})));
    wait("the restored network owned", || info(&rig.app, sid, &rx)["registry"]["state"] == "owned");
    sim.ncp(|n| n.config_mut().key_exportable = false);
    let reset = json!({"type": "network/reset", "confirm": "RESET", "channel": 15});
    let r = call(&rig.app, sid, &rx, 3, reset.clone());
    assert_eq!(r["error"]["code"], "backup_incomplete", "{r}");
    assert!(sim.ncp(|n| n.config().network_formed && n.config().network.extended_pan_id == [0xDD; 8]), "nothing changed");
    let kept = call(&rig.app, sid, &rx, 4, json!({"type": "network/backup/replaced"}));
    assert_eq!(ok(&kept)["replaced"], json!(["eeeeeeeeeeeeeeee"]), "{kept}");
    assert_eq!(ok(&kept)["backup"]["network_key"]["key"], "5e".repeat(16));
    let mut skip = reset;
    skip["skip_backup"] = json!(true);
    ok(&call(&rig.app, sid, &rx, 5, skip));
    rig.finish();
}

/// Every network a reset replaces keeps its backup, the last
/// REPLACED_KEPT of them, newest first; `ext_pan_id` picks one.
#[test]
fn ezsp_replaced_networks_keep_their_backups() {
    let rig = Rig::ezsp(ezsp_sim(true, 0xEE), Store::volatile());
    let (sid, rx) = session(&rig.app);
    wait("owned", || info(&rig.app, sid, &rx)["ownership"] == "owned");
    for (i, e) in ["11", "22", "33", "44"].into_iter().enumerate() {
        let r = call(&rig.app, sid, &rx, 2 + i as u64, json!({"type": "network/reset", "confirm": "RESET", "channel": 15, "ext_pan_id": e.repeat(8)}));
        assert_eq!(ok(&r)["network"]["ext_pan_id"], e.repeat(8), "{r}");
        wait("owned", || info(&rig.app, sid, &rx)["ownership"] == "owned");
    }
    let kept = call(&rig.app, sid, &rx, 10, json!({"type": "network/backup/replaced"}));
    assert_eq!(ok(&kept)["replaced"], json!(["3".repeat(16), "2".repeat(16), "1".repeat(16)]), "EE's went first: {kept}");
    assert_eq!(ok(&kept)["backup"]["extended_pan_id"], "3".repeat(16));
    let one = call(&rig.app, sid, &rx, 11, json!({"type": "network/backup/replaced", "ext_pan_id": "1".repeat(16)}));
    assert_eq!(ok(&one)["backup"]["extended_pan_id"], "1".repeat(16), "{one}");
    let gone = call(&rig.app, sid, &rx, 12, json!({"type": "network/backup/replaced", "ext_pan_id": "EE".repeat(8)}));
    assert_eq!(gone["error"]["code"], "not_found", "{gone}");
    rig.finish();
}

/// The NCP runs network DD with another key than the backup of DD
/// (`[0x5E; 16]`), over the hub's own records or another backend's
/// (closed); `key_exportable` as the NCP's.
fn dd_rekeyed(foreign: bool, key_exportable: bool) -> (Rig, J) {
    let src = Rig::ezsp(ezsp_sim(true, 0xDD), Store::volatile());
    let (sid, rx) = session(&src.app);
    wait("owned", || info(&src.app, sid, &rx)["ownership"] == "owned");
    let backup = ok(&call(&src.app, sid, &rx, 1, json!({"type": "network/backup"}))).clone();
    src.finish();
    let mut s = Store::volatile();
    if foreign {
        s.kv_put(OWNER_KEY, OwnerTag::new(BackendKind::Zrhp, None).encode().as_bytes()).unwrap();
    }
    let mut dd = ezsp_sim(true, 0xDD);
    dd.network_key = [0x5E; 16];
    dd.key_exportable = key_exportable;
    (Rig::ezsp(dd, s), backup)
}

/// A restore of DD over the NCP's network DD with another key: another
/// network, kept as a replaced one (over closed records, which keep no
/// last backup, and over the hub's own), not skipped as a retry.
#[test]
fn ezsp_the_same_extended_pan_id_with_another_key_is_kept() {
    for foreign in [true, false] {
        let (rig, backup) = dd_rekeyed(foreign, true);
        let (sid, rx) = session(&rig.app);
        let state = if foreign { "foreign" } else { "owned" };
        wait(state, || info(&rig.app, sid, &rx)["registry"]["state"] == state);
        let r = call(&rig.app, sid, &rx, 2, json!({"type": "network/restore", "backup": backup, "force": true, "confirm": true}));
        assert_eq!(ok(&r)["previous_network_backed_up"], true, "{r}");
        wait("the restored network owned", || info(&rig.app, sid, &rx)["registry"]["state"] == "owned");
        let kept = call(&rig.app, sid, &rx, 3, json!({"type": "network/backup/replaced"}));
        assert_eq!(ok(&kept)["replaced"], json!(["d".repeat(16)]), "foreign {foreign}: {kept}");
        assert_eq!(ok(&kept)["backup"]["network_key"]["key"], "5e".repeat(16), "the replaced key");
        rig.finish();
    }
}

/// The same network DD, its key not exportable, over closed records: the
/// automatic backup is incomplete and refused, also when the backup to
/// restore has the same extended PAN ID.
#[test]
fn ezsp_an_incomplete_backup_of_the_same_extended_pan_id_is_refused() {
    let (rig, backup) = dd_rekeyed(true, false);
    let sim = rig.ezsp.clone().unwrap();
    let (sid, rx) = session(&rig.app);
    wait("foreign", || info(&rig.app, sid, &rx)["registry"]["state"] == "foreign");
    let r = call(&rig.app, sid, &rx, 2, json!({"type": "network/restore", "backup": backup, "force": true, "confirm": true}));
    assert_eq!(r["error"]["code"], "backup_incomplete", "{r}");
    assert!(sim.ncp(|n| n.config().network_formed && n.config().network_key == [0x5E; 16]), "nothing changed");
    rig.finish();
}

// Migration from Zigbee2MQTT or ZHA, on both backends.
#[path = "tests_import.rs"]
mod import;

/// The plug as the NCP keeps it after its removal: a router neighbor (it
/// ignored its leave, or its entry has not aged out yet); ZRHP: also in the
/// `DEVICE` table, until `TBL_INVALIDATE`.
fn plug_neighbor() -> hub_ezsp::ezsp::NeighborTableEntry {
    hub_ezsp::ezsp::NeighborTableEntry {
        short_id: PLUG_NWK,
        long_id: Eui64::from_u64(PLUG),
        average_lqi: 200,
        ..Default::default()
    }
}

fn ezsp_with_plug() -> hub_ezsp::sim::SimConfig {
    let mut c = ezsp_sim(true, 0xDD);
    c.neighbors = vec![plug_neighbor()];
    c
}

fn devices_list(app: &App, sid: u64, rx: &Receiver<String>) -> Vec<String> {
    let l = call(app, sid, rx, 80, json!({"type": "devices/list"}));
    ok(&l).as_array().unwrap().iter().map(|d| d["id"].as_str().unwrap().to_string()).collect()
}

/// A removed device stays removed while the NCP's tables still list it:
/// neither a new NCP session nor a hub restart imports it again. The NCP
/// purges what it can (ZRHP: `TBL_INVALIDATE` and the host store's device
/// record, `NWK_REMOVE_DEVICE` with flags bit2, removal through the parent,
/// answered `UNSUPPORTED_COMMAND` as by the zigbee-rs NCP; EZSP: an APS
/// Remove-Device to the router), whether the device was reachable
/// (Mgmt_Leave_req) or not (`force`). It rejoins with the network key it
/// holds while joining is closed: asked to leave again, still removed.
/// Paired again (joining open) it is back as any device, and stays after
/// a restart.
#[test]
fn a_removed_device_is_not_imported_again_until_it_joins() {
    for kind in BOTH {
        for force in [true, false] {
            removed_device_stays_removed(kind, force);
        }
    }
}

fn removed_device_stays_removed(kind: Kind, force: bool) {
    use zigbee_zrhp::msg::ids;
    let what = format!("{kind:?} force {force}");
    let flash = SharedFlash::new();
    let plug_id = "zb:5a52530000000001";
    let mut rig = match kind {
        Kind::Zrhp => Rig::zrhp(true),
        Kind::Ezsp => Rig::ezsp(ezsp_sim(true, 0xDD), flash.store()),
    };
    let answered = Arc::new(Mutex::new(Vec::new()));
    rig.devices(vec![plug()], answered.clone());
    let (sid, rx) = session(&rig.app);
    rig.link.join(PLUG, PLUG_NWK);
    wait(&format!("{what}: the plug interviewed"), || state(&rig.app, sid, &rx, SWITCH) == json!(false));
    match kind {
        Kind::Zrhp => {
            let ncp = rig.zrhp.as_ref().unwrap();
            assert!(ncp.store_state().devices.contains_key(&PLUG.to_le_bytes()), "{what}: the joined device's record");
            rig.zsim.as_ref().unwrap().with(|s| {
                s.neighbors = Some(vec![(PLUG.to_le_bytes(), PLUG_NWK, 1, 2)]);
                s.table_devices = vec![(PLUG.to_le_bytes(), PLUG_NWK, None)];
                s.unsupported.push(ids::NWK_REMOVE_DEVICE);
            });
        }
        Kind::Ezsp => rig.ezsp.as_ref().unwrap().ncp(|n| n.config_mut().neighbors = vec![plug_neighbor()]),
    }

    ok(&call(&rig.app, sid, &rx, 1, json!({"type": "devices/remove", "device": plug_id, "force": force})));
    assert_eq!(devices_list(&rig.app, sid, &rx), Vec::<String>::new(), "{what}");
    match kind {
        Kind::Zrhp => {
            let (ncp, zsim) = (rig.zrhp.clone().unwrap(), rig.zsim.clone().unwrap());
            wait(&format!("{what}: TBL_INVALIDATE"), || zsim.state().log.iter().any(|(id, _)| *id == ids::TBL_INVALIDATE));
            let invalidated = zsim.state().log.into_iter().find(|(id, _)| *id == ids::TBL_INVALIDATE).unwrap().1;
            assert_eq!(invalidated, [&[5u8, 1][..], &PLUG.to_le_bytes()].concat(), "{what}: DEVICE table, by IEEE");
            let remove = zsim.state().trace.unwrap().into_iter().find(|(id, _)| *id == ids::NWK_REMOVE_DEVICE).unwrap().1;
            assert_eq!(remove, [&PLUG_NWK.to_le_bytes()[..], &PLUG.to_le_bytes(), &[0x04]].concat(), "{what}: through its parent, no rejoin");
            assert!(zsim.state().table_devices.is_empty(), "{what}");
            wait(&format!("{what}: the host store's record"), || !ncp.store_state().devices.contains_key(&PLUG.to_le_bytes()));
        }
        Kind::Ezsp => {
            let sim = rig.ezsp.clone().unwrap();
            wait(&format!("{what}: Remove-Device"), || !sim.ncp(|n| n.removals().is_empty()));
            let eui = Eui64::from_u64(PLUG);
            assert_eq!(sim.ncp(|n| n.removals().to_vec()), [(PLUG_NWK, eui, eui)], "{what}");
        }
    }
    let leave = || answered.lock().unwrap().iter().any(|r| r.profile == 0 && r.cluster == 0x0034);
    if force {
        assert!(!leave(), "{what}: no Mgmt_Leave_req");
    } else {
        wait(&format!("{what}: Mgmt_Leave_req"), leave);
    }
    let kv = rig.kv.clone();
    let stored = move || kv::with(&*kv, |s| s.get(crate::removed::KEY).map(<[u8]>::to_vec));
    wait(&format!("{what}: remembered"), || stored().is_some());

    // A new NCP session: the NCP lists the plug, the hub does not take it.
    match kind {
        Kind::Zrhp => rig.zsim.as_ref().unwrap().reboot(),
        Kind::Ezsp => rig.ezsp.as_ref().unwrap().ncp(|n| n.spontaneous_reset(hub_ezsp::ResetCode::WATCHDOG)),
    }
    wait(&format!("{what}: skipped after the NCP reset"), || rig.app.status().removed_skipped >= 1);
    assert_eq!(devices_list(&rig.app, sid, &rx), Vec::<String>::new(), "{what}");

    // A hub restart, the NCP unchanged: still not imported.
    let mut rig = match kind {
        Kind::Zrhp => rig.restart_zrhp(),
        Kind::Ezsp => {
            rig.finish();
            Rig::ezsp(ezsp_with_plug(), flash.store())
        }
    };
    let (sid, rx) = session(&rig.app);
    wait(&format!("{what}: skipped after the restart"), || rig.app.status().removed_skipped >= 1);
    assert_eq!(devices_list(&rig.app, sid, &rx), Vec::<String>::new(), "{what}");
    assert_eq!(rig.app.status().imported, 0, "{what}");

    // It rejoins (secured, with the network key it holds) while joining is
    // closed: asked to leave again, the coordinator removes it again, it
    // stays removed; a network event says so.
    let answered = Arc::new(Mutex::new(Vec::new()));
    rig.devices(vec![plug()], answered.clone());
    ok(&call(&rig.app, sid, &rx, 7, json!({"type": "subscribe", "topics": ["network"]})));
    rig.link.join(PLUG, PLUG_NWK);
    let leave = || answered.lock().unwrap().iter().filter(|r| r.profile == 0 && r.cluster == 0x0034).count();
    wait(&format!("{what}: asked to leave again"), || leave() == 1);
    let back = loop {
        let f: J = serde_json::from_str(&rx.recv_timeout(T).unwrap()).unwrap();
        if f["type"] == "event" && f["event"]["topic"] == "removed_device_back" {
            break f["event"].clone();
        }
    };
    assert_eq!((back["device"].as_str(), back["via"].as_str()), (Some(plug_id), Some("rejoin")), "{what}: {back}");
    match kind {
        Kind::Zrhp => {
            let zsim = rig.zsim.clone().unwrap();
            wait(&format!("{what}: purged again"), || zsim.state().log.iter().filter(|(id, _)| *id == ids::TBL_INVALIDATE).count() >= 2);
        }
        Kind::Ezsp => {
            // The NCP of the restarted hub.
            let sim = rig.ezsp.clone().unwrap();
            wait(&format!("{what}: Remove-Device again"), || !sim.ncp(|n| n.removals().is_empty()));
        }
    }
    // Its announcement too (at most one leave request a minute).
    rig.link.indicate(&plug().ind(0, 0, 0x0013, hub_devices::zdo::device_annce(2, PLUG_NWK, PLUG, 0x8E)));
    thread::sleep(Duration::from_millis(300));
    assert_eq!(devices_list(&rig.app, sid, &rx), Vec::<String>::new(), "{what}: still removed");
    assert_eq!(leave(), 1, "{what}");
    assert!(stored().is_some(), "{what}: still remembered");

    // Paired again (joining open): back, interviewed, and no longer
    // remembered.
    ok(&call(&rig.app, sid, &rx, 8, json!({"type": "network/permit_join", "duration_s": 60})));
    rig.link.join(PLUG, PLUG_NWK);
    wait(&format!("{what}: the plug back"), || state(&rig.app, sid, &rx, SWITCH) == json!(false));
    let kv = rig.kv.clone();
    wait(&format!("{what}: forgotten as removed"), || kv::with(&*kv, |s| s.get(crate::removed::KEY).is_none()));
    crate::tests::stop(&rig.app);

    // Restarted once more: restored as any device.
    let rig = match kind {
        Kind::Zrhp => rig.restart_zrhp(),
        Kind::Ezsp => {
            rig.finish();
            Rig::ezsp(ezsp_with_plug(), flash.store())
        }
    };
    let (sid, rx) = session(&rig.app);
    wait(&format!("{what}: restored"), || rig.app.status().restored == 1);
    assert_eq!(devices_list(&rig.app, sid, &rx), [plug_id], "{what}");
    rig.finish();
}

// ---- the Dongle-M's NCP replaced: backend probes and the switch ----

/// A byte port shared by the probes (and, between them, a simulated NCP's
/// line): the EZSP probe's transport, the ZRHP probe's halves.
struct Line2<R, W>(Arc<Mutex<R>>, Arc<Mutex<W>>);

impl<R, W> Clone for Line2<R, W> {
    fn clone(&self) -> Self {
        Line2(self.0.clone(), self.1.clone())
    }
}

trait ReadBytes: Send {
    fn read_bytes(&mut self, buf: &mut [u8], timeout: Duration) -> std::io::Result<usize>;
}
trait WriteBytes: Send {
    fn write_bytes(&mut self, data: &[u8]) -> std::io::Result<()>;
}

impl ReadBytes for hub_ncp::transport::pipe::Rx {
    fn read_bytes(&mut self, buf: &mut [u8], timeout: Duration) -> std::io::Result<usize> {
        hub_ncp::transport::PortRx::read(self, buf, timeout)
    }
}
impl WriteBytes for hub_ncp::transport::pipe::Tx {
    fn write_bytes(&mut self, data: &[u8]) -> std::io::Result<()> {
        hub_ncp::transport::PortTx::write_all(self, data)
    }
}
impl WriteBytes for Arc<Mutex<hub_ezsp::sim::SimTransport>> {
    fn write_bytes(&mut self, data: &[u8]) -> std::io::Result<()> {
        hub_ezsp::Transport::write_all(&mut *self.lock().unwrap(), data)
    }
}
impl ReadBytes for Arc<Mutex<hub_ezsp::sim::SimTransport>> {
    fn read_bytes(&mut self, buf: &mut [u8], timeout: Duration) -> std::io::Result<usize> {
        // Short slices: the writer gets the transport between them.
        let t0 = Instant::now();
        loop {
            let n = hub_ezsp::Transport::read(&mut *self.lock().unwrap(), buf, Duration::from_millis(5))?;
            if n > 0 || t0.elapsed() >= timeout {
                return Ok(n);
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
}

impl<R: ReadBytes, W: WriteBytes> hub_ncp::transport::PortRx for Line2<R, W> {
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> std::io::Result<usize> {
        self.0.lock().unwrap().read_bytes(buf, timeout)
    }
}
impl<R: ReadBytes, W: WriteBytes> hub_ncp::transport::PortTx for Line2<R, W> {
    fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.1.lock().unwrap().write_bytes(data)
    }
    fn set_line(&mut self, _: hub_ncp::transport::Line) -> std::io::Result<()> {
        Ok(())
    }
}
impl<R: ReadBytes, W: WriteBytes> hub_ezsp::Transport for Line2<R, W> {
    fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.1.lock().unwrap().write_bytes(data)
    }
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> std::io::Result<usize> {
        self.0.lock().unwrap().read_bytes(buf, timeout)
    }
}

fn quick() -> crate::backend::select::ProbeTimes {
    crate::backend::select::ProbeTimes { ezsp_rstack_ms: 600, ezsp_reset_retries: 0, zrhp_per_line: Duration::from_millis(1500) }
}

/// Each probe finds its own simulated NCP, and a probe of the other
/// protocol first leaves it as it was: the ZRHP NCP gets no request but
/// the session's own, the EmberZNet NCP keeps its network and answers the
/// EZSP probe after the ZRHP one.
#[test]
fn backend_probes_find_their_ncp_and_leave_the_other_unchanged() {
    use crate::backend::select::{Found, probe_ezsp, probe_zrhp};
    let lines = vec![hub_ncp::transport::Line::BR_FALLBACK, hub_ncp::transport::Line::BR_DEFAULT];

    // The zigbee-rs NCP.
    let (host, ncp_end) = hub_ncp::transport::pipe::pair();
    let zsim = hub_ncp::sim::SimNcp::start(ncp_end);
    let (rx, tx) = host.split();
    let port = Line2(Arc::new(Mutex::new(rx)), Arc::new(Mutex::new(tx)));
    let e = probe_ezsp(port.clone(), hub_ezsp::SystemClock::default(), quick()).unwrap_err();
    assert!(e.contains("no EZSP answer"), "{e}");
    let found = probe_zrhp(port.clone(), port.clone(), lines.clone(), 0x5EED_0001, quick()).unwrap();
    let Found::Zrhp { fw, build_id, ieee, .. } = &found else { panic!("{found:?}") };
    assert_eq!((fw.as_str(), *build_id), ("sim-ncp 0.1.0", [0x51; 8]));
    assert_eq!(*ieee, Some([0x5A, 0x51, 0, 0, 0, 0, 1, 2]));
    let asked: Vec<u16> = zsim.with(|s| s.log.iter().map(|(id, _)| *id).collect());
    assert!(asked.iter().all(|id| *id < 0x0100), "SYSTEM requests only: {asked:04X?}");
    // Asked again (the backend's session later): it answers as before.
    assert_eq!(probe_zrhp(port.clone(), port, lines.clone(), 0x5EED_0002, quick()).unwrap(), found);
    zsim.stop();

    // SONOFF's EmberZNet NCP.
    let (sim, transport, clock) = hub_ezsp::sim::Sim::new(ezsp_sim(true, 0xDD), hub_ezsp::sim::Faults::none());
    sim.set_pace(Some(1));
    let shared = Arc::new(Mutex::new(transport));
    let port = Line2(Arc::new(Mutex::new(shared.clone())), Arc::new(Mutex::new(shared)));
    assert_eq!(probe_zrhp(port.clone(), port.clone(), lines, 0x5EED_0003, quick()).unwrap_err(), "no ZRHP session");
    let found = probe_ezsp(port, clock, quick()).unwrap();
    let Found::Ezsp { version, eui64 } = &found else { panic!("{found:?}") };
    assert!(version.starts_with("EZSP 13"), "{version}");
    assert_eq!(*eui64, Some(0x000B_57FF_FE12_3456u64.to_le_bytes()));
    assert!(sim.ncp(|n| n.config().network_formed && n.config().network.extended_pan_id == [0xDD; 8]), "the network kept");
}

/// The SONOFF Dongle-M's MG24 gets the zigbee-rs NCP (EZSP → ZRHP): the
/// backup kept before the flash (`network/backup` with `keep`) survives
/// the backend change (hub-scoped), the new backend finds the records
/// another backend's (foreign), the kept network goes onto the empty NCP
/// under the same coordinator IEEE (the same chip), and the device works
/// on without pairing again, name and all.
#[test]
fn the_network_and_its_devices_move_from_the_ezsp_ncp_to_the_zrhp_ncp() {
    use crate::backend::select::{AfterSwitch, after_switch};
    // The ZRHP simulator's IEEE: the chip's own EUI-64 under both firmwares.
    const CHIP_EUI64: [u8; 8] = [0x5A, 0x51, 0, 0, 0, 0, 1, 2];
    let flash = SharedFlash::new();
    let mut c = ezsp_sim(true, 0xDD);
    c.eui64 = Eui64(CHIP_EUI64);
    c.nwk_frame_counter = 70_000;
    // As the hub forms an EmberZNet network without a TX power set.
    c.network.radio_tx_power = 0;
    let mut rig = Rig::ezsp(c, flash.store());
    rig.devices(vec![plug()], Arc::new(Mutex::new(Vec::new())));
    let (sid, rx) = session(&rig.app);
    wait("owned", || info(&rig.app, sid, &rx)["ownership"] == "owned");
    rig.link.join(PLUG, PLUG_NWK);
    wait("the plug interviewed", || state(&rig.app, sid, &rx, SWITCH) == json!(false));
    ok(&call(&rig.app, sid, &rx, 1, json!({"type": "devices/update", "device": "zb:5a52530000000001", "name": "Kettle"})));
    let r = call(&rig.app, sid, &rx, 2, json!({"type": "network/backup", "keep": true, "keys": false}));
    assert!(r["error"]["message"].as_str().unwrap().contains("keys true"), "{r}");
    let backup = ok(&call(&rig.app, sid, &rx, 3, json!({"type": "network/backup", "keep": true}))).clone();
    assert_eq!(backup["coordinator_ieee"], "020100000000515a");
    rig.finish();

    // The new firmware: the same store, an NCP without a network.
    let mut rig = Rig::zrhp_with(flash.store(), false);
    let answered = Arc::new(Mutex::new(Vec::new()));
    rig.devices(vec![plug()], answered.clone());
    let (sid, rx) = session(&rig.app);
    let i = info(&rig.app, sid, &rx);
    assert_eq!((&i["state"], &i["registry"]["state"], &i["registry"]["backend"]), (&json!("none"), &json!("foreign"), &json!("ezsp")), "{i}");
    let kept = ok(&call(&rig.app, sid, &rx, 4, json!({"type": "network/backup/replaced", "ext_pan_id": "dddddddddddddddd"})))["backup"].clone();
    assert_eq!(kept, backup, "kept through the backend change");
    assert_eq!(after_switch(&i, &kept), AfterSwitch::Restore);
    let r = call(&rig.app, sid, &rx, 5, json!({"type": "network/restore", "backup": kept, "confirm": true}));
    let rep = ok(&r);
    assert!(rep["nwk_fc_start"].as_u64().unwrap() >= 80_000, "the backup's counter plus the margin: {rep}");
    // EmberZNet's hashed link key root goes to the zigbee-rs NCP, which
    // derives the devices' link keys from it as EmberZNet did.
    let root = backup["stack_specific"]["ezsp"]["hashed_tclk"].as_str().unwrap().to_string();
    let stream = rig.zsim.as_ref().unwrap().with(|s| s.restored.clone()).unwrap();
    let roots: Vec<String> = zigbee_zrhp::records::RawRecords::new(&stream)
        .flatten()
        .filter_map(|(_, tlv)| match zigbee_zrhp::codec::Reader::new(tlv).read() {
            Ok(zigbee_zrhp::records::Record::EzspHashedTclk(k)) => Some(k.iter().map(|b| format!("{b:02x}")).collect()),
            _ => None,
        })
        .collect();
    assert_eq!(roots, [root], "the root record restored");
    // No TX power was set: the EmberZNet network's 0 dBm is not applied,
    // the zigbee-rs NCP keeps its own (0x7F), and none is set afterwards.
    assert_eq!(backup["metadata"]["internal"]["tx_power"], 0);
    assert_eq!(restored_tx_power(&rig), [0x7F]);
    assert_eq!(rig.zrhp.as_ref().unwrap().stored_tx_power(), None);
    wait("the restored network owned", || {
        let i = info(&rig.app, sid, &rx);
        i["ownership"] == "owned" && i["registry"]["state"] == "owned" && i["ext_pan_id"] == "dddddddddddddddd"
    });
    assert_eq!(after_switch(&info(&rig.app, sid, &rx), &kept), AfterSwitch::Done);
    wait("the registry loaded", || devices_list(&rig.app, sid, &rx).len() == 1);
    let dev = ok(&call(&rig.app, sid, &rx, 6, json!({"type": "devices/get", "device": "zb:5a52530000000001"}))).clone();
    assert_eq!((&dev["name"], &dev["interview"]), (&json!("Kettle"), &json!("complete")), "{dev}");
    ok(&call(&rig.app, sid, &rx, 7, json!({"type": "entities/command", "uid": SWITCH, "command": "turn_on"})));
    wait("On reached the plug through the ZRHP NCP", || {
        answered.lock().unwrap().iter().any(|r| r.cluster == 0x0006 && r.payload.get(2) == Some(&0x01))
    });
    rig.finish();
}

/// `tx_power` of the NETWORK records of the simulated ZRHP NCP's last restore.
fn restored_tx_power(rig: &Rig) -> Vec<i8> {
    let stream = rig.zsim.as_ref().unwrap().with(|s| s.restored.clone()).unwrap();
    zigbee_zrhp::records::RawRecords::new(&stream)
        .flatten()
        .filter_map(|(_, tlv)| match zigbee_zrhp::codec::Reader::new(tlv).read() {
            Ok(zigbee_zrhp::records::Record::Network(n)) => Some(n.tx_power),
            _ => None,
        })
        .collect()
}

/// The TX power the operator set goes with a backup of the hub: restored
/// on another hub, the NCP gets it and that hub keeps it (re-applied after
/// every NCP restart).
#[test]
fn the_operators_tx_power_goes_with_the_backup() {
    let rig = Rig::zrhp(true);
    let (sid, rx) = session(&rig.app);
    wait("owned", || info(&rig.app, sid, &rx)["ownership"] == "owned");
    ok(&call(&rig.app, sid, &rx, 1, json!({"type": "network/set_tx_power", "dbm": -3})));
    let backup = ok(&call(&rig.app, sid, &rx, 2, json!({"type": "network/backup"}))).clone();
    assert_eq!(backup["metadata"]["internal"]["zigbee_rs"]["tx_power"], -3);
    rig.finish();

    let rig = Rig::zrhp(false);
    let (sid, rx) = session(&rig.app);
    ok(&call(&rig.app, sid, &rx, 3, json!({"type": "network/restore", "backup": backup, "confirm": true})));
    assert_eq!(restored_tx_power(&rig), [-3]);
    assert_eq!(rig.zrhp.as_ref().unwrap().stored_tx_power(), Some(-3));
    assert_eq!(rig.zsim.as_ref().unwrap().state().tx_power, -3);
    rig.finish();
}

/// The result of a request: `Ok(result)` or `Err("code: message")`, as the
/// NCP updater's in-process session sees it.
fn outcome(r: &J) -> Result<J, String> {
    if r["success"] == true { Ok(r["result"].clone()) } else { Err(format!("{}: {}", r["error"]["code"], r["error"]["message"])) }
}

/// Before an NCP flash, a network the EmberZNet NCP stores but did not
/// bring up (`networkInit` failed otherwise than NOT_JOINED: formed, down)
/// counts as held: it is backed up first, and as that backup fails (the
/// export needs the network up) the flash is refused unless `force`;
/// never flashed over silently.
#[test]
fn a_formed_network_that_is_down_is_backed_up_or_the_flash_refused() {
    use crate::backend::select::{BeforeFlash, backed_up, before_flash};
    let mut c = ezsp_sim(true, 0xDD);
    c.network_init_fails = true;
    let rig = Rig::ezsp(c, Store::volatile());
    let (sid, rx) = session(&rig.app);
    wait("session", || info(&rig.app, sid, &rx)["session"] == "ready");
    let i = info(&rig.app, sid, &rx);
    assert_eq!(i["state"], "formed_down", "{i}");
    assert_eq!(before_flash(Some(&i), false, false), BeforeFlash::Backup { required: true });
    let r = outcome(&call(&rig.app, sid, &rx, 1, json!({"type": "network/backup", "keep": true})));
    let why = backed_up(true, r.clone()).unwrap_err();
    assert!(why.contains("nothing was changed") && why.contains("send force"), "{why}");
    assert_eq!(backed_up(false, r), Ok(None), "force: flashed without it");
    rig.finish();
}

/// The way back (ZRHP → SONOFF's EmberZNet): the network kept before the
/// NV clear goes onto the empty EmberZNet NCP (the chip's EUI-64 under
/// both firmwares), the zigbee-rs backend's records become its, and the
/// device works on through EZSP without pairing again.
#[test]
fn the_way_back_restores_the_zrhp_network_onto_emberznet() {
    use crate::backend::select::{AfterSwitch, after_switch};
    const CHIP_EUI64: [u8; 8] = [0x5A, 0x51, 0, 0, 0, 0, 1, 2];
    let flash = SharedFlash::new();
    let mut rig = Rig::zrhp_with(flash.store(), true);
    rig.devices(vec![plug()], Arc::new(Mutex::new(Vec::new())));
    let (sid, rx) = session(&rig.app);
    wait("owned", || info(&rig.app, sid, &rx)["ownership"] == "owned");
    rig.link.join(PLUG, PLUG_NWK);
    wait("the plug interviewed", || state(&rig.app, sid, &rx, SWITCH) == json!(false));
    ok(&call(&rig.app, sid, &rx, 1, json!({"type": "devices/update", "device": "zb:5a52530000000001", "name": "Kettle"})));
    let backup = ok(&call(&rig.app, sid, &rx, 2, json!({"type": "network/backup", "keep": true}))).clone();
    let epid = backup["extended_pan_id"].as_str().unwrap().to_string();
    rig.finish();

    // SONOFF's firmware after the NV clear: EmberZNet without a network.
    let mut c = ezsp_sim(false, 0x00);
    c.eui64 = Eui64(CHIP_EUI64);
    let mut rig = Rig::ezsp(c, flash.store());
    let answered = Arc::new(Mutex::new(Vec::new()));
    rig.devices(vec![plug()], answered.clone());
    let (sid, rx) = session(&rig.app);
    wait("the records foreign", || info(&rig.app, sid, &rx)["registry"]["state"] == "foreign");
    let i = info(&rig.app, sid, &rx);
    let kept = ok(&call(&rig.app, sid, &rx, 3, json!({"type": "network/backup/replaced", "ext_pan_id": epid})))["backup"].clone();
    assert_eq!(after_switch(&i, &kept), AfterSwitch::Restore, "{i}");
    let r = call(&rig.app, sid, &rx, 4, json!({"type": "network/restore", "backup": kept, "confirm": true}));
    ok(&r);
    let sim = rig.ezsp.clone().unwrap();
    assert!(sim.ncp(|n| n.config().custom_eui64.is_none()), "the same IEEE: no token written");
    wait("the restored network owned", || {
        let i = info(&rig.app, sid, &rx);
        i["ownership"] == "owned" && i["registry"]["state"] == "owned" && i["ext_pan_id"] == epid.as_str()
    });
    wait("the registry loaded", || devices_list(&rig.app, sid, &rx).len() == 1);
    let dev = ok(&call(&rig.app, sid, &rx, 5, json!({"type": "devices/get", "device": "zb:5a52530000000001"}))).clone();
    assert_eq!(dev["name"], "Kettle", "{dev}");
    ok(&call(&rig.app, sid, &rx, 6, json!({"type": "entities/command", "uid": SWITCH, "command": "turn_on"})));
    wait("On reached the plug through the EmberZNet NCP", || {
        answered.lock().unwrap().iter().any(|r| r.cluster == 0x0006 && r.payload.get(2) == Some(&0x01))
    });
    rig.finish();
}

/// A port that counts its use, for the probes' lifetime.
#[derive(Clone)]
struct Counted<P> {
    port: P,
    uses: Arc<std::sync::atomic::AtomicUsize>,
}

impl<P: hub_ncp::transport::PortRx> hub_ncp::transport::PortRx for Counted<P> {
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> std::io::Result<usize> {
        self.uses.fetch_add(1, Ordering::SeqCst);
        self.port.read(buf, timeout)
    }
}

impl<P: hub_ncp::transport::PortTx> hub_ncp::transport::PortTx for Counted<P> {
    fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.uses.fetch_add(1, Ordering::SeqCst);
        self.port.write_all(data)
    }
    fn set_line(&mut self, line: hub_ncp::transport::Line) -> std::io::Result<()> {
        self.port.set_line(line)
    }
}

/// The ZRHP probe's driver threads have ended when it returns, whether a
/// ZRHP NCP answered or not: nothing reads or writes the port afterwards
/// (it is the next probe's, or the backend's).
#[test]
fn the_zrhp_probe_leaves_no_thread_on_the_port() {
    use crate::backend::select::probe_zrhp;
    let lines = vec![hub_ncp::transport::Line::BR_FALLBACK];
    for ncp in [true, false] {
        let (host, ncp_end) = hub_ncp::transport::pipe::pair();
        let zsim = ncp.then(|| hub_ncp::sim::SimNcp::start(ncp_end));
        let (rx, tx) = host.split();
        let uses = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rx = Counted { port: rx, uses: uses.clone() };
        let tx = Counted { port: tx, uses: uses.clone() };
        let found = probe_zrhp(rx, tx, lines.clone(), 0x5EED_0010, quick());
        assert_eq!(found.is_ok(), ncp, "{found:?}");
        let after = uses.load(Ordering::SeqCst);
        assert!(after > 0);
        thread::sleep(Duration::from_millis(400));
        assert_eq!(uses.load(Ordering::SeqCst), after, "the port used after the probe returned (ncp {ncp})");
        if let Some(z) = zsim {
            z.stop();
        }
    }
}

/// The Dongle-M's boot (`ncp::detect_now`, then the backend): a zigbee-rs
/// NCP that runs and answers is not reset, so the records it kept since
/// its own reset (its first boot after a flash: the NV erased, the crystal
/// tuning) reach the hub's log once the backend's session grants LOG
/// credit (the probe grants none). Reset at the hub's start, as before and
/// still for an EmberZNet NCP seen last, the records are gone.
#[test]
fn a_running_zrhp_ncps_boot_log_survives_the_hubs_start() {
    use crate::backend::select::{Found, Probe, Prober, Setting, detect, probe_zrhp};
    use hub_ncp::transport::{Line, pipe};

    type Port = Line2<pipe::Rx, pipe::Tx>;
    struct SimProber {
        port: Port,
        sim: hub_ncp::sim::SimNcp,
        resets: usize,
    }
    impl Prober for SimProber {
        fn probe(&mut self, p: Probe) -> Result<Found, String> {
            match p {
                Probe::Zrhp => probe_zrhp(self.port.clone(), self.port.clone(), vec![Line::BR_DEFAULT], 0x5EED_0020, quick()),
                _ => Err("no answer".into()),
            }
        }
        fn reset_to_app(&mut self) {
            self.resets += 1;
            self.sim.reboot();
        }
    }

    for hint in [BackendKind::Zrhp, BackendKind::Ezsp] {
        let (host, ncp_end) = pipe::pair();
        let sim = hub_ncp::sim::SimNcp::start(ncp_end);
        for text in ["foreign NVM3 found: NV erased", "CTUNE 0x78 from MFG_CTUNE"] {
            sim.log_record(3, text);
        }
        // Queued on the NCP's LOG endpoint (its loop takes them in 2 ms).
        thread::sleep(Duration::from_millis(100));
        let (rx, tx) = host.split();
        let port = Line2(Arc::new(Mutex::new(rx)), Arc::new(Mutex::new(tx)));
        let mut prober = SimProber { port: port.clone(), sim: sim.clone(), resets: 0 };
        let d = detect(Setting::Auto, Some(hint), false, &mut prober);
        assert_eq!(d.backend, BackendKind::Zrhp, "{hint:?}");
        let mut cfg = hub_ncp::lifecycle::HubConfig::default();
        cfg.driver.lines = vec![Line::BR_DEFAULT];
        let hub = hub_ncp::Hub::start(cfg, port.clone(), port, None, Store::volatile(), crate::tests::rng());
        assert!(hub.wait_ready(T), "{hint:?}");
        if hint == BackendKind::Zrhp {
            assert_eq!(prober.resets, 0);
            wait("the NCP's boot records in the hub's log", || hub.ncp_log().forwarded == 2);
        } else {
            assert_eq!(prober.resets, 1);
            thread::sleep(Duration::from_millis(1500));
            assert_eq!(hub.ncp_log().forwarded, 0, "lost with the reset");
        }
        hub.shutdown();
        sim.stop();
    }
}
