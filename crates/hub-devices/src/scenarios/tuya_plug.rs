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
    // `tuya` vendor template, which the hub skips (generic + plug_1 win);
    // `ts011f_plug_1` needs `tuya_metering_zero_debounce`, which it has.
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

/// A failed `EVT_APS_DATA_CONFIRM` as the ZRHP backend reports it.
fn not_delivered(zb_status: u8) -> crate::transport::Terminal {
    use hub_coord::{BackendKind, Code, CoordError};
    let text = match zb_status {
        0xA7 => "no APS ACK",
        0xD0 => "no route to the device",
        _ => unreachable!(),
    };
    crate::transport::Terminal::NotDelivered(CoordError::NoAck(Code {
        backend: BackendKind::Zrhp,
        value: zb_status.into(),
        text: Some(text),
    }))
}

#[test]
fn plug_command_fails_fast_when_not_delivered() {
    let (mut hub, mut sim) = joined();
    let sw = format!("{ID}:1:switch");
    sim.dev(IEEE).responsive = false;
    hub.transport.take();
    hub.drain_events();
    let ack = hub
        .entity_command(&sw, "turn_on", &json!({}), T0 + 1000)
        .unwrap();
    let first = hub.transport.last_id().unwrap();
    let fail = |hub: &mut Hub<MockTransport>, now| {
        assert_eq!(hub.transport.aps().len(), 1);
        hub.transport.take();
        let id = hub.transport.last_id().unwrap();
        hub.on_outcome(id, not_delivered(0xD0).into(), now);
    };
    // The first failure retries at once, the second fails the command.
    fail(&mut hub, T0 + 1500);
    hub.tick(T0 + 1500);
    assert_eq!(result_of(&hub.drain_events(), ack.ticket), None);
    fail(&mut hub, T0 + 2000);
    hub.tick(T0 + 2000);
    let e = result_of(&hub.drain_events(), ack.ticket)
        .expect("failed fast")
        .unwrap_err();
    assert_eq!(e.code, "not_delivered");
    assert!(e.message.contains("no route"), "{}", e.message);
    // A failure of a request nobody waits for any more is ignored.
    hub.on_outcome(first, not_delivered(0xA7).into(), T0 + 2500);
    hub.tick(T0 + 2500);
    assert!(hub.transport.aps().is_empty());
}

#[test]
fn stale_failure_of_an_identical_request_spares_its_retry() {
    let (mut hub, mut sim) = joined();
    let sw = format!("{ID}:1:switch");
    sim.dev(IEEE).responsive = false;
    hub.transport.take();
    hub.drain_events();
    let ack = hub
        .entity_command(&sw, "turn_on", &json!({}), T0 + 1000)
        .unwrap();
    let first = hub.transport.last_id().unwrap();
    let sent = hub.transport.take();
    // No answer: the response timeout re-sends the same bytes.
    hub.tick(T0 + 9500);
    assert_eq!(hub.transport.take(), sent);
    let retry = hub.transport.last_id().unwrap();
    assert_ne!(first, retry);
    // The first send's failure arrives late: it is not the retry's.
    hub.on_outcome(first, not_delivered(0xD0).into(), T0 + 9600);
    hub.tick(T0 + 9600);
    assert_eq!(result_of(&hub.drain_events(), ack.ticket), None);
    assert!(hub.transport.aps().is_empty(), "no extra retry");
    // The retry's own failure fails the command (it was retried once).
    hub.on_outcome(retry, not_delivered(0xD0).into(), T0 + 9700);
    let e = result_of(&hub.drain_events(), ack.ticket)
        .expect("failed fast")
        .unwrap_err();
    assert_eq!(e.code, "not_delivered");
    assert_eq!(
        e.message,
        "not delivered: no route to the device (zb_status 0xD0)"
    );
}

fn electrical(sim: &mut Sim, attr: u16, v: i64, ty: ZclType) -> crate::transport::ApsIndication {
    let tv = if ty == ZclType::I16 {
        TypedValue::int(ty, v)
    } else {
        TypedValue::uint(ty, v as u64)
    };
    sim.dev(IEEE).clone().report(1, 0x0b04, None, &[(attr, tv)])
}

