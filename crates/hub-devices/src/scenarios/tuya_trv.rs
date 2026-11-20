//! Tuya TS0601 devices over EF00: the ME167 TRV (climate from DPs,
//! settings, schedules, time sync, magic packet) and the T/H sensor
//! (dataQuery on configure).

use serde_json::json;

use super::*;
use crate::testutil::sim::{Sim, SimDevice};
use crate::tuya::cmd;
use crate::zdo::LogicalType;

const TRV: u64 = 0xa4c138aabbccddee;

fn trv() -> SimDevice {
    SimDevice::new(TRV, 0x3001, LogicalType::EndDevice, false, 0x1002)
        .endpoint(1, 0x0104, 0x0051, &[0x0000, 0x0004, 0x0005, 0xEF00], &[0x000A, 0x0019])
        .basic("_TZE200_bvu2wnxz", "TS0601", 0x03)
}

fn value(v: i32) -> Vec<u8> {
    v.to_be_bytes().to_vec()
}

#[test]
fn tuya_trv_me167() {
    let mut hub = hub_with(&["tuya-ts0601-trv-me167", "tuya-ts0601-th-sensor"]);
    let mut sim = Sim::default();
    sim.devices.push(trv());
    sim.join(&mut hub, TRV, T0);
    let id = "zb:a4c138aabbccddee";
    let d = hub.device(id).unwrap();
    assert_eq!(d.resolution.layers[1].id, "tuya/ts0601-thermostat-3");
    assert_eq!(d.resolution.layers[1].score, Some(500));
    let rec = d.record();
    assert_eq!(rec["manufacturer"], "AVATTO");
    assert_eq!(rec["model"], "ME167");
    assert_eq!(rec["model_id"], "TS0601");
    // generic.disable: only device-level diagnostics besides the definition.
    let k = keys(&hub, id);
    assert_eq!(k.len(), 15 + 3);
    assert!(k.contains(&"1:thermostat".to_string()) && k.contains(&"0:linkquality".to_string()));
    // Magic packet: one Basic read of six attributes at configure.
    let magic = sim.aps().into_iter().find(|a| a.cluster == 0 && a.profile != 0 && a.payload.len() == 3 + 12).cloned().expect("magic packet");
    assert_eq!(&magic.payload[3..], &[0x04, 0, 0, 0, 0x01, 0, 0x05, 0, 0x07, 0, 0xFE, 0xFF]);
    let uid = format!("{id}:1:thermostat");
    let (_, e) = hub.entity(&uid).unwrap();
    let caps = e.record(id)["capabilities"].clone();
    assert_eq!(caps["hvac_modes"], json!(["auto", "heat", "off"]));
    assert_eq!(caps["min_temp"], json!(5.0));
    assert_eq!(caps["max_temp"], json!(35.0));
    assert_eq!(caps["target_temperature_step"], json!(0.5));
    hub.drain_events();
    sim.log.clear();

    // A dataReport with the climate DPs, the shared error DP and a schedule.
    let mut sched = vec![1u8];
    for (h, m, t) in [(6, 0, 200), (8, 0, 160), (11, 30, 160), (13, 30, 160), (17, 0, 210), (22, 0, 160)] {
        sched.extend_from_slice(&[h, m]);
        sched.extend_from_slice(&(t as u16).to_be_bytes());
    }
    let s = sim.dev(TRV).clone();
    let frame = s.tuya_frame(
        cmd::DATA_REPORT,
        1,
        &[(2, 4, vec![1]), (3, 4, vec![1]), (4, 2, value(215)), (5, 2, value(198)), (7, 1, vec![1]), (35, 2, value(1)), (28, 0, sched), (104, 2, value(37))],
    );
    hub.on_aps_indication(frame.clone(), T0 + 1000);
    let st = hub.state_of(&uid).unwrap();
    assert_eq!(st["state"], "heat");
    assert_eq!(st["attributes"]["target_temperature"], json!(21.5));
    assert_eq!(st["attributes"]["current_temperature"], json!(19.8));
    assert_eq!(st["attributes"]["hvac_action"], "idle");
    assert_eq!(state(&hub, &format!("{id}:1:child_lock")), true);
    assert_eq!(state(&hub, &format!("{id}:1:battery_low")), true);
    assert_eq!(state(&hub, &format!("{id}:1:error")), 1);
    assert_eq!(state(&hub, &format!("{id}:1:schedule_monday")), "06:00/20.0 08:00/16.0 11:30/16.0 13:30/16.0 17:00/21.0 22:00/16.0");
    let diag = hub.diagnostics(id).unwrap();
    assert_eq!(diag["tuya_dps"][0]["dp"], 104, "unknown DPs are kept as diagnostics");
    // The same frame again within 2 s is a duplicate.
    let n = states(&hub.drain_events(), &uid).len();
    assert!(n >= 1);
    hub.on_aps_indication(frame, T0 + 1500);
    assert_eq!(states(&hub.drain_events(), &uid).len(), 0);

    // set_temperature → dataRequest DP 4 = 225 (one frame, DDR set); the
    // dataResponse confirms and updates the state.
    let ack = hub.entity_command(&uid, "set_temperature", &json!({"temperature": 22.5}), T0 + 2000).unwrap();
    assert_eq!(ack.status, "pending");
    sim.run(&mut hub, T0 + 2000);
    let tx = sim.dev(TRV).tuya_rx.last().unwrap().clone();
    assert_eq!(tx.0, cmd::DATA_REQUEST);
    assert_eq!(&tx.1[2..], &[4, 2, 0, 4, 0, 0, 0, 225]);
    let req = sim.aps().into_iter().rev().find(|a| a.cluster == 0xEF00).unwrap().clone();
    assert_eq!(req.payload[0] & 0x10, 0x10, "disable default response");
    let ev = hub.drain_events();
    assert_eq!(result_of(&ev, ack.ticket), Some(Ok("ok".into())));
    assert_eq!(hub.state_of(&uid).unwrap()["attributes"]["target_temperature"], json!(22.5));
    // Mode and temperature in one frame.
    hub.entity_command(&uid, "set_temperature", &json!({"temperature": 20, "hvac_mode": "off"}), T0 + 3000).unwrap();
    sim.run(&mut hub, T0 + 3000);
    let tx = sim.dev(TRV).tuya_rx.last().unwrap().clone();
    assert_eq!(&tx.1[2..], &[2, 4, 0, 1, 2, 4, 2, 0, 4, 0, 0, 0, 200]);
    assert_eq!(state(&hub, &uid), "off");
    // Validation before anything is sent.
    let err = hub.entity_command(&uid, "set_temperature", &json!({"temperature": 40}), T0).unwrap_err();
    assert_eq!(err.code, "invalid_value");
    let err = hub.entity_command(&uid, "set_hvac_mode", &json!({"hvac_mode": "cool"}), T0).unwrap_err();
    assert_eq!(err.code, "invalid_value");
    // A negative calibration: value DP as 4-byte BE i32.
    hub.entity_command(&format!("{id}:1:local_temperature_calibration"), "set_value", &json!({"value": -2}), T0 + 4000).unwrap();
    sim.run(&mut hub, T0 + 4000);
    assert_eq!(&sim.dev(TRV).tuya_rx.last().unwrap().1[2..], &[47, 2, 0, 4, 0xFF, 0xFF, 0xFF, 0xFE]);
    // Schedule text → raw DP via the tuya_schedule_day codec.
    let text = "06:00/21.0 08:00/16.0 12:00/16.0 14:00/16.0 17:00/21.5 22:00/16.0";
    hub.entity_command(&format!("{id}:1:schedule_tuesday"), "set_value", &json!({"value": text}), T0 + 5000).unwrap();
    sim.run(&mut hub, T0 + 5000);
    let tx = sim.dev(TRV).tuya_rx.last().unwrap().clone();
    assert_eq!(&tx.1[2..8], &[29, 0, 0, 25, 2, 6]);
    assert_eq!(state(&hub, &format!("{id}:1:schedule_tuesday")), text);
    assert!(hub.entity_command(&format!("{id}:1:schedule_tuesday"), "set_value", &json!({"value": "06:00/21"}), T0).is_err());
    hub.drain_events();
    sim.log.clear();

    // mcuSyncTime: answered with seconds since 2000 (utc, local).
    let s = sim.dev(TRV).clone();
    hub.on_aps_indication(s.zcl(1, 0xEF00, crate::zcl::FrameType::Cluster, None, 0x21, cmd::MCU_SYNC_TIME, vec![0x00, 0x01]), T0 + 6000);
    let a = hub.transport.aps().into_iter().find(|a| a.cluster == 0xEF00).unwrap().clone();
    let f = crate::zcl::ZclFrame::parse(&a.payload).unwrap();
    assert_eq!(f.header.cmd, cmd::MCU_SYNC_TIME);
    let utc = u32::from_be_bytes(f.payload[2..6].try_into().unwrap());
    assert_eq!(&f.payload[..2], &[0x08, 0x00]);
    assert_eq!(i64::from(utc), (T0 as i64 + 6000) / 1000 - crate::tuya::EPOCH_2000);
    hub.transport.take();
    // mcuGatewayConnectionStatus: connected.
    hub.on_aps_indication(s.zcl(1, 0xEF00, crate::zcl::FrameType::Cluster, None, 0x22, cmd::MCU_GATEWAY_CONNECTION_STATUS, vec![0x00, 0x05]), T0 + 7000);
    let a = hub.transport.aps().into_iter().find(|a| a.cluster == 0xEF00).unwrap().clone();
    assert_eq!(&a.payload[2..], &[cmd::MCU_GATEWAY_CONNECTION_STATUS, 0x01, 0x00, 0x01]);
}

