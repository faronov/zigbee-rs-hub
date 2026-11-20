//! Vendor devices with definitions: Efekta-style settings, Aqara (Lumi
//! cluster, TLV struct, auto-reset, sleepy writes), Philips remote and
//! motion sensor, Innr plug, a generic remote, a self-describing zigbee-rs
//! device.

use serde_json::json;

use super::*;
use crate::testutil::sim::{Sim, SimDevice};
use crate::zcl::{FrameType, TypedValue, ZclType, ZclValue};
use crate::zdo::LogicalType;

fn ostr(b: &[u8]) -> TypedValue {
    TypedValue::new(ZclType::OCTET_STR, ZclValue::Octets(b.to_vec()))
}

fn sim_join_one(hub: &mut Hub<MockTransport>, dev: SimDevice, now: u64) -> Sim {
    let ieee = dev.ieee;
    let mut sim = Sim::default();
    sim.devices.push(dev);
    sim.join(hub, ieee, now);
    sim
}

#[test]
fn efekta_co2_settings_device() {
    let ieee = 0x00124b0012345678;
    let mut hub = hub_with(&["efekta-co2-smart-monitor", "innr-sp120"]);
    let mut sim = sim_join_one(
        &mut hub,
        SimDevice::new(ieee, 0x4001, LogicalType::Router, true, 0x1234)
            .endpoint(1, 0x0104, 0x000C, &[0x0000, 0x0003, 0x0402, 0x0405, 0x040D], &[])
            .basic("EfektaLab", "EFEKTA_CO2_Smart_Monitor", 0x01)
            .attr(1, 0x040D, 0x0000, TypedValue::new(ZclType::SINGLE, ZclValue::Float(f64::from(0.000_412f32))))
            .attr(1, 0x0402, 0x0000, TypedValue::int(ZclType::I16, 2350))
            .attr(1, 0x0405, 0x0000, TypedValue::uint(ZclType::U16, 4120))
            .attr(1, 0x040D, 0x0201, TypedValue::uint(ZclType::U16, 60))
            .attr(1, 0x040D, 0x0211, TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(true))))
            .attr(1, 0x0402, 0x0410, TypedValue::int(ZclType::I16, -15))
            .attr(1, 0x040D, 0x0221, TypedValue::uint(ZclType::U16, 1500)),
        T0,
    );
    let id = "zb:00124b0012345678";
    let d = hub.device(id).unwrap();
    assert_eq!(d.resolution.layers[1].id, "efekta/co2-smart-monitor");
    assert_eq!(d.resolution.layers[1].score, Some(300));
    let k = keys(&hub, id);
    for key in ["1:co2", "1:temperature", "1:humidity", "1:reading_interval", "1:temperature_offset", "1:forced_recalibration", "1:low_gas"] {
        assert!(k.contains(&key.to_string()), "{key} in {k:?}");
    }
    assert_eq!(k.len(), 4 + 14 + 3);
    // generic.patch changed the reporting of the generic entities.
    let s = sim.dev(ieee);
    let rep = |c: u16| s.reporting.iter().find(|(_, cc, r)| *cc == c && r.attr == 0).map(|(_, _, r)| (r.min, r.max, r.change));
    assert_eq!(rep(0x040D), Some((10, 600, Some(f64::from(0.000_001f32)))));
    assert_eq!(rep(0x0402), Some((30, 1800, Some(10.0))));
    assert_eq!(rep(0x0405), Some((30, 1800, Some(10.0))));
    // Settings were read at configure (unsupported ones tolerated).
    assert_eq!(state(&hub, &format!("{id}:1:co2")), json!(412.0));
    assert_eq!(state(&hub, &format!("{id}:1:temperature_offset")), json!(-1.5));
    assert_eq!(state(&hub, &format!("{id}:1:reading_interval")), json!(60));
    assert_eq!(state(&hub, &format!("{id}:1:light_indicator")), json!(true));
    assert_eq!(state(&hub, &format!("{id}:1:high_gas")), json!(1500));
    assert_eq!(state(&hub, &format!("{id}:1:low_gas")), J::Null);
    let (_, e) = hub.entity(&format!("{id}:1:manual_forced_recalibration")).unwrap();
    let r = e.record(id);
    assert_eq!(r["category"], "config");
    assert_eq!(r["capabilities"], json!({"native_min_value": 0.0, "native_max_value": 5000.0, "native_step": 1.0, "mode": "box"}));
    let (_, e) = hub.entity(&format!("{id}:1:temperature_offset")).unwrap();
    assert_eq!(e.record(id)["capabilities"]["native_step"], json!(0.1));
    hub.drain_events();
    sim.log.clear();

    // Spec 6.5: set_value 2.3 writes int16 23.
    let ack = hub.entity_command(&format!("{id}:1:temperature_offset"), "set_value", &json!({"value": 2.3}), T0 + 1000).unwrap();
    sim.run(&mut hub, T0 + 1000);
    let w = sim.dev(ieee).writes.last().unwrap().clone();
    assert_eq!((w.1, w.2, w.4), (0x0402, 0x0410, TypedValue::int(ZclType::I16, 23)));
    assert_eq!(result_of(&hub.drain_events(), ack.ticket), Some(Ok("ok".into())));
    assert_eq!(state(&hub, &format!("{id}:1:temperature_offset")), json!(2.3));
    assert_eq!(hub.entity_command(&format!("{id}:1:temperature_offset"), "set_value", &json!({"value": 60}), T0).unwrap_err().code, "invalid_value");
    // A button that is a one-shot write.
    hub.entity_command(&format!("{id}:1:forced_recalibration"), "press", &json!({}), T0 + 2000).unwrap();
    sim.run(&mut hub, T0 + 2000);
    let w = sim.dev(ieee).writes.last().unwrap().clone();
    assert_eq!((w.1, w.2, w.4.value), (0x040D, 0x0202, ZclValue::Bool(Some(true))));
    // A report of a setting updates it like any attribute.
    let s = sim.dev(ieee).clone();
    hub.on_aps_indication(s.report(1, 0x0402, None, &[(0x0410, TypedValue::int(ZclType::I16, -15))]), T0 + 3000);
    assert_eq!(state(&hub, &format!("{id}:1:temperature_offset")), json!(-1.5));
    // Switch settings write bools.
    hub.entity_command(&format!("{id}:1:enable_gas"), "turn_on", &json!({}), T0 + 4000).unwrap();
    sim.run(&mut hub, T0 + 4000);
    let w = sim.dev(ieee).writes.last().unwrap().clone();
    assert_eq!((w.2, w.4.value), (0x0220, ZclValue::Bool(Some(true))));
}

