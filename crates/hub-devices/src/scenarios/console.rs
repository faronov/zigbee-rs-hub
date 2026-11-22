//! ZCL console (14.7): raw requests to a device and their decoded
//! answers, the frame tap and Tuya DP sends.

use serde_json::{Value as J, json};

use super::*;
use crate::api::{ApiServer, SessionId};
use crate::console::{ConsoleOp, ConsoleReq};
use crate::model::LayerSource;
use crate::testutil::sim::{Sim, SimDevice};
use crate::testutil::yaml::load_definition;
use crate::zcl::{TypedValue, ZclType, ZclValue};
use crate::zdo::LogicalType;

const IEEE: u64 = 0xa4c1_38ee_1126_da72;
const ID: &str = "zb:a4c138ee1126da72";

fn sensor() -> SimDevice {
    SimDevice::new(IEEE, 0x3338, LogicalType::Router, true, 0x1141)
        .endpoint(
            1,
            0x0104,
            0x0302,
            &[0x0000, 0x0003, 0x0006, 0x0402, 0x0405],
            &[0x0019],
        )
        .basic("Slacky-DIY", "TS0201-z-SlD", 1)
        .attr(1, 0x0402, 0x0000, TypedValue::int(ZclType::I16, 2137))
        .attr(1, 0x0402, 0xF004, TypedValue::int(ZclType::I16, 3000))
        .attr(
            1,
            0x0006,
            0x0000,
            TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false))),
        )
}

fn joined() -> (Hub<MockTransport>, Sim) {
    let mut hub = hub_with(&[]);
    let mut cat = MemoryCatalog::default();
    cat.insert(
        load_definition(
            include_str!("fixtures/slacky-diy-ts0201-z-sld.yaml"),
            LayerSource::Catalog,
        )
        .unwrap(),
    );
    hub.set_catalog(Box::new(cat), T0);
    let mut sim = Sim::default();
    sim.devices.push(sensor());
    sim.join(&mut hub, IEEE, T0);
    hub.drain_events();
    sim.log.clear();
    (hub, sim)
}

fn answer(hub: &mut Hub<MockTransport>, ticket: u64) -> Result<J, crate::hub::ApiError> {
    hub.drain_events()
        .into_iter()
        .find_map(|e| match e {
            HubEvent::ConsoleResult { ticket: t, result } if t == ticket => Some(result),
            _ => None,
        })
        .expect("console result")
}

fn req(cluster: u16, op: ConsoleOp) -> ConsoleReq {
    ConsoleReq {
        device: ID.into(),
        ep: 1,
        cluster,
        mfr: None,
        op,
    }
}

