//! Plain ZCL devices: a battery temperature/humidity sensor and a colour
//! light, generic tier only.

use serde_json::json;

use super::*;
use crate::device::InterviewStep;
use crate::testutil::sim::{Sim, SimDevice};
use crate::transport::Sent;
use crate::zcl::{TypedValue, ZclType, ZclValue};
use crate::zdo::LogicalType;

const SENSOR: u64 = 0x00158d0001a2b3c4;
const LIGHT: u64 = 0x0017880100aabbcc;

fn sensor() -> SimDevice {
    SimDevice::new(SENSOR, 0x1234, LogicalType::EndDevice, false, 0x1037)
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

/// The hub's records adopted after the engine made the device again (an
/// NCP import): the stored device replaces it in place, without a removal.
#[test]
fn restoring_a_device_the_engine_has_replaces_it_in_place() {
    let id = "zb:00158d0001a2b3c4";
    let mut sim = Sim::default();
    sim.devices.push(sensor());
    let mut hub = hub_with(&[]);
    sim.join(&mut hub, SENSOR, T0);
    hub.device_mut(id).unwrap().user_name = Some("Kitchen".into());
    let snap = hub.snapshot_device(id).unwrap();

    let mut other = hub_with(&[]);
    other.on_device_joined(SENSOR, 0x4321, T0);
    let entities = other.device(id).unwrap().entities.len();
    let _ = other.drain_events();
    other.restore_device(snap, None, T0 + 1000).unwrap();
    let events = other.drain_events();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, HubEvent::DeviceRemoved(_) | HubEvent::DeviceAdded(_))),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, HubEvent::DeviceUpdated(i) if i == id))
    );
    let d = other.device(id).unwrap();
    assert_eq!(d.user_name.as_deref(), Some("Kitchen"));
    assert_eq!(d.nwk(), Some(0x4321), "the current address");
    assert!(
        d.entities.len() > entities,
        "the interviewed device's entities"
    );
}

/// A hub that met `dev` again and restores its snapshot (interviewed
/// elsewhere) in the middle of the new interview (`answered`: the
/// interview's first request is still to be delivered); the stored
/// fingerprint.
fn replaced_mid_interview(
    dev: SimDevice,
    answered: bool,
) -> (Hub<MockTransport>, crate::fingerprint::Fingerprint) {
    let (ieee, nwk, id) = (dev.ieee, dev.nwk, format!("zb:{:016x}", dev.ieee));
    let mut sim = Sim::default();
    sim.devices.push(dev);
    let mut hub = hub_with(&[]);
    sim.join(&mut hub, ieee, T0);
    let snap = hub.snapshot_device(&id).unwrap();
    let mut other = hub_with(&[]);
    other.on_device_joined(ieee, nwk, T0);
    if !answered {
        other.transport.take();
    }
    other.restore_device(snap, None, T0 + 100).unwrap();
    (other, hub.device(&id).unwrap().fp.clone())
}

fn untouched(h: &Hub<MockTransport>, id: &str, fp: &crate::fingerprint::Fingerprint) {
    let d = h.device(id).unwrap();
    assert_eq!(d.interview, InterviewStep::Done);
    assert!(!d.interview_partial);
    assert_eq!(&d.fp, fp);
}

/// The interview a replaced device was in is over: a late answer does not
/// touch the restored device.
#[test]
fn a_late_interview_answer_does_not_touch_a_replaced_device() {
    let (mut other, fp) = replaced_mid_interview(sensor(), true);
    let mut late = Sim::default();
    late.devices.push(sensor());
    late.run(&mut other, T0 + 200);
    assert_eq!(
        late.aps().len(),
        1,
        "only the request sent before: {:?}",
        late.aps()
    );
    untouched(&other, "zb:00158d0001a2b3c4", &fp);
}

/// ... nor does its timeout (a mains device: retried, then the step
/// fails): no retry, no failed step.
#[test]
fn a_lost_interview_request_does_not_touch_a_replaced_device() {
    let (mut other, fp) = replaced_mid_interview(light(), false);
    for s in 1..=60 {
        other.tick(T0 + s * 1000);
    }
    assert!(other.transport.take().is_empty());
    untouched(&other, "zb:0017880100aabbcc", &fp);
}

#[test]
fn restored_mains_device_reads_unknown_states() {
    let ieee = 0x6055f90000f69688;
    let mut sim = Sim::default();
    sim.devices.push(
        SimDevice::new(ieee, 0x93B2, LogicalType::Router, true, 0x1037)
            .endpoint(1, 0x0104, 0x0302, &[0x0000, 0x0003, 0x0402, 0x0405], &[])
            .basic("Zigbee-RS", "ESP32-C6-Sensor", 0x01)
            .attr(1, 0x0402, 0x0000, TypedValue::int(ZclType::I16, 2904))
            .attr(1, 0x0405, 0x0000, TypedValue::uint(ZclType::U16, 5600)),
    );
    sim.devices.push(sensor());
    let mut hub = hub_with(&[]);
    sim.join(&mut hub, ieee, T0);
    sim.join(&mut hub, SENSOR, T0);
    let mut restored = hub_with(&[]);
    for id in ["zb:6055f90000f69688", "zb:00158d0001a2b3c4"] {
        let snap = hub.snapshot_device(id).unwrap();
        restored.restore_device(snap, None, T0 + 60_000).unwrap();
    }
    let uid = "zb:6055f90000f69688:1:temperature";
    assert_eq!(state(&restored, uid), J::Null);
    // Measured entities of both devices are read (battery, voltage,
    // temperature and humidity of the sleepy one); the sleepy device's
    // reads wait until it is next heard from.
    assert_eq!(restored.refresh_unknown(T0 + 60_000), 6);
    sim.log.clear();
    sim.run(&mut restored, T0 + 60_000);
    let reads: Vec<(u16, u16)> = sim
        .aps()
        .iter()
        .filter(|a| a.payload[2] == 0x00)
        .map(|a| {
            (
                a.cluster,
                match a.dst {
                    crate::transport::ApsDst::Nwk(n) => n,
                    _ => 0,
                },
            )
        })
        .collect();
    assert_eq!(reads, [(0x0402, 0x93B2), (0x0405, 0x93B2)]);
    assert_eq!(state(&restored, uid), json!(29.04));
    assert_eq!(
        state(&restored, "zb:6055f90000f69688:1:humidity"),
        json!(56.0)
    );
    // The sleepy sensor reports humidity: the queued reads go out.
    sim.log.clear();
    let s = sim.dev(SENSOR).clone();
    restored.on_aps_indication(
        s.report(
            1,
            0x0405,
            None,
            &[(0, TypedValue::uint(ZclType::U16, 4600))],
        ),
        T0 + 120_000,
    );
    sim.run(&mut restored, T0 + 120_000);
    assert_eq!(
        state(&restored, "zb:00158d0001a2b3c4:1:temperature"),
        json!(21.37)
    );
    // Known now: nothing left to read.
    assert_eq!(restored.refresh_unknown(T0 + 121_000), 0);
}

