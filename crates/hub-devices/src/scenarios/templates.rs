//! Vendor templates `tuya` and `lumi` (spec 17.4): the generated catalog
//! definitions that need them run, with frames as the devices send them.

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
/// outage memory, countdown and electrical measurements;
/// `electricity_measurement_poll`): Tuya's settings on On/Off as zhc maps
/// them, the countdown set with On With Timed Off, no generic ZCL power-on
/// behaviour, the electrical measurements polled every 60 s (not the
/// energy: zhc polls it for some app versions only).
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
    // `electricity_measurement_poll: {}` (zhc's `metering` predicate left
    // out by the generator): voltage, current and power read every 60 s.
    let reads = |sim: &Sim, c: u16| {
        sim.aps()
            .iter()
            .filter(|a| a.cluster == c && a.payload[2] == global::READ_ATTRIBUTES)
            .count()
    };
    hub.tick(t + 1000);
    sim.run(&mut hub, t + 1000);
    sim.log.clear();
    hub.tick(t + 61_000);
    sim.run(&mut hub, t + 61_000);
    assert_eq!((reads(&sim, 0x0b04), reads(&sim, 0x0702)), (1, 0));
}

const KNOB: u64 = 0xa4c1_3800_0000_0102;
const KNOB_ID: &str = "zb:a4c1380000000102";

/// HOBEIAN ZG-101ZD `_TZ3000_402vrq2i` in command mode.
fn knob() -> SimDevice {
    knob_rx(true)
}

/// [`knob`], sleepy (`rx_on` false) or not.
fn knob_rx(rx_on: bool) -> SimDevice {
    SimDevice::new(KNOB, 0x4102, LogicalType::EndDevice, rx_on, 0x1002)
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
        .attr(1, 0x0006, 0x8004, TypedValue::uint(ZclType::ENUM8, 0))
}