#[test]
fn read_write_discover_and_commands() {
    let (mut hub, mut sim) = joined();
    let t = T0 + 1000;
    // Read: values with their types.
    let ack = hub
        .console(
            req(0x0402, ConsoleOp::Read(vec![0x0000, 0xF004, 0x1234])),
            t,
        )
        .unwrap();
    assert_eq!(ack.status, "pending");
    sim.run(&mut hub, t);
    let r = answer(&mut hub, ack.ticket).unwrap();
    assert_eq!(r["command"], 0x01);
    assert_eq!(
        r["records"][0],
        json!({"attribute": 0, "status": "SUCCESS", "type": 0x29, "value": 2137})
    );
    assert_eq!(r["records"][2]["status"], "UNSUPPORTED_ATTRIBUTE");
    // Write without a type: the definition's binding (high_temperature, i16).
    let ack = hub
        .console(
            req(
                0x0402,
                ConsoleOp::Write {
                    attr: 0xF004,
                    ty: None,
                    value: json!(2500),
                },
            ),
            t,
        )
        .unwrap();
    sim.run(&mut hub, t);
    assert_eq!(
        answer(&mut hub, ack.ticket).unwrap()["records"][0]["status"],
        "SUCCESS"
    );
    let w = sim.dev(IEEE).writes.last().cloned().unwrap();
    assert_eq!(
        (w.2, w.4.ty, w.4.value),
        (0xF004, ZclType::I16, ZclValue::Int(2500))
    );
    // An unknown attribute needs its type.
    let e = hub
        .console(
            req(
                0x0402,
                ConsoleOp::Write {
                    attr: 0x7777,
                    ty: None,
                    value: json!(1),
                },
            ),
            t,
        )
        .unwrap_err();
    assert!(e.message.contains("type"), "{}", e.message);
    // Discover Attributes.
    let ack = hub
        .console(
            req(
                0x0402,
                ConsoleOp::Discover {
                    what: 0,
                    start: 0,
                    max: 32,
                },
            ),
            t,
        )
        .unwrap();
    sim.run(&mut hub, t);
    let r = answer(&mut hub, ack.ticket).unwrap();
    assert_eq!(r["complete"], true);
    assert_eq!(
        r["records"],
        json!([{"attribute": 0, "type": 0x29}, {"attribute": 0xF004, "type": 0x29}])
    );
    // Configure reporting with the type from a read.
    let ack = hub
        .console(
            req(
                0x0402,
                ConsoleOp::ConfigureReporting {
                    attr: 0,
                    ty: Some(ZclType::I16),
                    min: 5,
                    max: 600,
                    change: Some(10.0),
                },
            ),
            t,
        )
        .unwrap();
    sim.run(&mut hub, t);
    assert_eq!(
        answer(&mut hub, ack.ticket).unwrap()["records"][0]["status"],
        "SUCCESS"
    );
    assert!(
        sim.dev(IEEE)
            .reporting
            .iter()
            .any(|(_, c, r)| *c == 0x0402 && r.min == 5 && r.max == 600)
    );
    // A cluster command: the default response completes it.
    let ack = hub
        .console(
            req(
                0x0006,
                ConsoleOp::Command {
                    cmd: 0x01,
                    payload: vec![],
                    global: false,
                    to_client: false,
                    no_default_rsp: false,
                },
            ),
            t,
        )
        .unwrap();
    sim.run(&mut hub, t);
    let r = answer(&mut hub, ack.ticket).unwrap();
    assert_eq!(
        (
            r["command"].clone(),
            r["for_command"].clone(),
            r["status"].clone()
        ),
        (json!(0x0B), json!(1), json!("SUCCESS"))
    );
    // The answer is also processed as usual: the read updated the sensor.
    assert_eq!(
        hub.state_of(&format!("{ID}:1:temperature")).unwrap()["state"],
        json!(21.37)
    );
    // No answer: the request times out.
    sim.dev(IEEE).responsive = false;
    let ack = hub
        .console(req(0x0402, ConsoleOp::Read(vec![0])), t)
        .unwrap();
    let mut res = None;
    for k in 1..40 {
        hub.tick(t + k * 1000);
        if let Some(r) = hub.drain_events().into_iter().find_map(|e| match e {
            HubEvent::ConsoleResult { ticket, result } if ticket == ack.ticket => Some(result),
            _ => None,
        }) {
            res = Some(r);
            break;
        }
    }
    assert_eq!(res.unwrap().unwrap_err().code, "timeout");
}