#[test]
fn last_values_come_back_marked_restored_until_a_report() {
    let mut sim = Sim::default();
    sim.devices.push(sensor());
    let mut hub = hub_with(&[]);
    sim.join(&mut hub, SENSOR, T0);
    let id = "zb:00158d0001a2b3c4";
    let uid = "zb:00158d0001a2b3c4:1:temperature";
    assert_eq!(state(&hub, uid), json!(21.37));
    // (The hub does not know the time here: no times are kept.)
    let kept = hub.last_values(id).unwrap();
    let mut restored = hub_with(&[]);
    restored
        .restore_device(hub.snapshot_device(id).unwrap(), None, T0 + 60_000)
        .unwrap();
    assert!(restored.restore_last_values(id, &kept, T0 + 60_000) >= 2);
    let (d, e) = restored.entity(uid).unwrap();
    let msg = e.state_msg(d.available);
    assert_eq!(
        (msg["state"].clone(), msg["attributes"]["restored"].clone()),
        (json!(21.37), json!(true))
    );
    assert_eq!(msg["last_reported"], J::Null);
    assert_eq!(d.lqi, hub.device(id).unwrap().lqi);
    // Restored values are read again like unknown ones; a report ends the mark.
    assert!(restored.refresh_unknown(T0 + 60_000) >= 2);
    let s = sim.dev(SENSOR).clone();
    restored.on_aps_indication(
        s.report(1, 0x0402, None, &[(0, TypedValue::int(ZclType::I16, 2200))]),
        T0 + 120_000,
    );
    let (d, e) = restored.entity(uid).unwrap();
    let msg = e.state_msg(d.available);
    assert_eq!(
        (msg["state"].clone(), msg["attributes"].get("restored")),
        (json!(22.0), None)
    );
}

/// A leave the coordinator reports: the device stays, unavailable with the
/// reason `left`, until its next frame or rejoin.
#[test]
fn a_device_that_left_is_unavailable_until_it_is_heard_again() {
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    sim.devices.push(sensor());
    sim.join(&mut hub, SENSOR, T0);
    let id = "zb:00158d0001a2b3c4";
    let uid = format!("{id}:1:temperature");
    let s = sim.dev(SENSOR).clone();
    assert_eq!(
        hub.device(id).unwrap().record().get("unavailable_reason"),
        None
    );
    hub.drain_events();

    hub.on_device_left(SENSOR);
    let d = hub.device(id).unwrap();
    assert!(!d.available);
    assert_eq!(d.record()["unavailable_reason"], "left");
    let ev = hub.drain_events();
    assert!(ev.contains(&HubEvent::DeviceUpdated(id.into())), "{ev:?}");
    assert_eq!(states(&ev, &uid).last().unwrap()["available"], false);
    hub.on_device_left(SENSOR);
    assert!(hub.drain_events().is_empty(), "reported once");
    // No probes while it is gone, whatever the time.
    hub.tick(T0 + 30 * 3_600_000);
    assert!(!hub.device(id).unwrap().available);

    // Its next frame: back.
    hub.on_aps_indication(
        s.report(1, 0x0402, None, &[(0, TypedValue::int(ZclType::I16, 2205))]),
        T0 + 31 * 3_600_000,
    );
    let d = hub.device(id).unwrap();
    assert!(d.available);
    assert_eq!(d.record().get("unavailable_reason"), None);
    assert!(
        hub.drain_events()
            .contains(&HubEvent::DeviceUpdated(id.into()))
    );

    // Left again, then a rejoin: back as well.
    hub.on_device_left(SENSOR);
    hub.drain_events();
    hub.on_device_joined(SENSOR, 0x1235, T0 + 32 * 3_600_000);
    let d = hub.device(id).unwrap();
    assert!(d.available && !d.left);
    assert_eq!(d.nwk(), Some(0x1235));
    assert!(
        hub.drain_events()
            .contains(&HubEvent::DeviceUpdated(id.into()))
    );

    // A device the hub does not know.
    hub.on_device_left(0x0011_2233_4455_6677);
    assert!(hub.drain_events().is_empty());
}

#[test]
fn last_seen_is_published_while_frames_come_more_than_once_a_minute() {
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    sim.devices.push(sensor());
    sim.join(&mut hub, SENSOR, T0);
    let uid = "zb:00158d0001a2b3c4:0:last_seen";
    let s = sim.dev(SENSOR).clone();
    let mut published = Vec::new();
    for i in 1..=10 {
        let now = T0 + i * 20_000;
        hub.on_aps_indication(
            s.report(
                1,
                0x0405,
                None,
                &[(0, TypedValue::uint(ZclType::U16, 4600))],
            ),
            now,
        );
        if state(&hub, uid) == json!(crate::device::ts(now)) {
            published.push(i);
        }
    }
    // Every 20 s for 200 s: published at least once a minute.
    assert!(published.len() >= 3, "{published:?}");
}

