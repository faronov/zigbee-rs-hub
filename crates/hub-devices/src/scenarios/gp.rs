//! Green Power: FlowerX GP model variants, BGT1 GP refresh, zigbee-rs GP
//! sensor and switch, EnOcean PTM 216Z, the generic GP tier, and GP
//! bindings (hub-side and on-device through ZRHP remote provisioning).

use serde_json::json;
use zigbee_gp::commissioning::{
    ApplicationInfo, CommissioningPayload, CommissioningSecurity, SwitchInformation,
};
use zigbee_gp::types::{KeyType, SecurityLevel};

use super::*;
use crate::gp::bindings::*;
use crate::gp::types::*;
use crate::testutil::sim::{Sim, SimDevice};
use crate::transport::Sent;
use crate::zcl::{TypedValue, ZclType, ZclValue};
use crate::zdo::LogicalType;

const ALL_GP: [&str; 5] = [
    "gp-diyruz-flowerx",
    "gp-efektalab-bgt1",
    "gp-zigbee-rs-sensor",
    "gp-zigbee-rs-switch",
    "gp-enocean-ptm216z",
];

fn payload(
    device_id: u8,
    mfr: Option<u16>,
    model: Option<u16>,
    cmds: &[u8],
    server: &[u16],
    level: u8,
    switch: Option<u8>,
) -> Vec<u8> {
    let p = CommissioningPayload {
        device_id,
        mac_seq_num_capability: true,
        rx_on_capability: false,
        pan_id_request: false,
        security_key_request: false,
        fixed_location: false,
        security: (level > 0).then_some(CommissioningSecurity {
            level: if level == 3 {
                SecurityLevel::Encrypted
            } else {
                SecurityLevel::FullCounterMic
            },
            key_type: KeyType::IndividualKey,
            key: None,
            outgoing_counter: Some(1),
        }),
        app_info: Some(ApplicationInfo {
            manufacturer_id: mfr,
            model_id: model,
            gpd_commands: (!cmds.is_empty()).then(|| heapless::Vec::from_slice(cmds).unwrap()),
            clusters: (!server.is_empty()).then(|| {
                (
                    heapless::Vec::from_slice(server).unwrap(),
                    heapless::Vec::new(),
                )
            }),
            switch_info: switch.map(|n| SwitchInformation {
                num_contacts: n,
                switch_type: 1,
                contact_status: 0,
            }),
            app_description_follows: false,
        }),
    };
    let mut out = [0u8; 64];
    let n = p.encode(&mut out).unwrap();
    out[..n].to_vec()
}

fn frame(
    gpd: GpdId,
    device_id: u8,
    cmd: u8,
    payload: Vec<u8>,
    counter: u32,
    level: u8,
    result: u8,
) -> GpEvent {
    GpEvent::Frame(GpFrame {
        origin: 2,
        result,
        gpd_id: gpd,
        device_id,
        command_id: cmd,
        frame_counter: counter,
        security_level: level,
        key_type: 4,
        flags: 0,
        proxy_nwk: 0x1234,
        gpp_gpd_link: 0xC8,
        rssi: -60,
        lqi: 180,
        payload,
    })
}

fn sink(gpd: GpdId, device_id: u8, level: u8, counter: u32) -> GpSinkRecord {
    GpSinkRecord {
        gpd_id: gpd,
        device_id,
        communication_mode: 2,
        group: 0x0B84,
        assigned_alias: 0xFFFF,
        flags: 0,
        security_level: level,
        key_type: 4,
        key: (level > 0).then_some([0xAB; 16]),
        frame_counter: counter,
        groupcast_radius: 0,
    }
}

/// Unidirectional commissioning: the payload frame (result 2), then
/// `EVT_GP_COMMISSIONED`.
fn commission(
    hub: &mut Hub<MockTransport>,
    gpd: GpdId,
    device_id: u8,
    p: Vec<u8>,
    level: u8,
    now: u64,
) {
    hub.on_gp_event(
        frame(
            gpd,
            device_id,
            0xE0,
            p,
            1,
            level,
            frame_result::COMMISSIONED,
        ),
        now,
    );
    hub.on_gp_event(
        GpEvent::Commissioned {
            gpd_id: gpd,
            device_id,
            flags: 0,
            record: sink(gpd, device_id, level, 1),
        },
        now,
    );
}