fn lumi_struct(mv: u16, temp: i8, outages: u16) -> Vec<u8> {
    let mut b = vec![0x01, 0x21];
    b.extend_from_slice(&mv.to_le_bytes());
    b.extend_from_slice(&[0x03, 0x28, temp as u8, 0x05, 0x21]);
    b.extend_from_slice(&outages.to_le_bytes());
    b.extend_from_slice(&[0x64, 0x10, 0x00]);
    b
}

fn aqara(ieee: u64) -> SimDevice {
    let mut dev = SimDevice::new(ieee, 0x5001, LogicalType::EndDevice, false, 0x115F)
        .endpoint(1, 0x0104, 0x0107, &[0x0000, 0x0001, 0x0003, 0xFCC0], &[0x0019])
        .basic("LUMI", "lumi.motion.ac02", 0x03)
        .attr(1, 0x0001, 0x0020, TypedValue::uint(ZclType::U8, 30));
    for (a, v) in [(0x0102u16, 30u64), (0x010C, 2), (0x0152, 1)] {
        dev.attrs.insert((1, 0xFCC0, a, Some(0x115F)), TypedValue::uint(ZclType::U8, v));
    }
    dev
}

#[test]
fn aqara_motion_p1_struct_reset_and_sleepy_writes() {
    let ieee = 0x54ef441000aabbcc;
    let mut hub = hub_with(&["aqara-rtcgq14lm-motion-p1"]);
    let mut sim = sim_join_one(&mut hub, aqara(ieee), T0);
    let id = "zb:54ef441000aabbcc";
    let d = hub.device(id).unwrap();
    assert_eq!(d.record()["manufacturer"], "Aqara");
    assert_eq!(d.record()["model"], "RTCGQ14LM");
    let (_, bv) = hub.entity(&format!("{id}:1:battery_voltage")).unwrap();
    assert_eq!(bv.r.source, crate::model::LayerSource::Catalog, "definition replaces the generic key");
    assert_eq!(state(&hub, &format!("{id}:1:detection_interval")), json!(30));
    assert_eq!(state(&hub, &format!("{id}:1:motion_sensitivity")), "medium");
    assert_eq!(state(&hub, &format!("{id}:1:trigger_indicator")), true);
    hub.drain_events();

    // Motion: 0xFCC0/0x0112 with illuminance + 65536 (manufacturer 0x115F).
    let s = sim.dev(ieee).clone();
    let t1 = T0 + 100_000;
    hub.on_aps_indication(s.report(1, 0xFCC0, Some(0x115F), &[(0x0112, TypedValue::uint(ZclType::U32, 65536 + 120))]), t1);
    assert_eq!(state(&hub, &format!("{id}:1:occupancy")), true);
    assert_eq!(state(&hub, &format!("{id}:1:illuminance")), 120);
    // A frame without the manufacturer code does not match the binding.
    hub.on_aps_indication(s.report(1, 0xFCC0, None, &[(0x0112, TypedValue::uint(ZclType::U32, 65536 + 999))]), t1 + 10);
    assert_eq!(state(&hub, &format!("{id}:1:illuminance")), 120);
    // reset_after = detection_interval (30) + 2 s.
    hub.tick(t1 + 31_000);
    assert_eq!(state(&hub, &format!("{id}:1:occupancy")), true);
    hub.tick(t1 + 32_001);
    assert_eq!(state(&hub, &format!("{id}:1:occupancy")), false);
    // Heartbeat struct 0x00F7 through the zcl_tagged codec.
    hub.on_aps_indication(s.report(1, 0xFCC0, Some(0x115F), &[(0x00F7, ostr(&lumi_struct(2950, 26, 7)))]), t1 + 40_000);
    assert_eq!(state(&hub, &format!("{id}:1:battery_voltage")), 2950);
    assert_eq!(state(&hub, &format!("{id}:1:battery")), json!(67.0));
    assert_eq!(state(&hub, &format!("{id}:1:device_temperature")), 26);
    assert_eq!(state(&hub, &format!("{id}:1:power_outage_count")), 6);

    // Sleepy write: queued until the device talks, `pending` meanwhile.
    hub.transport.take();
    let t2 = t1 + 120_000;
    let uid = format!("{id}:1:motion_sensitivity");
    let ack = hub.entity_command(&uid, "select_option", &json!({"option": "high"}), t2).unwrap();
    assert_eq!(ack.status, "queued");
    assert!(hub.transport.take().is_empty(), "nothing sent to a sleeping device");
    assert_eq!(hub.state_of(&uid).unwrap()["attributes"]["pending"]["params"]["option"], "high");
    hub.on_aps_indication(s.report(1, 0x0001, None, &[(0x0020, TypedValue::uint(ZclType::U8, 29))]), t2 + 60_000);
    sim.run(&mut hub, t2 + 60_000);
    let w = sim.dev(ieee).writes.last().unwrap().clone();
    assert_eq!((w.1, w.2, w.3, w.4), (0xFCC0, 0x010C, Some(0x115F), TypedValue::uint(ZclType::U8, 3)));
    let ev = hub.drain_events();
    assert_eq!(result_of(&ev, ack.ticket), Some(Ok("ok".into())));
    let st = hub.state_of(&uid).unwrap();
    assert_eq!(st["state"], "high");
    assert!(st["attributes"].get("pending").is_none());
    // Battery availability: max(2.5 × check-in 1 h, 25 h).
    assert_eq!(hub.availability_timeout_s(hub.device(id).unwrap()), Some(25 * 3600));
}