#[test]
fn temperature_sensor_interview_configure_and_reports() {
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    sim.devices.push(sensor());
    sim.join(&mut hub, SENSOR, T0);
    let id = "zb:00158d0001a2b3c4";
    let d = hub.device(id).unwrap();
    assert_eq!(d.fp.manufacturer_name.as_deref(), Some("ACME"));
    assert_eq!(d.fp.model_id.as_deref(), Some("TH-1"));
    assert!(d.sleepy());
    assert_eq!(d.interview, crate::device::InterviewStep::Done);
    assert_eq!(
        keys(&hub, id),
        [
            "1:battery",
            "1:battery_voltage",
            "1:temperature",
            "1:humidity",
            "1:identify",
            "0:linkquality",
            "0:rssi",
            "0:last_seen"
        ]
    );
    // Configure: binds to the hub, reporting with the 5.2 defaults (battery
    // max capped at 6 h), initial reads.
    let s = sim.dev(SENSOR);
    for c in [0x0402, 0x0405, 0x0001] {
        assert!(s.binds.contains(&(1, c)), "bind {c:04x}");
    }
    let rep = |c: u16, a: u16| {
        s.reporting
            .iter()
            .find(|(_, cc, r)| *cc == c && r.attr == a)
            .map(|(_, _, r)| (r.min, r.max, r.change))
    };
    assert_eq!(rep(0x0402, 0), Some((30, 900, Some(50.0))));
    assert_eq!(rep(0x0405, 0), Some((30, 900, Some(100.0))));
    assert_eq!(rep(0x0001, 0x0021), Some((3600, 21600, Some(2.0))));
    assert_eq!(state(&hub, &format!("{id}:1:temperature")), json!(21.37));
    assert_eq!(state(&hub, &format!("{id}:1:humidity")), json!(45.1));
    assert_eq!(state(&hub, &format!("{id}:1:battery")), json!(90.0));
    assert_eq!(state(&hub, &format!("{id}:1:battery_voltage")), json!(3.0));
    // Entity record maps 1:1 to HA.
    let (_, e) = hub.entity(&format!("{id}:1:temperature")).unwrap();
    let r = e.record(id);
    assert_eq!(r["platform"], "sensor");
    assert_eq!(r["device_class"], "temperature");
    assert_eq!(r["unit"], "°C");
    assert_eq!(r["state_class"], "measurement");
    assert_eq!(r["source"], "generic");
    assert_eq!(r["capabilities"]["suggested_display_precision"], 2);
    let (_, e) = hub.entity(&format!("{id}:1:battery_voltage")).unwrap();
    assert_eq!(e.record(id)["enabled_default"], false);
    assert_eq!(e.record(id)["category"], "diagnostic");
    hub.drain_events();

    // Report: 22.05 °C, then an invalid value, then the same value twice.
    let s = sim.dev(SENSOR).clone();
    let uid = format!("{id}:1:temperature");
    hub.on_aps_indication(
        s.report(1, 0x0402, None, &[(0, TypedValue::int(ZclType::I16, 2205))]),
        T0 + 60_000,
    );
    assert_eq!(state(&hub, &uid), json!(22.05));
    hub.on_aps_indication(
        s.report(
            1,
            0x0402,
            None,
            &[(0, TypedValue::int(ZclType::I16, -32768))],
        ),
        T0 + 120_000,
    );
    assert_eq!(state(&hub, &uid), J::Null);
    hub.on_aps_indication(
        s.report(1, 0x0402, None, &[(0, TypedValue::int(ZclType::I16, 1999))]),
        T0 + 180_000,
    );
    hub.on_aps_indication(
        s.report(1, 0x0402, None, &[(0, TypedValue::int(ZclType::I16, 1999))]),
        T0 + 240_000,
    );
    let ev = hub.drain_events();
    let st = states(&ev, &uid);
    assert_eq!(st.len(), 4, "every report publishes a state (4.5)");
    let last = st.last().unwrap();
    assert_eq!(last["state"], json!(19.99));
    assert_eq!(last["last_reported"], json!((T0 + 240_000) as f64 / 1000.0));
    assert_eq!(last["last_changed"], json!((T0 + 180_000) as f64 / 1000.0));
    // Reports from a sleepy device trigger a Default Response only if asked.
    assert!(hub.transport.take().is_empty());

    // Battery availability: 25 h without any frame.
    hub.tick(T0 + 240_000 + 24 * 3_600_000);
    assert!(hub.device(id).unwrap().available);
    hub.tick(T0 + 240_000 + 25 * 3_600_000 + 1);
    assert!(!hub.device(id).unwrap().available);
    let ev = hub.drain_events();
    assert_eq!(states(&ev, &uid).last().unwrap()["available"], false);
    hub.on_aps_indication(
        s.report(
            1,
            0x0405,
            None,
            &[(0, TypedValue::uint(ZclType::U16, 5000))],
        ),
        T0 + 26 * 3_600_000,
    );
    assert!(hub.device(id).unwrap().available);
}

fn light() -> SimDevice {
    SimDevice::new(LIGHT, 0x2001, LogicalType::Router, true, 0x100B)
        .endpoint(
            11,
            0x0104,
            0x010D,
            &[0x0000, 0x0003, 0x0004, 0x0005, 0x0006, 0x0008, 0x0300],
            &[0x0019],
        )
        .basic("ACME", "Bulb E27", 0x01)
        .attr(
            11,
            0x0006,
            0x0000,
            TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false))),
        )
        .attr(11, 0x0006, 0x4003, TypedValue::uint(ZclType::ENUM8, 255))
        .attr(11, 0x0008, 0x0000, TypedValue::uint(ZclType::U8, 128))
        .attr(11, 0x0300, 0x0007, TypedValue::uint(ZclType::U16, 370))
        .attr(11, 0x0300, 0x0003, TypedValue::uint(ZclType::U16, 0x5000))
        .attr(11, 0x0300, 0x0004, TypedValue::uint(ZclType::U16, 0x5500))
        .attr(11, 0x0300, 0x0008, TypedValue::uint(ZclType::ENUM8, 2))
        .attr(11, 0x0300, 0x400A, TypedValue::uint(ZclType::MAP16, 0x19))
        .attr(11, 0x0300, 0x400B, TypedValue::uint(ZclType::U16, 153))
        .attr(11, 0x0300, 0x400C, TypedValue::uint(ZclType::U16, 454))
}

