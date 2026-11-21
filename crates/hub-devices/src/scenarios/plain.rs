//! Plain ZCL devices: a battery temperature/humidity sensor and a colour
//! light, generic tier only.

use serde_json::json;

use super::*;
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