#[test]
fn philips_rwl022_remote_events() {
    let ieee = 0x0017880104aabbcc;
    let mut hub = hub_with(&["philips-rwl022-dimmer", "philips-sml001-motion"]);
    let mut sim = sim_join_one(
        &mut hub,
        SimDevice::new(ieee, 0x6001, LogicalType::EndDevice, false, 0x100B)
            .endpoint(1, 0x0104, 0x0820, &[0x0000, 0x0001, 0x0003, 0xFC00, 0x1000], &[0x0000, 0x0003, 0x0004, 0x0006, 0x0008, 0x0005, 0x0019])
            .basic("Signify Netherlands B.V.", "RWL022", 0x03)
            .attr(1, 0x0001, 0x0021, TypedValue::uint(ZclType::U8, 200)),
        T0,
    );
    let id = "zb:0017880104aabbcc";
    let s = sim.dev(ieee).clone();
    for c in [0x0006, 0x0008, 0xFC00, 0x0001] {
        assert!(s.binds.contains(&(1, c)), "bind {c:04x}");
    }
    assert!(s.writes.iter().any(|w| (w.1, w.2, w.3) == (0x0000, 0x0031, Some(0x100B)) && w.4 == TypedValue::uint(ZclType::MAP16, 0x000B)));
    let uid = format!("{id}:1:action");
    let (_, e) = hub.entity(&uid).unwrap();
    assert_eq!(e.r.source, crate::model::LayerSource::Catalog);
    assert_eq!(e.record(id)["capabilities"]["event_types"].as_array().unwrap().len(), 18);
    hub.drain_events();

    let fc00 = |tsn: u8, button: u8, kind: u8, hold: u8| s.zcl(1, 0xFC00, FrameType::Cluster, Some(0x100B), tsn, 0x00, vec![button, 0, 0, 0, kind, 0x30, hold, 0]);
    hub.on_aps_indication(fc00(1, 1, 0, 0), T0 + 1000);
    hub.on_aps_indication(fc00(1, 1, 0, 0), T0 + 1200); // retry: same TSN
    hub.on_aps_indication(fc00(2, 2, 1, 8), T0 + 2000);
    hub.on_aps_indication(s.zcl(1, 0x0005, FrameType::Cluster, None, 3, 0x05, vec![0x00, 0x00, 0x01]), T0 + 3000);
    // Standard On/Off duplicates are ignored (`ignore:`); FC00 without the
    // manufacturer code does not match.
    hub.on_aps_indication(s.zcl(1, 0x0006, FrameType::Cluster, None, 4, 0x01, vec![]), T0 + 4000);
    hub.on_aps_indication(s.zcl(1, 0xFC00, FrameType::Cluster, None, 5, 0x00, vec![4, 0, 0, 0, 0, 0, 0, 0]), T0 + 5000);
    let ev = hub.drain_events();
    let f = fired(&ev, &uid);
    let types: Vec<&str> = f.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(types, ["on_press", "up_hold", "recall_1"]);
    assert_eq!(f[1].1["hold_time"], 8);
    assert_eq!(f[1].1["endpoint"], 1);
    assert_eq!(state(&hub, &format!("{id}:1:battery")), json!(100.0));
}

