//! Property tests: codecs round-trip, the transform pipeline inverts, and
//! the device state machine keeps its invariants under random traffic.

use proptest::prelude::*;
use serde_json::json;

use super::*;
use crate::expr::{EmptyEnv, Value};
use crate::model::{Binding, Limits, Transforms, TuyaType, ValueDef, ZclBinding, ZclWrite};
use crate::testutil::sim::{Sim, SimDevice};
use crate::transport::Sent;
use crate::zcl::types::{decode_slice, encode_value, int_range};
use crate::zcl::{TypedValue, ZclType, ZclValue};
use crate::zdo::LogicalType;

fn int_types() -> impl Strategy<Value = ZclType> {
    prop::sample::select(vec![
        ZclType::U8,
        ZclType::U16,
        ZclType::U24,
        ZclType::U32,
        ZclType::U40,
        ZclType::U48,
        ZclType::I8,
        ZclType::I16,
        ZclType::I24,
        ZclType::I32,
        ZclType::I48,
        ZclType::ENUM8,
        ZclType::ENUM16,
        ZclType::MAP8,
        ZclType::MAP16,
        ZclType::MAP24,
    ])
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn zcl_integers_round_trip(ty in int_types(), seed in any::<i64>()) {
        let (lo, hi) = int_range(ty).unwrap();
        let v = lo + (i128::from(seed).rem_euclid(hi - lo + 1));
        let zv = if ty.int_width().is_some() { ZclValue::Int(v as i64) } else { ZclValue::Uint(v as u64) };
        let mut out = Vec::new();
        encode_value(ty, &zv, &mut out).unwrap();
        prop_assert_eq!(Some(out.len()), ty.fixed_size());
        prop_assert_eq!(decode_slice(ty, &out).unwrap().0, zv);
    }

    #[test]
    fn zcl_decoder_never_panics(ty in any::<u8>(), bytes in prop::collection::vec(any::<u8>(), 0..64)) {
        let _ = decode_slice(ZclType(ty), &bytes);
        let _ = crate::zcl::ZclFrame::parse(&bytes);
        let _ = crate::zcl::frame::parse_read_attributes_rsp(&bytes);
        let _ = crate::zdo::ZdoMessage::parse(0x8004, &bytes);
        let _ = crate::tuya::parse_data(&bytes);
        let _ = crate::gp::generic::decode_report(0xA2, &bytes);
        let _ = crate::codec::decode_tagged(&bytes);
    }

    #[test]
    fn tuya_dps_round_trip(v in any::<i32>(), e in any::<u8>(), b in any::<bool>(), raw in prop::collection::vec(any::<u8>(), 0..32), seq in any::<u16>()) {
        let recs = vec![
            crate::tuya::DpRecord { dp: 1, ty: 2, data: crate::tuya::dp_encode(TuyaType::Value, &Value::Int(i64::from(v))).unwrap() },
            crate::tuya::DpRecord { dp: 2, ty: 4, data: crate::tuya::dp_encode(TuyaType::Enum, &Value::Int(i64::from(e))).unwrap() },
            crate::tuya::DpRecord { dp: 3, ty: 1, data: crate::tuya::dp_encode(TuyaType::Bool, &Value::Bool(b)).unwrap() },
            crate::tuya::DpRecord { dp: 4, ty: 0, data: raw.clone() },
        ];
        let (s, back) = crate::tuya::parse_data(&crate::tuya::encode_data(seq, &recs)).unwrap();
        prop_assert_eq!(s, seq);
        prop_assert_eq!(&back, &recs);
        prop_assert_eq!(crate::tuya::dp_value(2, &back[0].data), Value::Int(i64::from(v)));
        prop_assert_eq!(crate::tuya::dp_value(4, &back[1].data), Value::Int(i64::from(e)));
        prop_assert_eq!(crate::tuya::dp_value(1, &back[2].data), Value::Bool(b));
        prop_assert_eq!(crate::tuya::dp_value(0, &back[3].data), Value::Bytes(raw));
    }

    #[test]
    fn schedule_codec_round_trips(day in 1u8..=7, items in prop::collection::vec((0u8..24, 0u8..60, 50u16..350), 6)) {
        let text = items.iter().map(|(h, m, t)| format!("{h:02}:{m:02}/{:.1}", f64::from(*t) / 10.0)).collect::<Vec<_>>().join(" ");
        let c = crate::model::Codec::TuyaScheduleDay { day, transitions: 6, variant: crate::model::ScheduleVariant::MultiDp };
        let raw = crate::codec::encode(&c, &Value::Str(text.clone())).unwrap();
        prop_assert_eq!(crate::codec::decode(&c, &raw), Value::Str(text));
    }

    #[test]
    fn transforms_invert(div in prop::sample::select(vec![1.0, 2.0, 10.0, 100.0, 1000.0]), offset in -50i32..50, raw in -30000i64..30000) {
        let vd = ValueDef {
            binding: Binding::Zcl(ZclBinding { cluster: 1, attr: 1, ty: ZclType::I16, mfr: None, write: ZclWrite::Attribute }),
            transforms: Transforms { div: Some(div), offset: Some(f64::from(offset)), ..Default::default() },
            limits: Limits::default(),
            ..Default::default()
        };
        let none = |_: u16, _: u16| None;
        let (state, _) = crate::values::decode(&vd, false, Value::Int(raw), &EmptyEnv, &none);
        let back = crate::values::encode(&vd, false, &state, &EmptyEnv).unwrap();
        prop_assert_eq!(back, Value::Int(raw));
    }

    #[test]
    fn fingerprint_hash_ignores_endpoint_order(mut eps in prop::collection::vec((1u8..240, any::<u16>(), prop::collection::vec(any::<u16>(), 0..6)), 1..5)) {
        eps.sort_by_key(|e| e.0);
        eps.dedup_by_key(|e| e.0);
        let mk = |list: &[(u8, u16, Vec<u16>)]| crate::fingerprint::Fingerprint {
            model_id: Some("m".into()),
            endpoints: list.iter().map(|(ep, d, c)| crate::fingerprint::EndpointFp { ep: *ep, profile: 0x0104, device_id: *d, in_clusters: c.clone(), out_clusters: vec![] }).collect(),
            ..Default::default()
        };
        let a = mk(&eps);
        eps.reverse();
        let b = mk(&eps);
        prop_assert_eq!(a.hash(), b.hash());
        prop_assert_eq!(a.hash(), crate::expr::hex(&crate::zdef_adapter::zdef_fingerprint(&a).hash16()));
    }
}