#[test]
fn on_off_light_entities_commands_and_mains_availability() {
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    sim.devices.push(light());
    sim.join(&mut hub, LIGHT, T0);
    let id = "zb:0017880100aabbcc";
    assert_eq!(
        keys(&hub, id),
        [
            "11:light",
            "11:power_on_behavior",
            "11:identify",
            "0:linkquality",
            "0:rssi",
            "0:last_seen"
        ]
    );
    let uid = format!("{id}:11:light");
    let (_, e) = hub.entity(&uid).unwrap();
    let rec = e.record(id);
    assert_eq!(
        rec["capabilities"]["supported_color_modes"],
        json!(["color_temp", "xy", "hs"])
    );
    assert_eq!(rec["capabilities"]["min_color_temp_kelvin"], 2203);
    assert_eq!(rec["capabilities"]["max_color_temp_kelvin"], 6536);
    let st = hub.state_of(&uid).unwrap();
    assert_eq!(st["state"], "off");
    assert_eq!(st["attributes"]["brightness"], 128);
    assert_eq!(st["attributes"]["color_temp_kelvin"], 2703);
    assert_eq!(st["attributes"]["color_mode"], "color_temp");
    assert_eq!(st["attributes"]["xy_color"], json!([0.3125, 0.332]));
    assert_eq!(
        state(&hub, &format!("{id}:11:power_on_behavior")),
        "previous"
    );
    let s = sim.dev(LIGHT);
    let rep: Vec<_> = s
        .reporting
        .iter()
        .map(|(_, c, r)| (*c, r.attr, r.min, r.max))
        .collect();
    assert!(rep.contains(&(0x0006, 0, 0, 900)));
    assert!(rep.contains(&(0x0008, 0, 1, 900)));
    assert!(rep.contains(&(0x0300, 0x0007, 1, 900)));
    hub.drain_events();
    sim.log.clear();

    // turn_on with brightness and transition → Move to Level with On/Off.
    let ack = hub
        .entity_command(
            &uid,
            "turn_on",
            &json!({"brightness": 200, "transition": 1.0}),
            T0 + 1000,
        )
        .unwrap();
    assert_eq!(ack.status, "pending");
    sim.run(&mut hub, T0 + 1000);
    let aps = sim.aps();
    assert_eq!(aps.len(), 1);
    assert_eq!(aps[0].cluster, 0x0008);
    assert_eq!(
        &aps[0].payload[..],
        &[0x01, aps[0].payload[1], 0x04, 200, 10, 0]
    );
    let ev = hub.drain_events();
    assert_eq!(result_of(&ev, ack.ticket), Some(Ok("ok".into())));
    let st = hub.state_of(&uid).unwrap();
    assert_eq!(st["state"], "on");
    assert_eq!(st["attributes"]["brightness"], 200);

    // Colour temperature in kelvin → Move to Color Temperature in mireds.
    sim.log.clear();
    let ack = hub
        .entity_command(
            &uid,
            "turn_on",
            &json!({"color_temp_kelvin": 2700}),
            T0 + 2000,
        )
        .unwrap();
    sim.run(&mut hub, T0 + 2000);
    let aps = sim.aps();
    let ct = aps.iter().find(|a| a.cluster == 0x0300).unwrap();
    assert_eq!(&ct.payload[2..], &[0x0A, 0x72, 0x01, 0, 0]);
    assert_eq!(
        result_of(&hub.drain_events(), ack.ticket),
        Some(Ok("ok".into()))
    );
    // A failing device answer surfaces the ZCL status.
    sim.dev(LIGHT).cmd_status = crate::zcl::Status::UNSUP_COMMAND;
    let ack = hub
        .entity_command(&uid, "turn_off", &json!({}), T0 + 3000)
        .unwrap();
    sim.run(&mut hub, T0 + 3000);
    let r = result_of(&hub.drain_events(), ack.ticket)
        .unwrap()
        .unwrap_err();
    assert_eq!(r.code, "zcl_status");
    assert_eq!(r.zcl_status.as_deref(), Some("UNSUP_COMMAND"));
    assert!(
        hub.entity_command(&uid, "open_cover", &json!({}), T0)
            .is_err()
    );

    // Mains availability: 10 min silence, then 3 failed checks over 30 s.
    sim.dev(LIGHT).responsive = false;
    let t = T0 + 3000 + 10 * 60_000 + 1;
    hub.tick(t);
    let probes = |h: &mut Hub<MockTransport>| {
        h.transport
            .take()
            .iter()
            .filter(|s| matches!(s, Sent::Aps(a) if a.cluster == 0 && a.payload[2] == 0x00))
            .count()
    };
    assert_eq!(probes(&mut hub), 1);
    hub.tick(t + 10_001);
    hub.tick(t + 20_002);
    assert_eq!(probes(&mut hub), 2);
    assert!(hub.device(id).unwrap().available);
    hub.tick(t + 30_003);
    assert!(!hub.device(id).unwrap().available);
    // Any frame makes it available again.
    let s = sim.dev(LIGHT).clone();
    hub.on_aps_indication(
        s.report(
            11,
            0x0006,
            None,
            &[(
                0,
                TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(true))),
            )],
        ),
        t + 40_000,
    );
    assert!(hub.device(id).unwrap().available);
    assert_eq!(state(&hub, &uid), "on");
}

#[test]
fn reinterview_keeps_uids_and_orphans_vanished_entities() {
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    sim.devices.push(sensor());
    sim.join(&mut hub, SENSOR, T0);
    let id = "zb:00158d0001a2b3c4";
    let before = keys(&hub, id);
    // A firmware update drops the humidity cluster.
    sim.dev(SENSOR).endpoints[0]
        .in_clusters
        .retain(|c| *c != 0x0405);
    hub.reinterview(id, T0 + 1000).unwrap();
    sim.run(&mut hub, T0 + 1000);
    let after = keys(&hub, id);
    assert!(!after.contains(&"1:humidity".to_string()));
    assert_eq!(
        before.iter().filter(|k| !k.ends_with("humidity")).count(),
        after.len()
    );
    let d = hub.device(id).unwrap();
    let orphan = d
        .entities
        .iter()
        .find(|e| e.r.def.key == "humidity")
        .unwrap();
    assert!(orphan.orphaned);
    assert_eq!(
        hub.state_of(&orphan.uid.clone()).unwrap()["available"],
        false
    );
    let ev = hub.drain_events();
    assert!(
        ev.iter().any(
            |e| matches!(e, HubEvent::ResolutionChanged { orphaned, .. } if orphaned.len() == 1)
        )
    );
    // The temperature entity kept its uid and state.
    assert_eq!(state(&hub, &format!("{id}:1:temperature")), json!(21.37));
    hub.delete_entity(&format!("{id}:1:humidity")).unwrap();
    assert!(hub.delete_entity(&format!("{id}:1:temperature")).is_err());
}

/// The device announces itself (Device_annce through the NCP): awake now.
fn announce(hub: &mut Hub<MockTransport>, sim: &mut Sim, ieee: u64, now: u64) {
    let s = sim.dev(ieee).clone();
    let cap = s.node.mac_capabilities;
    hub.on_aps_indication(
        s.ind(0, 0, 0x0013, crate::zdo::device_annce(1, s.nwk, ieee, cap)),
        now,
    );
    sim.run(hub, now);
}

/// ZDO requests of `cluster` the hub sent, and the log cleared.
fn zdo_sent(sim: &mut Sim, cluster: u16) -> usize {
    let n = sim
        .aps()
        .iter()
        .filter(|a| a.profile == 0 && a.cluster == cluster)
        .count();
    sim.log.clear();
    n
}