#[test]
fn tuya_th_sensor_queries_on_configure_and_announce() {
    let mut hub = hub_with(&["tuya-ts0601-th-sensor", "tuya-ts0601-trv-me167"]);
    let mut sim = Sim::default();
    let ieee = 0xa4c1380102030405;
    sim.devices.push(
        SimDevice::new(ieee, 0x3002, LogicalType::EndDevice, false, 0x1002)
            .endpoint(1, 0x0104, 0x0051, &[0x0000, 0xEF00], &[0x0019])
            .basic("_TZE200_yjjdcqsq", "TS0601", 0x03),
    );
    sim.join(&mut hub, ieee, T0);
    let id = "zb:a4c1380102030405";
    let d = hub.device(id).unwrap();
    assert_eq!(d.record()["model"], "ZTH01", "white label by manufacturer name");
    assert!(sim.dev(ieee).tuya_rx.iter().any(|(c, _)| *c == cmd::DATA_QUERY));
    let s = sim.dev(ieee).clone();
    hub.on_aps_indication(s.tuya_frame(cmd::DATA_REPORT, 7, &[(1, 2, value(234)), (2, 2, value(56)), (3, 4, vec![2]), (9, 4, vec![0])]), T0 + 1000);
    assert_eq!(state(&hub, &format!("{id}:1:temperature")), json!(23.4));
    assert_eq!(state(&hub, &format!("{id}:1:humidity")), json!(56));
    assert_eq!(state(&hub, &format!("{id}:1:battery_state")), "high");
    assert_eq!(state(&hub, &format!("{id}:1:temperature_unit")), "celsius");
    let (_, e) = hub.entity(&format!("{id}:1:battery_state")).unwrap();
    assert_eq!(e.record(id)["capabilities"]["options"], json!(["low", "medium", "high"]));
    // Device_annce → dataQuery again.
    sim.dev(ieee).tuya_rx.clear();
    hub.on_aps_indication(s.ind(0, 0, 0x0013, crate::zdo::device_annce(1, 0x3002, ieee, 0x80)), T0 + 2000);
    sim.run(&mut hub, T0 + 2000);
    assert!(sim.dev(ieee).tuya_rx.iter().any(|(c, _)| *c == cmd::DATA_QUERY));
}