#[derive(Debug, Clone)]
enum Op {
    Temp(i16),
    Hum(u16),
    Light(bool, u8),
    Tick(u32),
    Cmd(u8),
    Reinterview,
    Garbage(Vec<u8>),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        any::<i16>().prop_map(Op::Temp),
        any::<u16>().prop_map(Op::Hum),
        (any::<bool>(), any::<u8>()).prop_map(|(a, b)| Op::Light(a, b)),
        (0u32..7_200_000).prop_map(Op::Tick),
        (0u8..6).prop_map(Op::Cmd),
        Just(Op::Reinterview),
        prop::collection::vec(any::<u8>(), 0..24).prop_map(Op::Garbage),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn state_machine_invariants(ops in prop::collection::vec(op(), 1..40)) {
        let sensor = 0x00158d0001a2b3c4u64;
        let light = 0x0017880100aabbccu64;
        let mut hub = hub_with(&[]);
        let mut sim = Sim::default();
        sim.devices.push(
            SimDevice::new(sensor, 0x1234, LogicalType::EndDevice, false, 0x1037)
                .endpoint(1, 0x0104, 0x0302, &[0x0000, 0x0001, 0x0402, 0x0405], &[])
                .basic("ACME", "TH-1", 0x03)
                .attr(1, 0x0402, 0x0000, TypedValue::int(ZclType::I16, 2000))
                .attr(1, 0x0405, 0x0000, TypedValue::uint(ZclType::U16, 5000))
                .attr(1, 0x0001, 0x0021, TypedValue::uint(ZclType::U8, 180)),
        );
        sim.devices.push(
            SimDevice::new(light, 0x2001, LogicalType::Router, true, 0x100B)
                .endpoint(11, 0x0104, 0x0101, &[0x0000, 0x0006, 0x0008], &[])
                .basic("ACME", "Dimmer", 0x01)
                .attr(11, 0x0006, 0x0000, TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false))))
                .attr(11, 0x0008, 0x0000, TypedValue::uint(ZclType::U8, 10)),
        );
        let mut now = T0;
        sim.join(&mut hub, sensor, now);
        sim.join(&mut hub, light, now);
        let uids = |h: &Hub<MockTransport>| {
            let mut v: Vec<String> = h.devices().flat_map(|d| d.entities.iter().filter(|e| !e.orphaned).map(|e| e.uid.clone())).collect();
            v.sort();
            v
        };
        let initial = uids(&hub);
        let s = sim.devices[0].clone();
        let l = sim.devices[1].clone();
        let mut last_sensor_frame = now;
        for o in ops {
            now += 1000;
            match o {
                Op::Temp(v) => {
                    hub.on_aps_indication(s.report(1, 0x0402, None, &[(0, TypedValue::int(ZclType::I16, i64::from(v)))]), now);
                    last_sensor_frame = now;
                    let st = state(&hub, "zb:00158d0001a2b3c4:1:temperature");
                    if v == i16::MIN { prop_assert_eq!(st, J::Null); } else { prop_assert_eq!(st, json!(f64::from(v) / 100.0)); }
                }
                Op::Hum(v) => {
                    hub.on_aps_indication(s.report(1, 0x0405, None, &[(0, TypedValue::uint(ZclType::U16, u64::from(v)))]), now);
                    last_sensor_frame = now;
                }
                Op::Light(on, lvl) => {
                    hub.on_aps_indication(l.report(11, 0x0006, None, &[(0, TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(on))))]), now);
                    hub.on_aps_indication(l.report(11, 0x0008, None, &[(0, TypedValue::uint(ZclType::U8, u64::from(lvl)))]), now);
                    let st = hub.state_of("zb:0017880100aabbcc:11:light").unwrap();
                    prop_assert_eq!(&st["state"], &json!(if on { "on" } else { "off" }));
                    prop_assert_eq!(&st["attributes"]["brightness"], &json!(lvl));
                }
                Op::Tick(dt) => {
                    now += u64::from(dt);
                    hub.tick(now);
                }
                Op::Cmd(k) => {
                    let uid = "zb:0017880100aabbcc:11:light";
                    let p = match k {
                        0 => ("turn_on", json!({})),
                        1 => ("turn_off", json!({})),
                        2 => ("turn_on", json!({"brightness": 128})),
                        3 => ("toggle", json!({})),
                        4 => ("turn_on", json!({"brightness": "x"})),
                        _ => ("set_value", json!({"value": 1})),
                    };
                    let r = hub.entity_command(uid, p.0, &p.1, now);
                    prop_assert_eq!(r.is_ok(), k < 4);
                    sim.run(&mut hub, now);
                }
                Op::Reinterview => {
                    hub.reinterview("zb:0017880100aabbcc", now).unwrap();
                    sim.run(&mut hub, now);
                }
                Op::Garbage(b) => {
                    hub.on_aps_indication(s.ind(1, 0x0104, 0x0402, b.clone()), now);
                    hub.on_aps_indication(s.ind(0, 0, 0x8004, b), now);
                    last_sensor_frame = now;
                }
            }
            // Unanswered requests resolve through timeouts, never panics.
            sim.run(&mut hub, now);
            prop_assert_eq!(uids(&hub), initial.clone(), "uids are stable");
            for d in hub.devices() {
                for e in &d.entities {
                    if let (Some(c), Some(r)) = (e.state.last_changed, e.state.last_reported) {
                        prop_assert!(c <= r, "last_changed ≤ last_reported");
                    }
                }
            }
            let sd = hub.device("zb:00158d0001a2b3c4").unwrap();
            let timeout = hub.availability_timeout_s(sd).unwrap() * 1000;
            if now.saturating_sub(last_sensor_frame) < timeout {
                prop_assert!(sd.available);
            }
        }
        // Nothing is left waiting forever: after a long quiet period no
        // request is outstanding.
        hub.tick(now + 3_600_000);
        hub.tick(now + 3_700_000);
        prop_assert!(hub.transport.take().iter().all(|s| matches!(s, Sent::Aps(_))));
    }
}
