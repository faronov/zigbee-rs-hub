//! `--sim-z2m-devices`: four simulated Zigbee devices behind the simulated
//! NCP (a temperature/humidity sensor, a colour-temperature light, a
//! plug, an IKEA STYRBAR remote), answering the hub's interview, reads,
//! writes and commands (`hub_devices::sim_device`) and reporting like real
//! devices: the sensor every 10 s, the light and the plug after every
//! command; the remote sends a button command every 10 s (its `action`
//! event entity). For end-to-end tests of the API clients (web UI, Home
//! Assistant, the MQTT bridge) without hardware.

use std::thread;
use std::time::{Duration, Instant};

use hub_devices::sim_device::SimDevice;
use hub_devices::transport::{ApsDst, ApsIndication, ApsRequest, TxOptions};
use hub_devices::zcl::{Direction, FrameType, TypedValue, ZclFrame, ZclHeader, ZclType, ZclValue};
use hub_devices::zdo::LogicalType;
use hub_ncp::lifecycle::{FormRequest, Hub};
use hub_ncp::sim::SimNcp;
use hub_ncp::zrhp::codec::{Bytes16, Reader, Wire};
use hub_ncp::zrhp::msg::data::{self as wd, ApsDataRequest, EvtApsDataIndication, indication_flags};
use hub_ncp::zrhp::msg::ids;
use hub_ncp::zrhp::msg::network::EvtDeviceJoined;
use hub_ncp::zrhp::types::Ieee;

/// IEEE addresses of the simulated devices.
pub const SENSOR: u64 = 0x00158d0001000001;
/// The light.
pub const LIGHT: u64 = 0x00158d0001000002;
/// The plug.
pub const PLUG: u64 = 0x00158d0001000003;
/// The remote (IKEA E2001/E2002, STYRBAR).
pub const REMOTE: u64 = 0x00158d0001000004;
/// The Tuya TS011F plug of `--sim-ts011f`.
pub const TS011F: u64 = 0xa4c1380000000001;

/// What the remote sends, in turn: On/Off toggle, on, Level step up
/// (with on/off), off.
const PRESSES: [(u16, u8, &[u8]); 4] = [(0x0006, 0x02, &[]), (0x0006, 0x01, &[]), (0x0008, 0x06, &[0, 43, 5, 0]), (0x0006, 0x00, &[])];

/// A Tuya TS011F smart plug with metering, `_TZ3000_gjnozsaz` (white label
/// NEO NAS-WR01B; zigbee-rs-hub 0.3.0 kept it generic): its definition
/// comes from the catalog's shards (`hubctl catalog-push`).
fn ts011f() -> SimDevice {
    SimDevice::new(TS011F, 0x1005, LogicalType::Router, true, 0x1002)
        .endpoint(1, 0x0104, 0x0051, &[0x0000, 0x0003, 0x0004, 0x0005, 0x0006, 0x0702, 0x0B04, 0xE000, 0xE001], &[0x000A, 0x0019])
        .basic("_TZ3000_gjnozsaz", "TS011F", 1)
        .attr(1, 0x0000, 0x0001, TypedValue::uint(ZclType::U8, 77))
        .attr(1, 0x0006, 0x0000, TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(true))))
        .attr(1, 0x0B04, 0x0505, TypedValue::uint(ZclType::U16, 231))
        .attr(1, 0x0B04, 0x0508, TypedValue::uint(ZclType::U16, 120))
        .attr(1, 0x0B04, 0x050B, TypedValue::int(ZclType::I16, 25))
        .attr(1, 0x0702, 0x0000, TypedValue::uint(ZclType::U48, 1234))
}

