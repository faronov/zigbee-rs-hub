//! Reporting configuration (5.2, 5.3): global defaults, per-entity
//! overrides, the configure hash and the Configure Reporting bytes they
//! produce, validation, persistence and the readback (Read Reporting
//! Configuration).

use serde_json::json;

use super::*;
use crate::api::{ApiServer, SessionId};
use crate::persist::DeviceSnapshot;
use crate::testutil::sim::{Sim, SimDevice};
use crate::transport::Sent;
use crate::zcl::frame::{self as zf, global};
use crate::zcl::{FrameType, Status, TypedValue, ZclType, ZclValue};
use crate::zdo::LogicalType;

const MAINS: u64 = 0x00124b00aaaa0001;
const SLEEPY: u64 = 0x00158d0001a2b3c4;
const LIGHT: u64 = 0x0017880100aabbcc;
const EFEKTA: u64 = 0x00124b0012345678;

fn mains() -> SimDevice {
    SimDevice::new(MAINS, 0x5001, LogicalType::Router, true, 0x1234)
        .endpoint(1, 0x0104, 0x0302, &[0x0000, 0x0003, 0x0402, 0x0405], &[])
        .basic("ACME", "TH-M", 0x01)
        .attr(1, 0x0402, 0x0000, TypedValue::int(ZclType::I16, 2137))
        .attr(1, 0x0405, 0x0000, TypedValue::uint(ZclType::U16, 4510))
}

fn sleepy() -> SimDevice {
    SimDevice::new(SLEEPY, 0x1234, LogicalType::EndDevice, false, 0x1037)
        .endpoint(
            1,
            0x0104,
            0x0302,
            &[0x0000, 0x0001, 0x0003, 0x0402, 0x0405],
            &[0x0019],
        )
        .basic("ACME", "TH-1", 0x03)
        .attr(1, 0x0402, 0x0000, TypedValue::int(ZclType::I16, 2137))
        .attr(1, 0x0405, 0x0000, TypedValue::uint(ZclType::U16, 4510))
        .attr(1, 0x0001, 0x0021, TypedValue::uint(ZclType::U8, 180))
        .attr(1, 0x0001, 0x0020, TypedValue::uint(ZclType::U8, 30))
}

fn light() -> SimDevice {
    SimDevice::new(LIGHT, 0x2001, LogicalType::Router, true, 0x100B)
        .endpoint(11, 0x0104, 0x0101, &[0x0000, 0x0003, 0x0006, 0x0008], &[])
        .basic("ACME", "Dimmer", 0x01)
        .attr(
            11,
            0x0006,
            0x0000,
            TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false))),
        )
        .attr(11, 0x0008, 0x0000, TypedValue::uint(ZclType::U8, 10))
}

fn efekta() -> SimDevice {
    SimDevice::new(EFEKTA, 0x4001, LogicalType::Router, true, 0x1234)
        .endpoint(
            1,
            0x0104,
            0x000C,
            &[0x0000, 0x0003, 0x0402, 0x0405, 0x040D],
            &[],
        )
        .basic("EfektaLab", "EFEKTA_CO2_Smart_Monitor", 0x01)
        .attr(1, 0x0402, 0x0000, TypedValue::int(ZclType::I16, 2350))
        .attr(1, 0x0405, 0x0000, TypedValue::uint(ZclType::U16, 4120))
}

fn id(ieee: u64) -> String {
    format!("zb:{ieee:016x}")
}

/// Configure Reporting frames sent: `(nwk, cluster, payload after the
/// header)`.
fn configure_frames(sim: &Sim) -> Vec<(u16, u16, Vec<u8>)> {
    sim.aps()
        .iter()
        .filter(|a| a.profile != 0 && a.payload[2] == global::CONFIGURE_REPORTING)
        .map(|a| {
            let nwk = match a.dst {
                crate::transport::ApsDst::Nwk(n) => n,
                _ => 0,
            };
            (nwk, a.cluster, a.payload[3..].to_vec())
        })
        .collect()
}

fn hash(hub: &Hub<MockTransport>, ieee: u64) -> Option<String> {
    hub.device(&id(ieee)).unwrap().configure.hash.clone()
}

/// The rows of `entities/reporting` for a device, by uid (and part).
fn rows(hub: &mut Hub<MockTransport>, device: &str) -> Vec<J> {
    let r = hub.entity_reporting(None, Some(device), false, T0).unwrap();
    assert_eq!(r["status"], "cached");
    r["records"].as_array().unwrap().clone()
}

fn row(rows: &[J], uid: &str) -> J {
    rows.iter()
        .find(|r| r["uid"] == uid)
        .unwrap_or_else(|| panic!("no row {uid}"))
        .clone()
}