/// A sleepy sensor whose Simple descriptor failed at the join: its next
/// Device_annce (the moment it is awake) resumes the interview where it
/// failed, what was learnt stays, and the device gets its entities.
#[test]
fn a_partial_interview_resumes_when_the_device_announces_itself() {
    use crate::zdo::cluster::{ACTIVE_EP_REQ, NODE_DESC_REQ, SIMPLE_DESC_REQ};
    let id = "zb:00158d0001a2b3c4";
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    let mut s = sensor();
    s.zdo_failing.insert(SIMPLE_DESC_REQ);
    sim.devices.push(s);
    sim.join(&mut hub, SENSOR, T0);
    let d = hub.device(id).unwrap();
    assert_eq!(d.record()["interview"], "partial");
    assert!(d.entity(1, "temperature").is_none());

    // Asleep an hour, then it rejoins (its descriptor answers now).
    hub.device_mut(id).unwrap().user_name = Some("Bedroom".into());
    sim.dev(SENSOR).zdo_failing.clear();
    sim.log.clear();
    let t = T0 + 3_600_000;
    announce(&mut hub, &mut sim, SENSOR, t);
    let sent: Vec<u16> = sim.aps().iter().map(|a| a.cluster).collect();
    assert!(sent.contains(&SIMPLE_DESC_REQ), "{sent:04x?}");
    assert!(
        !sent.contains(&NODE_DESC_REQ) && !sent.contains(&ACTIVE_EP_REQ),
        "resumed at the Simple descriptor: {sent:04x?}"
    );
    let d = hub.device(id).unwrap();
    assert_eq!(d.record()["interview"], "complete");
    assert!(d.entity(1, "temperature").is_some());
    assert_eq!(hub.diagnostics(id).unwrap()["interview_resumes"], 1);
    assert_eq!(
        hub.device(id).unwrap().user_name.as_deref(),
        Some("Bedroom")
    );
}

/// The likeliest partial interview: the Node descriptor was lost right
/// after the join, the endpoints and their entities are fine. Resumed after
/// a rejoin where a Simple descriptor request would fail, it asks only the
/// Node descriptor: the endpoints and entities stay.
#[test]
fn a_resumed_interview_asks_only_what_the_device_lacks() {
    use crate::zdo::cluster::{ACTIVE_EP_REQ, NODE_DESC_REQ, SIMPLE_DESC_REQ};
    let id = "zb:0017880100aabbcc";
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    let mut l = light();
    l.zdo_failing.insert(NODE_DESC_REQ);
    sim.devices.push(l);
    sim.join(&mut hub, LIGHT, T0);
    let d = hub.device(id).unwrap();
    assert!(d.interview_partial && d.entity(11, "light").is_some());
    let eps = d.fp.endpoints.clone();

    sim.dev(LIGHT).zdo_failing = [SIMPLE_DESC_REQ].into();
    sim.log.clear();
    announce(&mut hub, &mut sim, LIGHT, T0 + 60_000);
    let d = hub.device(id).unwrap();
    assert_eq!(d.fp.endpoints, eps);
    assert!(d.entity(11, "light").is_some());
    assert!(d.node_desc.is_some());
    assert_eq!(d.record()["interview"], "complete");
    let zdo: Vec<u16> = sim
        .aps()
        .iter()
        .filter(|a| a.profile == 0)
        .map(|a| a.cluster)
        .collect();
    assert!(zdo.contains(&NODE_DESC_REQ), "{zdo:04x?}");
    assert!(
        !zdo.contains(&ACTIVE_EP_REQ) && !zdo.contains(&SIMPLE_DESC_REQ),
        "{zdo:04x?}"
    );
}

/// A resumption asks only the Simple descriptor the device lacks and
/// stores it where a full interview would (the endpoints' order).
#[test]
fn a_resumed_interview_stores_a_descriptor_in_the_interview_order() {
    use crate::zdo::cluster::SIMPLE_DESC_REQ;
    let id = "zb:0017880100aabbcc";
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    sim.devices
        .push(light().endpoint(12, 0x0104, 0x0100, &[0x0000, 0x0006], &[]));
    sim.join(&mut hub, LIGHT, T0);
    let eps = hub.device(id).unwrap().fp.endpoints.clone();
    assert_eq!(eps.iter().map(|e| e.ep).collect::<Vec<_>>(), [11, 12]);
    // Endpoint 11's descriptor failed.
    let d = hub.device_mut(id).unwrap();
    d.fp.endpoints.remove(0);
    d.interview_partial = true;

    sim.log.clear();
    announce(&mut hub, &mut sim, LIGHT, T0 + 60_000);
    let asked: Vec<u8> = sim
        .aps()
        .iter()
        .filter(|a| a.profile == 0 && a.cluster == SIMPLE_DESC_REQ)
        .map(|a| a.payload[3])
        .collect();
    assert_eq!(asked, [11]);
    let d = hub.device(id).unwrap();
    assert_eq!(d.fp.endpoints, eps);
    assert_eq!(d.record()["interview"], "complete");
}

/// An Active_EP_rsp with an error status fails the step (it is not an
/// empty endpoint list): partial, and resumed there.
#[test]
fn a_failed_active_endpoints_answer_makes_the_interview_partial() {
    use crate::zdo::cluster::{ACTIVE_EP_REQ, NODE_DESC_REQ};
    let id = "zb:0017880100aabbcc";
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    let mut l = light();
    l.zdo_failing.insert(ACTIVE_EP_REQ);
    sim.devices.push(l);
    sim.join(&mut hub, LIGHT, T0);
    assert_eq!(hub.device(id).unwrap().record()["interview"], "partial");
    sim.dev(LIGHT).zdo_failing.clear();
    sim.log.clear();
    announce(&mut hub, &mut sim, LIGHT, T0 + 60_000);
    assert_eq!(zdo_sent(&mut sim, NODE_DESC_REQ), 0);
    let d = hub.device(id).unwrap();
    assert_eq!(d.record()["interview"], "complete");
    assert!(d.entity(11, "light").is_some());
}

/// A rejoin of a device whose interview completed interviews nothing.
#[test]
fn a_complete_interview_is_not_resumed_on_a_rejoin() {
    use crate::zdo::cluster::{ACTIVE_EP_REQ, NODE_DESC_REQ, SIMPLE_DESC_REQ};
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    sim.devices.push(light());
    sim.join(&mut hub, LIGHT, T0);
    sim.log.clear();
    let t = T0 + 3_600_000;
    hub.on_device_joined(LIGHT, 0x2001, t);
    announce(&mut hub, &mut sim, LIGHT, t);
    let zdo: Vec<u16> = sim
        .aps()
        .iter()
        .filter(|a| a.profile == 0)
        .map(|a| a.cluster)
        .collect();
    assert!(
        !zdo.iter()
            .any(|c| [NODE_DESC_REQ, ACTIVE_EP_REQ, SIMPLE_DESC_REQ].contains(c)),
        "{zdo:04x?}"
    );
    let d = hub.device("zb:0017880100aabbcc").unwrap();
    assert_eq!(d.record()["interview"], "complete");
    assert_eq!(d.interview_resumes, 0);
}