fn a2(recs: &[(u16, u16, TypedValue)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (c, a, v) in recs {
        out.extend_from_slice(&c.to_le_bytes());
        out.extend_from_slice(&a.to_le_bytes());
        out.push(v.ty.0);
        crate::zcl::types::encode_value(v.ty, &v.value, &mut out).unwrap();
    }
    out
}

fn gp_keys(hub: &Hub<MockTransport>, id: &str) -> Vec<String> {
    let mut k: Vec<String> = keys(hub, id)
        .into_iter()
        .map(|k| k.trim_start_matches("242:").to_string())
        .collect();
    k.sort();
    k
}

#[test]
fn flowerx_gp_model_bit_variants() {
    let base = [
        "battery",
        "battery_voltage",
        "frame_counter",
        "illuminance",
        "last_seen",
        "linkquality",
        "rssi",
        "security_level",
        "soil_moisture",
        "temperature",
    ];
    for (model, extra) in [
        (0xF100u16, vec![]),
        (0xF102, vec![]),
        (0xF101, vec!["humidity", "pressure"]),
        (0xF103, vec!["humidity", "pressure", "soil_temperature"]),
    ] {
        let mut hub = hub_with(&ALL_GP);
        let gpd = GpdId::SrcId(0x0BAD_0000 | u32::from(model));
        commission(
            &mut hub,
            gpd,
            0x33,
            payload(0x33, Some(0x6D6B), Some(model), &[0xA2], &[], 0, None),
            0,
            T0,
        );
        let id = gpd.device_id();
        let d = hub.device(&id).unwrap();
        assert_eq!(
            d.resolution.layers[1].id, "diyruz/flowerx-gp",
            "{model:04x}"
        );
        // Spec 13.3: manufacturer + model ID = 600 (GP rows have no extras).
        assert_eq!(d.resolution.layers[1].score, Some(600));
        let mut want: Vec<String> = base
            .iter()
            .chain(extra.iter())
            .map(|s| s.to_string())
            .collect();
        want.sort();
        assert_eq!(gp_keys(&hub, &id), want, "model 0x{model:04x}");
        assert_eq!(
            d.warnings,
            ["gp_unsecured"],
            "level 0 commissioning is flagged"
        );
        assert_eq!(d.record()["model_id"], format!("0x6D6B/0x{model:04X}"));
        assert_eq!(d.record()["manufacturer"], "DIYRuZ");
    }
    // Reports of the full variant.
    let mut hub = hub_with(&ALL_GP);
    let gpd = GpdId::SrcId(0x0100_F103);
    commission(
        &mut hub,
        gpd,
        0x33,
        payload(0x33, Some(0x6D6B), Some(0xF103), &[0xA2], &[], 0, None),
        0,
        T0,
    );
    let id = gpd.device_id();
    let p = a2(&[
        (0x0408, 0, TypedValue::uint(ZclType::U16, 4567)),
        (0x0402, 0, TypedValue::int(ZclType::I16, 2210)),
        (0x0400, 0, TypedValue::uint(ZclType::U16, 30001)),
        (0x0001, 0x0021, TypedValue::uint(ZclType::U8, 150)),
        (0x0001, 0x0020, TypedValue::uint(ZclType::U8, 29)),
        (0x0405, 0, TypedValue::uint(ZclType::U16, 6000)),
        (0x0403, 0, TypedValue::int(ZclType::I16, 1013)),
        (0x0402, 0x5000, TypedValue::int(ZclType::I16, 1450)),
    ]);
    hub.on_gp_event(
        frame(gpd, 0x33, 0xA2, p, 2, 0, frame_result::PASSED_THROUGH),
        T0 + 1000,
    );
    let u = |k: &str| format!("{id}:242:{k}");
    assert_eq!(state(&hub, &u("soil_moisture")), json!(45.67));
    assert_eq!(state(&hub, &u("temperature")), json!(22.1));
    assert_eq!(state(&hub, &u("illuminance")), json!(1000));
    assert_eq!(state(&hub, &u("battery")), json!(75.0));
    assert_eq!(state(&hub, &u("battery_voltage")), json!(2.9));
    assert_eq!(state(&hub, &u("humidity")), json!(60.0));
    assert_eq!(state(&hub, &u("pressure")), json!(1013));
    assert_eq!(state(&hub, &u("soil_temperature")), json!(14.5));
    assert_eq!(state(&hub, &u("frame_counter")), json!(2));
    assert_eq!(state(&hub, &u("rssi")), json!(-60));
    // No known report interval: unavailable after 25 h (D19).
    hub.tick(T0 + 1000 + 25 * 3_600_000 - 1);
    assert!(hub.device(&id).unwrap().available);
    hub.tick(T0 + 1000 + 25 * 3_600_000);
    assert!(!hub.device(&id).unwrap().available);
}

#[test]
fn bgt1_identical_reports_refresh_every_time() {
    let mut hub = hub_with(&ALL_GP);
    let gpd = GpdId::SrcId(0x0200_F202);
    commission(
        &mut hub,
        gpd,
        0x30,
        payload(0x30, Some(0x6D6B), Some(0xF202), &[0xA2], &[], 3, None),
        3,
        T0,
    );
    let id = gpd.device_id();
    assert!(
        hub.device(&id).unwrap().warnings.is_empty(),
        "level 3 ≥ security_min 3"
    );
    hub.drain_events();
    let p = a2(&[
        (0x0402, 0, TypedValue::int(ZclType::I16, 2050)),
        (0x0001, 0x0021, TypedValue::uint(ZclType::U8, 200)),
        (0x0001, 0x0020, TypedValue::uint(ZclType::U8, 30)),
    ]);
    hub.on_gp_event(frame(gpd, 0x30, 0xA2, p.clone(), 10, 3, 1), T0 + 600_000);
    hub.on_gp_event(frame(gpd, 0x30, 0xA2, p, 11, 3, 1), T0 + 1_200_000);
    let ev = hub.drain_events();
    for k in ["temperature", "battery", "battery_voltage"] {
        let st = states(&ev, &format!("{id}:242:{k}"));
        assert_eq!(st.len(), 2, "{k}: one state per report even when unchanged");
        assert_eq!(
            st[1]["last_reported"],
            json!((T0 + 1_200_000) as f64 / 1000.0)
        );
        assert_eq!(st[1]["last_changed"], json!((T0 + 600_000) as f64 / 1000.0));
    }
    assert_eq!(hub.device(&id).unwrap().last_seen, Some(T0 + 1_200_000));
}

#[test]
fn zigbee_rs_gp_sensor_and_switch() {
    let mut hub = hub_with(&ALL_GP);
    let sensor = GpdId::SrcId(0x00F3_0001);
    commission(
        &mut hub,
        sensor,
        0x33,
        payload(0x33, Some(0x6D6B), Some(0xF300), &[0xA2], &[], 3, None),
        3,
        T0,
    );
    let sid = sensor.device_id();
    assert_eq!(
        hub.device(&sid).unwrap().resolution.layers[1].id,
        "zigbee-rs/gpd-sensor-mg22"
    );
    assert_eq!(
        hub.availability_timeout_s(hub.device(&sid).unwrap()),
        Some(180),
        "3 × report_interval_s"
    );
    let p = a2(&[
        (0x0402, 0, TypedValue::int(ZclType::I16, 2138)),
        (0x0405, 0, TypedValue::uint(ZclType::U16, 5000)),
    ]);
    hub.on_gp_event(frame(sensor, 0x33, 0xA2, p, 2, 3, 1), T0 + 1000);
    assert_eq!(state(&hub, &format!("{sid}:242:temperature")), json!(21.38));
    hub.tick(T0 + 1000 + 180_001);
    assert!(!hub.device(&sid).unwrap().available);

    let sw = GpdId::SrcId(0x00F4_0001);
    commission(
        &mut hub,
        sw,
        0x02,
        payload(
            0x02,
            Some(0x6D6B),
            Some(0xF400),
            &[0x20, 0x21, 0x22],
            &[],
            3,
            None,
        ),
        3,
        T0,
    );
    let wid = sw.device_id();
    let uid = format!("{wid}:242:action");
    let (_, e) = hub.entity(&uid).unwrap();
    assert_eq!(e.r.source, crate::model::LayerSource::Catalog);
    assert_eq!(
        e.record(&wid)["capabilities"]["event_types"],
        json!(["off", "on", "toggle"])
    );
    hub.drain_events();
    hub.on_gp_event(frame(sw, 0x02, 0x22, vec![], 5, 3, 1), T0 + 2000);
    hub.on_gp_event(frame(sw, 0x02, 0x22, vec![], 5, 3, 1), T0 + 2050); // second proxy
    hub.on_gp_event(frame(sw, 0x02, 0x20, vec![], 6, 3, 1), T0 + 3000);
    let f = fired(&hub.drain_events(), &uid);
    assert_eq!(
        f.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(),
        ["toggle", "off"]
    );
    // Switches are never unavailable.
    assert_eq!(hub.availability_timeout_s(hub.device(&wid).unwrap()), None);
    hub.tick(T0 + 100 * 3_600_000);
    assert!(hub.device(&wid).unwrap().available);
}

#[test]
fn enocean_ptm216z_contacts() {
    let mut hub = hub_with(&ALL_GP);
    let gpd = GpdId::SrcId(0x0150_1234);
    // Imported from the Sink Table: no commissioning payload, DeviceID only.
    hub.on_gp_event(
        GpEvent::Commissioned {
            gpd_id: gpd,
            device_id: 0x07,
            flags: 0,
            record: sink(gpd, 0x07, 0, 1),
        },
        T0,
    );
    let id = gpd.device_id();
    assert_eq!(
        hub.device(&id).unwrap().resolution.layers[1].id,
        "enocean/ptm216z"
    );
    assert_eq!(
        hub.device(&id).unwrap().resolution.layers[1].score,
        Some(600)
    );
    hub.drain_events();
    let seq = [
        (0x69u8, 0b0001u8),
        (0x6A, 0b0000),
        (0x69, 0b0010),
        (0x69, 0b0110),
        (0x6A, 0b0100),
    ];
    for (i, (c, bits)) in seq.iter().enumerate() {
        hub.on_gp_event(
            frame(gpd, 0x07, *c, vec![*bits], 10 + i as u32, 0, 1),
            T0 + 1000 * (i as u64 + 1),
        );
    }
    let ev = hub.drain_events();
    let at = fired(&ev, &format!("{id}:242:rocker_a_top"));
    assert_eq!(
        at.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(),
        ["press", "release"]
    );
    let ab = fired(&ev, &format!("{id}:242:rocker_a_bottom"));
    assert_eq!(
        ab.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(),
        ["press", "release"]
    );
    let bt = fired(&ev, &format!("{id}:242:rocker_b_top"));
    assert_eq!(
        bt.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(),
        ["press"]
    );
    assert_eq!(
        hub.availability_timeout_s(hub.device(&id).unwrap()),
        None,
        "energy harvesting"
    );
}

#[test]
fn generic_gp_tier() {
    let mut hub = hub_with(&ALL_GP);
    // Hue Tap: DeviceID 0x02 without application information.
    let tap = GpdId::SrcId(0x0040_0001);
    hub.on_gp_event(
        frame(
            tap,
            0x02,
            0xE0,
            payload(0x02, None, None, &[], &[], 0, None)[..2].to_vec(),
            1,
            0,
            2,
        ),
        T0,
    );
    hub.on_gp_event(
        GpEvent::Commissioned {
            gpd_id: tap,
            device_id: 0x02,
            flags: 0,
            record: sink(tap, 0x02, 0, 1),
        },
        T0,
    );
    let id = tap.device_id();
    let uid = format!("{id}:242:action");
    let (_, e) = hub.entity(&uid).unwrap();
    assert_eq!(e.r.def.event_types, ["off", "on", "toggle"]);
    assert_eq!(hub.device(&id).unwrap().record()["model"], "GPD 0x02");
    hub.drain_events();
    hub.on_gp_event(frame(tap, 0x02, 0x22, vec![], 2, 0, 1), T0 + 1000);
    hub.on_gp_event(frame(tap, 0x02, 0x11, vec![], 3, 0, 1), T0 + 2000);
    let ev = hub.drain_events();
    assert!(
        ev.iter()
            .any(|e| matches!(e, HubEvent::EntityUpdated(u) if *u == uid)),
        "event types grow as commands appear"
    );
    let f = fired(&ev, &uid);
    assert_eq!(
        f.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(),
        ["toggle", "recall_scene_1"]
    );
    assert_eq!(f[0].1["command"], 0x22);
    // A GP temperature sensor without a definition: entities from reports.
    let s = GpdId::SrcId(0x0123_4567);
    commission(
        &mut hub,
        s,
        0x30,
        payload(
            0x30,
            Some(0x1234),
            Some(0x0001),
            &[0xA0],
            &[0x0001],
            3,
            None,
        ),
        3,
        T0,
    );
    let sid = s.device_id();
    assert!(
        keys(&hub, &sid).contains(&"242:battery".to_string()),
        "announced clusters up front"
    );
    assert!(!keys(&hub, &sid).contains(&"242:temperature".to_string()));
    let mut p = 0x0402u16.to_le_bytes().to_vec();
    p.extend_from_slice(&[0x00, 0x00, 0x29, 0x34, 0x08]);
    hub.on_gp_event(frame(s, 0x30, 0xA0, p, 2, 3, 1), T0 + 5000);
    assert_eq!(state(&hub, &format!("{sid}:242:temperature")), json!(21.0));
    // Manufacturer-defined commands without a definition are diagnostics.
    hub.on_gp_event(
        frame(s, 0x30, 0xB0, vec![0x34, 0x12, 0x01], 3, 3, 1),
        T0 + 6000,
    );
    assert_eq!(hub.diagnostics(&sid).unwrap()["gp_data"]["176"], "341201");
    // Generic 8-bit vector switch with switch information: per-contact events.
    let v = GpdId::SrcId(0x0222_0001);
    commission(
        &mut hub,
        v,
        0x07,
        payload(0x07, None, None, &[0x69, 0x6A], &[], 0, Some(2)),
        0,
        T0,
    );
    let vid = v.device_id();
    assert!(keys(&hub, &vid).contains(&"242:button_2".to_string()));
    hub.drain_events();
    hub.on_gp_event(frame(v, 0x07, 0x69, vec![0b10], 2, 0, 1), T0 + 7000);
    hub.on_gp_event(frame(v, 0x07, 0x6A, vec![0b00], 3, 0, 1), T0 + 7100);
    let ev = hub.drain_events();
    assert_eq!(
        fired(&ev, &format!("{vid}:242:button_2"))
            .iter()
            .map(|x| x.0.as_str())
            .collect::<Vec<_>>(),
        ["press", "release"]
    );
    assert!(fired(&ev, &format!("{vid}:242:button_1")).is_empty());
}

// ---------------------------------------------------------------------------
// GP bindings.

const LIGHT: u64 = 0x6d6b00000000c0b0;

fn gp_light(functionality: Option<u32>) -> SimDevice {
    let mut d = SimDevice::new(LIGHT, 0x4444, LogicalType::Router, true, 0x6D6B)
        .endpoint(
            11,
            0x0104,
            0x0100,
            &[0x0000, 0x0003, 0x0004, 0x0005, 0x0006],
            &[],
        )
        .endpoint(242, 0xA1E0, 0x0066, &[0x0021], &[0x0021])
        .basic("zigbee-rs", "combo-light", 0x01)
        .attr(
            11,
            0x0006,
            0x0000,
            TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false))),
        );
    if let Some(f) = functionality {
        d = d
            .attr(242, 0x0021, 0x0000, TypedValue::uint(ZclType::U8, 16))
            .attr(
                242,
                0x0021,
                0x0006,
                TypedValue::uint(ZclType::MAP24, u64::from(f)),
            );
    }
    d
}