#[test]
fn override_changes_the_hash_and_reconfigures_with_its_bytes() {
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    sim.devices.push(mains());
    sim.join(&mut hub, MAINS, T0);
    let uid = format!("{}:1:temperature", id(MAINS));
    let h0 = hash(&hub, MAINS);
    let builtin = vec![0x00, 0x00, 0x00, 0x29, 30, 0, 0x84, 0x03, 50, 0];
    assert!(configure_frames(&sim).contains(&(0x5001, 0x0402, builtin.clone())));

    // [60, 1800, 25]: one Configure Reporting frame, nothing else (the
    // bind and the humidity config stand).
    sim.log.clear();
    hub.set_entity_report(&uid, None, &json!([60, 1800, 25]), T0 + 1000)
        .unwrap();
    sim.run(&mut hub, T0 + 1000);
    assert_eq!(
        configure_frames(&sim),
        [(
            0x5001,
            0x0402,
            vec![0x00, 0x00, 0x00, 0x29, 60, 0, 0x08, 0x07, 25, 0]
        )]
    );
    assert_eq!(sim.aps().len(), 1);
    let h1 = hash(&hub, MAINS);
    assert_ne!(h1, h0);
    let x = row(&rows(&mut hub, &id(MAINS)), &uid);
    assert_eq!(
        (
            x["report"].clone(),
            x["source"].clone(),
            x["default"].clone()
        ),
        (
            json!([60, 1800, 25]),
            json!("override"),
            json!([30, 900, 50])
        )
    );

    // off: maximum interval 0xFFFF, the device stops reporting.
    sim.log.clear();
    hub.set_entity_report(&uid, None, &json!("off"), T0 + 2000)
        .unwrap();
    sim.run(&mut hub, T0 + 2000);
    assert_eq!(
        configure_frames(&sim),
        [(
            0x5001,
            0x0402,
            vec![0x00, 0x00, 0x00, 0x29, 0, 0, 0xFF, 0xFF, 0, 0]
        )]
    );
    assert_ne!(hash(&hub, MAINS), h1);

    // Back to the default: the built-in config goes out again although it
    // succeeded before, and the hash is the original one.
    sim.log.clear();
    hub.set_entity_report(&uid, None, &J::Null, T0 + 3000)
        .unwrap();
    sim.run(&mut hub, T0 + 3000);
    assert_eq!(configure_frames(&sim), [(0x5001, 0x0402, builtin)]);
    assert_eq!(hash(&hub, MAINS), h0);
    let x = row(&rows(&mut hub, &id(MAINS)), &uid);
    assert_eq!(
        (x["report"].clone(), x["source"].clone(), x.get("default")),
        (json!([30, 900, 50]), json!("default"), None)
    );
    assert!(hub.device(&id(MAINS)).unwrap().report_overrides.is_empty());
    // Unchanged: nothing is sent.
    sim.log.clear();
    hub.set_entity_report(&uid, None, &json!("default"), T0 + 4000)
        .unwrap();
    sim.run(&mut hub, T0 + 4000);
    assert!(sim.aps().is_empty());
}

#[test]
fn user_off_is_not_polled_but_a_refused_default_is() {
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    let mut dev = mains();
    dev.unreportable.insert((0x0402, 0x0000));
    sim.devices.push(dev);
    sim.join(&mut hub, MAINS, T0);
    let reads = |sim: &Sim| {
        sim.aps()
            .iter()
            .filter(|a| a.cluster == 0x0402 && a.payload[2] == global::READ_ATTRIBUTES)
            .count()
    };
    // Refused: polled every `max` (900 s).
    sim.log.clear();
    hub.tick(T0 + 901_000);
    sim.run(&mut hub, T0 + 901_000);
    assert_eq!(reads(&sim), 1);
    // The user's off: sent (refused again), never polled.
    let uid = format!("{}:1:temperature", id(MAINS));
    hub.set_entity_report(&uid, None, &json!("off"), T0 + 902_000)
        .unwrap();
    sim.run(&mut hub, T0 + 902_000);
    sim.log.clear();
    for t in [1_900_000, 2_900_000, 3_900_000] {
        hub.tick(T0 + t);
        sim.run(&mut hub, T0 + t);
    }
    assert_eq!(reads(&sim), 0);
}

#[test]
fn settings_are_validated() {
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    sim.devices.push(sleepy());
    sim.devices.push(light());
    sim.join(&mut hub, SLEEPY, T0);
    sim.join(&mut hub, LIGHT, T0);
    let temp = format!("{}:1:temperature", id(SLEEPY));
    let battery = format!("{}:1:battery", id(SLEEPY));
    let lamp = format!("{}:11:light", id(LIGHT));
    let mut err = |uid: &str, part: Option<&str>, v: J| {
        hub.set_entity_report(uid, part, &v, T0 + 100)
            .unwrap_err()
            .code
    };
    for v in [
        json!([900, 60]),           // min above max
        json!([0, 65535]),          // 0xFFFF is `off`
        json!([0, 70000]),          // not an interval
        json!([30, 900, -1]),       // negative change
        json!("sometimes"),         // not a setting
        json!([1, 2, 3, 4]),        // too long
        json!([3600, 43200, 50.0]), // above the battery cap (6 h)
    ] {
        assert_eq!(err(&temp, None, v.clone()), "invalid_value", "{v}");
    }
    // Change beyond the attribute's type (uint8 battery percent).
    assert_eq!(
        err(&battery, None, json!([3600, 21600, 300])),
        "invalid_value"
    );
    // Composite entities name the part; change only for analog types.
    assert_eq!(err(&lamp, None, json!([1, 300])), "invalid_value");
    assert_eq!(err(&lamp, Some("nope"), json!([1, 300])), "invalid_value");
    assert_eq!(
        err(&lamp, Some("on_off"), json!([0, 300, 1])),
        "invalid_value"
    );
    // Entities without a ZCL attribute; unknown entities.
    let identify = format!("{}:1:identify", id(SLEEPY));
    assert_eq!(err(&identify, None, json!([1, 300])), "invalid_value");
    assert_eq!(
        err("zb:0000000000000001:1:x", None, json!("off")),
        "not_found"
    );
    // Nothing above changed anything.
    assert!(hub.device(&id(SLEEPY)).unwrap().report_overrides.is_empty());
    // Valid settings.
    hub.set_entity_report(&lamp, Some("on_off"), &json!([0, 300]), T0)
        .unwrap();
    hub.set_entity_report(&lamp, Some("brightness"), &json!([1, 300, 5]), T0)
        .unwrap();
    hub.set_entity_report(&temp, None, &json!([60, 21600, 10]), T0)
        .unwrap();
    let r = rows(&mut hub, &id(LIGHT));
    let reports: Vec<(J, J, J)> = r
        .iter()
        .filter(|x| x["uid"] == lamp)
        .map(|x| (x["part"].clone(), x["report"].clone(), x["source"].clone()))
        .collect();
    assert_eq!(
        reports,
        [
            (json!("on_off"), json!([0, 300]), json!("override")),
            (json!("brightness"), json!([1, 300, 5]), json!("override")),
        ]
    );
    // Global defaults: unknown classes and invalid settings are refused,
    // nothing applied.
    for d in [
        json!({"temperature": [60, 3600, 20], "nope": [1, 2]}),
        json!({"on_off": [0, 900, 1]}),
        json!({"humidity": [10, 5]}),
        json!(["temperature"]),
    ] {
        assert_eq!(
            hub.set_reporting_defaults(&d, T0).unwrap_err().code,
            "invalid_value",
            "{d}"
        );
    }
    assert!(hub.reporting_defaults().is_empty());
}