fn devices(with_ts011f: bool) -> Vec<SimDevice> {
    let u16v = |v: u64| TypedValue::uint(ZclType::U16, v);
    let sensor = SimDevice::new(SENSOR, 0x1001, LogicalType::EndDevice, true, 0x115F)
        .endpoint(1, 0x0104, 0x0302, &[0x0000, 0x0001, 0x0003, 0x0402, 0x0405], &[0x0003])
        .basic("LUMI", "lumi.weather", 3)
        .attr(1, 0x0001, 0x0020, TypedValue::uint(ZclType::U8, 30))
        .attr(1, 0x0001, 0x0021, TypedValue::uint(ZclType::U8, 200))
        .attr(1, 0x0402, 0x0000, TypedValue::int(ZclType::I16, 2137))
        .attr(1, 0x0405, 0x0000, u16v(5520));
    let light = SimDevice::new(LIGHT, 0x1002, LogicalType::Router, true, 0x117C)
        .endpoint(1, 0x0104, 0x010C, &[0x0000, 0x0003, 0x0004, 0x0005, 0x0006, 0x0008, 0x0300], &[0x0019])
        .basic("IKEA of Sweden", "TRADFRI bulb E27 WS opal 980lm", 1)
        .attr(1, 0x0006, 0x0000, TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false))))
        .attr(1, 0x0008, 0x0000, TypedValue::uint(ZclType::U8, 128))
        .attr(1, 0x0300, 0x0007, u16v(370))
        .attr(1, 0x0300, 0x0008, TypedValue::uint(ZclType::ENUM8, 2))
        .attr(1, 0x0300, 0x400A, TypedValue::uint(ZclType::MAP16, 0x0010))
        .attr(1, 0x0300, 0x400B, u16v(250))
        .attr(1, 0x0300, 0x400C, u16v(454));
    let plug = SimDevice::new(PLUG, 0x1003, LogicalType::Router, true, 0x1166)
        .endpoint(1, 0x0104, 0x010A, &[0x0000, 0x0003, 0x0004, 0x0005, 0x0006], &[])
        .basic("innr", "SP 120", 1)
        .attr(1, 0x0006, 0x0000, TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false))));
    let remote = SimDevice::new(REMOTE, 0x1004, LogicalType::EndDevice, false, 0x117C)
        .endpoint(1, 0x0104, 0x0820, &[0x0000, 0x0001, 0x0003, 0x1000], &[0x0003, 0x0004, 0x0006, 0x0008, 0x1000])
        .basic("IKEA of Sweden", "Remote Control N2", 3)
        .attr(1, 0x0001, 0x0020, TypedValue::uint(ZclType::U8, 30))
        .attr(1, 0x0001, 0x0021, TypedValue::uint(ZclType::U8, 174));
    let mut v = vec![sensor, light, plug, remote];
    if with_ts011f {
        v.push(ts011f());
    }
    v
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
        flags: indication_flags::NWK_SECURED | if i.src_ieee.is_some() { indication_flags::SRC_IEEE_VALID } else { 0 },
        lqi: i.lqi,
        rssi: i.rssi,
        last_hop_nwk: i.src_nwk,
        aps_counter: 0,
        key_source: 0,
        rx_time_ms: 0,
        asdu: Bytes16(&i.payload),
    };
    if let Ok(p) = hub_ncp::driver::encode_body(&ev) {
        sim.inject_event(ids::EVT_APS_DATA_INDICATION, p);
    }
}

fn request(p: &[u8]) -> Option<ApsRequest> {
    let r: ApsDataRequest = Wire::read(&mut Reader::new(p)).ok()?;
    Some(ApsRequest {
        dst: match r.dst {
            wd::ApsDst::Nwk(n) => ApsDst::Nwk(n),
            wd::ApsDst::Ieee(i) => ApsDst::Ieee(i.to_u64()),
            wd::ApsDst::Group(g) => ApsDst::Group(g),
            wd::ApsDst::Broadcast(_) => return None,
        },
        dst_ep: r.dst_ep,
        src_ep: r.src_ep,
        profile: r.profile,
        cluster: r.cluster,
        payload: r.asdu.to_vec(),
        options: TxOptions::default(),
    })
}

