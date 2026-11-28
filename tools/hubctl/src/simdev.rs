//! `--sim-devices`: Zigbee devices behind the simulated NCP (ZRHP, or EZSP
//! with `--backend ezsp`), answering the hub's interview, configure, reads
//! and commands with the engine's own device simulator, so the hub (and its
//! automations) can be tried end to end without radios: two On/Off plugs
//! and a temperature sensor that reports a slowly changing value.
//!
//! `sim/*` requests ([`SimHook`], `hubctl serve` with `--sim-devices`) make
//! the devices' network events happen, for clients' end-to-end tests: a
//! join (refused, a rejoin, a new device whose interview stays partial), an
//! announcement, a leave.

use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use hub_devices::testutil::sim::SimDevice;
use hub_devices::transport::{ApsDst, ApsIndication, ApsRequest, TxOptions};
use hub_devices::zcl::{TypedValue, ZclType, ZclValue};
use hub_devices::zdo::LogicalType;
use hub_devices::hub::ApiError;
use hub_ezsp::ezsp::command::{Callback, Command};
use hub_ezsp::ezsp::{
    ApsFrame, ApsOptions, DeviceUpdate, Eui64, IncomingMessageType, JoinDecision, NeighborTableEntry, OutgoingMessageType,
};
use serde_json::{Value as J, json};
use hub_ncp::lifecycle::{FormRequest, Hub};
use hub_ncp::sim::SimNcp;
use zigbee_zrhp::codec::{Bytes16, Reader, Wire};
use zigbee_zrhp::msg::data::{self as wd, ApsDataRequest, EvtApsDataIndication, indication_flags};
use zigbee_zrhp::msg::ids;
use zigbee_zrhp::msg::network::{EvtDeviceJoined, EvtDeviceLeft};
use zigbee_zrhp::types::Ieee;

/// IEEE of simulated device `n` (1 = plug 1, 2 = plug 2, 3 = sensor).
pub fn ieee(n: u8) -> u64 {
    0x5A52_5300_0000_0000 | u64::from(n)
}

fn devices() -> Vec<SimDevice> {
    let off = || TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false)));
    let plug = |n: u8| {
        SimDevice::new(ieee(n), 0x5A00 | u16::from(n), LogicalType::Router, true, 0x1234)
            .endpoint(1, 0x0104, 0x0051, &[0x0000, 0x0003, 0x0006], &[])
            .basic("zigbee-rs", "SIM-PLUG", 1)
            .attr(1, 0x0006, 0x0000, off())
    };
    let sensor = SimDevice::new(ieee(3), 0x5A03, LogicalType::Router, true, 0x1234)
        .endpoint(1, 0x0104, 0x0302, &[0x0000, 0x0003, 0x0402], &[])
        .basic("zigbee-rs", "SIM-TEMP", 1)
        .attr(1, 0x0402, 0x0000, TypedValue::int(ZclType::I16, 2150));
    vec![plug(1), plug(2), sensor]
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
        flags: if i.src_ieee.is_some() { indication_flags::SRC_IEEE_VALID } else { 0 },
        lqi: i.lqi,
        rssi: i.rssi,
        last_hop_nwk: i.src_nwk,
        aps_counter: 0,
        key_source: 0,
        rx_time_ms: 0,
        asdu: Bytes16(&i.payload),
    };
    if let Ok(b) = hub_ncp::driver::encode_body(&ev) {
        sim.inject_event(ids::EVT_APS_DATA_INDICATION, b);
    }
}

/// An On/Off cluster command (off, on, toggle) in a ZCL payload.
fn on_off_command(cluster: u16, p: &[u8]) -> bool {
    let cmd = if p.first().is_some_and(|fc| fc & 0x04 != 0) { p.get(4) } else { p.get(2) };
    cluster == 0x0006 && p.first().is_some_and(|fc| fc & 0x03 == 0x01) && cmd.is_some_and(|c| *c <= 2)
}

/// The simulated NCP the devices are behind.
trait Link: Send + 'static {
    /// Unicasts to a short address the NCP sent since the last call.
    fn requests(&mut self) -> Vec<ApsRequest>;
    /// A device's frame reaches the NCP.
    fn indicate(&self, i: &ApsIndication);
}

/// The ZRHP simulated NCP.
struct Zrhp(SimNcp);