#[test]
fn precedence_override_definition_global_builtin() {
    let mut hub = hub_with(&["efekta-co2-smart-monitor"]);
    let mut sim = Sim::default();
    sim.devices.push(mains());
    sim.devices.push(efekta());
    sim.join(&mut hub, MAINS, T0);
    sim.join(&mut hub, EFEKTA, T0);
    let (m, e) = (id(MAINS), id(EFEKTA));
    let (mt, et) = (format!("{m}:1:temperature"), format!("{e}:1:temperature"));
    let r = rows(&mut hub, &m);
    assert_eq!(
        (
            row(&r, &mt)["report"].clone(),
            row(&r, &mt)["source"].clone(),
            row(&r, &mt)["class"].clone()
        ),
        (json!([30, 900, 50]), json!("default"), json!("temperature"))
    );
    let h_efekta = hash(&hub, EFEKTA);

    // A global default replaces the built-in one of generic entities; the
    // definition's generic.patch keeps its own.
    sim.log.clear();
    let rec = hub
        .set_reporting_defaults(&json!({"temperature": [60, 3600, 20]}), T0 + 1000)
        .unwrap();
    sim.run(&mut hub, T0 + 1000);
    let t = rec["classes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["key"] == "temperature")
        .unwrap()
        .clone();
    assert_eq!(
        (
            t["report"].clone(),
            t["builtin"].clone(),
            t["edited"].clone()
        ),
        (json!([60, 3600, 20]), json!([30, 900, 50]), json!(true))
    );
    assert_eq!(
        configure_frames(&sim),
        [(
            0x5001,
            0x0402,
            vec![0x00, 0x00, 0x00, 0x29, 60, 0, 0x10, 0x0E, 20, 0]
        )]
    );
    assert_eq!(hash(&hub, EFEKTA), h_efekta);
    let r = rows(&mut hub, &m);
    assert_eq!(row(&r, &mt)["report"], json!([60, 3600, 20]));
    assert_eq!(row(&r, &mt)["source"], "default");
    let r = rows(&mut hub, &e);
    assert_eq!(
        (
            row(&r, &et)["report"].clone(),
            row(&r, &et)["source"].clone()
        ),
        (json!([30, 1800, 10]), json!("definition"))
    );

    // An override beats the definition.
    sim.log.clear();
    hub.set_entity_report(&et, None, &json!([10, 600, 5]), T0 + 2000)
        .unwrap();
    sim.run(&mut hub, T0 + 2000);
    assert_eq!(
        configure_frames(&sim),
        [(
            0x4001,
            0x0402,
            vec![0x00, 0x00, 0x00, 0x29, 10, 0, 0x58, 0x02, 5, 0]
        )]
    );
    let r = rows(&mut hub, &e);
    let x = row(&r, &et);
    assert_eq!(
        (
            x["report"].clone(),
            x["source"].clone(),
            x["default"].clone()
        ),
        (
            json!([10, 600, 5]),
            json!("override"),
            json!([30, 1800, 10])
        )
    );

    // Global back to the built-in value (equal to it: no edit kept).
    sim.log.clear();
    hub.set_reporting_defaults(&json!({"temperature": [30, 900, 50]}), T0 + 3000)
        .unwrap();
    sim.run(&mut hub, T0 + 3000);
    assert!(hub.reporting_defaults().is_empty());
    assert_eq!(
        configure_frames(&sim),
        [(
            0x5001,
            0x0402,
            vec![0x00, 0x00, 0x00, 0x29, 30, 0, 0x84, 0x03, 50, 0]
        )]
    );
}

#[test]
fn sleepy_device_is_reconfigured_at_its_next_contact_with_the_battery_cap() {
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    sim.devices.push(sleepy());
    sim.join(&mut hub, SLEEPY, T0);
    let d = id(SLEEPY);
    let mut api = ApiServer::new("hub", "secret");
    let (s, _): (SessionId, _) = api.connect();
    api.handle_text(&mut hub, s, r#"{"type":"auth","token":"secret"}"#, T0);
    api.handle_text(
        &mut hub,
        s,
        r#"{"id":1,"type":"subscribe","topics":["entities"]}"#,
        T0,
    );
    // Asleep: nothing goes out yet.
    let t = T0 + 120_000;
    sim.log.clear();
    hub.set_entity_report(&format!("{d}:1:humidity"), None, &json!([60, 3600, 50]), t)
        .unwrap();
    // Valid globally; capped at 6 h for this device.
    hub.set_reporting_defaults(&json!({"battery_pct": [3600, 43200, 4]}), t)
        .unwrap();
    sim.run(&mut hub, t);
    assert!(sim.aps().is_empty());
    // The whole device: three clusters, three frames, one answer.
    let r = hub.entity_reporting(None, Some(&d), true, t).unwrap();
    assert_eq!(r["status"], "queued");
    assert_eq!(r["records"][0]["uid"], format!("{d}:1:battery"));
    assert_eq!(r["records"][0]["report"], json!([3600, 21600, 4]));
    api.pump(&mut hub);
    // The device reports: the queued work goes out.
    let s = sim.dev(SLEEPY).clone();
    hub.on_aps_indication(
        s.report(1, 0x0402, None, &[(0, TypedValue::int(ZclType::I16, 2200))]),
        t + 60_000,
    );
    sim.run(&mut hub, t + 60_000);
    let frames = configure_frames(&sim);
    assert!(
        frames.contains(&(
            0x1234,
            0x0405,
            vec![0x00, 0x00, 0x00, 0x21, 60, 0, 0x10, 0x0E, 50, 0]
        )),
        "{frames:?}"
    );
    // One frame per cluster: the edited percentage and the voltage.
    assert!(
        frames.contains(&(
            0x1234,
            0x0001,
            vec![
                0x00, 0x21, 0x00, 0x20, 0x10, 0x0E, 0x60, 0x54, 4, //
                0x00, 0x20, 0x00, 0x20, 0x10, 0x0E, 0x60, 0x54, 1,
            ]
        )),
        "{frames:?}"
    );
    // The queued readback was answered too, before the new configuration
    // went out (frames leave in order): one `entity_reporting` event with
    // the body `entities/reporting` returns.
    let ev: Vec<J> = api
        .pump(&mut hub)
        .iter()
        .map(|(_, t)| serde_json::from_str::<J>(t).unwrap()["event"].clone())
        .filter(|e| e["topic"] == "entity_reporting")
        .collect();
    assert_eq!(ev.len(), 1, "{ev:?}");
    let e = &ev[0];
    assert_eq!(
        (e["device"].clone(), e["uid"].clone(), e["status"].clone()),
        (json!(d), J::Null, json!("ok"))
    );
    let bat = row(e["records"].as_array().unwrap(), &format!("{d}:1:battery"));
    assert_eq!(
        (
            bat["applied"]["status"].clone(),
            bat["applied"]["report"].clone()
        ),
        (json!("SUCCESS"), json!([3600, 21600, 2]))
    );
    let r = hub
        .entity_reporting(None, Some(&d), false, t + 60_000)
        .unwrap();
    assert_eq!(r["records"], e["records"]);
    let hum = row(e["records"].as_array().unwrap(), &format!("{d}:1:humidity"));
    assert_eq!(hum["applied"]["report"], json!([60, 3600, 50]));
}

#[test]
fn superseded_config_sent_while_asleep_does_not_count_as_done() {
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    sim.devices.push(sleepy());
    sim.join(&mut hub, SLEEPY, T0);
    let uid = format!("{}:1:humidity", id(SLEEPY));
    let t = T0 + 120_000;
    // Set while asleep (the frame waits in the queue), then back to the
    // default before the device wakes up.
    hub.set_entity_report(&uid, None, &json!([60, 3600, 50]), t)
        .unwrap();
    hub.set_entity_report(&uid, None, &J::Null, t + 1000)
        .unwrap();
    sim.log.clear();
    let s = sim.dev(SLEEPY).clone();
    hub.on_aps_indication(
        s.report(1, 0x0402, None, &[(0, TypedValue::int(ZclType::I16, 2200))]),
        t + 60_000,
    );
    sim.run(&mut hub, t + 60_000);
    // The stale frame went out, then the default again: the device ends
    // with the default.
    let hum: Vec<Vec<u8>> = configure_frames(&sim)
        .into_iter()
        .filter(|f| f.1 == 0x0405)
        .map(|f| f.2)
        .collect();
    assert_eq!(
        hum,
        [
            vec![0x00, 0x00, 0x00, 0x21, 60, 0, 0x10, 0x0E, 50, 0],
            vec![0x00, 0x00, 0x00, 0x21, 30, 0, 0x84, 0x03, 100, 0],
        ]
    );
    let last = sim
        .dev(SLEEPY)
        .reporting
        .iter()
        .rfind(|r| r.1 == 0x0405)
        .unwrap()
        .2
        .clone();
    assert_eq!((last.min, last.max), (30, 900));
    assert!(hub.device(&id(SLEEPY)).unwrap().configure.stale.is_empty());
}

#[test]
fn overrides_persist_and_old_snapshots_load() {
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    sim.devices.push(light());
    sim.join(&mut hub, LIGHT, T0);
    let d = id(LIGHT);
    let lamp = format!("{d}:11:light");
    // Without overrides the snapshot has no new field (byte-identical to
    // earlier builds).
    let plain = hub.snapshot_device(&d).unwrap().to_bytes();
    assert!(!String::from_utf8_lossy(&plain).contains("\"reporting\""));
    hub.set_entity_report(&lamp, Some("brightness"), &json!([1, 300, 5]), T0)
        .unwrap();
    hub.set_entity_report(&lamp, Some("on_off"), &json!("off"), T0)
        .unwrap();
    sim.run(&mut hub, T0);
    let snap = hub.snapshot_device(&d).unwrap();
    let bytes = snap.to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.ends_with(
            r#","reporting":[{"ep":11,"cluster":6,"attr":0,"report":"off"},{"ep":11,"cluster":8,"attr":0,"report":[1,300,5]}]}"#
        ),
        "{text}"
    );
    let back = DeviceSnapshot::from_bytes(&bytes).unwrap();
    assert_eq!(back, snap);
    let mut restored = hub_with(&[]);
    restored.restore_device(back, None, T0 + 1000).unwrap();
    assert_eq!(
        restored.device(&d).unwrap().report_overrides,
        hub.device(&d).unwrap().report_overrides
    );
    assert_eq!(
        restored.device(&d).unwrap().configure.hash,
        hub.device(&d).unwrap().configure.hash
    );
    // The configure hash after a rebuild matches too: nothing to send.
    restored.transport.take();
    restored.redo_configure(&d, T0 + 2000);
    assert_eq!(
        restored.device(&d).unwrap().configure.hash,
        hub.device(&d).unwrap().configure.hash
    );
    assert!(restored.transport.take().is_empty());

    // A snapshot of an earlier build (no `reporting`) loads; one of this
    // build loads in an earlier one (serde ignores the unknown field).
    let mut v: J = serde_json::from_slice(&bytes).unwrap();
    v.as_object_mut().unwrap().remove("reporting");
    let old = DeviceSnapshot::from_bytes(&serde_json::to_vec(&v).unwrap()).unwrap();
    assert!(old.reporting.is_empty());
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct Earlier {
        v: u8,
        id: String,
        entities: Vec<crate::persist::EntitySnap>,
    }
    let e: Earlier = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(e.id, d);
}

#[test]
fn readback_answers_after_the_device_and_records_statuses() {
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    let mut dev = mains();
    dev.unreportable.insert((0x0405, 0x0000));
    sim.devices.push(dev);
    sim.join(&mut hub, MAINS, T0);
    let d = id(MAINS);
    let mut api = ApiServer::new("hub", "secret");
    let (s, _): (SessionId, _) = api.connect();
    api.handle_text(&mut hub, s, r#"{"type":"auth","token":"secret"}"#, T0);
    // Cached (never read): no `applied`.
    let out = api.handle_text(
        &mut hub,
        s,
        &json!({"id": 1, "type": "entities/reporting", "device": d}).to_string(),
        T0,
    );
    let r: J = serde_json::from_str(&out[0]).unwrap();
    assert_eq!(r["result"]["status"], "cached");
    assert!(r["result"]["records"][0].get("applied").is_none());
    // Read: the answer follows the device's.
    sim.log.clear();
    let out = api.handle_text(
        &mut hub,
        s,
        &json!({"id": 2, "type": "entities/reporting", "device": d, "read": true}).to_string(),
        T0 + 1000,
    );
    assert!(out.is_empty(), "{out:?}");
    sim.run(&mut hub, T0 + 1000);
    let reads: Vec<Vec<u8>> = sim
        .aps()
        .iter()
        .filter(|a| a.payload[2] == global::READ_REPORTING_CONFIG)
        .map(|a| a.payload[3..].to_vec())
        .collect();
    assert_eq!(reads, [vec![0, 0, 0], vec![0, 0, 0]]);
    let ev: Vec<J> = api
        .pump(&mut hub)
        .iter()
        .map(|(_, t)| serde_json::from_str(t).unwrap())
        .collect();
    let res = ev.iter().find(|e| e["id"] == 2).unwrap();
    assert_eq!(res["result"]["status"], "ok");
    let recs = res["result"]["records"].as_array().unwrap();
    let t = row(recs, &format!("{d}:1:temperature"));
    assert_eq!(
        (
            t["applied"]["status"].clone(),
            t["applied"]["report"].clone()
        ),
        (json!("SUCCESS"), json!([30, 900, 50]))
    );
    let h = row(recs, &format!("{d}:1:humidity"));
    assert_eq!(h["applied"]["status"], "UNREPORTABLE_ATTRIBUTE");
    assert!(h["applied"].get("report").is_none());

    // A device without the command answers a Default Response.
    hub.transport.take();
    let r = hub
        .entity_reporting(Some(&format!("{d}:1:temperature")), None, true, T0 + 2000)
        .unwrap();
    assert_eq!(r, J::Null);
    let ticket = hub.take_deferred().unwrap();
    let sent = hub.transport.take();
    let Some(Sent::Aps(req)) = sent.first() else {
        panic!("{sent:?}")
    };
    let tsn = req.payload[1];
    let dev = sim.dev(MAINS).clone();
    hub.on_aps_indication(
        dev.zcl(
            1,
            0x0402,
            FrameType::Global,
            None,
            tsn,
            global::DEFAULT_RSP,
            zf::default_rsp(global::READ_REPORTING_CONFIG, Status(0x82)),
        ),
        T0 + 2100,
    );
    let ans = hub
        .drain_events()
        .into_iter()
        .find_map(|e| match e {
            HubEvent::Answer { ticket: t, result } if t == ticket => Some(result),
            _ => None,
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        ans["records"][0]["applied"]["status"],
        "UNSUP_GENERAL_COMMAND"
    );
    // A timeout fails the request.
    hub.entity_reporting(Some(&format!("{d}:1:temperature")), None, true, T0 + 3000)
        .unwrap();
    let ticket = hub.take_deferred().unwrap();
    hub.transport.take();
    for t in [12_000, 21_000, 30_000, 39_000] {
        hub.tick(T0 + t);
        hub.transport.take();
    }
    let err = hub
        .drain_events()
        .into_iter()
        .find_map(|e| match e {
            HubEvent::Answer { ticket: t, result } if t == ticket => Some(result),
            _ => None,
        })
        .unwrap()
        .unwrap_err();
    assert_eq!(err.code, "timeout");
    // Green Power and unknown devices.
    assert_eq!(
        hub.entity_reporting(None, Some("zb:0000000000000001"), false, T0)
            .unwrap_err()
            .code,
        "not_found"
    );
    assert_eq!(
        hub.entity_reporting(None, None, false, T0)
            .unwrap_err()
            .code,
        "invalid_value"
    );
}

#[test]
fn builtin_classes_match_the_generic_tier() {
    use crate::fingerprint::{EndpointFp, Fingerprint};
    use crate::generic::{GenericInput, generic_entities};
    use crate::model::{Binding, PowerKind};
    use crate::reporting::{CLASSES, class_of};
    // Endpoint 1: everything but electrical measurement (metering power
    // comes from 0x0702 only without it); endpoint 2: electrical and a
    // cooling-only thermostat (setpoint 0x0011).
    let ep2 = [0x0B04, 0x0201];
    let all: Vec<(u16, u16)> = CLASSES
        .iter()
        .flat_map(|c| c.attrs.iter().map(|a| (a.0, a.1)))
        .collect();
    let mut c1: Vec<u16> = all.iter().map(|a| a.0).filter(|c| *c != 0x0B04).collect();
    c1.extend([0x0000, 0x0300]);
    c1.sort_unstable();
    c1.dedup();
    let fp = Fingerprint {
        endpoints: vec![
            EndpointFp {
                ep: 1,
                profile: 0x0104,
                device_id: 0x0102,
                in_clusters: c1,
                out_clusters: vec![],
            },
            EndpointFp {
                ep: 2,
                profile: 0x0104,
                device_id: 0x0301,
                in_clusters: ep2.to_vec(),
                out_clusters: vec![],
            },
        ],
        ..Default::default()
    };
    let mut attrs = crate::device::AttrCache::default();
    for &(c, a) in &all {
        for ep in [1, 2] {
            attrs.set(ep, c, a, None, Some(ZclValue::Uint(0)));
        }
    }
    // Colour capabilities: hue/saturation, xy, temperature.
    attrs.set(1, 0x0300, 0x400A, None, Some(ZclValue::Uint(0x19)));
    attrs.set(2, 0x0201, 0x001B, None, Some(ZclValue::Uint(0)));
    let ents = generic_entities(
        &fp,
        &GenericInput {
            attrs: &attrs,
            power: PowerKind::Battery,
        },
    );
    let mut seen = std::collections::BTreeSet::new();
    for e in &ents {
        for (_, v) in e.values() {
            let Binding::Zcl(z) = &v.binding else {
                continue;
            };
            let Some(c) = class_of(z.cluster, z.attr) else {
                continue;
            };
            let ty = c.attrs.iter().find(|a| a.1 == z.attr).unwrap().2;
            assert_eq!(ty, z.ty, "{} {:04x}/{:04x}", c.key, z.cluster, z.attr);
            assert_eq!(v.report, Some(c.builtin.spec()), "{}", c.key);
            seen.insert((z.cluster, z.attr));
        }
    }
    for (c, a) in all {
        assert!(
            seen.contains(&(c, a)),
            "{c:04x}/{a:04x} not in the generic tier"
        );
    }
}

#[test]
fn defaults_store_round_trip() {
    use crate::reporting::ReportDefaults;
    let mut d = ReportDefaults::default();
    assert!(
        d.update(&json!({"temperature": [60, 3600, 20], "battery_voltage": "off"}))
            .unwrap()
    );
    let b = d.to_bytes();
    assert_eq!(
        String::from_utf8(b.clone()).unwrap(),
        r#"{"battery_voltage":"off","temperature":[60,3600,20]}"#
    );
    let back = ReportDefaults::from_bytes(&b).unwrap();
    assert_eq!(back.record(), d.record());
    // A later build's class and a corrupt entry are skipped.
    let back = ReportDefaults::from_bytes(br#"{"radon":[1,2],"humidity":[9,1],"pressure":"off"}"#)
        .unwrap();
    assert_eq!(back.to_bytes(), br#"{"pressure":"off"}"#);
    assert!(ReportDefaults::from_bytes(b"[").is_err());
    // "default" and null remove an edit; unchanged is not a change.
    assert!(!d.update(&json!({"temperature": [60, 3600, 20]})).unwrap());
    assert!(
        d.update(&json!({"temperature": "default", "battery_voltage": null}))
            .unwrap()
    );
    assert!(d.is_empty());
}

#[test]
fn api_requests() {
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    sim.devices.push(mains());
    sim.join(&mut hub, MAINS, T0);
    let mut api = ApiServer::new("hub", "secret");
    let (s, _): (SessionId, _) = api.connect();
    api.handle_text(&mut hub, s, r#"{"type":"auth","token":"secret"}"#, T0);
    let uid = format!("{}:1:temperature", id(MAINS));
    let mut call = |hub: &mut Hub<MockTransport>, body: J| -> J {
        let out = api.handle_text(hub, s, &body.to_string(), T0 + 1000);
        assert_eq!(out.len(), 1, "{out:?}");
        serde_json::from_str(&out[0]).unwrap()
    };
    let r = call(
        &mut hub,
        json!({"id": 1, "type": "entities/update", "uid": uid, "report": [60, 1800, 25]}),
    );
    assert!(
        r["result"]["uid"] == uid && r["result"].get("report").is_none(),
        "{r}"
    );
    let src = |hub: &mut Hub<MockTransport>| {
        let r = hub.entity_reporting(Some(&uid), None, false, T0).unwrap();
        (
            r["records"][0]["report"].clone(),
            r["records"][0]["source"].clone(),
        )
    };
    assert_eq!(src(&mut hub), (json!([60, 1800, 25]), json!("override")));
    // Checked before anything changes: the name stays.
    let r = call(
        &mut hub,
        json!({"id": 2, "type": "entities/update", "uid": uid, "name": "T", "report": "bogus"}),
    );
    assert_eq!(r["error"]["code"], "invalid_value");
    assert_eq!(hub.entity(&uid).unwrap().1.user_name, None);
    // Name only: the override stays; null removes it.
    let r = call(
        &mut hub,
        json!({"id": 3, "type": "entities/update", "uid": uid, "name": "T"}),
    );
    assert_eq!(r["result"]["name"], "T");
    assert_eq!(src(&mut hub), (json!([60, 1800, 25]), json!("override")));
    let r = call(
        &mut hub,
        json!({"id": 4, "type": "entities/update", "uid": uid, "report": null}),
    );
    assert_eq!(r["result"]["uid"], uid);
    assert_eq!(src(&mut hub), (json!([30, 900, 50]), json!("default")));
    let r = call(&mut hub, json!({"id": 5, "type": "reporting/defaults/get"}));
    assert_eq!(
        r["result"]["classes"].as_array().unwrap().len(),
        crate::reporting::CLASSES.len()
    );
    let r = call(
        &mut hub,
        json!({"id": 6, "type": "reporting/defaults/set", "defaults": {"humidity": [60, 1800, 200]}}),
    );
    let h = r["result"]["classes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["key"] == "humidity")
        .unwrap()
        .clone();
    assert_eq!(
        (h["edited"].clone(), h["report"].clone()),
        (json!(true), json!([60, 1800, 200]))
    );
    let r = call(
        &mut hub,
        json!({"id": 7, "type": "reporting/defaults/set", "defaults": {"nope": [1, 2]}}),
    );
    assert_eq!(r["error"]["code"], "invalid_value");
    let r = call(&mut hub, json!({"id": 8, "type": "entities/reporting"}));
    assert_eq!(r["error"]["code"], "invalid_value");
}

// ---- Review regressions: what the device ends with matches the rows. ----

const DEF: u64 = 0x00124b00dddd0001;

/// A mains device matched by a definition `yaml` (entities and configure
/// ops below `model: DEF-1`).
fn with_definition(yaml: &str, dev: SimDevice) -> (Hub<MockTransport>, Sim) {
    use crate::model::LayerSource;
    use crate::resolve::MemoryCatalog;
    use crate::testutil::yaml::load_definition;
    let src = format!(
        "zdef: 1\nid: test/def-1\nrevision: 1\nvendor: Test\nmodel: DEF-1\nmatch:\n  - model: DEF-1\n{yaml}"
    );
    let mut hub = hub_with(&[]);
    let mut cat = MemoryCatalog::default();
    cat.insert(load_definition(&src, LayerSource::Catalog).unwrap());
    hub.set_catalog(Box::new(cat), T0);
    let mut sim = Sim::default();
    sim.devices.push(dev.basic("Test", "DEF-1", 0x01));
    sim.join(&mut hub, DEF, T0);
    sim.log.clear();
    (hub, sim)
}

fn def_device(clusters: &[u16]) -> SimDevice {
    SimDevice::new(DEF, 0x6001, LogicalType::Router, true, 0x1234)
        .endpoint(1, 0x0104, 0x0302, clusters, &[])
        .attr(1, 0x0402, 0x0000, TypedValue::int(ZclType::I16, 2137))
}

/// The last configuration the simulated device received for an attribute.
fn device_config(sim: &mut Sim, cluster: u16, attr: u16) -> Option<(u16, u16)> {
    sim.dev(DEF)
        .reporting
        .iter()
        .rfind(|r| r.1 == cluster && r.2.attr == attr)
        .map(|r| (r.2.min, r.2.max))
}

const TEMP_NO_REPORT: &str = "entities:
  - key: temperature
    platform: sensor
    zcl: {cluster: 0x0402, attr: 0x0000, type: i16}
    div: 100
";

#[test]
fn clearing_an_override_with_nothing_inherited_reverts_the_device_to_its_default() {
    for (name, yaml) in [
        ("no report", TEMP_NO_REPORT.to_string()),
        ("report: off", format!("{TEMP_NO_REPORT}    report: off\n")),
    ] {
        let (mut hub, mut sim) = with_definition(&yaml, def_device(&[0x0000, 0x0402]));
        let uid = format!("{}:1:temperature", id(DEF));
        let h0 = hash(&hub, DEF);
        hub.set_entity_report(&uid, None, &json!("off"), T0 + 1000)
            .unwrap();
        sim.run(&mut hub, T0 + 1000);
        assert_eq!(
            configure_frames(&sim),
            [(
                0x6001,
                0x0402,
                vec![0x00, 0x00, 0x00, 0x29, 0, 0, 0xFF, 0xFF, 0, 0]
            )],
            "{name}"
        );
        // Back: min 0xFFFF, max 0 (ZCL: revert to the device's default).
        sim.log.clear();
        hub.set_entity_report(&uid, None, &J::Null, T0 + 2000)
            .unwrap();
        sim.run(&mut hub, T0 + 2000);
        assert_eq!(
            configure_frames(&sim),
            [(
                0x6001,
                0x0402,
                vec![0x00, 0x00, 0x00, 0x29, 0xFF, 0xFF, 0, 0, 0, 0]
            )],
            "{name}"
        );
        assert_eq!(device_config(&mut sim, 0x0402, 0), Some((0xFFFF, 0)));
        let x = row(&rows(&mut hub, &id(DEF)), &uid);
        assert_eq!(
            (x["report"].clone(), x["source"].clone(), x.get("default")),
            (J::Null, json!("definition"), None),
            "{name}"
        );
        assert_ne!(hash(&hub, DEF), h0);
        // A reconfigure sends it again; a new override replaces it.
        sim.log.clear();
        hub.reconfigure(&id(DEF), T0 + 3000).unwrap();
        sim.run(&mut hub, T0 + 3000);
        assert!(
            configure_frames(&sim)
                .iter()
                .any(|f| f.2 == [0x00, 0x00, 0x00, 0x29, 0xFF, 0xFF, 0, 0, 0, 0]),
            "{name}"
        );
        hub.set_entity_report(&uid, None, &json!([60, 1800, 25]), T0 + 4000)
            .unwrap();
        sim.run(&mut hub, T0 + 4000);
        assert_eq!(device_config(&mut sim, 0x0402, 0), Some((60, 1800)));
    }
}

#[test]
fn definition_configure_report_ops_yield_to_overrides() {
    let yaml = format!(
        "{TEMP_NO_REPORT}configure:\n  - report: {{ep: 1, cluster: 0x0402, attr: 0x0000, type: i16, report: [10, 600, 10]}}\n"
    );
    let (mut hub, mut sim) = with_definition(&yaml, def_device(&[0x0000, 0x0402]));
    let uid = format!("{}:1:temperature", id(DEF));
    assert_eq!(device_config(&mut sim, 0x0402, 0), Some((10, 600)));
    // The row shows what the definition op sends.
    let x = row(&rows(&mut hub, &id(DEF)), &uid);
    assert_eq!(
        (x["report"].clone(), x["source"].clone()),
        (json!([10, 600, 10]), json!("definition"))
    );
    hub.set_entity_report(&uid, None, &json!([60, 1800, 25]), T0 + 1000)
        .unwrap();
    sim.run(&mut hub, T0 + 1000);
    assert_eq!(device_config(&mut sim, 0x0402, 0), Some((60, 1800)));
    // A reconfigure sends the override only: the device keeps it.
    sim.log.clear();
    hub.reconfigure(&id(DEF), T0 + 2000).unwrap();
    sim.run(&mut hub, T0 + 2000);
    let temp: Vec<Vec<u8>> = configure_frames(&sim)
        .into_iter()
        .filter(|f| f.1 == 0x0402)
        .map(|f| f.2)
        .collect();
    assert_eq!(
        temp,
        [vec![0x00, 0x00, 0x00, 0x29, 60, 0, 0x08, 0x07, 25, 0]]
    );
    assert_eq!(device_config(&mut sim, 0x0402, 0), Some((60, 1800)));
    let x = row(&rows(&mut hub, &id(DEF)), &uid);
    assert_eq!(
        (
            x["report"].clone(),
            x["source"].clone(),
            x["default"].clone()
        ),
        (
            json!([60, 1800, 25]),
            json!("override"),
            json!([10, 600, 10])
        )
    );
    // Cleared: the definition op goes out again.
    sim.log.clear();
    hub.set_entity_report(&uid, None, &J::Null, T0 + 3000)
        .unwrap();
    sim.run(&mut hub, T0 + 3000);
    assert_eq!(
        configure_frames(&sim),
        [(
            0x6001,
            0x0402,
            vec![0x00, 0x00, 0x00, 0x29, 10, 0, 0x58, 0x02, 10, 0]
        )]
    );
    assert_eq!(device_config(&mut sim, 0x0402, 0), Some((10, 600)));
    let x = row(&rows(&mut hub, &id(DEF)), &uid);
    assert_eq!(
        (x["report"].clone(), x["source"].clone()),
        (json!([10, 600, 10]), json!("definition"))
    );
}

#[test]
fn entities_sharing_an_attribute_share_its_override() {
    // Two entities decode one manufacturer attribute (Aqara style).
    let yaml = "entities:
  - key: occupancy
    platform: binary_sensor
    zcl: {cluster: 0xFCC0, attr: 0x0112, type: u32, mfr: 0x115F}
    report: [1, 3600, 1]
  - key: illuminance
    platform: sensor
    zcl: {cluster: 0xFCC0, attr: 0x0112, type: u32, mfr: 0x115F}
";
    let (mut hub, mut sim) = with_definition(yaml, def_device(&[0x0000, 0xFCC0]));
    let (occ, lux) = (
        format!("{}:1:occupancy", id(DEF)),
        format!("{}:1:illuminance", id(DEF)),
    );
    assert_eq!(device_config(&mut sim, 0xFCC0, 0x0112), Some((1, 3600)));
    let r = rows(&mut hub, &id(DEF));
    for (u, other) in [(&occ, &lux), (&lux, &occ)] {
        let x = row(&r, u);
        assert_eq!(
            (x["report"].clone(), x["shared"].clone()),
            (json!([1, 3600, 1]), json!([other]))
        );
    }
    // `off` on the second entity is the attribute's: sent, and both rows
    // show it.
    hub.set_entity_report(&lux, None, &json!("off"), T0 + 1000)
        .unwrap();
    sim.run(&mut hub, T0 + 1000);
    assert_eq!(device_config(&mut sim, 0xFCC0, 0x0112), Some((0, 0xFFFF)));
    let r = rows(&mut hub, &id(DEF));
    for u in [&occ, &lux] {
        let x = row(&r, u);
        assert_eq!(
            (x["report"].clone(), x["source"].clone()),
            (json!("off"), json!("override")),
            "{u}"
        );
    }
    // Cleared from the other entity: the definition's config again.
    hub.set_entity_report(&occ, None, &J::Null, T0 + 2000)
        .unwrap();
    sim.run(&mut hub, T0 + 2000);
    assert_eq!(device_config(&mut sim, 0xFCC0, 0x0112), Some((1, 3600)));
    let r = rows(&mut hub, &id(DEF));
    for u in [&occ, &lux] {
        assert_eq!(row(&r, u)["report"], json!([1, 3600, 1]), "{u}");
    }
}

#[test]
fn a_definition_op_with_report_off_is_not_inherited() {
    // The entity's record is what the attribute inherits; the `off` op
    // sends nothing.
    let yaml = format!(
        "{TEMP_NO_REPORT}    report: [30, 900, 50]\nconfigure:\n  - report: {{ep: 1, cluster: 0x0402, attr: 0x0000, type: i16, report: off}}\n"
    );
    let (mut hub, mut sim) = with_definition(&yaml, def_device(&[0x0000, 0x0402]));
    let uid = format!("{}:1:temperature", id(DEF));
    assert_eq!(device_config(&mut sim, 0x0402, 0), Some((30, 900)));
    hub.set_entity_report(&uid, None, &json!("off"), T0 + 1000)
        .unwrap();
    sim.run(&mut hub, T0 + 1000);
    let x = row(&rows(&mut hub, &id(DEF)), &uid);
    assert_eq!(
        (x["report"].clone(), x["default"].clone()),
        (json!("off"), json!([30, 900, 50]))
    );
    // Cleared: the entity's record again, no revert.
    sim.log.clear();
    hub.set_entity_report(&uid, None, &J::Null, T0 + 2000)
        .unwrap();
    sim.run(&mut hub, T0 + 2000);
    let builtin = vec![0x00, 0x00, 0x00, 0x29, 30, 0, 0x84, 0x03, 50, 0];
    assert_eq!(configure_frames(&sim), [(0x6001, 0x0402, builtin.clone())]);
    assert!(hub.device(&id(DEF)).unwrap().report_overrides.is_empty());
    let x = row(&rows(&mut hub, &id(DEF)), &uid);
    assert_eq!(
        (x["report"].clone(), x["source"].clone()),
        (json!([30, 900, 50]), json!("definition"))
    );
    sim.log.clear();
    hub.reconfigure(&id(DEF), T0 + 3000).unwrap();
    sim.run(&mut hub, T0 + 3000);
    let temp: Vec<Vec<u8>> = configure_frames(&sim)
        .into_iter()
        .filter(|f| f.1 == 0x0402)
        .map(|f| f.2)
        .collect();
    assert_eq!(temp, [builtin]);
}

#[test]
fn a_setting_superseded_twice_while_asleep_counts_when_set_again() {
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    sim.devices.push(sleepy());
    sim.join(&mut hub, SLEEPY, T0);
    let d = id(SLEEPY);
    let uid = format!("{d}:1:humidity");
    let t = T0 + 120_000;
    sim.log.clear();
    // A goes out while asleep (queued), then B and C replace it.
    for (i, v) in [[60, 3600, 50], [61, 3600, 50], [62, 3600, 50]]
        .iter()
        .enumerate()
    {
        hub.set_entity_report(&uid, None, &json!(v), t + i as u64)
            .unwrap();
    }
    let s = sim.dev(SLEEPY).clone();
    hub.on_aps_indication(
        s.report(1, 0x0402, None, &[(0, TypedValue::int(ZclType::I16, 2200))]),
        t + 60_000,
    );
    sim.run(&mut hub, t + 60_000);
    let hum = |sim: &Sim| -> Vec<u8> {
        configure_frames(sim)
            .into_iter()
            .filter(|f| f.1 == 0x0405)
            .map(|f| f.2[4])
            .collect()
    };
    assert_eq!(hum(&sim), [60, 62]);
    assert!(hub.device(&d).unwrap().configure.stale.is_empty());
    // A again (awake): its result is recorded, so a rebuild sends nothing.
    sim.log.clear();
    hub.set_entity_report(&uid, None, &json!([60, 3600, 50]), t + 61_000)
        .unwrap();
    sim.run(&mut hub, t + 61_000);
    assert_eq!(hum(&sim), [60]);
    sim.log.clear();
    hub.redo_configure(&d, t + 62_000);
    sim.run(&mut hub, t + 62_000);
    assert!(sim.aps().is_empty(), "{:?}", sim.aps());
    let dev = hub.device(&d).unwrap();
    assert!(dev.configure.done.values().all(|v| *v));
    assert!(dev.configure.stale.is_empty());
}