/// gpsFunctionality of a zigbee-rs combo with a Translation Table: GP
/// feature, direct, derived/pre-commissioned groupcast, full unicast,
/// CT-based commissioning (11), levels 0/2/3 (13, 15, 16), translation
/// table (18).
const COMBO: u32 =
    0x1F | (1 << 7) | (1 << 10) | (1 << 11) | (1 << 13) | (1 << 15) | (1 << 16) | (1 << 18);

struct Bench {
    hub: Hub<MockTransport>,
    sim: Sim,
    sw: GpdId,
}

fn bench(functionality: Option<u32>) -> Bench {
    let mut hub = hub_with(&ALL_GP);
    let mut sim = Sim::default();
    sim.devices.push(gp_light(functionality));
    sim.join(&mut hub, LIGHT, T0);
    let sw = GpdId::SrcId(0x00F4_0002);
    commission(
        &mut hub,
        sw,
        0x02,
        payload(
            0x02,
            Some(0x6D6B),
            Some(0xF400),
            &[0x20, 0x21, 0x22],
            &[],
            3,
            None,
        ),
        3,
        T0,
    );
    hub.transport.take();
    hub.drain_events();
    Bench { hub, sim, sw }
}

fn create(b: &mut Bench, executor: ExecutorChoice) -> u32 {
    let r = b
        .hub
        .create_binding(
            CreateBinding {
                gpd: b.sw.device_id(),
                trigger: BindTrigger {
                    command_id: 0x22,
                    contact_mask: None,
                    contact_value: None,
                },
                target: BindTarget::Device {
                    device: "zb:6d6b00000000c0b0".into(),
                    endpoint: 11,
                },
                action: BindAction {
                    profile: 0x0104,
                    cluster: 0x0006,
                    command: 0x02,
                    payload: TranslationPayload::Fixed(vec![]),
                },
                executor,
            },
            T0 + 1000,
        )
        .unwrap();
    r["id"].as_u64().unwrap() as u32
}