impl Link for Zrhp {
    fn requests(&mut self) -> Vec<ApsRequest> {
        let raw = self.0.with(|s| std::mem::take(&mut s.aps));
        raw.iter()
            .filter_map(|raw| {
                let r = ApsDataRequest::read(&mut Reader::new(raw)).ok()?;
                let wd::ApsDst::Nwk(nwk) = r.dst else { return None };
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
        inject(&self.0, i);
    }
}

/// The EZSP simulated NCP.
struct Ezsp {
    sim: hub_ezsp::sim::Sim,
    /// Messages of the NCP's send log already answered.
    seen: usize,
}

impl Link for Ezsp {
    fn requests(&mut self) -> Vec<ApsRequest> {
        let seen = &mut self.seen;
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
}

/// Forms a network if the NCP has none, puts the devices in the NCP's
/// device table (the hub imports and interviews them) and answers their
/// traffic until the process ends.
pub fn start(hub: &Hub, sim: &SimNcp) -> Result<(), String> {
    if !hub.wait_ready(Duration::from_secs(10)) {
        return Err("simulated NCP not ready".into());
    }
    if hub.status().network_state == 0 {
        hub.form(FormRequest { channel_mask: 1 << 15, ..FormRequest::default() }).map_err(|e| e.to_string())?;
    }
    let devs = devices();
    sim.with(|s| s.table_devices.extend(devs.iter().map(|d| (d.ieee.to_le_bytes(), d.nwk, None))));
    let devs = Arc::new(Mutex::new(devs));
    let _ = CONTROL.set(Control { ncp: Ncp::Zrhp(sim.clone()), devs: devs.clone() });
    answer(Zrhp(sim.clone()), devs);
    Ok(())
}

/// [`start`] behind the EZSP simulated NCP, which runs a network already:
/// the devices are in its neighbor table (the hub imports and interviews
/// them).
pub fn start_ezsp(sim: &hub_ezsp::sim::Sim) {
    let devs = devices();
    sim.ncp(|n| {
        n.config_mut().neighbors.extend(devs.iter().map(|d| NeighborTableEntry {
            short_id: d.nwk,
            average_lqi: 255,
            in_cost: 1,
            out_cost: 1,
            age: 3,
            long_id: Eui64::from_u64(d.ieee),
        }))
    });
    let devs = Arc::new(Mutex::new(devs));
    let _ = CONTROL.set(Control { ncp: Ncp::Ezsp(sim.clone()), devs: devs.clone() });
    answer(Ezsp { sim: sim.clone(), seen: 0 }, devs);
}

/// Answers the devices' traffic on `link` until the process ends.
fn answer(mut link: impl Link, devs: Arc<Mutex<Vec<SimDevice>>>) {
    thread::Builder::new()
        .name("sim-devices".into())
        .spawn(move || {
            let mut next_report = Instant::now() + Duration::from_secs(15);
            let mut temp: i64 = 2150;
            let mut up = true;
            loop {
                for req in link.requests() {
                    let ApsDst::Nwk(nwk) = req.dst else { continue };
                    let out = {
                        let mut devs = devs.lock().unwrap();
                        let Some(d) = devs.iter_mut().find(|d| d.nwk == nwk) else { continue };
                        let mut out = d.answer(&req);
                        // A plug reports its new state, as real ones do.
                        if req.profile == 0x0104 && on_off_command(req.cluster, &req.payload)
                            && let Some(v) = d.attrs.get(&(req.dst_ep, 0x0006, 0x0000, None)).cloned()
                        {
                            out.push(d.report(req.dst_ep, 0x0006, None, &[(0x0000, v)]));
                        }
                        out
                    };
                    for i in &out {
                        link.indicate(i);
                    }
                }
                if Instant::now() >= next_report {
                    next_report += Duration::from_secs(15);
                    if !(1900..=2400).contains(&temp) {
                        up = temp < 1900;
                    }
                    temp += if up { 50 } else { -50 };
                    let report = devs.lock().unwrap().iter_mut().find(|d| d.ieee == ieee(3)).map(|d| {
                        let v = TypedValue::int(ZclType::I16, temp);
                        d.attrs.insert((1, 0x0402, 0x0000, None), v.clone());
                        d.report(1, 0x0402, None, &[(0x0000, v)])
                    });
                    if let Some(r) = report {
                        link.indicate(&r);
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
        })
        .expect("sim-devices thread");
    log::info!("sim devices: 2 plugs (zb:{:016x}, zb:{:016x}), a temperature sensor (zb:{:016x})", ieee(1), ieee(2), ieee(3));
}

/// The simulated NCP's events a [`SimHook`] injects.
enum Ncp {
    Zrhp(SimNcp),
    Ezsp(hub_ezsp::sim::Sim),
}

/// What [`SimHook`] drives: the NCP and the devices that answer.
struct Control {
    ncp: Ncp,
    devs: Arc<Mutex<Vec<SimDevice>>>,
}

static CONTROL: OnceLock<Control> = OnceLock::new();

/// ZRHP `join_type` (0 association, 1 secured rejoin, 2 TC rejoin).
fn join_type(how: &str) -> Result<u8, ApiError> {
    match how {
        "association" => Ok(0),
        "secured" => Ok(1),
        "tc" => Ok(2),
        _ => Err(ApiError::new("invalid_value", "how: association, secured or tc")),
    }
}

impl Control {
    fn join(&self, ieee: u64, nwk: u16, how: u8, denied: bool) {
        match &self.ncp {
            Ncp::Zrhp(sim) => {
                let ev = EvtDeviceJoined {
                    nwk,
                    ieee: Ieee::from_u64(ieee),
                    parent_nwk: 0,
                    join_type: how,
                    capability: 0x8E,
                    decision: denied.into(),
                };
                if let Ok(b) = hub_ncp::driver::encode_body(&ev) {
                    sim.inject_event(ids::EVT_DEVICE_JOINED, b);
                }
            }
            Ncp::Ezsp(sim) => sim.ncp(|n| {
                n.callback(Callback::TrustCenterJoin {
                    new_node_id: nwk,
                    new_node_eui64: Eui64::from_u64(ieee),
                    status: match how {
                        1 => DeviceUpdate::STANDARD_SECURITY_SECURED_REJOIN,
                        2 => DeviceUpdate::STANDARD_SECURITY_UNSECURED_REJOIN,
                        _ => DeviceUpdate::STANDARD_SECURITY_UNSECURED_JOIN,
                    },
                    policy_decision: if denied { JoinDecision::DENY_JOIN } else { JoinDecision::USE_PRECONFIGURED_KEY },
                    parent_of_new_node_id: 0,
                })
            }),
        }
    }

    /// A leave; `aged_out`: the NCP dropped the child after a timeout
    /// (ZRHP reason 4; EZSP has none).
    fn leave(&self, ieee: u64, nwk: u16, rejoin: bool, aged_out: bool) -> Result<(), ApiError> {
        match &self.ncp {
            Ncp::Zrhp(sim) => {
                let reason = if aged_out { 4 } else { 1 };
                let ev = EvtDeviceLeft { nwk, ieee: Ieee::from_u64(ieee), flags: rejoin.into(), reason };
                if let Ok(b) = hub_ncp::driver::encode_body(&ev) {
                    sim.inject_event(ids::EVT_DEVICE_LEFT, b);
                }
            }
            Ncp::Ezsp(_) if aged_out => return Err(ApiError::new("unsupported", "aged_out: ZRHP only")),
            // The trust center does not say whether it rejoins.
            Ncp::Ezsp(sim) => sim.ncp(|n| {
                n.callback(Callback::TrustCenterJoin {
                    new_node_id: nwk,
                    new_node_eui64: Eui64::from_u64(ieee),
                    status: DeviceUpdate::DEVICE_LEFT,
                    policy_decision: JoinDecision::NO_ACTION,
                    parent_of_new_node_id: 0,
                })
            }),
        }
        Ok(())
    }

    fn indicate(&self, i: &ApsIndication) {
        match &self.ncp {
            Ncp::Zrhp(sim) => inject(sim, i),
            Ncp::Ezsp(sim) => Ezsp { sim: sim.clone(), seen: 0 }.indicate(i),
        }
    }

    /// The device `device` (`zb:<ieee>` or the IEEE in hex) names: its
    /// IEEE and, when it is a simulated device, its short address.
    fn lookup(&self, device: &str) -> Result<(u64, Option<u16>), ApiError> {
        let ieee = u64::from_str_radix(device.trim_start_matches("zb:").trim_start_matches("0x"), 16)
            .map_err(|_| ApiError::new("invalid_value", "device: zb:<16 hex> or the IEEE in hex"))?;
        Ok((ieee, self.devs.lock().unwrap().iter().find(|d| d.ieee == ieee).map(|d| d.nwk)))
    }

    /// A simulated device.
    fn find(&self, device: &str) -> Result<(u64, u16), ApiError> {
        match self.lookup(device)? {
            (ieee, Some(nwk)) => Ok((ieee, nwk)),
            _ => Err(ApiError::new("not_found", format!("no simulated device {device}"))),
        }
    }
}

/// `sim/*` requests (see the module docs).
pub struct SimHook;

impl SimHook {
    /// The hook, when simulated devices run.
    pub fn new() -> Option<Self> {
        CONTROL.get().map(|_| SimHook)
    }
}

impl hub_app::RequestHook for SimHook {
    fn prefix(&self) -> &'static str {
        "sim/"
    }
    fn feature(&self) -> Option<&'static str> {
        Some("sim")
    }
    fn request(&mut self, ty: &str, req: &J) -> Result<J, ApiError> {
        let c = CONTROL.get().ok_or_else(|| ApiError::new("unsupported", "no simulated devices"))?;
        let device = req["device"].as_str().unwrap_or_default();
        let nwk_of = |old: u16| req["nwk"].as_u64().map_or(old, |n| n as u16);
        match ty {
            // A device that joins, rejoins (`how`, a new `nwk`) or is refused
            // (`denied`); `partial`: a new plug whose Simple descriptor read
            // fails (its interview ends partial).
            "sim/join" => {
                let how = join_type(req["how"].as_str().unwrap_or("association"))?;
                let denied = req["denied"].as_bool().unwrap_or(false);
                let (ieee, nwk) = if req["partial"].as_bool().unwrap_or(false) {
                    let mut devs = c.devs.lock().unwrap();
                    let n = devs.len() as u8 + 1;
                    let mut d = SimDevice::new(ieee(n), 0x5A00 | u16::from(n), LogicalType::Router, true, 0x1234)
                        .endpoint(1, 0x0104, 0x0051, &[0x0000, 0x0003, 0x0006], &[])
                        .basic("zigbee-rs", "SIM-PARTIAL", 1);
                    d.zdo_failing.insert(hub_devices::zdo::cluster::SIMPLE_DESC_REQ);
                    let at = (d.ieee, d.nwk);
                    devs.push(d);
                    at
                } else {
                    // Any IEEE may be refused; only simulated devices join.
                    let (ieee, old) = match c.lookup(device)? {
                        (ieee, Some(nwk)) => (ieee, nwk),
                        (ieee, None) if denied => (ieee, 0x5AFF),
                        _ => return Err(ApiError::new("not_found", format!("no simulated device {device}"))),
                    };
                    let nwk = nwk_of(old);
                    if !denied && let Some(d) = c.devs.lock().unwrap().iter_mut().find(|d| d.ieee == ieee) {
                        d.nwk = nwk;
                    }
                    (ieee, nwk)
                };
                c.join(ieee, nwk, how, denied);
                Ok(json!({"device": hub_devices::hub::zigbee_id(ieee), "nwk": nwk}))
            }
            "sim/announce" => {
                let (ieee, old) = c.find(device)?;
                let nwk = nwk_of(old);
                let annce = {
                    let mut devs = c.devs.lock().unwrap();
                    let d = devs.iter_mut().find(|d| d.ieee == ieee).expect("found");
                    d.nwk = nwk;
                    d.ind(0, 0, 0x0013, hub_devices::zdo::device_annce(1, nwk, ieee, 0x8E))
                };
                c.indicate(&annce);
                Ok(json!({"device": hub_devices::hub::zigbee_id(ieee), "nwk": nwk}))
            }
            "sim/leave" => {
                let (ieee, nwk) = c.find(device)?;
                c.leave(ieee, nwk, req["rejoin"].as_bool().unwrap_or(false), req["aged_out"].as_bool().unwrap_or(false))?;
                Ok(json!({"device": hub_devices::hub::zigbee_id(ieee)}))
            }
            other => Err(ApiError::new("invalid_format", format!("unknown request {other}"))),
        }
    }
}