#[test]
fn generic_remote_events_and_poll_control() {
    let ieee = 0x000d6ffffe112233;
    let mut hub = hub_with(&["philips-rwl022-dimmer"]);
    let sim = sim_join_one(
        &mut hub,
        SimDevice::new(ieee, 0x6002, LogicalType::EndDevice, false, 0x117C)
            .endpoint(1, 0x0104, 0x0820, &[0x0000, 0x0001, 0x0003, 0x0009, 0x0020, 0x1000], &[0x0003, 0x0004, 0x0006, 0x0008, 0x0019])
            .basic("IKEA of Sweden", "TRADFRI on/off switch", 0x03),
        T0,
    );
    let id = "zb:000d6ffffe112233";
    let s = sim.devices[0].clone();
    assert!(s.binds.contains(&(1, 0x0006)) && s.binds.contains(&(1, 0x0008)) && s.binds.contains(&(1, 0x0020)));
    let uid = format!("{id}:1:action");
    let (_, e) = hub.entity(&uid).unwrap();
    assert_eq!(e.r.source, crate::model::LayerSource::Generic);
    hub.drain_events();
    hub.on_aps_indication(s.zcl(1, 0x0006, FrameType::Cluster, None, 10, 0x01, vec![]), T0 + 1000);
    hub.on_aps_indication(s.zcl(1, 0x0008, FrameType::Cluster, None, 11, 0x06, vec![0x01, 0x2B, 0x05, 0x00]), T0 + 2000);
    hub.on_aps_indication(s.zcl(1, 0x0008, FrameType::Cluster, None, 11, 0x06, vec![0x01, 0x2B, 0x05, 0x00]), T0 + 2500);
    hub.on_aps_indication(s.zcl(1, 0x0008, FrameType::Cluster, None, 12, 0x05, vec![0x00, 0x53]), T0 + 3000);
    hub.on_aps_indication(s.zcl(1, 0x0008, FrameType::Cluster, None, 13, 0x07, vec![]), T0 + 4000);
    let f = fired(&hub.drain_events(), &uid);
    let types: Vec<&str> = f.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(types, ["on", "brightness_step_down", "brightness_move_up", "brightness_stop"]);
    assert_eq!(f[1].1["step_size"], 0x2B);
    assert_eq!(f[1].1["transition_time"], 5);
    assert_eq!(f[2].1["rate"], 0x53);
    // Poll Control check-in → Check-in Response (no fast poll: queue empty).
    hub.transport.take();
    hub.on_aps_indication(s.zcl(1, 0x0020, FrameType::Cluster, None, 14, 0x00, vec![]), T0 + 5000);
    let a = hub.transport.aps().into_iter().find(|a| a.cluster == 0x0020).cloned().unwrap();
    assert_eq!(&a.payload[1..], &[14, 0x00, 0x00, 0x28, 0x00]);
}