/// A router that keeps failing a step and keeps announcing itself: one
/// resumption for the join and the Device_annce of one rejoin, then at
/// most one per 10, 20, 40, 80 min, five per run; a requested
/// re-interview starts over.
#[test]
fn resumptions_back_off_however_often_the_device_announces() {
    use crate::zdo::cluster::NODE_DESC_REQ;
    let id = "zb:0017880100aabbcc";
    let min = 60_000;
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    let mut l = light();
    l.zdo_failing.insert(NODE_DESC_REQ);
    sim.devices.push(l);
    sim.join(&mut hub, LIGHT, T0);
    assert!(hub.device(id).unwrap().interview_partial);
    sim.log.clear();

    // A rejoin: the trust center's join, then its Device_annce.
    let mut t = T0 + min;
    hub.on_device_joined(LIGHT, 0x2001, t);
    announce(&mut hub, &mut sim, LIGHT, t);
    assert_eq!(zdo_sent(&mut sim, NODE_DESC_REQ), 1, "one interview");
    let d = hub.device(id).unwrap();
    assert!(d.interview_partial && d.interview == InterviewStep::Done);
    for (wait, gap) in [(10, 5), (20, 15), (40, 30), (80, 60)] {
        // Within the interval: nothing, however often.
        for _ in 0..3 {
            announce(&mut hub, &mut sim, LIGHT, t + gap * min);
        }
        assert_eq!(zdo_sent(&mut sim, NODE_DESC_REQ), 0, "before {wait} min");
        t += wait * min;
        announce(&mut hub, &mut sim, LIGHT, t);
        assert_eq!(zdo_sent(&mut sim, NODE_DESC_REQ), 1, "after {wait} min");
    }
    assert_eq!(hub.device(id).unwrap().interview_resumes, 5);
    announce(&mut hub, &mut sim, LIGHT, t + 24 * 60 * min);
    assert_eq!(zdo_sent(&mut sim, NODE_DESC_REQ), 0, "five per run");

    t += 25 * 60 * min;
    hub.reinterview(id, t).unwrap();
    sim.run(&mut hub, t);
    sim.log.clear();
    announce(&mut hub, &mut sim, LIGHT, t + min);
    assert_eq!(zdo_sent(&mut sim, NODE_DESC_REQ), 1, "started over");
}

#[test]
fn ias_contact_sensor_enrollment_and_notifications() {
    let ieee = 0x00158d00aaaa0001;
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    sim.devices.push(
        SimDevice::new(ieee, 0x1301, LogicalType::EndDevice, false, 0x1234)
            .endpoint(1, 0x0104, 0x0402, &[0x0000, 0x0001, 0x0500], &[])
            .basic("ACME", "Door-1", 0x03)
            .attr(1, 0x0500, 0x0000, TypedValue::uint(ZclType::ENUM8, 0))
            .attr(1, 0x0500, 0x0001, TypedValue::uint(ZclType::ENUM16, 0x0015))
            .attr(1, 0x0500, 0x0002, TypedValue::uint(ZclType::MAP16, 0)),
    );
    sim.join(&mut hub, ieee, T0);
    let id = "zb:00158d00aaaa0001";
    let s = sim.dev(ieee).clone();
    // CIE address = coordinator IEEE.
    let cie = s
        .writes
        .iter()
        .find(|w| (w.1, w.2) == (0x0500, 0x0010))
        .expect("IAS_CIE_Address write");
    assert_eq!(
        cie.4,
        TypedValue::uint(
            ZclType::IEEE,
            crate::hub::HubConfig::default().coordinator_ieee
        )
    );
    let k = keys(&hub, id);
    assert!(
        k.contains(&"1:contact".to_string())
            && k.contains(&"1:tamper".to_string())
            && k.contains(&"1:battery_low".to_string())
    );
    let (_, e) = hub.entity(&format!("{id}:1:contact")).unwrap();
    assert_eq!(e.r.def.device_class.as_deref(), Some("door"));
    assert_eq!(state(&hub, &format!("{id}:1:contact")), false);
    // Zone Enroll Request → Zone Enroll Response (success).
    hub.transport.take();
    hub.on_aps_indication(
        s.zcl(
            1,
            0x0500,
            crate::zcl::FrameType::Cluster,
            None,
            0x30,
            0x01,
            vec![0x15, 0x00, 0x34, 0x12],
        ),
        T0 + 1000,
    );
    let a = hub
        .transport
        .aps()
        .into_iter()
        .find(|a| a.cluster == 0x0500)
        .cloned()
        .unwrap();
    assert_eq!(&a.payload[2..4], &[0x00, 0x00]);
    // Zone Status Change Notification: alarm1 + tamper.
    hub.on_aps_indication(
        s.zcl(
            1,
            0x0500,
            crate::zcl::FrameType::Cluster,
            None,
            0x31,
            0x00,
            vec![0x05, 0x00, 0x00, 0x01, 0x00, 0x00],
        ),
        T0 + 2000,
    );
    assert_eq!(state(&hub, &format!("{id}:1:contact")), true);
    assert_eq!(state(&hub, &format!("{id}:1:tamper")), true);
    assert_eq!(state(&hub, &format!("{id}:1:battery_low")), false);
}