fn binding(b: &Bench, id: u32) -> GpBindingRecord {
    b.hub
        .export_bindings()
        .into_iter()
        .find(|x| x.id == id)
        .unwrap()
}

/// Delivers the hub's APS traffic to the simulator; returns the GP
/// requests it sent.
fn pump(b: &mut Bench, now: u64) -> Vec<(GpRequest, Option<u8>)> {
    b.sim.run(&mut b.hub, now);
    let log = std::mem::take(&mut b.sim.log);
    let mut gp = Vec::new();
    for s in log {
        match s {
            Sent::Gp(r, t) => gp.push((r, t)),
            other => b.sim.log.push(other),
        }
    }
    gp
}

fn ok(b: &mut Bench, tsn: Option<u8>, cmd: u8, now: u64) {
    b.hub.on_gp_event(
        GpEvent::RemoteStatus {
            src_nwk: 0x4444,
            zcl_tsn: tsn.unwrap(),
            command_id: cmd,
            zcl_status: 0,
        },
        now,
    );
}

#[test]
fn hub_side_binding_executes_once_and_events_still_fire() {
    let mut b = bench(None);
    let id = create(&mut b, ExecutorChoice::Hub);
    assert_eq!(binding(&b, id).state, BindState::Active);
    assert_eq!(binding(&b, id).executor, Some(Executor::Hub));
    // The same press from two proxies (same frame counter) → one command.
    b.hub.on_gp_event(
        frame(b.sw, 0x02, 0x22, vec![], 9, 3, frame_result::PASSED_THROUGH),
        T0 + 2000,
    );
    let mut dup = frame(b.sw, 0x02, 0x22, vec![], 9, 3, frame_result::PASSED_THROUGH);
    if let GpEvent::Frame(f) = &mut dup {
        f.proxy_nwk = 0x5555;
    }
    b.hub.on_gp_event(dup, T0 + 2030);
    let sent: Vec<_> = b.hub.transport.aps().into_iter().cloned().collect();
    assert_eq!(sent.len(), 1);
    let a = &sent[0];
    assert_eq!(
        (a.dst, a.dst_ep, a.src_ep, a.profile, a.cluster),
        (crate::transport::ApsDst::Nwk(0x4444), 11, 1, 0x0104, 0x0006)
    );
    assert_eq!(
        a.payload[0], 0x01,
        "cluster-specific, client → server, default response allowed"
    );
    assert_eq!(a.payload[2], 0x02);
    b.sim.run(&mut b.hub, T0 + 2000);
    assert_eq!(
        b.sim.dev(LIGHT).attrs[&(11, 6, 0, None)].value,
        ZclValue::Bool(Some(true))
    );
    let ev = b.hub.drain_events();
    let f = fired(&ev, &format!("{}:242:action", b.sw.device_id()));
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].1["bindings"], json!([id]));
    assert_eq!(f[0].1["executor"], "hub");
    // Derived payload: GPD Step Up → Level Step with the GP default derivation.
    let r = b
        .hub
        .create_binding(
            CreateBinding {
                gpd: b.sw.device_id(),
                trigger: BindTrigger {
                    command_id: 0x32,
                    contact_mask: None,
                    contact_value: None,
                },
                target: BindTarget::Group {
                    group: 0x0100,
                    members: vec![],
                },
                action: BindAction {
                    profile: 0x0104,
                    cluster: 0x0008,
                    command: 0x02,
                    payload: TranslationPayload::Derived,
                },
                executor: ExecutorChoice::Auto,
            },
            T0 + 3000,
        )
        .unwrap();
    assert_eq!(
        r["executor"], "hub",
        "a group without known members runs hub-side"
    );
    b.hub
        .on_gp_event(frame(b.sw, 0x02, 0x32, vec![0x20], 10, 3, 1), T0 + 4000);
    let a = b.hub.transport.aps().into_iter().last().unwrap().clone();
    assert_eq!(a.dst, crate::transport::ApsDst::Group(0x0100));
    assert_eq!(a.payload[0], 0x11, "groupcast: default response disabled");
    assert_eq!(&a.payload[2..], &[0x02, 0x00, 0x20, 0xFF, 0xFF]);
}

