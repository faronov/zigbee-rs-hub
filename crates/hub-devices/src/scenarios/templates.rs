//! Vendor template `tuya` (spec 17.4): the generated catalog definitions
//! that need it run, with frames as the devices send them.

use serde_json::json;

use super::*;
use crate::model::{Definition, LayerSource};
use crate::testutil::sim::{Sim, SimDevice};
use crate::transport::ApsIndication;
use crate::zcl::frame::{self as zf, global};
use crate::zcl::{FrameType, TypedValue, ZclType, ZclValue};
use crate::zdo::LogicalType;

/// A generated definition as the hub loads a fetched one (zdefc CBOR).
fn def(src: &str) -> Definition {
    let c = zdefc::compile_str(src, zdefc::Format::Yaml, &zdefc::Options::default()).unwrap();
    crate::zdef_adapter::load_cbor(&c.cbor, LayerSource::Catalog).unwrap()
}

fn hub_of(srcs: &[&str]) -> Hub<MockTransport> {
    let mut cat = MemoryCatalog::default();
    for s in srcs {
        let d = def(s);
        assert!(
            crate::resolve::missing_features(&d, crate::resolve::HUB_FEATURES).is_empty(),
            "{}: {:?}",
            d.id,
            d.requires
        );
        cat.insert(d);
    }
    Hub::new(
        MockTransport::default(),
        crate::hub::HubConfig::default(),
        Box::new(cat),
    )
}

/// A Report Attributes frame with its own TSN.
fn report(
    d: &SimDevice,
    ep: u8,
    cluster: u16,
    mfr: Option<u16>,
    tsn: u8,
    recs: &[(u16, TypedValue)],
) -> ApsIndication {
    let p = zf::report_attributes(recs).unwrap();
    d.zcl(
        ep,
        cluster,
        FrameType::Global,
        mfr,
        tsn,
        global::REPORT_ATTRIBUTES,
        p,
    )
}

fn cmd(d: &SimDevice, ep: u8, cluster: u16, tsn: u8, id: u8, p: &[u8]) -> ApsIndication {
    d.zcl(ep, cluster, FrameType::Cluster, None, tsn, id, p.to_vec())
}

fn events(hub: &mut Hub<MockTransport>, uid: &str) -> Vec<String> {
    fired(&hub.drain_events(), uid)
        .into_iter()
        .map(|e| e.0)
        .collect()
}

fn layer(hub: &Hub<MockTransport>, id: &str) -> String {
    let d = hub.device(id).unwrap();
    d.resolution.layers.last().unwrap().id.clone()
}

const PLUG: u64 = 0xa4c1_3800_0000_0101;
const PLUG_ID: &str = "zb:a4c1380000000101";

/// TS011F_plug_3 (`tuya_on_off` with child lock, indicator mode, power
/// outage memory, countdown and metering): Tuya's settings on On/Off as
/// zhc maps them, the countdown set with On With Timed Off, no generic
/// ZCL power-on behaviour.
#[test]
fn ts011f_plug_3_runs_its_tuya_on_off() {
    let mut hub = hub_of(&[include_str!("fixtures/ts011f_plug_3.yaml")]);
    let mut sim = Sim::default();
    sim.devices.push(
        SimDevice::new(PLUG, 0x4101, LogicalType::Router, true, 0x1002)
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
                TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(true))),
            )
            .attr(1, 0x0b04, 0x0505, TypedValue::uint(ZclType::U16, 231))
            .attr(1, 0x0b04, 0x0508, TypedValue::uint(ZclType::U16, 120))
            .attr(1, 0x0b04, 0x050b, TypedValue::int(ZclType::I16, 25))
            .attr(1, 0x0702, 0x0000, TypedValue::uint(ZclType::U48, 12))
            .attr(1, 0x0006, 0x4003, TypedValue::uint(ZclType::ENUM8, 0xFF))
            .attr(1, 0x0006, 0x8001, TypedValue::uint(ZclType::ENUM8, 1)),
    );
    sim.join(&mut hub, PLUG, T0);
    assert_eq!(layer(&hub, PLUG_ID), "tuya/ts011f_plug_3");
    let k = keys(&hub, PLUG_ID);
    for want in [
        "1:switch",
        "1:child_lock",
        "1:indicator_mode",
        "1:power_outage_memory",
        "1:countdown",
        "1:power",
        "1:energy",
    ] {
        assert!(k.iter().any(|x| x == want), "{want}: {k:?}");
    }
    assert!(
        !k.iter().any(|x| x.ends_with(":power_on_behavior")),
        "{k:?}"
    );
    let uid = |key: &str| format!("{PLUG_ID}:1:{key}");
    // Read at configure; the device's reports update them.
    assert_eq!(state(&hub, &uid("indicator_mode")), json!("off/on"));
    let d = sim.dev(PLUG).clone();
    let r = report(
        &d,
        1,
        0x0006,
        None,
        0x21,
        &[(0x8001, TypedValue::uint(ZclType::ENUM8, 2))],
    );
    hub.on_aps_indication(r, T0 + 1000);
    assert_eq!(state(&hub, &uid("indicator_mode")), json!("on/off"));
    let r = report(
        &d,
        1,
        0x0006,
        None,
        0x22,
        &[(0x4001, TypedValue::uint(ZclType::U16, 120))],
    );
    hub.on_aps_indication(r, T0 + 1100);
    assert_eq!(state(&hub, &uid("countdown")), json!(120));
    // Countdown: On With Timed Off, on time = off wait time (s).
    let t = T0 + 2000;
    hub.entity_command(&uid("countdown"), "set_value", &json!({"value": 300}), t)
        .unwrap();
    sim.run(&mut hub, t);
    let c = sim.dev(PLUG).commands.last().cloned().unwrap();
    assert_eq!(
        c,
        (1, 0x0006, 0x42, None, vec![0x00, 0x2C, 0x01, 0x2C, 0x01])
    );
    // The settings write their On/Off attributes.
    hub.entity_command(
        &uid("power_outage_memory"),
        "select_option",
        &json!({"option": "restore"}),
        t,
    )
    .unwrap();
    hub.entity_command(&uid("child_lock"), "turn_on", &json!({}), t)
        .unwrap();
    sim.run(&mut hub, t);
    let w: Vec<_> = sim
        .dev(PLUG)
        .writes
        .iter()
        .map(|(ep, c, a, m, v)| (*ep, *c, *a, *m, v.value.clone()))
        .collect();
    assert!(
        w.contains(&(1, 0x0006, 0x8002, None, ZclValue::Uint(2))),
        "{w:?}"
    );
    assert!(
        w.contains(&(1, 0x0006, 0x8000, None, ZclValue::Bool(Some(true)))),
        "{w:?}"
    );
}