/// `pushed` (local automations' `reported`): what the device sent on its
/// own, never a read response or a value the hub set; an IAS Zone bit only
/// when set and either just set or alone in changing nothing.
#[test]
fn pushed_states_are_what_the_device_sent_on_its_own() {
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    sim.devices.push(light());
    sim.join(&mut hub, LIGHT, T0);
    let uid = "zb:0017880100aabbcc:11:light";
    let pushed = |ev: &[HubEvent]| -> Vec<bool> {
        states(ev, uid)
            .iter()
            .map(|s| s["pushed"] == true)
            .collect()
    };
    hub.drain_events();
    hub.entity_command(uid, "turn_on", &json!({}), T0 + 1000)
        .unwrap();
    sim.run(&mut hub, T0 + 1000);
    let p = pushed(&hub.drain_events());
    assert!(
        !p.is_empty() && !p.contains(&true),
        "a command's completion: {p:?}"
    );
    let s = sim.dev(LIGHT).clone();
    let on = [(
        0,
        TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(true))),
    )];
    hub.on_aps_indication(s.report(11, 0x0006, None, &on), T0 + 2000);
    assert_eq!(pushed(&hub.drain_events()), [true], "a report");
    let read_rsp = vec![0x00, 0x00, 0x00, 0x10, 0x01];
    hub.on_aps_indication(
        s.zcl(
            11,
            0x0006,
            crate::zcl::FrameType::Global,
            None,
            0x40,
            0x01,
            read_rsp,
        ),
        T0 + 3000,
    );
    assert_eq!(pushed(&hub.drain_events()), [false], "a read response");

    // An SOS button: IAS Zone type 0x002C (alarm, tamper, battery low).
    let ieee = 0x00158d00aaaa0002;
    sim.devices.push(
        SimDevice::new(ieee, 0x1302, LogicalType::EndDevice, false, 0x1234)
            .endpoint(1, 0x0104, 0x0402, &[0x0000, 0x0001, 0x0500], &[])
            .basic("ACME", "SOS-1", 0x03)
            .attr(1, 0x0500, 0x0000, TypedValue::uint(ZclType::ENUM8, 0))
            .attr(1, 0x0500, 0x0001, TypedValue::uint(ZclType::ENUM16, 0x002C))
            .attr(1, 0x0500, 0x0002, TypedValue::uint(ZclType::MAP16, 0)),
    );
    sim.join(&mut hub, ieee, T0 + 10_000);
    hub.drain_events();
    let s = sim.dev(ieee).clone();
    let mut notify = |tsn: u8, zs: u16, t: u64| -> Vec<&str> {
        let p = vec![zs as u8, (zs >> 8) as u8, 0, 0, 0, 0];
        let f = s.zcl(
            1,
            0x0500,
            crate::zcl::FrameType::Cluster,
            None,
            tsn,
            0x00,
            p,
        );
        hub.on_aps_indication(f, T0 + t);
        let ev = hub.drain_events();
        ["alarm", "tamper", "battery_low"]
            .into_iter()
            .filter(|k| {
                states(&ev, &format!("zb:00158d00aaaa0002:1:{k}"))
                    .iter()
                    .any(|s| s["pushed"] == true)
            })
            .collect()
    };
    assert_eq!(notify(1, 0x0001, 20_000), ["alarm"], "a press");
    assert!(notify(1, 0x0001, 21_000).is_empty(), "sent again");
    assert_eq!(notify(2, 0x0001, 30_000), ["alarm"], "another press");
    assert_eq!(notify(3, 0x0009, 40_000), ["battery_low"]);
    assert!(notify(4, 0x0008, 50_000).is_empty(), "the alarm clears");
    assert_eq!(notify(5, 0x0009, 60_000), ["alarm"]);
    assert_eq!(notify(6, 0x000D, 70_000), ["tamper"]);
}

#[test]
fn generic_cover_lock_thermostat_and_polling() {
    let ieee = 0x00158d00bbbb0001;
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    let mut dev = SimDevice::new(ieee, 0x1401, LogicalType::Router, true, 0x1234)
        .endpoint(1, 0x0104, 0x0202, &[0x0000, 0x0102], &[])
        .endpoint(2, 0x0104, 0x000A, &[0x0101], &[])
        .endpoint(3, 0x0104, 0x0301, &[0x0201, 0x0204], &[])
        .endpoint(4, 0x0104, 0x0302, &[0x0402], &[])
        .basic("ACME", "Multi", 0x01)
        .attr(1, 0x0102, 0x0008, TypedValue::uint(ZclType::U8, 25))
        .attr(2, 0x0101, 0x0000, TypedValue::uint(ZclType::ENUM8, 1))
        .attr(3, 0x0201, 0x0000, TypedValue::int(ZclType::I16, 2050))
        .attr(3, 0x0201, 0x0012, TypedValue::int(ZclType::I16, 2100))
        .attr(3, 0x0201, 0x001C, TypedValue::uint(ZclType::ENUM8, 4))
        .attr(3, 0x0201, 0x0029, TypedValue::uint(ZclType::MAP16, 1))
        .attr(3, 0x0201, 0x001B, TypedValue::uint(ZclType::ENUM8, 2))
        .attr(3, 0x0201, 0x0015, TypedValue::int(ZclType::I16, 500))
        .attr(3, 0x0201, 0x0016, TypedValue::int(ZclType::I16, 3000))
        .attr(3, 0x0204, 0x0001, TypedValue::uint(ZclType::ENUM8, 0))
        .attr(4, 0x0402, 0x0000, TypedValue::int(ZclType::I16, 1900));
    dev.unreportable.insert((0x0402, 0x0000));
    sim.devices.push(dev);
    sim.join(&mut hub, ieee, T0);
    let id = "zb:00158d00bbbb0001";
    // Cover: HA position = 100 − lift percentage.
    let cover = format!("{id}:1:cover");
    let st = hub.state_of(&cover).unwrap();
    assert_eq!(
        (
            st["state"].clone(),
            st["attributes"]["current_position"].clone()
        ),
        (json!("open"), json!(75))
    );
    sim.log.clear();
    hub.entity_command(
        &cover,
        "set_cover_position",
        &json!({"position": 30}),
        T0 + 1000,
    )
    .unwrap();
    hub.entity_command(&cover, "stop_cover", &json!({}), T0 + 1000)
        .unwrap();
    sim.run(&mut hub, T0 + 1000);
    let aps = sim.aps();
    assert_eq!(&aps[0].payload[2..], &[0x05, 70]);
    assert_eq!(aps[1].payload[2], 0x02);
    // Lock.
    let lock = format!("{id}:2:lock");
    assert_eq!(state(&hub, &lock), "locked");
    sim.log.clear();
    hub.entity_command(&lock, "unlock", &json!({}), T0 + 2000)
        .unwrap();
    sim.run(&mut hub, T0 + 2000);
    assert_eq!(&sim.aps()[0].payload[2..], &[0x01, 0x00]);
    assert!(keys(&hub, id).contains(&"2:lock_event".to_string()));
    // Thermostat: hvac modes from ControlSequenceOfOperation, limits.
    let th = format!("{id}:3:thermostat");
    let st = hub.state_of(&th).unwrap();
    assert_eq!(st["state"], "heat");
    assert_eq!(st["attributes"]["hvac_action"], "heating");
    assert_eq!(st["attributes"]["current_temperature"], json!(20.5));
    let (_, e) = hub.entity(&th).unwrap();
    let caps = e.record(id)["capabilities"].clone();
    assert_eq!(caps["hvac_modes"], json!(["off", "heat"]));
    assert_eq!(
        (caps["min_temp"].clone(), caps["max_temp"].clone()),
        (json!(5.0), json!(30.0))
    );
    sim.log.clear();
    hub.entity_command(
        &th,
        "set_temperature",
        &json!({"temperature": 22.5}),
        T0 + 3000,
    )
    .unwrap();
    sim.run(&mut hub, T0 + 3000);
    let w = sim.dev(ieee).writes.last().unwrap().clone();
    assert_eq!(
        (w.1, w.2, w.4),
        (0x0201, 0x0012, TypedValue::int(ZclType::I16, 2250))
    );
    assert_eq!(state(&hub, &format!("{id}:3:keypad_lockout")), "unlocked");
    // UNREPORTABLE_ATTRIBUTE on a mains primary measurement → polled every max s.
    sim.log.clear();
    hub.tick(T0 + 3000 + 900_001);
    sim.run(&mut hub, T0 + 3000 + 900_001);
    assert!(
        sim.aps()
            .iter()
            .any(|a| a.cluster == 0x0402 && a.payload[2] == 0x00),
        "polled"
    );
    assert_eq!(state(&hub, &format!("{id}:4:temperature")), json!(19.0));
}