/// What a real device reports after a command it carried out.
fn after_command(d: &mut SimDevice, req: &ApsRequest) -> Option<ApsIndication> {
    let (cluster, cmd, payload) = d.commands.last().map(|c| (c.1, c.2, c.4.clone()))?;
    if req.cluster != cluster || req.profile == 0 {
        return None;
    }
    let attr = |d: &SimDevice, c: u16, a: u16| d.attrs.get(&(1, c, a, None)).cloned();
    match (cluster, cmd) {
        (0x0006, 0x00..=0x02) => Some(d.report(1, 0x0006, None, &[(0x0000, attr(d, 0x0006, 0)?)])),
        // Move to Level (with On/Off): the level, and on.
        (0x0008, 0x00 | 0x04) => {
            let level = *payload.first()?;
            d.attrs.insert((1, 0x0008, 0, None), TypedValue::uint(ZclType::U8, u64::from(level)));
            if cmd == 0x04 {
                d.attrs.insert((1, 0x0006, 0, None), TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(level > 0))));
            }
            Some(d.report(1, 0x0008, None, &[(0x0000, attr(d, 0x0008, 0)?)]))
        }
        // Move to Color Temperature.
        (0x0300, 0x0A) => {
            let m = u16::from_le_bytes([*payload.first()?, *payload.get(1)?]);
            d.attrs.insert((1, 0x0300, 0x0007, None), TypedValue::uint(ZclType::U16, u64::from(m)));
            Some(d.report(1, 0x0300, None, &[(0x0007, attr(d, 0x0300, 0x0007)?)]))
        }
        _ => None,
    }
}

/// Forms the network if needed and runs the devices: they join once the
/// hub application registered its endpoint.
pub fn start(hub: &Hub, sim: &SimNcp, with_ts011f: bool) -> Result<(), String> {
    if !hub.wait_ready(Duration::from_secs(10)) {
        return Err("simulated NCP not ready".into());
    }
    if hub.status().network_state == 0 {
        hub.form(FormRequest { channel_mask: 1 << 15, ..FormRequest::default() }).map_err(|e| e.to_string())?;
    }
    let sim = sim.clone();
    thread::Builder::new()
        .name("sim-devices".into())
        .spawn(move || run(sim, with_ts011f))
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn run(sim: SimNcp, with_ts011f: bool) {
    let mut devs = devices(with_ts011f);
    // The app registers its endpoint when it starts.
    let t = Instant::now();
    while sim.with(|s| s.endpoints.is_empty()) && t.elapsed() < Duration::from_secs(30) {
        thread::sleep(Duration::from_millis(50));
    }
    let mut seen = sim.with(|s| s.aps.len());
    for d in &devs {
        let ev = EvtDeviceJoined { nwk: d.nwk, ieee: Ieee::from_u64(d.ieee), parent_nwk: 0, join_type: 0, capability: 0x8E, decision: 0 };
        if let Ok(p) = hub_ncp::driver::encode_body(&ev) {
            sim.inject_event(ids::EVT_DEVICE_JOINED, p);
        }
    }
    log::info!("sim-devices: {} devices joined", devs.len());
    let mut next_report = Instant::now() + Duration::from_secs(10);
    let mut temp = 2137i64;
    let mut press = 0usize;
    loop {
        thread::sleep(Duration::from_millis(20));
        let new: Vec<Vec<u8>> = sim.with(|s| {
            let v = s.aps.get(seen..).map(<[Vec<u8>]>::to_vec).unwrap_or_default();
            seen = s.aps.len();
            v
        });
        for p in new {
            let Some(req) = request(&p) else { continue };
            let ApsDst::Nwk(n) = req.dst else { continue };
            let Some(d) = devs.iter_mut().find(|d| d.nwk == n) else { continue };
            let before = d.commands.len();
            for i in d.answer(&req) {
                inject(&sim, &i);
            }
            if d.commands.len() > before
                && let Some(i) = after_command(d, &req)
            {
                inject(&sim, &i);
            }
        }
        if Instant::now() >= next_report {
            next_report += Duration::from_secs(10);
            temp = if temp >= 2400 { 2137 } else { temp + 7 };
            let s = &mut devs[0];
            s.attrs.insert((1, 0x0402, 0, None), TypedValue::int(ZclType::I16, temp));
            let i = s.report(1, 0x0402, None, &[(0x0000, TypedValue::int(ZclType::I16, temp))]);
            inject(&sim, &i);
            let (cluster, cmd, payload) = PRESSES[press % PRESSES.len()];
            let header = ZclHeader {
                frame_type: FrameType::Cluster,
                mfr: None,
                direction: Direction::ClientToServer,
                disable_default_response: true,
                tsn: press as u8,
                cmd,
            };
            inject(&sim, &devs[3].ind(1, 0x0104, cluster, ZclFrame { header, payload: payload.to_vec() }.encode()));
            press += 1;
        }
    }
}