#[test]
fn philips_sml001_only_endpoints_and_mfr_settings() {
    let ieee = 0x0017880103334444;
    let mut hub = hub_with(&["philips-sml001-motion", "philips-rwl022-dimmer"]);
    let mut dev = SimDevice::new(ieee, 0x6003, LogicalType::EndDevice, false, 0x100B)
        .endpoint(1, 0xC05E, 0x0850, &[0x0000], &[0x0000, 0x0003, 0x0004, 0x0006, 0x0008, 0x0300])
        .endpoint(2, 0x0104, 0x0107, &[0x0000, 0x0001, 0x0003, 0x0406, 0x0400, 0x0402], &[0x0019])
        .basic("Signify Netherlands B.V.", "SML001", 0x03)
        .attr(2, 0x0406, 0x0010, TypedValue::uint(ZclType::U16, 30))
        .attr(2, 0x0406, 0x0000, TypedValue::uint(ZclType::MAP8, 1))
        .attr(2, 0x0400, 0x0000, TypedValue::uint(ZclType::U16, 30001));
    dev.attrs.insert((2, 0x0406, 0x0030, Some(0x100B)), TypedValue::uint(ZclType::U8, 2));
    dev.attrs.insert((2, 0x0000, 0x0033, Some(0x100B)), TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(true))));
    sim_join_one(&mut hub, dev, T0);
    let id = "zb:0017880103334444";
    let k = keys(&hub, id);
    assert!(!k.iter().any(|k| k.starts_with("1:")), "generic.only_endpoints [2]: {k:?}");
    assert_eq!(state(&hub, &format!("{id}:2:occupancy")), true);
    assert_eq!(state(&hub, &format!("{id}:2:illuminance")), 1000);
    assert_eq!(state(&hub, &format!("{id}:2:motion_sensitivity")), "high");
    assert_eq!(state(&hub, &format!("{id}:2:led_indication")), true);
    let (_, e) = hub.entity(&format!("{id}:2:occupancy_timeout")).unwrap();
    assert_eq!(e.record(id)["capabilities"]["native_max_value"], json!(65535.0));
}