/// New firmware shows in Basic SWBuildID: read again after an OTA upgrade
/// (`force`) and on a rejoin (at most hourly), it re-selects the white
/// label, as zigbee-herdsman-converters does after Zigbee2MQTT's re-read.
#[test]
fn the_identity_is_read_again_after_an_upgrade_and_on_rejoin() {
    let id = "zb:00158d0001a2b3c4";
    let src = "zdef: 1\nid: acme/th-1\nrevision: 1\nvendor: ACME\nmodel: TH-1\nmatch: [{model: TH-1}]\nwhite_label: [{vendor: ACME, model: TH-1 v2, model_id: [TH-1], sw_build: ['2.0']}]\n";
    let cbor = zdefc::compile_str(src, zdefc::Format::Yaml, &zdefc::Options::default())
        .unwrap()
        .cbor;
    let mut cat = MemoryCatalog::default();
    cat.insert(crate::zdef_adapter::load_cbor(&cbor, crate::model::LayerSource::Catalog).unwrap());
    let mut hub = Hub::new(
        MockTransport::default(),
        HubConfig::default(),
        Box::new(cat),
    );
    let sw = |v: &str| TypedValue::new(ZclType::CHAR_STR, ZclValue::String(v.into()));
    let mut sim = Sim::default();
    sim.devices
        .push(sensor().attr(1, 0x0000, 0x4000, sw("1.0")));
    sim.join(&mut hub, SENSOR, T0);
    let model = |h: &Hub<MockTransport>| h.device(id).unwrap().resolution.model.clone();
    assert_eq!(model(&hub).as_deref(), Some("TH-1"));

    // The upgrade: read at once.
    sim.devices[0]
        .attrs
        .insert((1, 0, 0x4000, None), sw("2.0\0"));
    let _ = hub.drain_events();
    hub.refresh_identity(id, true, T0 + 1000);
    sim.run(&mut hub, T0 + 1000);
    assert_eq!(
        hub.device(id).unwrap().fp.sw_build_id.as_deref(),
        Some("2.0")
    );
    assert_eq!(model(&hub).as_deref(), Some("TH-1 v2"));
    assert!(
        hub.drain_events()
            .iter()
            .any(|e| matches!(e, HubEvent::DeviceUpdated(i) if i == id)),
        "stored again"
    );

    // A rejoin within the hour reads nothing; one later does.
    let basic_reads = |sim: &Sim| sim.aps().iter().filter(|a| a.cluster == 0x0000).count();
    sim.devices[0].attrs.insert((1, 0, 0x4000, None), sw("1.0"));
    sim.log.clear();
    hub.on_device_joined(SENSOR, 0x1234, T0 + 60_000);
    sim.run(&mut hub, T0 + 60_000);
    assert_eq!(basic_reads(&sim), 0);
    let later = T0 + 1000 + 3_600_000;
    hub.on_device_joined(SENSOR, 0x1234, later);
    sim.run(&mut hub, later);
    assert_eq!(basic_reads(&sim), 1);
    assert_eq!(model(&hub).as_deref(), Some("TH-1"));
}

/// A router with a GP proxy endpoint (242) as the interview stores it:
/// zigbee-herdsman-converters' fingerprints list 242, so a definition that
/// needs it matches; the GP-profile endpoint gets no entities, binds or
/// reporting.
#[test]
fn the_gp_endpoint_is_kept_for_matching_only() {
    const BULB: u64 = 0x0017880100ddeeff;
    let id = "zb:0017880100ddeeff";
    let src = "zdef: 1\nid: acme/bulb\nrevision: 1\nvendor: ACME\nmodel: BULB-1\nmatch:\n  - model: BULB-1\n    manufacturer: ACME\n    endpoints:\n      1: {profile: 0x0104, device_id: 0x0101, in: [0x0000, 0x0006, 0x0008]}\n      242: {profile: 0xA1E0, device_id: 0x0061, out: [0x0021]}\n";
    let cbor = zdefc::compile_str(src, zdefc::Format::Yaml, &zdefc::Options::default())
        .unwrap()
        .cbor;
    let mut cat = MemoryCatalog::default();
    cat.insert(crate::zdef_adapter::load_cbor(&cbor, crate::model::LayerSource::Catalog).unwrap());
    let mut hub = Hub::new(
        MockTransport::default(),
        HubConfig::default(),
        Box::new(cat),
    );
    let mut sim = Sim::default();
    sim.devices.push(
        SimDevice::new(BULB, 0x2345, LogicalType::Router, true, 0x100b)
            .endpoint(1, 0x0104, 0x0101, &[0x0000, 0x0003, 0x0006, 0x0008], &[])
            .endpoint(242, 0xA1E0, 0x0061, &[], &[0x0021])
            .basic("ACME", "BULB-1", 0x01),
    );
    sim.join(&mut hub, BULB, T0);
    let d = hub.device(id).unwrap();
    assert!(!d.interview_partial);
    let eps: Vec<u8> = d.fp.endpoints.iter().map(|e| e.ep).collect();
    assert_eq!(eps, [1, 242]);
    let layers: Vec<&str> = d.resolution.layers.iter().map(|l| l.id.as_str()).collect();
    assert!(layers.contains(&"acme/bulb"), "{layers:?}");
    assert!(
        !d.entities.iter().any(|e| e.r.ep == 242),
        "no entity on 242"
    );
    let dev = sim.dev(BULB);
    assert!(!dev.binds.iter().any(|b| b.0 == 242), "{:?}", dev.binds);
    assert!(!dev.reporting.iter().any(|r| r.0 == 242));
}