/// Template `tuya_metering_zero_debounce` (zhc
/// `tuya.fz.TS011F_electrical_measurement`): zero readings wait twice the
/// minimum reporting interval plus a second (generic power, current and
/// voltage: 5 s, so 11 s) and only stand if nothing else came; a zero
/// current also zeroes the power; while off the power is 0 at once.
#[test]
fn plug_metering_zero_readings_are_debounced() {
    let (mut hub, mut sim) = joined();
    let d = hub.device(ID).unwrap();
    assert!(d.resolution.metering_zero_debounce);
    // TS011F_plug_3 (template `tuya`) loads too; plug_1 matches better.
    assert!(
        d.resolution
            .diagnostics
            .iter()
            .all(|x| !x.contains("requires"))
    );
    let uid = |k: &str| format!("{ID}:1:{k}");
    let on = sim.dev(IEEE).clone().report(
        1,
        0x0006,
        None,
        &[(
            0,
            TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(true))),
        )],
    );
    hub.on_aps_indication(on, T0 + 1000);
    let t = T0 + 10_000;
    hub.on_aps_indication(electrical(&mut sim, 0x050b, 120, ZclType::I16), t);
    hub.on_aps_indication(electrical(&mut sim, 0x0508, 500, ZclType::U16), t);
    assert_eq!(state(&hub, &uid("power")), json!(120.0));
    // A zero power is held back for 11 s ...
    hub.on_aps_indication(electrical(&mut sim, 0x050b, 0, ZclType::I16), t + 1000);
    assert_eq!(state(&hub, &uid("power")), json!(120.0));
    hub.tick(t + 11_999);
    assert_eq!(state(&hub, &uid("power")), json!(120.0));
    hub.tick(t + 12_000);
    assert_eq!(state(&hub, &uid("power")), json!(0.0));
    // ... and dropped when another reading comes first.
    let t = t + 20_000;
    hub.on_aps_indication(electrical(&mut sim, 0x050b, 50, ZclType::I16), t);
    hub.on_aps_indication(electrical(&mut sim, 0x050b, 0, ZclType::I16), t + 1000);
    hub.on_aps_indication(electrical(&mut sim, 0x050b, 60, ZclType::I16), t + 2000);
    hub.tick(t + 30_000);
    assert_eq!(state(&hub, &uid("power")), json!(60.0));
    // A zero current, once due, zeroes the power as well.
    let t = t + 40_000;
    hub.on_aps_indication(electrical(&mut sim, 0x0508, 0, ZclType::U16), t);
    assert_eq!(state(&hub, &uid("current")), json!(500.0));
    hub.tick(t + 11_000);
    assert_eq!(
        (state(&hub, &uid("current")), state(&hub, &uid("power"))),
        (json!(0.0), json!(0.0))
    );
    // Switched off: any Electrical Measurement report sets power 0 at once.
    let t = t + 20_000;
    hub.on_aps_indication(electrical(&mut sim, 0x050b, 75, ZclType::I16), t);
    let off = sim.dev(IEEE).clone().report(
        1,
        0x0006,
        None,
        &[(
            0,
            TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false))),
        )],
    );
    hub.on_aps_indication(off, t + 100);
    hub.on_aps_indication(electrical(&mut sim, 0x0505, 231, ZclType::U16), t + 200);
    assert_eq!(
        (state(&hub, &uid("power")), state(&hub, &uid("voltage"))),
        (json!(0.0), json!(231.0))
    );
}

/// Without the template (a definition that does not ask for it) zeros are
/// published at once, as before.
#[test]
fn zero_readings_without_the_template_are_immediate() {
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    sim.devices.push(plug());
    sim.join(&mut hub, IEEE, T0);
    assert!(!hub.device(ID).unwrap().resolution.metering_zero_debounce);
    hub.on_aps_indication(electrical(&mut sim, 0x050b, 120, ZclType::I16), T0 + 1000);
    hub.on_aps_indication(electrical(&mut sim, 0x050b, 0, ZclType::I16), T0 + 2000);
    assert_eq!(state(&hub, &format!("{ID}:1:power")), json!(0.0));
}

fn refused(id: &str, model: &str, requires: &str, score: u32) -> crate::device::Refused {
    crate::device::Refused {
        id: id.into(),
        vendor: "Tuya".into(),
        model: model.into(),
        requires: vec![requires.into()],
        score,
        priority: 0,
        shard: 0,
    }
}

