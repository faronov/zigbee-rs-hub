//! `--sim-devices`: Zigbee devices behind the simulated NCP, answering the
//! hub's interview, configure, reads and commands with the engine's own
//! device simulator, so the hub (and its automations) can be tried end to
//! end without radios: two On/Off plugs and a temperature sensor that
//! reports a slowly changing value.

use std::thread;
use std::time::{Duration, Instant};

use hub_devices::testutil::sim::SimDevice;
use hub_devices::transport::{ApsDst, ApsIndication, ApsRequest, TxOptions};
use hub_devices::zcl::{TypedValue, ZclType, ZclValue};
use hub_devices::zdo::LogicalType;
use hub_ncp::lifecycle::{FormRequest, Hub};
use hub_ncp::sim::SimNcp;
use zigbee_zrhp::codec::{Bytes16, Reader, Wire};
use zigbee_zrhp::msg::data::{self as wd, ApsDataRequest, EvtApsDataIndication, indication_flags};
use zigbee_zrhp::msg::ids;
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
    let mut devs = devices();
    sim.with(|s| s.table_devices.extend(devs.iter().map(|d| (d.ieee.to_le_bytes(), d.nwk, None))));
    let sim = sim.clone();
    thread::Builder::new()
        .name("sim-devices".into())
        .spawn(move || {
            let mut next_report = Instant::now() + Duration::from_secs(15);
            let mut temp: i64 = 2150;
            let mut up = true;
            loop {
                for raw in sim.with(|s| std::mem::take(&mut s.aps)) {
                    let Ok(r) = ApsDataRequest::read(&mut Reader::new(&raw)) else { continue };
                    let wd::ApsDst::Nwk(nwk) = r.dst else { continue };
                    let Some(d) = devs.iter_mut().find(|d| d.nwk == nwk) else { continue };
                    let req = ApsRequest {
                        dst: ApsDst::Nwk(nwk),
                        dst_ep: r.dst_ep,
                        src_ep: r.src_ep,
                        profile: r.profile,
                        cluster: r.cluster,
                        payload: r.asdu.to_vec(),
                        options: TxOptions::default(),
                    };
                    let mut out = d.answer(&req);
                    // A plug reports its new state, as real ones do.
                    if r.profile == 0x0104 && on_off_command(r.cluster, r.asdu)
                        && let Some(v) = d.attrs.get(&(r.dst_ep, 0x0006, 0x0000, None)).cloned()
                    {
                        out.push(d.report(r.dst_ep, 0x0006, None, &[(0x0000, v)]));
                    }
                    for i in &out {
                        inject(&sim, i);
                    }
                }
                if Instant::now() >= next_report {
                    next_report += Duration::from_secs(15);
                    if !(1900..=2400).contains(&temp) {
                        up = temp < 1900;
                    }
                    temp += if up { 50 } else { -50 };
                    if let Some(d) = devs.iter_mut().find(|d| d.ieee == ieee(3)) {
                        let v = TypedValue::int(ZclType::I16, temp);
                        d.attrs.insert((1, 0x0402, 0x0000, None), v.clone());
                        inject(&sim, &d.report(1, 0x0402, None, &[(0x0000, v)]));
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
        })
        .expect("sim-devices thread");
    log::info!("sim devices: 2 plugs (zb:{:016x}, zb:{:016x}), a temperature sensor (zb:{:016x})", ieee(1), ieee(2), ieee(3));
    Ok(())
}