/// A TS004F knob `_TZ3000_402vrq2i` (HOBEIAN ZG-101ZD, `multi_action` and
/// `operation_mode`): event mode writes On/Off 0x8004 = 1 at configure; its
/// scene commands 0xFD (single, double, hold) and 0xFC (rotation) are
/// actions; in command mode its Level Control steps are the generic
/// remote's brightness steps on the same `action` entity.
#[test]
fn ts004f_knob_actions_and_operation_mode() {
    let mut hub = hub_of(&[include_str!("fixtures/hobeian-zg-101zd.yaml")]);
    let mut sim = Sim::default();
    sim.devices.push(knob());
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

/// A TS004F paired before, generic (its definition needed `template.tuya`,
/// which that firmware lacked), keeps its operation mode once the hub runs
/// the definition (restored by a firmware that has the template, the
/// definition fetched, the device resolved again): the configure write of
/// On/Off 0x8004 is not sent (zhc writes it at pairing only); the mode
/// select is the user's, and in command mode the generic remote events
/// stay. A fresh pairing writes it (`ts004f_knob_actions_and_operation_mode`).
#[test]
fn ts004f_paired_before_keeps_its_mode() {
    let src = include_str!("fixtures/hobeian-zg-101zd.yaml");
    let mut old = hub_of(&[src]);
    old.features.retain(|f| *f != "template.tuya");
    let mut sim = Sim::default();
    sim.devices.push(knob());
    sim.join(&mut old, KNOB, T0);
    assert_eq!(layer(&old, KNOB_ID), "generic");
    assert_eq!(mode_writes(&mut sim), 0);
    let mut snap = old.snapshot_device(KNOB_ID).unwrap();
    // Saved by 0.4.2: no `paired`.
    snap.paired = 0;
    let mut hub = hub_of(&[src]);
    hub.features.retain(|f| *f != "template.tuya");
    hub.restore_device(snap, None, T0 + 1000).unwrap();
    hub.features.push("template.tuya");
    let t = T0 + 2000;
    hub.reresolve(None, t);
    assert_eq!(layer(&hub, KNOB_ID), "hobeian/zg-101zd");
    let s = sim.dev(KNOB).clone();
    hub.on_aps_indication(cmd(&s, 1, 0x0008, 9, 0x02, &[0x00, 0x0D, 0x01, 0x00]), t);
    sim.run(&mut hub, t);
    assert_eq!(mode_writes(&mut sim), 0);
    let uid = format!("{KNOB_ID}:1:action");
    let ev = fired(&hub.drain_events(), &uid);
    assert!(ev.iter().any(|e| e.0 == "brightness_step_up"), "{ev:?}");
    // The user switches it.
    let mode = format!("{KNOB_ID}:1:operation_mode");
    hub.entity_command(&mode, "select_option", &json!({"option": "event"}), t)
        .unwrap();
    sim.run(&mut hub, t);
    assert_eq!(mode_writes(&mut sim), 1);
}

fn mode_writes(sim: &mut Sim) -> usize {
    sim.dev(KNOB)
        .writes
        .iter()
        .filter(|w| (w.1, w.2) == (0x0006, 0x8004))
        .count()
}

/// A TS004F paired by this build that runs generic until its definition
/// is fetched after the interview (template definitions come in shards,
/// fetched for interviewed devices), a restart in between: that definition
/// writes the mode, once.
#[test]
fn ts004f_definition_after_the_interview_writes_its_mode() {
    let mut first = hub_of(&[]);
    let mut sim = Sim::default();
    sim.devices.push(knob());
    sim.join(&mut first, KNOB, T0);
    assert_eq!(layer(&first, KNOB_ID), "generic");
    assert_eq!(mode_writes(&mut sim), 0);
    let mut hub = hub_of(&[]);
    let snap = first.snapshot_device(KNOB_ID).unwrap();
    hub.restore_device(snap, None, T0 + 1000).unwrap();
    let mut cat = MemoryCatalog::default();
    cat.insert(def(include_str!("fixtures/hobeian-zg-101zd.yaml")));
    let t = T0 + 5000;
    hub.set_catalog(Box::new(cat), t);
    assert_eq!(layer(&hub, KNOB_ID), "hobeian/zg-101zd");
    let s = sim.dev(KNOB).clone();
    hub.on_aps_indication(cmd(&s, 1, 0x0006, 1, 0xFD, &[0x00]), t);
    sim.run(&mut hub, t);
    assert_eq!(mode_writes(&mut sim), 1);
    // Configured under it now: another resolution does not write again.
    hub.reresolve(None, t + 1000);
    hub.on_aps_indication(cmd(&s, 1, 0x0006, 2, 0xFD, &[0x00]), t + 1000);
    sim.run(&mut hub, t + 1000);
    assert_eq!(mode_writes(&mut sim), 1);
}

/// The knob paired generic, its definition fetched while it sleeps: the
/// mode write waits for it, and the hub restarts (its queue is not stored;
/// at boot the definition is installed before the registry loads).
fn knob_restarted_before_its_mode_write(sim: &mut Sim) -> (Hub<MockTransport>, u64) {
    let src = include_str!("fixtures/hobeian-zg-101zd.yaml");
    let mut first = hub_of(&[]);
    sim.devices.push(knob_rx(false));
    sim.join(&mut first, KNOB, T0);
    let t = T0 + 600_000;
    let mut cat = MemoryCatalog::default();
    cat.insert(def(src));
    first.set_catalog(Box::new(cat), t);
    assert_eq!(layer(&first, KNOB_ID), "hobeian/zg-101zd");
    sim.run(&mut first, t);
    assert_eq!(mode_writes(sim), 0, "it waits for the knob");
    let snap = first.snapshot_device(KNOB_ID).unwrap();
    let mut hub = hub_of(&[src]);
    hub.restore_device(snap, None, t + 1000).unwrap();
    assert_eq!(layer(&hub, KNOB_ID), "hobeian/zg-101zd");
    (hub, t + 1000)
}

/// After that restart its first frame sends the mode write, once; a
/// reporting change later neither repeats nor drops it.
#[test]
fn ts004f_mode_write_queued_before_a_restart_is_sent_after_it() {
    let mut sim = Sim::default();
    let (mut hub, t) = knob_restarted_before_its_mode_write(&mut sim);
    let s = sim.dev(KNOB).clone();
    hub.on_aps_indication(cmd(&s, 1, 0x0006, 1, 0xFD, &[0x00]), t + 1000);
    sim.run(&mut hub, t + 1000);
    assert_eq!(mode_writes(&mut sim), 1);
    let mode = format!("{KNOB_ID}:1:operation_mode");
    hub.set_entity_report(&mode, None, &json!([60, 3600]), t + 2000)
        .unwrap();
    hub.on_aps_indication(cmd(&s, 1, 0x0006, 2, 0xFD, &[0x00]), t + 2000);
    sim.run(&mut hub, t + 2000);
    assert_eq!(mode_writes(&mut sim), 1);
    let d = hub.device(KNOB_ID).unwrap();
    let done = |k: &String| k.starts_with("write:1:0006:-:8004");
    assert!(d.configure.done.iter().any(|(k, ok)| done(k) && *ok));
    assert_eq!(d.configure.paired, 0, "configured under its definition");
    let configure = |a: &&&crate::transport::ApsRequest| {
        let global = |c: u8| {
            zf::ZclFrame::parse(&a.payload)
                .is_ok_and(|f| f.header.frame_type == FrameType::Global && f.header.cmd == c)
        };
        (a.profile == 0 && a.cluster == 0x0021)
            || (a.profile != 0
                && (global(global::WRITE_ATTRIBUTES) || global(global::CONFIGURE_REPORTING)))
    };
    assert!(sim.aps().iter().filter(configure).count() > 0);
    // Another restart: its first frame sends nothing done again.
    let snap = hub.snapshot_device(KNOB_ID).unwrap();
    let mut hub = hub_of(&[include_str!("fixtures/hobeian-zg-101zd.yaml")]);
    hub.restore_device(snap, None, t + 3000).unwrap();
    sim.log.clear();
    hub.on_aps_indication(cmd(&s, 1, 0x0006, 3, 0xFD, &[0x00]), t + 4000);
    sim.run(&mut hub, t + 4000);
    assert_eq!(sim.aps().iter().filter(configure).count(), 0);
}

/// A configure change before its first frame after that restart (a
/// reporting setting) does not take the unsent mode write as done.
#[test]
fn ts004f_mode_write_survives_a_change_before_its_first_frame() {
    let mut sim = Sim::default();
    let (mut hub, t) = knob_restarted_before_its_mode_write(&mut sim);
    let mode = format!("{KNOB_ID}:1:operation_mode");
    hub.set_entity_report(&mode, None, &json!([60, 3600]), t + 1000)
        .unwrap();
    let s = sim.dev(KNOB).clone();
    hub.on_aps_indication(cmd(&s, 1, 0x0006, 1, 0xFD, &[0x00]), t + 2000);
    sim.run(&mut hub, t + 2000);
    assert_eq!(mode_writes(&mut sim), 1);
}

/// A changed reporting setting rebuilds the configure ops: a mode write
/// that failed is retried, not taken as done.
#[test]
fn ts004f_failed_mode_write_survives_a_reporting_change() {
    let mut hub = hub_of(&[include_str!("fixtures/hobeian-zg-101zd.yaml")]);
    let mut sim = Sim::default();
    sim.devices.push(knob());
    sim.join(&mut hub, KNOB, T0);
    assert_eq!(mode_writes(&mut sim), 1);
    let d = hub.devices.get_mut(KNOB_ID).unwrap();
    let key = d
        .configure
        .done
        .keys()
        .find(|k| k.starts_with("write:1:0006:-:8004"))
        .unwrap()
        .clone();
    d.configure.done.insert(key.clone(), false);
    let t = T0 + 5000;
    let mode = format!("{KNOB_ID}:1:operation_mode");
    hub.set_entity_report(&mode, None, &json!([60, 3600]), t)
        .unwrap();
    let s = sim.dev(KNOB).clone();
    hub.on_aps_indication(cmd(&s, 1, 0x0006, 1, 0xFD, &[0x00]), t);
    sim.run(&mut hub, t);
    assert_eq!(mode_writes(&mut sim), 2);
    assert!(hub.device(KNOB_ID).unwrap().configure.done[&key]);
}

const LIDL: u64 = 0xa4c1_3800_0000_0103;
const LIDL_ID: &str = "zb:a4c1380000000103";

/// Lidl FB21-001 (`switch_scene`): the scene buttons' On/Off 0xFD with its
/// scene number and the remote's On/Off and Level Control commands are
/// events of the same `action` entity.
#[test]
fn fb21_001_scene_and_remote_commands() {
    let mut hub = hub_of(&[include_str!("fixtures/lidl-fb21-001.yaml")]);
    let mut sim = Sim::default();
    sim.devices.push(
        SimDevice::new(LIDL, 0x4103, LogicalType::EndDevice, false, 0x1002)
            .endpoint(
                1,
                0x0104,
                0x0820,
                &[0x0000, 0x0001, 0x0003, 0x1000],
                &[0x0003, 0x0004, 0x0005, 0x0006, 0x0008, 0x0300, 0x1000],
            )
            .basic("_TYZB01_hww2py6b", "TS1001", 1),
    );
    sim.join(&mut hub, LIDL, T0);
    assert_eq!(layer(&hub, LIDL_ID), "lidl/fb21-001");
    let s = sim.dev(LIDL).clone();
    let uid = format!("{LIDL_ID}:1:action");
    hub.drain_events();
    let t = T0 + 10_000;
    hub.on_aps_indication(cmd(&s, 1, 0x0006, 1, 0xFD, &[0x02]), t);
    hub.on_aps_indication(cmd(&s, 1, 0x0006, 2, 0x01, &[]), t + 100);
    hub.on_aps_indication(
        cmd(&s, 1, 0x0008, 3, 0x02, &[0x00, 0x20, 0x0A, 0x00]),
        t + 200,
    );
    let ev = fired(&hub.drain_events(), &uid);
    let types: Vec<&str> = ev.iter().map(|e| e.0.as_str()).collect();
    assert_eq!(types, ["switch_scene", "on", "brightness_step_up"]);
    assert_eq!(ev[0].1["scene"], json!(2));
}

const DIN: u64 = 0xa4c1_3800_0000_0104;
const DIN_ID: &str = "zb:a4c1380000000104";
const DIN_SLEEPY: u64 = 0xa4c1_3800_0000_0105;

/// TS011F_din_smart_relay_polling (`electricity_measurement_poll`):
/// Electrical Measurement voltage, current and power read on endpoint 1
/// every 60 s, less what the relay reported meanwhile; never from a sleepy
/// device, nor while the relay is unavailable or has left (its next frame
/// resumes the polls).
#[test]
fn ts011f_din_smart_relay_polling() {
    let mut hub = hub_of(&[include_str!("fixtures/ts011f_din_smart_relay_polling.yaml")]);
    let mut sim = Sim::default();
    for (ieee, nwk, sleepy) in [(DIN, 0x4104, false), (DIN_SLEEPY, 0x4105, true)] {
        let ty = if sleepy {
            LogicalType::EndDevice
        } else {
            LogicalType::Router
        };
        sim.devices.push(
            SimDevice::new(ieee, nwk, ty, !sleepy, 0x1002)
                .endpoint(
                    1,
                    0x0104,
                    0x0051,
                    &[
                        0x0000, 0x0003, 0x0004, 0x0005, 0x0006, 0x0702, 0x0b04, 0xe001,
                    ],
                    &[0x0019, 0x000a],
                )
                .basic("_TZ3000_qeuvnohg", "TS011F", 1),
        );
        sim.join(&mut hub, ieee, T0);
    }
    assert_eq!(layer(&hub, DIN_ID), "tuya/ts011f_din_smart_relay_polling");
    let reads = |sim: &Sim, nwk: u16, attrs: &[u8]| {
        sim.aps()
            .iter()
            .filter(|a| {
                a.dst == crate::transport::ApsDst::Nwk(nwk)
                    && a.cluster == 0x0b04
                    && a.payload[2] == global::READ_ATTRIBUTES
                    && a.payload[3..] == *attrs
            })
            .count()
    };
    let all = [0x05, 0x05, 0x08, 0x05, 0x0b, 0x05];
    let polls = |sim: &Sim, nwk: u16| reads(sim, nwk, &all);
    let at = |hub: &mut Hub<MockTransport>, sim: &mut Sim, s: u64| {
        sim.log.clear();
        hub.tick(T0 + s * 1000);
        sim.run(hub, T0 + s * 1000);
        (polls(sim, 0x4104), polls(sim, 0x4105))
    };
    at(&mut hub, &mut sim, 1);
    assert_eq!(at(&mut hub, &mut sim, 30), (0, 0));
    assert_eq!(at(&mut hub, &mut sim, 61), (1, 0));
    // Power reported at +100 s: at +121 s voltage and current only.
    let d = sim.dev(DIN).clone();
    let w = TypedValue::int(ZclType::I16, 120);
    let p = report(&d, 1, 0x0b04, None, 0x30, &[(0x050b, w)]);
    hub.on_aps_indication(p, T0 + 100_000);
    assert_eq!(at(&mut hub, &mut sim, 121), (0, 0));
    assert_eq!(reads(&sim, 0x4104, &[0x05, 0x05, 0x08, 0x05]), 1);
    // Unavailable: none; a frame from it resumes them.
    hub.device_mut(DIN_ID).unwrap().available = false;
    assert_eq!(at(&mut hub, &mut sim, 182), (0, 0));
    let r = report(
        &d,
        1,
        0x0006,
        None,
        0x31,
        &[(
            0x0000,
            TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(true))),
        )],
    );
    hub.on_aps_indication(r, T0 + 183_000);
    assert_eq!(at(&mut hub, &mut sim, 184), (1, 0));
    // Left: none.
    hub.device_mut(DIN_ID).unwrap().left = true;
    assert_eq!(at(&mut hub, &mut sim, 245), (0, 0));
}

