//! Tuya TS011F plug `_TZ3000_okaz9tjs` (field report on hub 0.1.4/0.1.5):
//! metering works, the switch must switch.

use serde_json::json;

use super::*;
use crate::model::LayerSource;
use crate::testutil::sim::{Sim, SimDevice};
use crate::testutil::yaml::load_definition;
use crate::zcl::{TypedValue, ZclType, ZclValue};
use crate::zdo::LogicalType;

const IEEE: u64 = 0x70b3_d52b_6002_3660;
const ID: &str = "zb:70b3d52b60023660";

fn plug() -> SimDevice {
    SimDevice::new(IEEE, 0x3361, LogicalType::Router, true, 0x1002)
        .endpoint(
            1,
            0x0104,
            0x0051,
            &[
                0x0000, 0x0003, 0x0004, 0x0005, 0x0006, 0x0702, 0x0b04, 0xe000, 0xe001,
            ],
            &[0x0019, 0x000a],
        )
        .basic("_TZ3000_okaz9tjs", "TS011F", 1)
        .attr(
            1,
            0x0006,
            0x0000,
            TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false))),
        )
        .attr(1, 0x0b04, 0x0505, TypedValue::uint(ZclType::U16, 223))
        .attr(1, 0x0b04, 0x050b, TypedValue::int(ZclType::I16, 0))
        .attr(1, 0x0702, 0x0000, TypedValue::uint(ZclType::U48, 12))
}

fn joined() -> (Hub<MockTransport>, Sim) {
    let mut hub = hub_with(&[]);
    let mut cat = MemoryCatalog::default();
    // Generated catalog entries for this plug; `ts011f_plug_3` needs the
    // `tuya` vendor template, which the hub skips (generic + plug_1 win).
    for src in [
        include_str!("fixtures/ts011f_plug_1.yaml"),
        include_str!("fixtures/ts011f_plug_3.yaml"),
    ] {
        if let Ok(d) = load_definition(src, LayerSource::Catalog) {
            cat.insert(d);
        }
    }
    hub.set_catalog(Box::new(cat), T0);
    let mut sim = Sim::default();
    sim.devices.push(plug());
    sim.join(&mut hub, IEEE, T0);
    (hub, sim)
}

#[test]
fn plug_switch_sends_on_off_and_reports_a_bool() {
    let (mut hub, mut sim) = joined();
    let d = hub.device(ID).unwrap();
    let layers: Vec<_> = d.resolution.layers.iter().map(|l| l.id.as_str()).collect();
    assert_eq!(layers, ["generic", "tuya/ts011f_plug_1"]);
    // White label by manufacturer name; the base model for its picture.
    let rec = d.record();
    assert_eq!(rec["manufacturer"], "Elivco");
    assert_eq!(rec["model"], "LSPA9");
    assert_eq!(rec["definition_model"], "TS011F_plug_1");
    let sw = format!("{ID}:1:switch");
    assert_eq!(
        hub.entity(&sw).unwrap().1.record(ID)["device_class"],
        "outlet"
    );
    // Configure: OnOff bound and reported, Tuya magic packet read.
    let s = sim.dev(IEEE);
    assert!(s.binds.contains(&(1, 0x0006)));
    assert!(
        s.reporting
            .iter()
            .any(|(ep, c, r)| (*ep, *c, r.attr) == (1, 0x0006, 0))
    );
    assert!(
        sim.aps()
            .iter()
            .any(|a| a.cluster == 0 && a.payload.ends_with(&[0x05, 0x00, 0x07, 0x00, 0xfe, 0xff]))
    );
    assert_eq!(hub.state_of(&sw).unwrap()["state"], json!(false));

    sim.log.clear();
    hub.drain_events();
    let ack = hub
        .entity_command(&sw, "turn_on", &json!({}), T0 + 1000)
        .unwrap();
    assert_eq!(ack.status, "pending");
    sim.run(&mut hub, T0 + 1000);
    let aps = sim.aps();
    assert_eq!(aps.len(), 1);
    let a = aps[0];
    assert_eq!((a.dst_ep, a.cluster, a.profile), (1, 0x0006, 0x0104));
    assert_eq!(
        a.payload[0], 0x01,
        "cluster-specific, client to server, no mfr code"
    );
    assert_eq!(a.payload[2], 0x01, "On");
    assert!(a.options.ack);
    assert_eq!(
        result_of(&hub.drain_events(), ack.ticket),
        Some(Ok("ok".into()))
    );
    // A switch state is a bool (4.3), not "on"/"off" as for lights: the
    // web UI compared it with "on" and always showed (and sent) "off→on".
    assert_eq!(hub.state_of(&sw).unwrap()["state"], json!(true));

    // The device's own report wins.
    let ind = sim.dev(IEEE).report(
        1,
        0x0006,
        None,
        &[(
            0,
            TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false))),
        )],
    );
    hub.on_aps_indication(ind, T0 + 2000);
    assert_eq!(hub.state_of(&sw).unwrap()["state"], json!(false));
}

#[test]
fn plug_command_without_answer_times_out_with_an_error() {
    let (mut hub, mut sim) = joined();
    let sw = format!("{ID}:1:switch");
    sim.dev(IEEE).responsive = false;
    hub.drain_events();
    let ack = hub
        .entity_command(&sw, "turn_off", &json!({}), T0 + 1000)
        .unwrap();
    let mut t = T0 + 1000;
    let mut res = None;
    while res.is_none() && t < T0 + 60_000 {
        t += 1000;
        hub.tick(t);
        res = result_of(&hub.drain_events(), ack.ticket);
    }
    let e = res.expect("a result").unwrap_err();
    assert_eq!(e.code, "timeout");
    // Two retries, then the error: about 3 × 8 s.
    assert!(t - T0 - 1000 <= 30_000, "{}", t - T0);
}
