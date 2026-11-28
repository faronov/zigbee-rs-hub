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
        .endpoint(
            1,
            0x0104,
            0x0051,
            &[0x0000, 0x0004, 0x0005, 0xEF00],
            &[0x000A, 0x0019],
        )
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
    super::assert_zdef_resolve_agrees(&hub, id, "tuya-ts0601-trv-me167");
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
    let magic = sim
        .aps()
        .into_iter()
        .find(|a| a.cluster == 0 && a.profile != 0 && a.payload.len() == 3 + 12)
        .cloned()
        .expect("magic packet");
    assert_eq!(
        &magic.payload[3..],
        &[0x04, 0, 0, 0, 0x01, 0, 0x05, 0, 0x07, 0, 0xFE, 0xFF]
    );
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
    for (h, m, t) in [
        (6, 0, 200),
        (8, 0, 160),
        (11, 30, 160),
        (13, 30, 160),
        (17, 0, 210),
        (22, 0, 160),
    ] {
        sched.extend_from_slice(&[h, m]);
        sched.extend_from_slice(&(t as u16).to_be_bytes());
    }
    let s = sim.dev(TRV).clone();
    let frame = s.tuya_frame(
        cmd::DATA_REPORT,
        1,
        &[
            (2, 4, vec![1]),
            (3, 4, vec![1]),
            (4, 2, value(215)),
            (5, 2, value(198)),
            (7, 1, vec![1]),
            (35, 2, value(1)),
            (28, 0, sched),
            (104, 2, value(37)),
        ],
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
    assert_eq!(
        state(&hub, &format!("{id}:1:schedule_monday")),
        "06:00/20.0 08:00/16.0 11:30/16.0 13:30/16.0 17:00/21.0 22:00/16.0"
    );
    let diag = hub.diagnostics(id).unwrap();
    assert_eq!(
        diag["tuya_dps"][0]["dp"], 104,
        "unknown DPs are kept as diagnostics"
    );
    // The same frame again within 2 s is a duplicate.
    let n = states(&hub.drain_events(), &uid).len();
    assert!(n >= 1);
    hub.on_aps_indication(frame, T0 + 1500);
    assert_eq!(states(&hub.drain_events(), &uid).len(), 0);

    // set_temperature → dataRequest DP 4 = 225 (one frame, DDR set); the
    // dataResponse confirms and updates the state.
    let ack = hub
        .entity_command(
            &uid,
            "set_temperature",
            &json!({"temperature": 22.5}),
            T0 + 2000,
        )
        .unwrap();
    assert_eq!(ack.status, "pending");
    sim.run(&mut hub, T0 + 2000);
    let tx = sim.dev(TRV).tuya_rx.last().unwrap().clone();
    assert_eq!(tx.0, cmd::DATA_REQUEST);
    assert_eq!(&tx.1[2..], &[4, 2, 0, 4, 0, 0, 0, 225]);
    let req = sim
        .aps()
        .into_iter()
        .rev()
        .find(|a| a.cluster == 0xEF00)
        .unwrap()
        .clone();
    assert_eq!(req.payload[0] & 0x10, 0x10, "disable default response");
    let ev = hub.drain_events();
    assert_eq!(result_of(&ev, ack.ticket), Some(Ok("ok".into())));
    assert_eq!(
        hub.state_of(&uid).unwrap()["attributes"]["target_temperature"],
        json!(22.5)
    );
    // Mode and temperature in one frame.
    hub.entity_command(
        &uid,
        "set_temperature",
        &json!({"temperature": 20, "hvac_mode": "off"}),
        T0 + 3000,
    )
    .unwrap();
    sim.run(&mut hub, T0 + 3000);
    let tx = sim.dev(TRV).tuya_rx.last().unwrap().clone();
    assert_eq!(&tx.1[2..], &[2, 4, 0, 1, 2, 4, 2, 0, 4, 0, 0, 0, 200]);
    assert_eq!(state(&hub, &uid), "off");
    // Validation before anything is sent.
    let err = hub
        .entity_command(&uid, "set_temperature", &json!({"temperature": 40}), T0)
        .unwrap_err();
    assert_eq!(err.code, "invalid_value");
    let err = hub
        .entity_command(&uid, "set_hvac_mode", &json!({"hvac_mode": "cool"}), T0)
        .unwrap_err();
    assert_eq!(err.code, "invalid_value");
    // A negative calibration: value DP as 4-byte BE i32.
    hub.entity_command(
        &format!("{id}:1:local_temperature_calibration"),
        "set_value",
        &json!({"value": -2}),
        T0 + 4000,
    )
    .unwrap();
    sim.run(&mut hub, T0 + 4000);
    assert_eq!(
        &sim.dev(TRV).tuya_rx.last().unwrap().1[2..],
        &[47, 2, 0, 4, 0xFF, 0xFF, 0xFF, 0xFE]
    );
    // Schedule text → raw DP via the tuya_schedule_day codec.
    let text = "06:00/21.0 08:00/16.0 12:00/16.0 14:00/16.0 17:00/21.5 22:00/16.0";
    hub.entity_command(
        &format!("{id}:1:schedule_tuesday"),
        "set_value",
        &json!({"value": text}),
        T0 + 5000,
    )
    .unwrap();
    sim.run(&mut hub, T0 + 5000);
    let tx = sim.dev(TRV).tuya_rx.last().unwrap().clone();
    assert_eq!(&tx.1[2..8], &[29, 0, 0, 25, 2, 6]);
    assert_eq!(state(&hub, &format!("{id}:1:schedule_tuesday")), text);
    assert!(
        hub.entity_command(
            &format!("{id}:1:schedule_tuesday"),
            "set_value",
            &json!({"value": "06:00/21"}),
            T0
        )
        .is_err()
    );
    hub.drain_events();
    sim.log.clear();

    // mcuSyncTime: answered with seconds since 2000 (utc, local).
    let s = sim.dev(TRV).clone();
    hub.on_aps_indication(
        s.zcl(
            1,
            0xEF00,
            crate::zcl::FrameType::Cluster,
            None,
            0x21,
            cmd::MCU_SYNC_TIME,
            vec![0x00, 0x01],
        ),
        T0 + 6000,
    );
    let a = hub
        .transport
        .aps()
        .into_iter()
        .find(|a| a.cluster == 0xEF00)
        .unwrap()
        .clone();
    let f = crate::zcl::ZclFrame::parse(&a.payload).unwrap();
    assert_eq!(f.header.cmd, cmd::MCU_SYNC_TIME);
    let utc = u32::from_be_bytes(f.payload[2..6].try_into().unwrap());
    assert_eq!(&f.payload[..2], &[0x08, 0x00]);
    assert_eq!(
        i64::from(utc),
        (T0 as i64 + 6000) / 1000 - crate::tuya::EPOCH_2000
    );
    hub.transport.take();
    // The Time cluster server on the hub endpoint (ZCL 3.12).
    let utc = ((T0 as i64 + 7000) / 1000 - crate::hub::ZCL_EPOCH_UNIX) as u32;
    hub.cfg.clock = crate::hub::Clock {
        synced: true,
        tz_s: 7200,
        dst: Some((utc - 10, utc + 3600, 3600)),
        last_set: Some(utc - 60),
    };
    hub.on_aps_indication(
        s.zcl(
            1,
            0x000A,
            crate::zcl::FrameType::Global,
            None,
            0x22,
            0x00,
            crate::zcl::frame::read_attributes(&[0x0000, 0x0001, 0x0002, 0x0007, 0x0042]),
        ),
        T0 + 7000,
    );
    let a = hub
        .transport
        .aps()
        .into_iter()
        .find(|a| a.cluster == 0x000A)
        .unwrap()
        .clone();
    let f = crate::zcl::ZclFrame::parse(&a.payload).unwrap();
    assert_eq!((f.header.cmd, f.header.tsn, a.src_ep), (0x01, 0x22, 1));
    let recs = crate::zcl::frame::parse_read_attributes_rsp(&f.payload).unwrap();
    let v = |i: usize| recs[i].value.as_ref().map(|v| v.value.clone());
    assert_eq!(v(0), Some(crate::zcl::ZclValue::Uint(u64::from(utc))));
    assert_eq!(v(1), Some(crate::zcl::ZclValue::Uint(0x07)));
    assert_eq!(v(2), Some(crate::zcl::ZclValue::Int(7200)));
    // Local time: UTC + 2 h + 1 h of daylight saving.
    assert_eq!(
        v(3),
        Some(crate::zcl::ZclValue::Uint(u64::from(utc + 10800)))
    );
    assert_eq!(recs[4].status, crate::zcl::Status::UNSUPPORTED_ATTRIBUTE);
    hub.transport.take();
    // mcuGatewayConnectionStatus: connected.
    hub.on_aps_indication(
        s.zcl(
            1,
            0xEF00,
            crate::zcl::FrameType::Cluster,
            None,
            0x22,
            cmd::MCU_GATEWAY_CONNECTION_STATUS,
            vec![0x00, 0x05],
        ),
        T0 + 7000,
    );
    let a = hub
        .transport
        .aps()
        .into_iter()
        .find(|a| a.cluster == 0xEF00)
        .unwrap()
        .clone();
    assert_eq!(
        &a.payload[2..],
        &[cmd::MCU_GATEWAY_CONNECTION_STATUS, 0x01, 0x00, 0x01]
    );
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
    super::assert_zdef_resolve_agrees(&hub, id, "tuya-ts0601-th-sensor");
    let d = hub.device(id).unwrap();
    assert_eq!(
        d.record()["model"],
        "ZTH01",
        "white label by manufacturer name"
    );
    assert!(
        sim.dev(ieee)
            .tuya_rx
            .iter()
            .any(|(c, _)| *c == cmd::DATA_QUERY)
    );
    let s = sim.dev(ieee).clone();
    hub.on_aps_indication(
        s.tuya_frame(
            cmd::DATA_REPORT,
            7,
            &[
                (1, 2, value(234)),
                (2, 2, value(56)),
                (3, 4, vec![2]),
                (9, 4, vec![0]),
            ],
        ),
        T0 + 1000,
    );
    assert_eq!(state(&hub, &format!("{id}:1:temperature")), json!(23.4));
    assert_eq!(state(&hub, &format!("{id}:1:humidity")), json!(56));
    assert_eq!(state(&hub, &format!("{id}:1:battery_state")), "high");
    assert_eq!(state(&hub, &format!("{id}:1:temperature_unit")), "celsius");
    let (_, e) = hub.entity(&format!("{id}:1:battery_state")).unwrap();
    assert_eq!(
        e.record(id)["capabilities"]["options"],
        json!(["low", "medium", "high"])
    );
    // Device_annce → dataQuery again.
    sim.dev(ieee).tuya_rx.clear();
    hub.on_aps_indication(
        s.ind(
            0,
            0,
            0x0013,
            crate::zdo::device_annce(1, 0x3002, ieee, 0x80),
        ),
        T0 + 2000,
    );
    sim.run(&mut hub, T0 + 2000);
    assert!(
        sim.dev(ieee)
            .tuya_rx
            .iter()
            .any(|(c, _)| *c == cmd::DATA_QUERY)
    );
}