const BUTTON: u64 = 0x0015_8d00_00cc_0001;
const BUTTON_ID: &str = "zb:00158d0000cc0001";

fn onoff(d: &SimDevice, tsn: u8, attr: u16, v: TypedValue) -> ApsIndication {
    report(d, 1, 0x0006, None, tsn, &[(attr, v)])
}

/// WXKG01LM (`lumi_action_wxkg01_lm`, `lumi_basic`): press and release
/// are a single click; pressed for 1 s a hold, then a release with its
/// duration; the click count 0x8000 double and more; battery and voltage
/// from Basic 0xFF02 (CR2032 curve 2850–3000 mV), power outage count.
#[test]
fn wxkg01lm_clicks_hold_and_battery() {
    let mut hub = hub_of(&[include_str!("fixtures/xiaomi-wxkg01lm.yaml")]);
    let mut sim = Sim::default();
    sim.devices.push(
        SimDevice::new(BUTTON, 0x6101, LogicalType::EndDevice, false, 0x1037)
            .endpoint(
                1,
                0x0104,
                0x0104,
                &[0x0000, 0x0003, 0x0019, 0xFFFF],
                &[0x0000, 0x0004, 0x0003, 0x0006, 0x0008, 0x0005, 0x0019],
            )
            .basic("LUMI", "lumi.sensor_switch", 0x03),
    );
    sim.join(&mut hub, BUTTON, T0);
    assert_eq!(layer(&hub, BUTTON_ID), "xiaomi/wxkg01lm");
    let s = sim.dev(BUTTON).clone();
    let uid = format!("{BUTTON_ID}:1:action");
    let b = |v: bool| TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(v)));
    hub.drain_events();
    let t = T0 + 10_000;
    hub.on_aps_indication(onoff(&s, 1, 0, b(false)), t);
    hub.on_aps_indication(onoff(&s, 2, 0, b(true)), t + 300);
    hub.tick(t + 2000);
    assert_eq!(events(&mut hub, &uid), ["single"]);
    // Held: hold after 1 s, release with the time since the hold.
    let t = t + 10_000;
    hub.on_aps_indication(onoff(&s, 3, 0, b(false)), t);
    hub.tick(t + 999);
    assert!(events(&mut hub, &uid).is_empty());
    hub.tick(t + 1000);
    assert_eq!(events(&mut hub, &uid), ["hold"]);
    hub.on_aps_indication(onoff(&s, 4, 0, b(true)), t + 2500);
    let ev = fired(&hub.drain_events(), &uid);
    assert_eq!(ev.len(), 1);
    assert_eq!(
        (ev[0].0.as_str(), &ev[0].1["duration"]),
        ("release", &json!(1500))
    );
    // Multiple clicks: 0x8000 (the button does not send 0x0000 for them).
    let n = |v: u64| TypedValue::uint(ZclType::U8, v);
    hub.on_aps_indication(onoff(&s, 5, 0x8000, n(2)), t + 5000);
    hub.on_aps_indication(onoff(&s, 6, 0x8000, n(4)), t + 6000);
    hub.on_aps_indication(onoff(&s, 7, 0x8000, n(7)), t + 7000);
    assert_eq!(events(&mut hub, &uid), ["double", "quadruple", "many"]);
    // Basic 0xFF02 (bogus manufacturer code): 2955 mV → 70 %, element 4
    // the power outage count + 1.
    let mut p = vec![0x02, 0xFF, 0x4C, 0x06, 0x00, 0x10, 0x01, 0x21];
    p.extend_from_slice(&2955u16.to_le_bytes());
    p.extend_from_slice(&[
        0x21, 0xA8, 0x13, 0x24, 1, 0, 0, 0, 0, 0x21, 0x6C, 0x00, 0x20, 0x00,
    ]);
    let ind = s.zcl(
        1,
        0x0000,
        FrameType::Global,
        Some(0x1234),
        0x33,
        global::REPORT_ATTRIBUTES,
        p,
    );
    hub.on_aps_indication(ind, t + 8000);
    let u = |k: &str| format!("{BUTTON_ID}:1:{k}");
    assert_eq!(state(&hub, &u("voltage")), json!(2955));
    assert_eq!(state(&hub, &u("battery")), json!(70.0));
    assert_eq!(state(&hub, &u("power_outage_count")), json!(107.0));
}