/// `definition_approximate`: the plug runs TS011F_plug_1 while a catalog
/// definition that outranks it needs a feature this firmware lacks (here
/// made up: `template.hue_light`). What a
/// shard found is replaced only by that shard's next check (a shard checked
/// for another device does not clear the others'), kept in the snapshot;
/// a firmware that gained the feature drops it on restore and marks the
/// device for the catalog service to check again.
#[test]
fn approximate_definition_is_recorded_kept_and_rechecked() {
    let (mut hub, _sim) = joined();
    let d = hub.device(ID).unwrap();
    let s = d.resolution.layers.last().unwrap().score.unwrap();
    assert!(d.record().get("definition_approximate").is_none());
    let both = [0x10, 0x20];
    let approx = |hub: &Hub<MockTransport>| {
        hub.device(ID)
            .unwrap()
            .record()
            .get("definition_approximate")
            .cloned()
    };
    // A lower-scoring refusal is no better: not approximate.
    hub.note_refused(
        ID,
        7,
        0x10,
        &both,
        true,
        vec![refused("tuya/x", "X", "template.hue_light", s)],
    );
    assert_eq!(approx(&hub), None);
    hub.drain_events();
    let plug_3 = || {
        refused(
            "tuya/ts011f_plug_3",
            "JR-ZPM03",
            "template.hue_light",
            s + 10,
        )
    };
    hub.note_refused(ID, 7, 0x20, &both, true, vec![plug_3()]);
    assert!(
        hub.drain_events()
            .iter()
            .any(|e| matches!(e, HubEvent::DeviceUpdated(i) if i == ID))
    );
    let want =
        json!({"better": "tuya/ts011f_plug_3 (Tuya JR-ZPM03)", "needs": ["template.hue_light"]});
    assert_eq!(approx(&hub), Some(want.clone()));
    assert_eq!(hub.diagnostics(ID).unwrap()["definition_approximate"], want);
    // A newer catalog: shard 0x10 checked (e.g. for another device sharing
    // it) replaces only its own findings; plug_3 (0x20) stays.
    hub.note_refused(
        ID,
        8,
        0x10,
        &both,
        true,
        vec![refused("future/x", "Y", "feature 13", s + 5)],
    );
    let d = hub.device(ID).unwrap();
    let ids = |d: &crate::device::Device| {
        d.refused
            .iter()
            .map(|r| (r.id.clone(), r.shard))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(d),
        [
            ("tuya/ts011f_plug_3".to_string(), 0x20),
            ("future/x".to_string(), 0x10)
        ]
    );
    assert_eq!(
        (d.refused_version, d.refused_checked.clone()),
        (8, vec![0x10])
    );
    assert_eq!(approx(&hub), Some(want.clone()));
    // A failed store does not count as checked.
    hub.note_refused(ID, 8, 0x20, &both, false, vec![plug_3()]);
    assert_eq!(hub.device(ID).unwrap().refused_checked, [0x10]);
    hub.note_refused(ID, 8, 0x20, &both, true, vec![plug_3()]);
    assert_eq!(hub.device(ID).unwrap().refused_checked, [0x10, 0x20]);
    let snap = hub.snapshot_device(ID).unwrap();
    let mut snap = crate::persist::DeviceSnapshot::from_bytes(&snap.to_bytes()).unwrap();
    assert_eq!(
        (
            snap.refused.len(),
            snap.refused_version,
            snap.refused_checked.clone()
        ),
        (2, 8, vec![0x10, 0x20])
    );
    // Once every shard of the device is checked, findings of shards no
    // longer its own go.
    hub.note_refused(ID, 9, 0x20, &[0x20], true, vec![plug_3()]);
    assert_eq!(
        ids(hub.device(ID).unwrap()),
        [("tuya/ts011f_plug_3".to_string(), 0x20)]
    );
    // Saved by a build that named code 20 `feature 20`.
    let p3 = snap
        .refused
        .iter()
        .position(|r| r.id == "tuya/ts011f_plug_3")
        .unwrap();
    snap.refused[p3].requires = vec!["feature 20".into()];
    // Restored by a firmware without template.hue_light: unchanged.
    let mut same = hub_with(&[]);
    same.restore_device(snap.clone(), None, T0).unwrap();
    let d = same.device(ID).unwrap();
    assert_eq!((d.refused.len(), d.refused_version), (2, 8));
    assert_eq!(d.refused[p3].requires, ["template.hue_light"]);
    // By one that has it: plug_3 is dropped (to be fetched now), every
    // shard is checked again.
    let mut newer = hub_with(&[]);
    newer.features.push("template.hue_light");
    newer.restore_device(snap, None, T0).unwrap();
    let d = newer.device(ID).unwrap();
    assert_eq!(
        d.refused.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        ["future/x"]
    );
    assert_eq!(
        (
            d.refused_version,
            d.refused_checked.len(),
            d.refused_recheck
        ),
        (0, 0, true)
    );
    // The flag stays until every shard of the device is checked (a shard
    // checked for another device sharing it does not end the recheck).
    newer.note_refused(ID, 9, 0x10, &both, true, vec![]);
    assert!(newer.device(ID).unwrap().refused_recheck);
    newer.note_refused(ID, 9, 0x20, &both, true, vec![]);
    assert!(!newer.device(ID).unwrap().refused_recheck);
    // A new fingerprint (re-interview): no shard counts as checked.
    assert_eq!(hub.device(ID).unwrap().refused_checked, [0x20]);
    hub.device_mut(ID).unwrap().fp.app_version = Some(70);
    hub.resolve_device(ID, T0 + 1);
    let d = hub.device(ID).unwrap();
    assert_eq!((d.refused_version, d.refused_checked.len()), (0, 0));
    assert_eq!(d.refused.len(), 1, "what was found stays until checked");
}
