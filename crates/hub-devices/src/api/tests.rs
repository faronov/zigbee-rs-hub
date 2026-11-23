//! WebSocket API flows over the engine (no socket: text frames in, text
//! frames out).

use serde_json::{Value as J, json};

use super::*;
use crate::gp::types::GpRequest;
use crate::scenarios::{T0, golden_cbor, hub_with};
use crate::testutil::sim::{Sim, SimDevice};
use crate::transport::MockTransport;
use crate::zcl::{TypedValue, ZclType, ZclValue};
use crate::zdo::LogicalType;

fn parse(frames: &[String]) -> Vec<J> {
    frames
        .iter()
        .map(|f| serde_json::from_str(f).unwrap())
        .collect()
}

fn call(api: &mut ApiServer, hub: &mut Hub<MockTransport>, s: SessionId, id: u64, body: J) -> J {
    let mut req = body;
    req["id"] = id.into();
    let out = parse(&api.handle_text(hub, s, &req.to_string(), T0 + 10_000));
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(out[0]["id"], id);
    out[0].clone()
}

fn light() -> SimDevice {
    SimDevice::new(
        0x0017880100aabbcc,
        0x2001,
        LogicalType::Router,
        true,
        0x100B,
    )
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

#[test]
fn auth_subscribe_snapshot_deltas_and_commands() {
    let mut hub = hub_with(&["innr-sp120"]);
    let mut api = ApiServer::new("zrs-hub-7cdfa1", "secret");
    let (s, hello) = api.connect();
    assert_eq!(
        serde_json::from_str::<J>(&hello).unwrap()["type"],
        "auth_required"
    );
    let bad = parse(&api.handle_text(&mut hub, s, r#"{"type": "auth", "token": "nope"}"#, T0));
    assert_eq!(bad[0]["type"], "auth_invalid");
    let ok = parse(&api.handle_text(&mut hub, s, r#"{"type": "auth", "token": "secret"}"#, T0));
    assert_eq!(ok[0]["type"], "auth_ok");
    assert_eq!(ok[0]["api_version"], "1.3");
    assert_eq!(ok[0]["zdef_versions"], json!([1]));
    assert!(ok[0]["features"].as_array().unwrap().contains(&json!("gp")));

    // Subscribe: result + snapshot with the current seq.
    let out = parse(&api.handle_text(
        &mut hub,
        s,
        r#"{"id": 18, "type": "subscribe", "topics": ["devices", "entities", "events", "gp"]}"#,
        T0,
    ));
    assert_eq!(out[0]["success"], true);
    assert_eq!(out[1]["type"], "event");
    assert_eq!(out[1]["event"]["topic"], "snapshot");
    assert_eq!(out[1]["event"]["devices"], json!([]));

    // A device joins: deltas with increasing seq.
    let mut sim = Sim::default();
    sim.devices.push(light());
    sim.join(&mut hub, 0x0017880100aabbcc, T0);
    let ev = api.pump(&mut hub);
    let ev: Vec<J> = ev
        .iter()
        .map(|(sid, t)| {
            assert_eq!(*sid, s);
            serde_json::from_str(t).unwrap()
        })
        .collect();
    let seqs: Vec<u64> = ev.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1));
    let topics: Vec<&str> = ev
        .iter()
        .map(|e| e["event"]["topic"].as_str().unwrap())
        .collect();
    assert_eq!(topics[0], "device_added");
    assert!(
        topics.contains(&"entity_added")
            && topics.contains(&"entity_state")
            && topics.contains(&"interview_progress")
    );
    let added = ev
        .iter()
        .find(|e| {
            e["event"]["topic"] == "entity_added" && e["event"]["entity"]["platform"] == "light"
        })
        .unwrap();
    assert_eq!(
        added["event"]["entity"]["uid"],
        "zb:0017880100aabbcc:11:light"
    );

    // devices/list, entities/list, entities/states.
    let r = call(&mut api, &mut hub, s, 1, json!({"type": "devices/list"}));
    assert_eq!(r["result"][0]["id"], "zb:0017880100aabbcc");
    assert_eq!(r["result"][0]["nwk"], 0x2001);
    assert_eq!(r["result"][0]["interview"], "complete");
    let r = call(
        &mut api,
        &mut hub,
        s,
        2,
        json!({"type": "entities/list", "device": "zb:0017880100aabbcc"}),
    );
    assert!(r["result"].as_array().unwrap().len() >= 4);
    let r = call(
        &mut api,
        &mut hub,
        s,
        3,
        json!({"type": "entities/states", "uids": ["zb:0017880100aabbcc:11:light"]}),
    );
    assert_eq!(r["result"][0]["state"], "off");
    let r = call(
        &mut api,
        &mut hub,
        s,
        4,
        json!({"type": "devices/get", "device": "zb:nope"}),
    );
    assert_eq!(r["error"]["code"], "not_found");

    // entities/command: the result follows the device's confirmation.
    let out = api.handle_text(&mut hub, s, r#"{"id": 20, "type": "entities/command", "uid": "zb:0017880100aabbcc:11:light", "command": "turn_on", "params": {"brightness": 77}}"#, T0 + 1000);
    assert!(out.is_empty());
    sim.run(&mut hub, T0 + 1000);
    let ev: Vec<J> = api
        .pump(&mut hub)
        .iter()
        .map(|(_, t)| serde_json::from_str(t).unwrap())
        .collect();
    let res = ev.iter().find(|e| e["type"] == "result").unwrap();
    assert_eq!(
        (res["id"].as_u64(), &res["result"]["status"]),
        (Some(20), &json!("ok"))
    );
    let st = ev
        .iter()
        .rfind(|e| {
            e["event"]["topic"] == "entity_state"
                && e["event"]["uid"] == "zb:0017880100aabbcc:11:light"
        })
        .unwrap();
    assert_eq!(st["event"]["attributes"]["brightness"], 77);
    // Validation errors come back immediately.
    let out = parse(&api.handle_text(&mut hub, s, r#"{"id": 21, "type": "entities/command", "uid": "zb:0017880100aabbcc:11:light", "command": "set_temperature", "params": {}}"#, T0));
    assert_eq!(out[0]["error"]["code"], "unsupported");
    let out = parse(&api.handle_text(&mut hub, s, r#"{"id": 22, "type": "bogus"}"#, T0));
    assert_eq!(out[0]["error"]["code"], "invalid_format");
    assert_eq!(out[0]["id"], 22);
    // entities/update renames (HA user-editable name) and devices/update.
    let r = call(
        &mut api,
        &mut hub,
        s,
        5,
        json!({"type": "entities/update", "uid": "zb:0017880100aabbcc:11:light", "name": "Desk"}),
    );
    assert_eq!(r["result"]["name"], "Desk");
    let r = call(
        &mut api,
        &mut hub,
        s,
        6,
        json!({"type": "devices/update", "device": "zb:0017880100aabbcc", "name": "Desk lamp"}),
    );
    assert_eq!(r["result"]["name"], "Desk lamp");
    // permit_join with the GP window.
    let r = call(
        &mut api,
        &mut hub,
        s,
        7,
        json!({"type": "network/permit_join", "duration_s": 120, "include_gp": true}),
    );
    assert_eq!(r["success"], true);
    let sent = hub.transport.take();
    assert!(
        sent.iter()
            .any(|x| matches!(x, crate::transport::Sent::PermitJoin(120, None)))
    );
    assert!(sent.iter().any(|x| matches!(
        x,
        crate::transport::Sent::Gp(
            GpRequest::SinkCommissioning {
                action: 1,
                window_s: 120,
                ..
            },
            _
        )
    )));
}

#[test]
fn definitions_validate_upload_preview_scaffold() {
    let mut hub = hub_with(&[]);
    let mut api = ApiServer::new("hub", "t");
    let (s, _) = api.connect();
    api.handle_text(&mut hub, s, r#"{"type": "auth", "token": "t"}"#, T0);
    let mut sim = Sim::default();
    sim.devices.push(
        SimDevice::new(
            0x00124b0012345678,
            0x4001,
            LogicalType::Router,
            true,
            0x1234,
        )
        .endpoint(1, 0x0104, 0x000C, &[0x0000, 0x0402, 0x0405, 0x040D], &[])
        .basic("EfektaLab", "EFEKTA_CO2_Smart_Monitor", 0x01)
        .attr(1, 0x0402, 0x0410, TypedValue::int(ZclType::I16, -15)),
    );
    sim.join(&mut hub, 0x00124b0012345678, T0);
    let dev = "zb:00124b0012345678";
    let cbor =
        base64::engine::general_purpose::STANDARD.encode(golden_cbor("efekta-co2-smart-monitor"));
    let r = call(
        &mut api,
        &mut hub,
        s,
        1,
        json!({"type": "definitions/validate", "cbor": cbor}),
    );
    assert_eq!(r["result"]["ok"], true);
    assert_eq!(r["result"]["matches"], json!([dev]));
    assert!(r["result"]["content_hash"].as_str().unwrap().len() >= 16);
    let bad = base64::engine::general_purpose::STANDARD.encode([0xA1, 0x00, 0x02]);
    let r = call(
        &mut api,
        &mut hub,
        s,
        2,
        json!({"type": "definitions/validate", "cbor": bad}),
    );
    assert_eq!(r["result"]["ok"], false);
    // Preview: what the device would get, nothing applied.
    let r = call(
        &mut api,
        &mut hub,
        s,
        3,
        json!({"type": "definitions/preview", "device": dev, "cbor": cbor}),
    );
    assert_eq!(r["result"]["matches"], true);
    assert!(
        r["result"]["added"]
            .as_array()
            .unwrap()
            .contains(&json!(format!("{dev}:1:temperature_offset")))
    );
    assert!(hub.entity(&format!("{dev}:1:temperature_offset")).is_none());
    // Scaffold: identity, a match clause, attributes seen.
    let r = call(
        &mut api,
        &mut hub,
        s,
        4,
        json!({"type": "definitions/scaffold", "device": dev}),
    );
    assert_eq!(r["result"]["match"][0]["model"], "EFEKTA_CO2_Smart_Monitor");
    // Upload: the user definition re-resolves the device.
    let r = call(
        &mut api,
        &mut hub,
        s,
        5,
        json!({"type": "definitions/upload", "cbor": cbor}),
    );
    assert_eq!(r["result"]["matches"], json!([dev]));
    assert!(
        r["result"]["resolution"][0]["added"]
            .as_array()
            .unwrap()
            .len()
            >= 14
    );
    assert_eq!(
        hub.device(dev).unwrap().resolution.layers[1].source,
        LayerSource::User
    );
    assert_eq!(
        serde_json::to_value(
            hub.entity(&format!("{dev}:1:temperature_offset"))
                .unwrap()
                .1
                .r
                .source
        )
        .unwrap(),
        "user"
    );
    let r = call(
        &mut api,
        &mut hub,
        s,
        6,
        json!({"type": "definitions/list", "source": "user"}),
    );
    assert_eq!(r["result"][0]["id"], "efekta/co2-smart-monitor");
    let r = call(
        &mut api,
        &mut hub,
        s,
        7,
        json!({"type": "definitions/get", "definition": "efekta/co2-smart-monitor"}),
    );
    let back = base64::engine::general_purpose::STANDARD
        .decode(r["result"]["cbor"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        back,
        golden_cbor("efekta-co2-smart-monitor"),
        "canonical CBOR round trip"
    );
    let r = call(
        &mut api,
        &mut hub,
        s,
        8,
        json!({"type": "devices/diagnostics", "device": dev}),
    );
    assert!(r["result"]["layers"].as_array().unwrap().len() == 2);
    let r = call(
        &mut api,
        &mut hub,
        s,
        9,
        json!({"type": "definitions/delete", "definition": "efekta/co2-smart-monitor"}),
    );
    assert_eq!(r["success"], true);
    // Same fingerprint, definition gone: its entities are removed.
    assert!(hub.entity(&format!("{dev}:1:temperature_offset")).is_none());
    let r = call(
        &mut api,
        &mut hub,
        s,
        10,
        json!({"type": "definitions/fetch", "device": dev}),
    );
    assert_eq!(r["error"]["code"], "unsupported");
}

#[test]
fn gp_api_bindings_and_config() {
    let mut hub = hub_with(&["gp-zigbee-rs-switch"]);
    let mut api = ApiServer::new("hub", "t");
    let (s, _) = api.connect();
    api.handle_text(&mut hub, s, r#"{"type": "auth", "token": "t"}"#, T0);
    let mut sim = Sim::default();
    sim.devices.push(light());
    sim.join(&mut hub, 0x0017880100aabbcc, T0);
    let gpd = crate::gp::types::GpdId::SrcId(0x00F4_0003);
    hub.on_gp_event(
        crate::gp::types::GpEvent::Commissioned {
            gpd_id: gpd,
            device_id: 0x02,
            flags: 0,
            record: crate::gp::types::GpSinkRecord {
                gpd_id: gpd,
                device_id: 2,
                communication_mode: 2,
                group: 0x0B84,
                assigned_alias: 0xFFFF,
                flags: 0,
                security_level: 0,
                key_type: 0,
                key: None,
                frame_counter: 1,
                groupcast_radius: 0,
            },
        },
        T0,
    );
    let r = call(&mut api, &mut hub, s, 1, json!({"type": "gp/devices/list"}));
    assert_eq!(r["result"][0]["id"], "gp:00f40003");
    assert_eq!(r["result"][0]["warnings"], json!(["gp_unsecured"]));
    let r = call(
        &mut api,
        &mut hub,
        s,
        2,
        json!({"type": "gp/bindings/create", "gpd": "gp:00f40003", "trigger": {"command_id": 0x22},
               "target": {"device": "zb:0017880100aabbcc", "endpoint": 11},
               "action": {"cluster": 6, "command": 2, "payload": {"mode": "fixed", "bytes": []}}, "executor": "hub"}),
    );
    assert_eq!(r["result"]["state"], "active", "{r}");
    assert_eq!(r["result"]["executor"], "hub");
    let r = call(
        &mut api,
        &mut hub,
        s,
        3,
        json!({"type": "gp/bindings/list", "target": "zb:0017880100aabbcc"}),
    );
    assert_eq!(r["result"].as_array().unwrap().len(), 1);
    // The web UI's update: a derived payload carries no bytes.
    let r = call(
        &mut api,
        &mut hub,
        s,
        31,
        json!({"type": "gp/bindings/update", "binding": 1, "executor": "hub", "trigger": {"command_id": 0x69, "contact_mask": 1},
               "action": {"cluster": 8, "command": 6, "payload": {"mode": "derived"}}}),
    );
    assert_eq!(r["result"]["action"]["payload"], json!({"mode": "derived"}));
    assert_eq!(r["result"]["trigger"]["contact_mask"], 1);
    let r = call(
        &mut api,
        &mut hub,
        s,
        4,
        json!({"type": "gp/targets/probe", "device": "zb:0017880100aabbcc"}),
    );
    assert_eq!(r["result"]["probing"], true);
    let r = call(
        &mut api,
        &mut hub,
        s,
        5,
        json!({"type": "gp/config", "security_level": 0}),
    );
    assert_eq!(r["success"], true);
    let r = call(
        &mut api,
        &mut hub,
        s,
        6,
        json!({"type": "gp/bindings/delete", "binding": 1}),
    );
    assert_eq!(r["success"], true);
    let r = call(
        &mut api,
        &mut hub,
        s,
        7,
        json!({"type": "gp/devices/remove", "device": "gp:00f40003"}),
    );
    assert_eq!(r["success"], true);
    assert!(hub.device("gp:00f40003").is_none());
    let r = call(
        &mut api,
        &mut hub,
        s,
        8,
        json!({"type": "gp/commissioning/open", "duration_s": 700}),
    );
    assert_eq!(r["error"]["code"], "invalid_value");
}

fn gp_frame(gpd: u32, cmd: u8, result: u8) -> crate::gp::types::GpEvent {
    crate::gp::types::GpEvent::Frame(crate::gp::types::GpFrame {
        origin: 1,
        result,
        gpd_id: crate::gp::types::GpdId::SrcId(gpd),
        device_id: 0x02,
        command_id: cmd,
        frame_counter: 1,
        security_level: 0,
        key_type: 0,
        flags: 0,
        proxy_nwk: 0,
        gpp_gpd_link: 0,
        rssi: -50,
        lqi: 200,
        payload: Vec::new(),
    })
}

fn gp_topics(api: &mut ApiServer, hub: &mut Hub<MockTransport>) -> Vec<J> {
    api.pump(hub)
        .iter()
        .map(|(_, t)| serde_json::from_str::<J>(t).unwrap()["event"].clone())
        .collect()
}

#[test]
fn gp_api_commissioning_status_and_config() {
    use crate::gp::types::{GpEvent, GpSinkRecord, GpdId, frame_result};
    let mut hub = hub_with(&["gp-zigbee-rs-switch"]);
    let mut api = ApiServer::new("hub", "t");
    let (s, _) = api.connect();
    api.handle_text(&mut hub, s, r#"{"type": "auth", "token": "t"}"#, T0);
    let out = parse(&api.handle_text(
        &mut hub,
        s,
        r#"{"id": 1, "type": "subscribe", "topics": ["gp"]}"#,
        T0,
    ));
    assert_eq!(out[1]["event"]["gp_commissioning"]["open"], false);
    let r = call(
        &mut api,
        &mut hub,
        s,
        2,
        json!({"type": "gp/commissioning/status"}),
    );
    assert_eq!(
        r["result"],
        json!({"open": false, "state": "closed", "remaining_s": 0, "involve_proxies": true,
               "exit_on_first_pairing": false, "exit_reason": "none", "pending": [], "failed": []})
    );

    // Open (at T0 + 10 s): the hub keeps the window itself and reports it.
    let r = call(
        &mut api,
        &mut hub,
        s,
        3,
        json!({"type": "gp/commissioning/open", "duration_s": 120, "involve_proxies": false, "exit_on_first_pairing": true}),
    );
    assert_eq!(r["success"], true);
    let ev = gp_topics(&mut api, &mut hub);
    assert_eq!(ev[0]["topic"], "gp/commissioning");
    assert_eq!(
        (&ev[0]["state"], &ev[0]["remaining_s"]),
        (&json!("open"), &json!(120))
    );
    let now = T0 + 10_000;

    // A bidirectional GPD, then failures (bounded, newest first).
    hub.on_gp_event(
        gp_frame(0x0000_00A1, 0xE0, frame_result::COMMISSIONING_REPLY_SENT),
        now + 1_000,
    );
    hub.on_gp_event(
        gp_frame(0x0000_00A1, 0xE3, frame_result::COMMISSIONING_STEP),
        now + 2_000,
    );
    for i in 0..10u32 {
        hub.on_gp_event(
            GpEvent::CommissioningFailed {
                gpd_id: GpdId::SrcId(0xB0 + i),
                reason: 2,
            },
            now + 3_000 + u64::from(i),
        );
    }
    let ev = gp_topics(&mut api, &mut hub);
    assert!(
        ev.iter()
            .any(|e| e["topic"] == "gp/pending" && e["gpd"] == "gp:000000a1" && e["time"].is_f64())
    );
    let failed = ev.iter().find(|e| e["topic"] == "gp/failed").unwrap();
    assert_eq!(
        (&failed["reason"], &failed["reason_code"]),
        (&json!("SecurityPolicy"), &json!(2))
    );
    let st = hub.gp_commissioning_status(now + 20_000);
    assert_eq!(st["open"], true);
    assert_eq!(st["remaining_s"], 100);
    assert_eq!(
        (
            st["involve_proxies"].clone(),
            st["exit_on_first_pairing"].clone()
        ),
        (json!(false), json!(true))
    );
    assert_eq!(st["pending"].as_array().unwrap().len(), 1);
    assert_eq!(st["pending"][0]["command_id"], 0xE3);
    assert_eq!(st["pending"][0]["since"], crate::device::ts(now + 1_000));
    let f = st["failed"].as_array().unwrap();
    assert_eq!(f.len(), crate::gp::MAX_FAILED);
    assert_eq!(f[0]["gpd"], "gp:000000b9");
    assert_eq!(f[7]["gpd"], "gp:000000b2");

    // The pending GPD pairs: it leaves `pending`, and the window closes on
    // the first pairing.
    let gpd = GpdId::SrcId(0xA1);
    hub.on_gp_event(
        GpEvent::Commissioned {
            gpd_id: gpd,
            device_id: 2,
            flags: 1,
            record: GpSinkRecord {
                gpd_id: gpd,
                device_id: 2,
                communication_mode: 2,
                group: 0x0B84,
                assigned_alias: 0xFFFF,
                flags: 0,
                security_level: 0,
                key_type: 0,
                key: None,
                frame_counter: 1,
                groupcast_radius: 0,
            },
        },
        now + 30_000,
    );
    let st = hub.gp_commissioning_status(now + 30_000);
    assert_eq!(
        (
            &st["open"],
            &st["exit_reason"],
            st["pending"].as_array().unwrap().len()
        ),
        (&json!(false), &json!("first_pairing"), 0)
    );
    let r = call(&mut api, &mut hub, s, 4, json!({"type": "gp/devices/list"}));
    assert_eq!(r["result"][0]["device_id"], 2);
    assert!(r["result"][0]["commands"].is_array());

    // The window expires on the hub's deadline; stale pending GPDs go.
    hub.gp_commissioning_open(60, true, false, now + 40_000)
        .unwrap();
    hub.on_gp_event(
        gp_frame(0x0000_00C1, 0xE0, frame_result::COMMISSIONING_REPLY_SENT),
        now + 41_000,
    );
    gp_topics(&mut api, &mut hub);
    hub.tick(now + 100_000);
    let ev = gp_topics(&mut api, &mut hub);
    let closed = ev
        .iter()
        .find(|e| e["topic"] == "gp/commissioning")
        .unwrap();
    assert_eq!(
        (&closed["state"], &closed["exit_reason"]),
        (&json!("closed"), &json!("timeout"))
    );
    hub.tick(now + 102_000);
    assert_eq!(
        hub.gp_commissioning_status(now + 102_000)["pending"],
        json!([])
    );

    // An NCP-reported window without a deadline.
    hub.on_gp_event(
        GpEvent::Commissioning {
            state: 1,
            window_s: 0xFFFF,
            exit_reason: 0,
        },
        now + 110_000,
    );
    assert_eq!(
        hub.gp_commissioning_status(now + 110_000)["remaining_s"],
        J::Null
    );

    // gp/config: readable, absent fields keep their values, validated.
    let r = call(&mut api, &mut hub, s, 5, json!({"type": "gp/config/get"}));
    assert_eq!(
        r["result"],
        json!({"security_level": 0, "communication_mode": 2, "group": 0x0B84, "source": "default", "warning": null})
    );
    hub.transport.take();
    let r = call(
        &mut api,
        &mut hub,
        s,
        6,
        json!({"type": "gp/config", "security_level": 1}),
    );
    assert_eq!(
        (&r["result"]["communication_mode"], &r["result"]["source"]),
        (&json!(2), &json!("api"))
    );
    assert!(r["result"]["warning"].is_string());
    assert!(hub.transport.take().iter().any(|x| matches!(
        x,
        crate::transport::Sent::Gp(
            GpRequest::SinkConfig {
                security_level: 1,
                communication_mode: 2,
                ..
            },
            _
        )
    )));
    let r = call(
        &mut api,
        &mut hub,
        s,
        7,
        json!({"type": "gp/config", "communication_mode": 1}),
    );
    assert_eq!(r["result"]["security_level"], 1);
    let r = call(&mut api, &mut hub, s, 8, json!({"type": "gp/config/get"}));
    assert_eq!(
        (
            &r["result"]["security_level"],
            &r["result"]["communication_mode"]
        ),
        (&json!(1), &json!(1))
    );
    assert!(
        gp_topics(&mut api, &mut hub)
            .iter()
            .any(|e| e["topic"] == "gp/config" && e["communication_mode"] == 1)
    );
    let r = call(
        &mut api,
        &mut hub,
        s,
        9,
        json!({"type": "gp/config", "security_level": 4}),
    );
    assert_eq!(r["error"]["code"], "invalid_value");
}

#[test]
fn request_model_round_trips() {
    for text in [
        r#"{"id":1,"type":"devices/list"}"#,
        r#"{"id":2,"type":"entities/command","uid":"zb:00158d0001a2b3c4:1:thermostat","command":"set_temperature","params":{"temperature":21.5}}"#,
        r#"{"id":3,"type":"gp/commissioning/open","duration_s":180,"involve_proxies":true,"exit_on_first_pairing":false}"#,
        r#"{"id":4,"type":"subscribe","topics":["devices","entities","events","gp"]}"#,
        r#"{"id":5,"type":"devices/remove","device":"zb:00158d0001a2b3c4","force":true}"#,
        r#"{"id":7,"type":"gp/commissioning/status"}"#,
        r#"{"id":8,"type":"gp/config/get"}"#,
        r#"{"id":9,"type":"gp/config","security_level":1}"#,
        r#"{"id":6,"type":"devices/remove"}"#,
    ] {
        let parsed: Result<Envelope, _> = serde_json::from_str(text);
        if text.ends_with(r#""devices/remove"}"#) {
            assert!(parsed.is_err());
            continue;
        }
        let e = parsed.unwrap();
        let back: Envelope = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(back, e);
    }
    let m = ServerMessage::err(17, ApiError::zcl(crate::zcl::Status::UNSUPPORTED_ATTRIBUTE));
    assert_eq!(
        serde_json::to_value(m).unwrap(),
        json!({"type": "result", "id": 17, "success": false, "error": {"code": "zcl_status", "message": "device answered UNSUPPORTED_ATTRIBUTE", "zcl_status": "UNSUPPORTED_ATTRIBUTE"}})
    );
}

#[test]
fn hub_info_carries_the_application_fields() {
    let mut hub = hub_with(&["innr-sp120"]);
    let mut api = ApiServer::new("zigbee-rs-hub 0.1.0", "t");
    let (s, _) = api.connect();
    api.handle_text(&mut hub, s, r#"{"type": "auth", "token": "t"}"#, T0);
    let r = call(&mut api, &mut hub, s, 1, json!({"type": "hub/info"}));
    assert_eq!(r["result"]["name"], "zigbee-rs-hub 0.1.0");
    assert_eq!(r["result"]["api_version"], API_VERSION);
    assert!(r["result"].get("coordinator_ieee").is_none());
    api.set_info("coordinator_ieee", json!("00124b0000000001"));
    let r = call(&mut api, &mut hub, s, 2, json!({"type": "hub/info"}));
    assert_eq!(r["result"]["coordinator_ieee"], "00124b0000000001");
    api.set_info("coordinator_ieee", J::Null);
    let r = call(&mut api, &mut hub, s, 3, json!({"type": "hub/info"}));
    assert!(r["result"].get("coordinator_ieee").is_none());
}

#[test]
fn network_info_and_network_events() {
    let mut hub = hub_with(&[]);
    let mut api = ApiServer::new("zrs-hub-7cdfa1", "secret");
    let (s, _) = api.connect();
    api.handle_text(&mut hub, s, r#"{"type": "auth", "token": "secret"}"#, T0);

    // Without host-core facts the method is refused, not invented.
    let r = call(&mut api, &mut hub, s, 1, json!({"type": "network/info"}));
    assert_eq!(r["success"], false);
    assert_eq!(r["error"]["code"], "unsupported");

    hub.transport.network = Some(json!({"pan_id": 0x1C06, "channel": 17, "permit_join_s": 0}));
    let r = call(&mut api, &mut hub, s, 2, json!({"type": "network/info"}));
    assert_eq!(r["result"]["channel"], 17);
    assert_eq!(r["result"]["time"], crate::device::ts(T0 + 10_000));

    // The network topic: snapshot carries the same facts, then events.
    let out = parse(&api.handle_text(
        &mut hub,
        s,
        r#"{"id": 3, "type": "subscribe", "topics": ["network"]}"#,
        T0,
    ));
    assert_eq!(out[0]["success"], true);
    assert_eq!(out[1]["event"]["network"]["pan_id"], 0x1C06);
    hub.network_event(json!({"topic": "permit_join", "remaining_s": 120}));
    let ev = parse(
        &api.pump(&mut hub)
            .into_iter()
            .map(|(_, t)| t)
            .collect::<Vec<_>>(),
    );
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0]["id"], 3);
    assert_eq!(ev[0]["event"]["topic"], "permit_join");
    assert_eq!(ev[0]["event"]["remaining_s"], 120);
}

#[test]
fn token_rotation_keeps_authenticated_sessions() {
    let mut hub = hub_with(&[]);
    let mut api = ApiServer::new("hub", "old");
    let (a, _) = api.connect();
    assert!(!api.is_authed(a));
    api.handle_text(&mut hub, a, r#"{"type": "auth", "token": "old"}"#, T0);
    assert!(api.is_authed(a));
    api.set_token("new");
    assert_eq!(api.token(), "new");
    assert!(api.is_authed(a), "an open session stays");
    let (b, _) = api.connect();
    let bad = parse(&api.handle_text(&mut hub, b, r#"{"type": "auth", "token": "old"}"#, T0));
    assert_eq!(bad[0]["type"], "auth_invalid");
    let ok = parse(&api.handle_text(&mut hub, b, r#"{"type": "auth", "token": "new"}"#, T0));
    assert_eq!(ok[0]["type"], "auth_ok");
    assert!(!api.is_authed(99));
}