/// `pushed` (local automations' `reported`) by context, not by command:
/// a TS0601 data frame, dataResponse too, is the device's own unless it
/// answers the hub's dataQuery (any DP) or DP write (that DP) that went
/// out less than 10 s before.
#[test]
fn tuya_data_frames_are_the_devices_own_unless_they_answer_the_hub() {
    let mut hub = hub_with(&["tuya-ts0601-th-sensor", "tuya-ts0601-trv-me167"]);
    let mut sim = Sim::default();
    let ieee = 0xa4c1380102030405;
    sim.devices.push(
        SimDevice::new(ieee, 0x3002, LogicalType::EndDevice, false, 0x1002)
            .endpoint(1, 0x0104, 0x0051, &[0x0000, 0xEF00], &[0x0019])
            .basic("_TZE200_yjjdcqsq", "TS0601", 0x03),
    );
    sim.devices.push(trv());
    sim.join(&mut hub, ieee, T0);
    sim.join(&mut hub, TRV, T0);
    let temp = "zb:a4c1380102030405:1:temperature";
    let s = sim.dev(ieee).clone();
    let frame = |hub: &mut Hub<MockTransport>, c: u8, seq: u16, t: u64| -> Vec<bool> {
        hub.drain_events();
        hub.on_aps_indication(
            s.tuya_frame(c, seq, &[(1, 2, value(200 + i32::from(seq)))]),
            T0 + t,
        );
        states(&hub.drain_events(), temp)
            .iter()
            .map(|m| m["pushed"] == true)
            .collect()
    };
    // The configure query went out at the join, long ago.
    assert_eq!(
        frame(&mut hub, cmd::DATA_RESPONSE, 1, 60_000),
        [true],
        "a dataResponse nobody asked for"
    );
    assert_eq!(frame(&mut hub, cmd::DATA_REPORT, 2, 61_000), [true]);
    let queried = |sim: &mut Sim| {
        let q = sim
            .dev(ieee)
            .tuya_rx
            .iter()
            .any(|(c, _)| *c == cmd::DATA_QUERY);
        sim.dev(ieee).tuya_rx.clear();
        q
    };
    queried(&mut sim);
    // The refresh after a restart queues a dataQuery for the sleeping
    // sensor: the uplink that releases it is no answer, what follows is.
    assert!(hub.refresh_unknown(T0 + 67_000) > 0);
    sim.run(&mut hub, T0 + 67_000);
    assert!(!queried(&mut sim), "queued");
    assert_eq!(
        frame(&mut hub, cmd::DATA_RESPONSE, 3, 68_000),
        [true],
        "the press that wakes it"
    );
    sim.run(&mut hub, T0 + 68_000);
    assert!(queried(&mut sim), "sent");
    assert_eq!(
        frame(&mut hub, cmd::DATA_RESPONSE, 4, 68_500),
        [false],
        "the query's answer"
    );
    assert_eq!(frame(&mut hub, cmd::DATA_RESPONSE, 5, 79_000), [true]);
    // Device_annce → dataQuery: what follows answers it, either command.
    hub.on_aps_indication(
        s.ind(
            0,
            0,
            0x0013,
            crate::zdo::device_annce(1, 0x3002, ieee, 0x80),
        ),
        T0 + 80_000,
    );
    sim.run(&mut hub, T0 + 80_000);
    assert!(queried(&mut sim));
    assert_eq!(frame(&mut hub, cmd::DATA_RESPONSE, 6, 81_000), [false]);
    assert_eq!(frame(&mut hub, cmd::DATA_REPORT, 7, 89_000), [false]);
    assert_eq!(frame(&mut hub, cmd::DATA_RESPONSE, 8, 91_000), [true]);

    // A DP write: its answer (dataResponse) is not the device's own, nor
    // the command's completion; another DP in the meantime is, and so is
    // the written DP later (the knob turned).
    let uid = "zb:a4c138aabbccddee:1:thermostat";
    let lock = "zb:a4c138aabbccddee:1:child_lock";
    let t = sim.dev(TRV).clone();
    // The TRV's refresh query, queued too, goes out with its next frame:
    // 10 s later, a frame wakes it for the write.
    hub.on_aps_indication(
        t.tuya_frame(cmd::DATA_REPORT, 1, &[(4, 2, value(200))]),
        T0 + 100_000,
    );
    sim.run(&mut hub, T0 + 100_000);
    hub.on_aps_indication(
        t.tuya_frame(cmd::DATA_REPORT, 9, &[(4, 2, value(205))]),
        T0 + 110_000,
    );
    hub.drain_events();
    let ack = hub
        .entity_command(
            uid,
            "set_temperature",
            &json!({"temperature": 22.5}),
            T0 + 110_500,
        )
        .unwrap();
    assert_eq!(ack.status, "pending");
    sim.run(&mut hub, T0 + 110_500);
    let ev = hub.drain_events();
    assert_eq!(result_of(&ev, ack.ticket), Some(Ok("ok".into())));
    let p: Vec<bool> = states(&ev, uid)
        .iter()
        .map(|m| m["pushed"] == true)
        .collect();
    assert!(
        !p.is_empty() && !p.contains(&true),
        "the write's answer: {p:?}"
    );
    let pushed = |hub: &mut Hub<MockTransport>, f, at: u64, uid: &str| -> Vec<bool> {
        hub.on_aps_indication(f, T0 + at);
        states(&hub.drain_events(), uid)
            .iter()
            .map(|m| m["pushed"] == true)
            .collect()
    };
    let f = t.tuya_frame(cmd::DATA_RESPONSE, 2, &[(7, 1, vec![1])]);
    assert_eq!(pushed(&mut hub, f, 111_000, lock), [true], "another DP");
    let f = t.tuya_frame(cmd::DATA_REPORT, 3, &[(4, 2, value(225))]);
    assert_eq!(
        pushed(&mut hub, f, 112_000, uid),
        [false],
        "the write's report"
    );
    let f = t.tuya_frame(cmd::DATA_RESPONSE, 4, &[(4, 2, value(230))]);
    assert_eq!(pushed(&mut hub, f, 121_000, uid), [true], "the knob");
    // A slow answer: past the 10 s, while the command still waits for it.
    let ack = hub
        .entity_command(
            uid,
            "set_temperature",
            &json!({"temperature": 23}),
            T0 + 121_500,
        )
        .unwrap();
    assert_eq!(ack.status, "pending");
    hub.transport.take();
    let f = t.tuya_frame(cmd::DATA_RESPONSE, 5, &[(4, 2, value(230))]);
    assert_eq!(pushed(&mut hub, f, 133_000, uid), [false], "a slow answer");
    let f = t.tuya_frame(cmd::DATA_RESPONSE, 6, &[(4, 2, value(235))]);
    assert_eq!(pushed(&mut hub, f, 140_000, uid), [true]);
}