const MINI: u64 = 0x0015_8d00_00cc_0002;
const MINI_ID: &str = "zb:00158d0000cc0002";

/// WXKG11LM (`lumi_action_multistate`, `lumi_action`, `lumi_basic`):
/// Multistate Input presentValue clicks (newer firmware) and On/Off
/// reports (older); device temperature from Basic 0xFF01.
#[test]
fn wxkg11lm_multistate_and_on_off_clicks() {
    let mut hub = hub_of(&[include_str!("fixtures/aqara-wxkg11lm.yaml")]);
    let mut sim = Sim::default();
    sim.devices.push(
        SimDevice::new(MINI, 0x6102, LogicalType::EndDevice, false, 0x115F)
            .endpoint(
                1,
                0x0104,
                0x5F01,
                &[0x0000, 0x0001, 0x0003, 0x0012],
                &[0x0000, 0x0006],
            )
            .basic("LUMI", "lumi.remote.b1acn01", 0x03),
    );
    sim.join(&mut hub, MINI, T0);
    assert_eq!(layer(&hub, MINI_ID), "aqara/wxkg11lm");
    let s = sim.dev(MINI).clone();
    let uid = format!("{MINI_ID}:1:action");
    hub.drain_events();
    let pv = |tsn, v| {
        report(
            &s,
            1,
            0x0012,
            None,
            tsn,
            &[(0x0055, TypedValue::uint(ZclType::U16, v))],
        )
    };
    let t = T0 + 10_000;
    hub.on_aps_indication(pv(1, 1), t);
    hub.on_aps_indication(pv(2, 2), t + 100);
    hub.on_aps_indication(pv(3, 0), t + 200);
    hub.on_aps_indication(pv(4, 255), t + 300);
    let b = TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(true)));
    hub.on_aps_indication(onoff(&s, 5, 0, b), t + 400);
    hub.on_aps_indication(
        onoff(&s, 6, 0x8000, TypedValue::uint(ZclType::U8, 3)),
        t + 500,
    );
    assert_eq!(
        events(&mut hub, &uid),
        ["single", "double", "hold", "release", "single", "triple"]
    );
    // Basic 0xFF01: tag 3 device temperature (int8 °C), tag 1 voltage.
    let ff01 = TypedValue::new(
        ZclType::OCTET_STR,
        ZclValue::Octets(vec![0x01, 0x21, 0xB8, 0x0B, 0x03, 0x28, 0x17]),
    );
    hub.on_aps_indication(
        report(&s, 1, 0x0000, Some(0x115F), 7, &[(0xFF01, ff01)]),
        t + 600,
    );
    let u = |k: &str| format!("{MINI_ID}:1:{k}");
    assert_eq!(state(&hub, &u("device_temperature")), json!(23));
    assert_eq!(state(&hub, &u("voltage")), json!(3000));
}