#[test]
fn on_device_binding_programs_pairing_and_translation_table() {
    let mut b = bench(Some(COMBO));
    let id = create(&mut b, ExecutorChoice::Auto);
    // Probe: Read gpsMaxSinkTableEntries / gpsFunctionality on EP 242.
    let gp = pump(&mut b, T0 + 1000);
    let probe = b
        .sim
        .aps()
        .into_iter()
        .find(|a| a.cluster == 0x0021 && a.profile != 0)
        .cloned()
        .expect("probe read");
    assert_eq!((probe.dst_ep, probe.profile), (242, 0xA1E0));
    let (req, tsn) = gp.into_iter().next().expect("remote sink table request");
    assert!(matches!(
        req,
        GpRequest::RemoteSinkTableRequest {
            dst: 0x4444,
            index: 0xFF,
            ..
        }
    ));
    assert_eq!(req.zrhp_id(), 0x0A0C);
    b.hub.on_gp_event(
        GpEvent::RemoteSinkTable {
            src_nwk: 0x4444,
            zcl_tsn: tsn.unwrap(),
            status: 0x8B,
            records: vec![],
        },
        T0 + 1100,
    );
    // Step 1: GP Pairing Configuration.
    let gp = pump(&mut b, T0 + 1200);
    let (GpRequest::PairingConfiguration(pc), tsn) = gp[0].clone() else {
        panic!("{gp:?}")
    };
    assert_eq!(gp[0].0.zrhp_id(), 0x0A08);
    assert_eq!(pc.dst, 0x4444);
    assert_eq!(
        pc.actions,
        pairing_actions::EXTEND | pairing_actions::SEND_GP_PAIRING
    );
    assert_eq!(
        (pc.communication_mode, pc.groups.clone(), pc.paired),
        (2, vec![(0x0B84, 0xFFFF)], 0x00)
    );
    assert_eq!(
        (
            pc.security_level,
            pc.key,
            pc.flags & pairing_config_flags::SECURITY
        ),
        (3, [0xAB; 16], pairing_config_flags::SECURITY)
    );
    assert_eq!((pc.manufacturer_id, pc.model_id), (0x6D6B, 0xF400));
    assert_eq!(binding(&b, id).executor, Some(Executor::Device));
    assert_eq!(binding(&b, id).state, BindState::Pending);
    ok(
        &mut b,
        tsn,
        gp_cluster_cmd::PAIRING_CONFIGURATION,
        T0 + 1300,
    );
    // Step 2: GP Translation Table Update.
    let gp = pump(&mut b, T0 + 1300);
    let (
        GpRequest::TranslationUpdate {
            dst,
            action,
            translations,
            ..
        },
        tsn,
    ) = gp[0].clone()
    else {
        panic!()
    };
    assert_eq!(gp[0].0.zrhp_id(), 0x0A0A);
    let want = GpTranslation {
        gpd_command: 0x22,
        endpoint: 11,
        profile: 0x0104,
        cluster: 0x0006,
        zcl_command: 0x02,
        payload: TranslationPayload::Fixed(vec![]),
    };
    assert_eq!(
        (dst, action, translations.clone()),
        (0x4444, translation_action::REPLACE, vec![want.clone()])
    );
    ok(
        &mut b,
        tsn,
        gp_cluster_cmd::TRANSLATION_TABLE_UPDATE,
        T0 + 1400,
    );
    // Step 3: verify with GP Translation Table Request.
    let gp = pump(&mut b, T0 + 1400);
    let (
        GpRequest::TranslationRequest {
            dst: 0x4444,
            start_index: 0,
        },
        tsn,
    ) = gp[0].clone()
    else {
        panic!()
    };
    b.hub.on_gp_event(
        GpEvent::RemoteTranslationTable {
            src_nwk: 0x4444,
            zcl_tsn: tsn.unwrap(),
            status: 0,
            records: vec![(b.sw, want.clone())],
        },
        T0 + 1500,
    );
    assert_eq!(binding(&b, id).state, BindState::Active);
    b.hub.drain_events();
    // A press: no hub-side command (the target executes it), event says so.
    b.hub.transport.take();
    b.hub
        .on_gp_event(frame(b.sw, 0x02, 0x22, vec![], 20, 3, 1), T0 + 2000);
    assert!(b.hub.transport.aps().is_empty());
    let f = fired(
        &b.hub.drain_events(),
        &format!("{}:242:action", b.sw.device_id()),
    );
    assert_eq!(f[0].1["executor"], "device");

    // Drift: the target factory-reset and announces itself.
    let s = b.sim.dev(LIGHT).clone();
    b.hub.on_aps_indication(
        s.ind(
            0,
            0,
            0x0013,
            crate::zdo::device_annce(9, 0x4444, LIGHT, 0x8E),
        ),
        T0 + 3000,
    );
    let gp = pump(&mut b, T0 + 3000);
    let (GpRequest::TranslationRequest { .. }, tsn) = gp
        .iter()
        .find(|g| matches!(g.0, GpRequest::TranslationRequest { .. }))
        .cloned()
        .unwrap()
    else {
        panic!()
    };
    b.hub.on_gp_event(
        GpEvent::RemoteTranslationTable {
            src_nwk: 0x4444,
            zcl_tsn: tsn.unwrap(),
            status: 0,
            records: vec![],
        },
        T0 + 3100,
    );
    assert_eq!(binding(&b, id).state, BindState::Drift);
    let gp = pump(&mut b, T0 + 3100);
    let (GpRequest::PairingConfiguration(pc), _) = gp[0].clone() else {
        panic!()
    };
    assert_eq!(pc.frame_counter, 20, "the current frame counter is re-sent");

    // Executor change device → hub: removal first, then hub-side.
    let mut b = bench(Some(COMBO));
    let id = create(&mut b, ExecutorChoice::Auto);
    let (_, t) = pump(&mut b, T0 + 1000).remove(0);
    b.hub.on_gp_event(
        GpEvent::RemoteSinkTable {
            src_nwk: 0x4444,
            zcl_tsn: t.unwrap(),
            status: 0,
            records: vec![],
        },
        T0 + 1000,
    );
    let (_, t) = pump(&mut b, T0 + 1000).remove(0);
    ok(&mut b, t, 0x09, T0 + 1000);
    let (_, t) = pump(&mut b, T0 + 1000).remove(0);
    ok(&mut b, t, 0x07, T0 + 1000);
    let (_, t) = pump(&mut b, T0 + 1000).remove(0);
    b.hub.on_gp_event(
        GpEvent::RemoteTranslationTable {
            src_nwk: 0x4444,
            zcl_tsn: t.unwrap(),
            status: 0,
            records: vec![(b.sw, want.clone())],
        },
        T0 + 1000,
    );
    assert_eq!(binding(&b, id).state, BindState::Active);
    b.hub
        .update_binding(id, Some(ExecutorChoice::Hub), None, None, T0 + 2000)
        .unwrap();
    assert_eq!(binding(&b, id).state, BindState::Removing);
    // While removing, a press runs nowhere twice: not hub-side yet.
    b.hub.transport.take();
    b.hub
        .on_gp_event(frame(b.sw, 0x02, 0x22, vec![], 30, 3, 1), T0 + 2100);
    let (rm, t) = b
        .hub
        .transport
        .take()
        .into_iter()
        .find_map(|s| match s {
            Sent::Gp(r, t) => Some((r, t)),
            _ => None,
        })
        .map_or_else(|| (None, None), |(r, t)| (Some(r), t));
    assert!(rm.is_none(), "the removal request was sent at update time");
    let _ = t;
    // Replay the removal from the recorded update.
    let mut b2 = b;
    let rec = binding(&b2, id);
    assert_eq!(rec.targets[0].step, TargetStep::RemoveTranslation);
    let tsn = *b2.hub.gp.remote.keys().next().unwrap();
    ok(&mut b2, Some(tsn), 0x07, T0 + 2200);
    let gp = pump(&mut b2, T0 + 2200);
    let (GpRequest::PairingConfiguration(pc), t) = gp[0].clone() else {
        panic!("{gp:?}")
    };
    assert_eq!(
        pc.actions,
        pairing_actions::REMOVE_PAIRING | pairing_actions::SEND_GP_PAIRING
    );
    ok(&mut b2, t, 0x09, T0 + 2300);
    assert_eq!(binding(&b2, id).executor, Some(Executor::Hub));
    assert_eq!(binding(&b2, id).state, BindState::Active);
    b2.hub.transport.take();
    b2.hub
        .on_gp_event(frame(b2.sw, 0x02, 0x22, vec![], 31, 3, 1), T0 + 2400);
    assert_eq!(b2.hub.transport.aps().len(), 1, "now hub-side");
}