fn session(api: &mut ApiServer, hub: &mut Hub<MockTransport>) -> SessionId {
    let (s, _) = api.connect();
    api.handle_text(hub, s, r#"{"type": "auth", "token": "secret"}"#, T0);
    s
}

fn send(api: &mut ApiServer, hub: &mut Hub<MockTransport>, s: SessionId, body: J) -> Vec<J> {
    api.handle_text(hub, s, &body.to_string(), T0 + 2000)
        .iter()
        .map(|f| serde_json::from_str(f).unwrap())
        .collect()
}

fn pump(api: &mut ApiServer, hub: &mut Hub<MockTransport>) -> Vec<J> {
    let ev = hub.drain_events();
    api.pump_events(hub, ev)
        .iter()
        .map(|(_, t)| serde_json::from_str(t).unwrap())
        .collect()
}

#[test]
fn api_requests_frames_topic_and_tuya_dps() {
    let (mut hub, mut sim) = joined();
    let mut api = ApiServer::new("hub", "secret");
    let s = session(&mut api, &mut hub);
    let out = send(
        &mut api,
        &mut hub,
        s,
        json!({"id": 1, "type": "subscribe", "topics": ["frames"]}),
    );
    assert_eq!(out[0]["success"], true);
    // zcl/read: the result follows the device's answer (as for commands).
    let out = send(
        &mut api,
        &mut hub,
        s,
        json!({"id": 2, "type": "zcl/read", "device": ID, "endpoint": 1, "cluster": 0x0402, "attributes": [0]}),
    );
    assert!(out.is_empty(), "{out:?}");
    // The tap is switched on by the subscription at the next pump.
    api.pump_events(&mut hub, Vec::new());
    assert!(hub.frame_tap);
    sim.run(&mut hub, T0 + 2000);
    let msgs = pump(&mut api, &mut hub);
    let res = msgs.iter().find(|m| m["id"] == 2).unwrap();
    assert_eq!(res["result"]["records"][0]["value"], 2137);
    // The frames of the next request are tapped both ways.
    let out = send(
        &mut api,
        &mut hub,
        s,
        json!({"id": 3, "type": "zcl/write", "device": ID, "cluster": 0x0402, "attribute": 0xF004, "data_type": "0x29", "value": 2600}),
    );
    assert!(out.is_empty(), "{out:?}");
    sim.run(&mut hub, T0 + 2000);
    let msgs = pump(&mut api, &mut hub);
    let frames: Vec<&J> = msgs
        .iter()
        .filter(|m| m["event"]["topic"] == "frame")
        .map(|m| &m["event"])
        .collect();
    assert!(
        frames
            .iter()
            .any(|f| f["dir"] == "tx" && f["zcl"]["command"] == 0x02 && f["cluster"] == 0x0402)
    );
    assert!(
        frames
            .iter()
            .any(|f| f["dir"] == "rx" && f["zcl"]["command"] == 0x04)
    );
    assert_eq!(msgs.iter().find(|m| m["id"] == 3).unwrap()["success"], true);
    // Bad arguments answer at once.
    let out = send(
        &mut api,
        &mut hub,
        s,
        json!({"id": 4, "type": "zcl/read", "device": ID, "cluster": 6}),
    );
    assert_eq!(out[0]["error"]["code"], "invalid_value");
    let out = send(
        &mut api,
        &mut hub,
        s,
        json!({"id": 5, "type": "zcl/command", "device": ID, "cluster": 6, "command": 1, "payload": "0g"}),
    );
    assert_eq!(out[0]["error"]["code"], "invalid_value");
    // A Tuya frame: DPs decoded on the frame tap.
    let ind = sim.dev(IEEE).tuya_frame(
        0x02,
        7,
        &[(1, 1, vec![1]), (2, 2, 215i32.to_be_bytes().to_vec())],
    );
    hub.on_aps_indication(ind, T0 + 3000);
    let msgs = pump(&mut api, &mut hub);
    let f = &msgs
        .iter()
        .find(|m| m["event"]["cluster"] == 0xEF00)
        .unwrap()["event"];
    assert_eq!(
        f["tuya"]["dps"][0],
        json!({"dp": 1, "type": 1, "value": true, "data": "01"})
    );
    assert_eq!(f["tuya"]["dps"][1]["value"], 215);
    // tuya/send: done when the device answers the DP.
    let out = send(
        &mut api,
        &mut hub,
        s,
        json!({"id": 6, "type": "tuya/send", "device": ID, "dps": [{"dp": 101, "type": "value", "value": -5}]}),
    );
    assert!(out.is_empty(), "{out:?}");
    sim.run(&mut hub, T0 + 3000);
    assert_eq!(
        sim.dev(IEEE).tuya[&101],
        (2, (-5i32).to_be_bytes().to_vec())
    );
    let msgs = pump(&mut api, &mut hub);
    assert_eq!(
        msgs.iter().find(|m| m["id"] == 6).unwrap()["result"]["status"],
        "ok"
    );
    // Unsubscribed: no more tapping.
    let out = send(
        &mut api,
        &mut hub,
        s,
        json!({"id": 7, "type": "unsubscribe", "subscription": 1}),
    );
    assert_eq!(out[0]["success"], true);
    api.pump_events(&mut hub, Vec::new());
    assert!(!hub.frame_tap);
}