const OPPLE: u64 = 0x0415_8d00_00cc_0003;
const OPPLE_ID: &str = "zb:04158d0000cc0003";

/// Aqara Opple WXCJKG13LM (6 buttons): event mode written at configure
/// (0xFCC0 0x0009 = 1, manufacturer 0x115F); presentValue clicks per
/// button endpoint, a hold released 5 s later unless the button reports.
#[test]
fn opple_switch_event_mode_buttons_and_commands() {
    let mut hub = hub_of(&[include_str!("fixtures/aqara-wxcjkg13lm.yaml")]);
    let mut sim = Sim::default();
    let mut dev = SimDevice::new(OPPLE, 0x6103, LogicalType::EndDevice, false, 0x115F).endpoint(
        1,
        0x0104,
        0x0105,
        &[0x0000, 0x0001, 0x0003, 0x0012, 0xFCC0],
        &[0x0003, 0x0006, 0x0008, 0x0300],
    );
    for ep in 2..=6 {
        dev = dev.endpoint(ep, 0x0104, 0x0105, &[0x0003, 0x0012], &[0x0006]);
    }
    sim.devices
        .push(dev.basic("LUMI", "lumi.remote.b686opcn01", 0x03));
    sim.join(&mut hub, OPPLE, T0);
    assert_eq!(layer(&hub, OPPLE_ID), "aqara/wxcjkg13lm");
    let s = sim.dev(OPPLE).clone();
    assert!(
        s.writes.iter().any(|w| (w.1, w.2, w.3, w.4.value.clone())
            == (0xFCC0, 0x0009, Some(0x115F), ZclValue::Uint(1))),
        "{:?}",
        s.writes
    );
    assert!(
        hub.entity(&format!("{OPPLE_ID}:1:operation_mode"))
            .is_some()
    );
    let uid = format!("{OPPLE_ID}:1:action");
    hub.drain_events();
    let pv = |ep, tsn, v| {
        report(
            &s,
            ep,
            0x0012,
            None,
            tsn,
            &[(0x0055, TypedValue::uint(ZclType::U16, v))],
        )
    };
    let t = T0 + 10_000;
    hub.on_aps_indication(pv(3, 1, 1), t);
    hub.on_aps_indication(pv(6, 2, 2), t + 100);
    hub.on_aps_indication(pv(5, 3, 0), t + 200);
    hub.tick(t + 5199);
    assert_eq!(
        events(&mut hub, &uid),
        ["button_3_single", "button_6_double", "button_5_hold"]
    );
    hub.tick(t + 5200);
    assert_eq!(events(&mut hub, &uid), ["button_5_release"]);
    // Released in time: its own release, no second one.
    hub.on_aps_indication(pv(2, 4, 0), t + 6000);
    hub.on_aps_indication(pv(2, 5, 255), t + 7000);
    hub.tick(t + 20_000);
    assert_eq!(
        events(&mut hub, &uid),
        ["button_2_hold", "button_2_release"]
    );
    // Command mode (On/Off, Level): no actions, the template writes
    // event mode.
    hub.on_aps_indication(cmd(&s, 1, 0x0006, 6, 0x01, &[]), t + 30_000);
    assert!(events(&mut hub, &uid).is_empty());
}