#[test]
fn on_device_binding_failures() {
    // FAILURE from the target at the Translation Table Update.
    let mut b = bench(Some(COMBO));
    let id = create(&mut b, ExecutorChoice::Device);
    let (_, t) = pump(&mut b, T0 + 1000).remove(0);
    b.hub.on_gp_event(
        GpEvent::RemoteSinkTable {
            src_nwk: 0x4444,
            zcl_tsn: t.unwrap(),
            status: 0,
            records: vec![],
        },
        T0 + 1000,
    );
    let (_, t) = pump(&mut b, T0 + 1000).remove(0);
    ok(&mut b, t, 0x09, T0 + 1000);
    let (_, t) = pump(&mut b, T0 + 1000).remove(0);
    b.hub.on_gp_event(
        GpEvent::RemoteStatus {
            src_nwk: 0x4444,
            zcl_tsn: t.unwrap(),
            command_id: 0x07,
            zcl_status: 0x01,
        },
        T0 + 1000,
    );
    let rec = binding(&b, id);
    assert!(
        matches!(&rec.state, BindState::Failed { zcl_status: Some(s), .. } if s == "FAILURE"),
        "{:?}",
        rec.state
    );
    let ev = b.hub.drain_events();
    assert!(ev.iter().any(|e| matches!(e, HubEvent::Gp { kind, body } if kind == "gp/binding" && body["zcl_status"] == "FAILURE")));

    // No answer: 2 retries, then failed with a timeout.
    let mut b = bench(Some(COMBO));
    let id = create(&mut b, ExecutorChoice::Device);
    let (_, t) = pump(&mut b, T0 + 1000).remove(0);
    b.hub.on_gp_event(
        GpEvent::RemoteSinkTable {
            src_nwk: 0x4444,
            zcl_tsn: t.unwrap(),
            status: 0,
            records: vec![],
        },
        T0 + 1000,
    );
    let first = pump(&mut b, T0 + 1000);
    assert!(matches!(first[0].0, GpRequest::PairingConfiguration(_)));
    for i in 1..=3u64 {
        b.hub.tick(T0 + 1000 + i * 5_001);
    }
    let retries = b
        .hub
        .transport
        .take()
        .into_iter()
        .filter(|s| matches!(s, Sent::Gp(GpRequest::PairingConfiguration(_), _)))
        .count();
    assert_eq!(retries, 2);
    assert!(
        matches!(&binding(&b, id).state, BindState::Failed { reason, .. } if reason.contains("timeout"))
    );

    // A proxy-only lamp (UNSUPPORTED_ATTRIBUTE): auto → hub, device → unsupported.
    let mut b = bench(None);
    let auto = create(&mut b, ExecutorChoice::Auto);
    pump(&mut b, T0 + 1000);
    assert_eq!(binding(&b, auto).executor, Some(Executor::Hub));
    assert_eq!(binding(&b, auto).state, BindState::Active);
    let forced = create(&mut b, ExecutorChoice::Device);
    pump(&mut b, T0 + 1000);
    assert_eq!(binding(&b, forced).state, BindState::Unsupported);
    // A target without the GPD's security level does not qualify.
    let mut b = bench(Some(COMBO & !(1 << 16)));
    let id = create(&mut b, ExecutorChoice::Auto);
    let (_, t) = pump(&mut b, T0 + 1000).remove(0);
    b.hub.on_gp_event(
        GpEvent::RemoteSinkTable {
            src_nwk: 0x4444,
            zcl_tsn: t.unwrap(),
            status: 0,
            records: vec![],
        },
        T0 + 1000,
    );
    assert_eq!(binding(&b, id).executor, Some(Executor::Hub));
}