#[test]
fn innr_plug_fixed_divisors() {
    let ieee = 0x00158d0000aa0001;
    let mut hub = hub_with(&["innr-sp120", "efekta-co2-smart-monitor"]);
    let mut sim = sim_join_one(
        &mut hub,
        SimDevice::new(ieee, 0x7001, LogicalType::Router, true, 0x1166)
            .endpoint(1, 0x0104, 0x0051, &[0x0000, 0x0003, 0x0004, 0x0005, 0x0006, 0x0702, 0x0B04], &[0x0019])
            .basic("innr", "SP 120", 0x01)
            .attr(1, 0x0006, 0x0000, TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(true))))
            .attr(1, 0x0006, 0x4003, TypedValue::uint(ZclType::ENUM8, 1))
            .attr(1, 0x0702, 0x0000, TypedValue::uint(ZclType::U48, 12345))
            .attr(1, 0x0702, 0x0301, TypedValue::uint(ZclType::U24, 1))
            .attr(1, 0x0702, 0x0302, TypedValue::uint(ZclType::U24, 1))
            .attr(1, 0x0B04, 0x0505, TypedValue::uint(ZclType::U16, 230))
            .attr(1, 0x0B04, 0x0508, TypedValue::uint(ZclType::U16, 523))
            .attr(1, 0x0B04, 0x050B, TypedValue::int(ZclType::I16, 120))
            .attr(1, 0x0B04, 0x0602, TypedValue::uint(ZclType::U16, 1))
            .attr(1, 0x0B04, 0x0603, TypedValue::uint(ZclType::U16, 1)),
        T0,
    );
    let id = "zb:00158d0000aa0001";
    let k = keys(&hub, id);
    assert!(!k.contains(&"1:power_on_behavior".to_string()), "excluded");
    assert_eq!(state(&hub, &format!("{id}:1:switch")), true);
    assert_eq!(hub.entity(&format!("{id}:1:switch")).unwrap().1.r.def.device_class.as_deref(), Some("outlet"));
    assert_eq!(state(&hub, &format!("{id}:1:current")), json!(0.523));
    assert_eq!(state(&hub, &format!("{id}:1:voltage")), json!(230.0));
    assert_eq!(state(&hub, &format!("{id}:1:power")), json!(120.0));
    assert_eq!(state(&hub, &format!("{id}:1:energy")), json!(123.45));
    // On/Off via cluster commands.
    sim.log.clear();
    let ack = hub.entity_command(&format!("{id}:1:switch"), "turn_off", &json!({}), T0 + 1000).unwrap();
    sim.run(&mut hub, T0 + 1000);
    assert_eq!(sim.aps()[0].cluster, 0x0006);
    assert_eq!(sim.aps()[0].payload[2], 0x00);
    assert_eq!(result_of(&hub.drain_events(), ack.ticket), Some(Ok("ok".into())));
    assert_eq!(state(&hub, &format!("{id}:1:switch")), false);
}