const H1: u64 = 0x5415_8d00_00cc_0004;
const H1_ID: &str = "zb:54158d0000cc0004";

/// Aqara H1 wall switch WS-EUK03 (`lumi_specific` and settings on 0xFCC0):
/// the 0x00F7 struct's device temperature, energy and power outage count;
/// the button's operation mode and the flip indicator write 0xFCC0 with
/// the Lumi manufacturer code; a Basic 0xFFF0 reset announcement is
/// answered (`lumi_prevent_reset`).
#[test]
fn aqara_h1_switch_struct_settings_and_prevent_reset() {
    let mut hub = hub_of(&[include_str!("fixtures/aqara-ws-euk03.yaml")]);
    let mut sim = Sim::default();
    sim.devices.push(
        SimDevice::new(H1, 0x6104, LogicalType::Router, true, 0x115F)
            .endpoint(
                1,
                0x0104,
                0x0002,
                &[
                    0x0000, 0x0002, 0x0003, 0x0004, 0x0005, 0x0006, 0x0009, 0x0702, 0x0B04, 0xFCC0,
                ],
                &[0x000A, 0x0019],
            )
            .endpoint(41, 0x0104, 0x0100, &[0x0012], &[])
            .basic("LUMI", "lumi.switch.n1aeu1", 0x01)
            .attr(
                1,
                0x0006,
                0x0000,
                TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false))),
            ),
    );
    sim.join(&mut hub, H1, T0);
    assert_eq!(layer(&hub, H1_ID), "aqara/ws-euk03");
    let k = keys(&hub, H1_ID);
    for want in [
        "1:switch",
        "1:operation_mode",
        "1:flip_indicator_light",
        "1:led_disabled_night",
        "1:power_outage_memory",
        "1:device_temperature",
        "1:energy",
        "1:power_outage_count",
    ] {
        assert!(k.iter().any(|x| x == want), "{want}: {k:?}");
    }
    let s = sim.dev(H1).clone();
    // 0x00F7: tag 3 device temperature 31 °C, tag 5 outage count 9 + 1,
    // tag 0x95 energy 1.25 kWh (single).
    let mut f7 = vec![0x03, 0x28, 31, 0x05, 0x21, 0x0A, 0x00, 0x95, 0x39];
    f7.extend_from_slice(&1.25f32.to_le_bytes());
    let v = TypedValue::new(ZclType::OCTET_STR, ZclValue::Octets(f7));
    hub.on_aps_indication(
        report(&s, 1, 0xFCC0, Some(0x115F), 1, &[(0x00F7, v)]),
        T0 + 1000,
    );
    let u = |k: &str| format!("{H1_ID}:1:{k}");
    assert_eq!(state(&hub, &u("device_temperature")), json!(31));
    assert_eq!(state(&hub, &u("power_outage_count")), json!(9.0));
    assert_eq!(state(&hub, &u("energy")), json!(1.25));
    let t = T0 + 2000;
    hub.entity_command(
        &u("operation_mode"),
        "select_option",
        &json!({"option": "decoupled"}),
        t,
    )
    .unwrap();
    hub.entity_command(&u("flip_indicator_light"), "turn_on", &json!({}), t)
        .unwrap();
    sim.run(&mut hub, t);
    let w: Vec<_> = sim
        .dev(H1)
        .writes
        .iter()
        .map(|(ep, c, a, m, v)| (*ep, *c, *a, *m, v.value.clone()))
        .collect();
    assert!(
        w.contains(&(1, 0xFCC0, 0x0200, Some(0x115F), ZclValue::Uint(0))),
        "{w:?}"
    );
    assert!(
        w.contains(&(1, 0xFCC0, 0x00F0, Some(0x115F), ZclValue::Uint(1))),
        "{w:?}"
    );
    // The reset announcement and its answer.
    let ask = vec![0xAA, 0x10, 0x05, 0x41, 0x87, 0x01, 0x01, 0x10, 0x00];
    let v = TypedValue::new(ZclType::OCTET_STR, ZclValue::Octets(ask));
    hub.on_aps_indication(
        report(&s, 1, 0x0000, Some(0x115F), 2, &[(0xFFF0, v)]),
        t + 1000,
    );
    sim.run(&mut hub, t + 1000);
    let last = sim.dev(H1).writes.last().cloned().unwrap();
    assert_eq!(
        (last.1, last.2, last.3, last.4.value),
        (
            0x0000,
            0xFFF0,
            Some(0x115F),
            ZclValue::Octets(vec![0xAA, 0x10, 0x05, 0x41, 0x47, 0x01, 0x01, 0x10, 0x01])
        )
    );
}