#[test]
fn removing_a_gpd_removes_its_on_device_bindings_first() {
    let mut b = bench(Some(COMBO));
    let id = create(&mut b, ExecutorChoice::Auto);
    let (_, t) = pump(&mut b, T0 + 1000).remove(0);
    b.hub.on_gp_event(
        GpEvent::RemoteSinkTable {
            src_nwk: 0x4444,
            zcl_tsn: t.unwrap(),
            status: 0,
            records: vec![],
        },
        T0 + 1000,
    );
    let (_, t) = pump(&mut b, T0 + 1000).remove(0);
    ok(&mut b, t, 0x09, T0 + 1000);
    let (_, t) = pump(&mut b, T0 + 1000).remove(0);
    ok(&mut b, t, 0x07, T0 + 1000);
    pump(&mut b, T0 + 1000);
    b.hub.transport.take();
    b.hub
        .remove_device(&b.sw.device_id(), false, T0 + 2000)
        .unwrap();
    let sent = b.hub.transport.take();
    let order: Vec<u16> = sent
        .iter()
        .filter_map(|s| match s {
            Sent::Gp(r, _) => Some(r.zrhp_id()),
            _ => None,
        })
        .collect();
    assert_eq!(
        order,
        [0x0A0A, 0x0A06],
        "Translation Table Update (remove) before GP_SINK_REMOVE"
    );
    assert!(matches!(&sent.iter().find_map(|s| match s {
        Sent::Gp(GpRequest::TranslationUpdate { action, .. }, _) => Some(*action),
        _ => None,
    }), Some(a) if *a == translation_action::REMOVE));
    assert_eq!(binding(&b, id).state, BindState::Removing);
    assert!(b.hub.device(&b.sw.device_id()).is_none());
}