#[test]
fn self_describing_zigbee_rs_device() {
    use sha2::Digest;
    let cbor = golden_cbor("zigbee-rs-selfdesc-thermometer");
    let blob = miniz_oxide::deflate::compress_to_vec(&cbor, 9);
    let hash = sha2::Sha256::digest(&cbor)[..8].to_vec();
    let ieee = 0x6d6b000000000001;
    let mut hub = hub_with(&[]);
    hub.set_schema_decoder(Box::new(crate::zdef_adapter::ZdefSchemaDecoder));
    let mut dev = SimDevice::new(ieee, 0x7101, LogicalType::Router, true, 0x6D6B)
        .endpoint(1, 0x0104, 0x0302, &[0x0000, 0x0402, 0x0405, 0xFC6B, 0xFC6C], &[])
        .basic("zigbee-rs", "zrs-thermo-1", 0x01)
        .attr(1, 0x0402, 0x0000, TypedValue::int(ZclType::I16, 2000));
    let m = Some(0x6D6B);
    dev.attrs.insert((1, 0xFC6B, 0x0000, m), TypedValue::uint(ZclType::U8, 0x11));
    dev.attrs.insert((1, 0xFC6B, 0x0001, m), ostr(&hash));
    dev.attrs.insert((1, 0xFC6B, 0x0002, m), TypedValue::uint(ZclType::U16, blob.len() as u64));
    dev.attrs.insert((1, 0xFC6B, 0x0003, m), TypedValue::uint(ZclType::U8, 48));
    for (i, c) in blob.chunks(48).enumerate() {
        dev.attrs.insert((1, 0xFC6B, 0x0100 + i as u16, m), ostr(c));
    }
    dev.attrs.insert((1, 0xFC6C, 0x0000, m), TypedValue::uint(ZclType::U16, 300));
    dev.attrs.insert((1, 0xFC6C, 0x0003, m), TypedValue::uint(ZclType::ENUM8, 1));
    let sim = sim_join_one(&mut hub, dev, T0);
    let id = "zb:6d6b000000000001";
    let d = hub.device(id).unwrap();
    assert!(d.schema.error.is_none(), "{:?}", d.schema.error);
    assert_eq!(d.resolution.layers.last().unwrap().source, crate::model::LayerSource::SelfDescription);
    let k = keys(&hub, id);
    for key in ["1:temperature", "1:report_interval", "1:temperature_offset", "1:led_mode", "1:tx_failures", "1:reset_settings"] {
        assert!(k.contains(&key.to_string()), "{key}");
    }
    assert_eq!(state(&hub, &format!("{id}:1:report_interval")), 300);
    assert_eq!(state(&hub, &format!("{id}:1:led_mode")), "on_report");
    let (_, e) = hub.entity(&format!("{id}:1:tx_failures")).unwrap();
    assert_eq!(e.r.source, crate::model::LayerSource::SelfDescription);
    assert_eq!(e.record(id)["enabled_default"], false);
    // The fragment's generic.patch reached the generic temperature report.
    let s = &sim.devices[0];
    assert!(s.reporting.iter().any(|(_, c, r)| *c == 0x0402 && r.attr == 0 && (r.min, r.max) == (30, 600)));
    assert!(s.reporting.iter().any(|(_, c, r)| *c == 0xFC6C && r.attr == 0x0100 && (r.min, r.max) == (3600, 43200)));
    // Cached by hash.
    assert!(hub.schema_cache.contains_key(&hash));
    // A corrupted chunk fails verification: generic + catalog only.
    let mut hub2 = hub_with(&[]);
    hub2.set_schema_decoder(Box::new(crate::zdef_adapter::ZdefSchemaDecoder));
    let mut bad = sim.devices[0].clone();
    bad.attrs.insert((1, 0xFC6B, 0x0100, m), ostr(&[0u8; 48]));
    sim_join_one(&mut hub2, bad, T0);
    let d = hub2.device(id).unwrap();
    assert!(d.schema.error.is_some());
    assert!(keys(&hub2, id).contains(&"1:temperature".to_string()));
    assert!(!keys(&hub2, id).contains(&"1:report_interval".to_string()));
}

#[test]
fn yaml_and_cbor_paths_agree() {
    // The test YAML loader + reference interpreter and the production
    // zdefc CBOR + zdef VM path give the same entities and states.
    for (name, model, man) in [("aqara-rtcgq14lm-motion-p1", "lumi.motion.ac02", "LUMI"), ("efekta-co2-smart-monitor", "EFEKTA_CO2_Smart_Monitor", "EfektaLab")] {
        let run = |mut hub: Hub<MockTransport>| {
            let ieee = 0x0011223344556677;
            let mut dev = SimDevice::new(ieee, 0x7777, LogicalType::EndDevice, false, 0x115F)
                .endpoint(1, 0x0104, 0x0107, &[0x0000, 0x0001, 0x0402, 0x0405, 0x040D, 0xFCC0], &[])
                .basic(man, model, 0x03);
            dev.attrs.insert((1, 0xFCC0, 0x0102, Some(0x115F)), TypedValue::uint(ZclType::U8, 60));
            let s = dev.clone();
            sim_join_one(&mut hub, dev, T0);
            hub.on_aps_indication(s.report(1, 0xFCC0, Some(0x115F), &[(0x0112, TypedValue::uint(ZclType::U32, 65536 + 300))]), T0 + 1000);
            hub.on_aps_indication(s.report(1, 0xFCC0, Some(0x115F), &[(0x00F7, ostr(&lumi_struct(2900, 20, 3)))]), T0 + 2000);
            hub.on_aps_indication(s.report(1, 0x0402, None, &[(0x0410, TypedValue::int(ZclType::I16, 7))]), T0 + 3000);
            let id = "zb:0011223344556677";
            let d = hub.device(id).unwrap();
            d.entities.iter().map(|e| (e.uid.clone(), hub.state_of(&e.uid).unwrap()["state"].clone())).collect::<Vec<_>>()
        };
        let a = run(hub_with(&[name]));
        let b = run(hub_with_yaml(&[name]));
        assert_eq!(a, b, "{name}");
    }
}