const Z1: u64 = 0x5415_8d00_00cc_0005;
const Z1_ID: &str = "zb:54158d0000cc0005";

/// Aqara Z1 double rocker ZNQBKG39LM (zhc's modern `lumiAction` and
/// `lumiOnOff` with arguments): operation mode and relay lock per rocker,
/// power-on behaviour; presentValue actions `<click>_<rocker>`; voltage,
/// current and energy from 0x00F7; event mode written at configure.
#[test]
fn aqara_z1_switch_modern_actions_and_settings() {
    let mut hub = hub_of(&[include_str!("fixtures/aqara-znqbkg39lm.yaml")]);
    let mut sim = Sim::default();
    sim.devices.push(
        SimDevice::new(Z1, 0x6105, LogicalType::Router, true, 0x115F)
            .endpoint(
                1,
                0x0104,
                0x0002,
                &[0x0000, 0x0003, 0x0006, 0x000C, 0x0012, 0xFCC0],
                &[0x000A, 0x0019],
            )
            .endpoint(2, 0x0104, 0x0002, &[0x0006, 0x0012], &[])
            .basic("LUMI", "lumi.switch.acn049", 0x01)
            .attr(
                1,
                0x0006,
                0x0000,
                TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(true))),
            )
            .attr(
                2,
                0x0006,
                0x0000,
                TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false))),
            ),
    );
    sim.join(&mut hub, Z1, T0);
    assert_eq!(layer(&hub, Z1_ID), "aqara/znqbkg39lm");
    let k = keys(&hub, Z1_ID);
    for want in [
        "1:switch",
        "2:switch",
        "1:operation_mode",
        "2:operation_mode",
        "1:lock_relay",
        "2:lock_relay",
        "1:power_on_behavior",
        "1:voltage",
        "1:current",
        "1:energy",
    ] {
        assert!(k.iter().any(|x| x == want), "{want}: {k:?}");
    }
    let s = sim.dev(Z1).clone();
    assert!(
        s.writes.iter().any(|w| (w.1, w.2, w.3, w.4.value.clone())
            == (0xFCC0, 0x0009, Some(0x115F), ZclValue::Uint(1))),
        "{:?}",
        s.writes
    );
    let uid = format!("{Z1_ID}:1:action");
    hub.drain_events();
    let pv = |ep, tsn, v| {
        report(
            &s,
            ep,
            0x0012,
            None,
            tsn,
            &[(0x0055, TypedValue::uint(ZclType::U16, v))],
        )
    };
    let t = T0 + 10_000;
    for (i, (ep, v)) in [(1, 1), (2, 2), (2, 0), (2, 255)].into_iter().enumerate() {
        hub.on_aps_indication(pv(ep, i as u8 + 1, v), t + 100 * i as u64);
    }
    assert_eq!(
        events(&mut hub, &uid),
        [
            "single_top",
            "double_bottom",
            "hold_bottom",
            "release_bottom"
        ]
    );
    // 0x00F7: tag 150 voltage (0.1 V), 151 current (mA), 149 energy.
    let mut f7 = vec![0x96, 0x23];
    f7.extend_from_slice(&2305u32.to_le_bytes());
    f7.extend_from_slice(&[0x97, 0x23]);
    f7.extend_from_slice(&1500u32.to_le_bytes());
    f7.extend_from_slice(&[0x95, 0x39]);
    f7.extend_from_slice(&2.5f32.to_le_bytes());
    let v = TypedValue::new(ZclType::OCTET_STR, ZclValue::Octets(f7));
    hub.on_aps_indication(
        report(&s, 1, 0xFCC0, Some(0x115F), 9, &[(0x00F7, v)]),
        t + 1000,
    );
    let u = |k: &str| format!("{Z1_ID}:1:{k}");
    assert_eq!(state(&hub, &u("voltage")), json!(230.5));
    assert_eq!(state(&hub, &u("current")), json!(1.5));
    assert_eq!(state(&hub, &u("energy")), json!(2.5));
    // The bottom rocker decoupled: 0xFCC0 0x0200 on its endpoint.
    let t = t + 2000;
    hub.entity_command(
        &format!("{Z1_ID}:2:operation_mode"),
        "select_option",
        &json!({"option": "decoupled"}),
        t,
    )
    .unwrap();
    sim.run(&mut hub, t);
    let w = sim.dev(Z1).writes.last().cloned().unwrap();
    assert_eq!(
        (w.0, w.1, w.2, w.3, w.4.value),
        (2, 0xFCC0, 0x0200, Some(0x115F), ZclValue::Uint(0))
    );
}