const KNOB: u64 = 0xa4c1_3800_0000_0102;
const KNOB_ID: &str = "zb:a4c1380000000102";

/// A TS004F knob `_TZ3000_402vrq2i` (HOBEIAN ZG-101ZD, `multi_action` and
/// `operation_mode`): event mode writes On/Off 0x8004 = 1 at configure; its
/// scene commands 0xFD (single, double, hold) and 0xFC (rotation) are
/// actions; in command mode its Level Control steps are the generic
/// remote's brightness steps on the same `action` entity.
#[test]
fn ts004f_knob_actions_and_operation_mode() {
    let mut hub = hub_of(&[include_str!("fixtures/hobeian-zg-101zd.yaml")]);
    let mut sim = Sim::default();
    sim.devices.push(
        SimDevice::new(KNOB, 0x4102, LogicalType::EndDevice, true, 0x1002)
            .endpoint(
                1,
                0x0104,
                0x0104,
                &[0x0000, 0x0001, 0x0003, 0x0004, 0x0006, 0x1000],
                &[
                    0x0019, 0x000a, 0x0003, 0x0004, 0x0006, 0x0008, 0x0300, 0x1000,
                ],
            )
            .basic("_TZ3000_402vrq2i", "TS004F", 3)
            .attr(1, 0x0006, 0x8004, TypedValue::uint(ZclType::ENUM8, 0)),
    );
    sim.join(&mut hub, KNOB, T0);
    assert_eq!(layer(&hub, KNOB_ID), "hobeian/zg-101zd");
    let d = hub.device(KNOB_ID).unwrap();
    assert_eq!(d.record()["model"], "ZG-101Z_D_1");
    let s = sim.dev(KNOB).clone();
    assert!(
        s.writes
            .iter()
            .any(|w| (w.0, w.1, w.2, w.4.value.clone()) == (1, 0x0006, 0x8004, ZclValue::Uint(1))),
        "{:?}",
        s.writes
    );
    let mode = format!("{KNOB_ID}:1:operation_mode");
    assert!(hub.entity(&mode).is_some());
    let uid = format!("{KNOB_ID}:1:action");
    hub.drain_events();
    let mut t = T0 + 10_000;
    for (tsn, id, p) in [
        (1, 0xFD, &[0x00][..]),
        (2, 0xFD, &[0x01]),
        (3, 0xFD, &[0x02]),
        (4, 0xFC, &[0x00]),
        (5, 0xFC, &[0x01]),
    ] {
        hub.on_aps_indication(cmd(&s, 1, 0x0006, tsn, id, p), t);
        t += 500;
    }
    // Command mode: Step (up, 13 steps, 0.1 s).
    hub.on_aps_indication(cmd(&s, 1, 0x0008, 6, 0x02, &[0x00, 0x0D, 0x01, 0x00]), t);
    assert_eq!(
        events(&mut hub, &uid),
        [
            "single",
            "double",
            "hold",
            "rotate_right",
            "rotate_left",
            "brightness_step_up"
        ]
    );
    // A repeated frame (same TSN) is one press.
    hub.on_aps_indication(cmd(&s, 1, 0x0006, 7, 0xFD, &[0x00]), t + 100);
    hub.on_aps_indication(cmd(&s, 1, 0x0006, 7, 0xFD, &[0x00]), t + 200);
    assert_eq!(events(&mut hub, &uid), ["single"]);
    hub.entity_command(&mode, "select_option", &json!({"option": "command"}), t)
        .unwrap();
    sim.run(&mut hub, t);
    let w = sim.dev(KNOB).writes.last().cloned().unwrap();
    assert_eq!((w.1, w.2, w.4.value), (0x0006, 0x8004, ZclValue::Uint(0)));
}
