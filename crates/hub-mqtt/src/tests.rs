//! The bridge against the real device engine and API server (no threads):
//! simulated devices join, their records and states reach the bridge as
//! API frames, and the MQTT side is checked against Zigbee2MQTT's topics
//! and payloads. Golden files in `tests/golden/` (`GOLDEN_UPDATE=1`
//! rewrites them) hold the full JSON of the Zigbee2MQTT-facing documents.

use std::path::PathBuf;

use hub_devices::api::ApiServer;
use hub_devices::hub::{Hub, HubConfig};
use hub_devices::resolve::{Catalog, MemoryCatalog};
use hub_devices::sim_device::{Sim, SimDevice};
use hub_devices::transport::{MockTransport, Sent};
use hub_devices::tuya::cmd as tuya_cmd;
use hub_devices::zcl::{TypedValue, ZclType, ZclValue};
use hub_devices::zdo::LogicalType;
use serde_json::{Value as J, json};

use crate::bridge::{Bridge, Limits, Msg};
use crate::config::MqttConfig;

const SENSOR: u64 = 0x00158d0001000001;
const LIGHT: u64 = 0x00158d0001000002;
const PLUG: u64 = 0x00158d0001000003;
const PLUG2: u64 = 0x00158d0001000004;
const TRV: u64 = 0xa4c138aabbccddee;

fn sensor() -> SimDevice {
    SimDevice::new(SENSOR, 0x1001, LogicalType::EndDevice, true, 0x115F)
        .endpoint(
            1,
            0x0104,
            0x0302,
            &[0x0000, 0x0001, 0x0003, 0x0402, 0x0405],
            &[0x0003],
        )
        .basic("LUMI", "lumi.weather", 3)
        .attr(1, 0x0001, 0x0020, TypedValue::uint(ZclType::U8, 30))
        .attr(1, 0x0001, 0x0021, TypedValue::uint(ZclType::U8, 200))
        .attr(1, 0x0402, 0x0000, TypedValue::int(ZclType::I16, 2137))
        .attr(1, 0x0405, 0x0000, TypedValue::uint(ZclType::U16, 5520))
}

fn light() -> SimDevice {
    let u16v = |v: u64| TypedValue::uint(ZclType::U16, v);
    SimDevice::new(LIGHT, 0x1002, LogicalType::Router, true, 0x117C)
        .endpoint(
            1,
            0x0104,
            0x010C,
            &[0x0000, 0x0003, 0x0004, 0x0005, 0x0006, 0x0008, 0x0300],
            &[0x0019],
        )
        .basic("IKEA of Sweden", "TRADFRI bulb E27 WS opal 980lm", 1)
        .attr(
            1,
            0x0006,
            0x0000,
            TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(true))),
        )
        .attr(1, 0x0008, 0x0000, TypedValue::uint(ZclType::U8, 254))
        .attr(1, 0x0300, 0x0007, u16v(370))
        .attr(1, 0x0300, 0x0008, TypedValue::uint(ZclType::ENUM8, 2))
        .attr(1, 0x0300, 0x400A, TypedValue::uint(ZclType::MAP16, 0x0010))
        .attr(1, 0x0300, 0x400B, u16v(250))
        .attr(1, 0x0300, 0x400C, u16v(454))
}

/// A plug the example catalog knows (Innr SP 120).
fn plug() -> SimDevice {
    SimDevice::new(PLUG, 0x1003, LogicalType::Router, true, 0x1166)
        .endpoint(
            1,
            0x0104,
            0x010A,
            &[0x0000, 0x0003, 0x0004, 0x0005, 0x0006],
            &[],
        )
        .basic("innr", "SP 120", 1)
        .attr(
            1,
            0x0006,
            0x0000,
            TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(true))),
        )
}

/// A two-gang relay without a definition: two generic `switch` entities.
fn plug2() -> SimDevice {
    let off = || TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false)));
    SimDevice::new(PLUG2, 0x1004, LogicalType::Router, true, 0x1234)
        .endpoint(1, 0x0104, 0x0002, &[0x0000, 0x0006], &[])
        .endpoint(2, 0x0104, 0x0002, &[0x0006], &[])
        .basic("ACME", "relay2", 1)
        .attr(1, 0x0006, 0x0000, off())
        .attr(2, 0x0006, 0x0000, off())
}

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

struct Rig {
    hub: Hub<MockTransport>,
    sim: Sim,
    api: ApiServer,
    sid: u64,
    b: Bridge,
    now: u64,
    out: Vec<Msg>,
}

impl Rig {
    fn new(cfg: MqttConfig) -> Self {
        Self::with(cfg, Box::new(MemoryCatalog::default()), Limits::default())
    }

    fn with(cfg: MqttConfig, catalog: Box<dyn Catalog>, limits: Limits) -> Self {
        let mut hub = Hub::new(MockTransport::default(), HubConfig::default(), catalog);
        hub.transport.network = Some(
            json!({"state": "up", "pan_id": 6754, "ext_pan_id": "dddddddddddddddd", "channel": 15, "permit_join_s": 0,
            "ncp": {"fw": "0.1.2", "chip": "esp32h2", "board": "tbr", "product": "zigbee-rs NCP", "protocol": "1.0"}}),
        );
        let mut api = ApiServer::new("zigbee-rs-hub 0.1.8", "t");
        api.set_info("coordinator_ieee", "00124b0000000001".into());
        let (sid, hello) = api.connect();
        let mut b = Bridge::new(cfg, limits);
        b.on_api(&hello);
        let mut r = Self {
            hub,
            sim: Sim::default(),
            api,
            sid,
            b,
            now: 1_000_000,
            out: Vec::new(),
        };
        for f in r
            .api
            .handle_text(&mut r.hub, sid, r#"{"type":"auth","token":"t"}"#, r.now)
        {
            r.b.on_api(&f);
        }
        r.b.on_connected();
        r.pump();
        r
    }

    /// Delivers API requests, device traffic and events until quiet, then
    /// takes every publication.
    fn pump(&mut self) {
        for _ in 0..50 {
            let reqs = self.b.take_api();
            let mut frames = Vec::new();
            for q in &reqs {
                frames.extend(self.api.handle_text(&mut self.hub, self.sid, q, self.now));
            }
            // Events first: a join is seen before its interview runs.
            frames.extend(self.api.pump(&mut self.hub).into_iter().map(|(_, f)| f));
            self.sim.run(&mut self.hub, self.now);
            frames.extend(self.api.pump(&mut self.hub).into_iter().map(|(_, f)| f));
            for f in &frames {
                self.b.on_api(f);
            }
            if reqs.is_empty() && frames.is_empty() {
                break;
            }
        }
        self.now += 2000;
        self.b.tick(self.now);
        while let Some(m) = self.b.next_msg() {
            self.out.push(m);
        }
    }

    fn join(&mut self, d: SimDevice) {
        let (ieee, nwk) = (d.ieee, d.nwk);
        self.sim.devices.push(d);
        self.hub.on_device_joined(ieee, nwk, self.now);
        self.pump();
    }

    fn permit_joins(&self) -> Vec<(u8, Option<u16>)> {
        self.sim
            .log
            .iter()
            .filter_map(|s| {
                if let Sent::PermitJoin(d, t) = s {
                    Some((*d, *t))
                } else {
                    None
                }
            })
            .collect()
    }

    fn mqtt(&mut self, topic: &str, payload: &str) {
        self.b.on_message(topic, payload.as_bytes());
        self.pump();
    }

    fn take(&mut self) -> Vec<Msg> {
        std::mem::take(&mut self.out)
    }

    /// The last publication on `topic`.
    fn last(&self, topic: &str) -> Option<&Msg> {
        self.out.iter().rev().find(|m| m.topic == topic)
    }

    fn json(&self, topic: &str) -> J {
        let m = self
            .last(topic)
            .unwrap_or_else(|| panic!("nothing on {topic}; topics: {:?}", self.topics()));
        serde_json::from_slice(&m.payload).unwrap_or_else(|e| panic!("{topic}: {e}"))
    }

    fn topics(&self) -> Vec<&str> {
        self.out.iter().map(|m| m.topic.as_str()).collect()
    }

    /// `(endpoint, cluster, ZCL frame)` of what the engine sent since the
    /// last call.
    fn sent_aps(&mut self) -> Vec<(u8, u16, Vec<u8>)> {
        self.sim
            .log
            .drain(..)
            .filter_map(|s| {
                if let Sent::Aps(a) = s {
                    Some((a.dst_ep, a.cluster, a.payload))
                } else {
                    None
                }
            })
            .collect()
    }
}

fn cfg(discovery: bool) -> MqttConfig {
    MqttConfig {
        enabled: true,
        host: "broker".into(),
        discovery,
        ..MqttConfig::default()
    }
}

fn golden(name: &str, v: &J) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.json"));
    let text = serde_json::to_string_pretty(v).unwrap() + "\n";
    if std::env::var_os("GOLDEN_UPDATE").is_some() {
        std::fs::write(&path, &text).unwrap();
        return;
    }
    let want: J = serde_json::from_str(
        &std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())),
    )
    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert_eq!(*v, want, "{name}: got\n{text}");
}

/// `bridge/devices` entry of a device.
fn device_entry(r: &Rig, friendly: &str) -> J {
    let all = r.json("zigbee2mqtt/bridge/devices");
    all.as_array()
        .unwrap()
        .iter()
        .find(|d| d["friendly_name"] == friendly)
        .cloned()
        .unwrap_or_else(|| panic!("{friendly} not in {all}"))
}

fn cleared(r: &Rig) -> Vec<&str> {
    r.out
        .iter()
        .filter(|m| m.payload.is_empty() && m.retain)
        .map(|m| m.topic.as_str())
        .collect()
}

fn ops(aps: &[(u8, u16, Vec<u8>)]) -> Vec<(u8, u16, u8)> {
    aps.iter().map(|a| (a.0, a.1, a.2[2])).collect()
}

#[test]
fn birth_bridge_topics_and_lwt() {
    let mut r = Rig::new(cfg(false));
    let first: Vec<(&str, bool, u8)> = r
        .out
        .iter()
        .take(4)
        .map(|m| (m.topic.as_str(), m.retain, m.qos))
        .collect();
    assert_eq!(
        first,
        vec![
            ("zigbee2mqtt/bridge/state", true, 1),
            ("zigbee2mqtt/bridge/groups", true, 0),
            ("zigbee2mqtt/bridge/info", true, 0),
            ("zigbee2mqtt/bridge/devices", true, 0),
        ]
    );
    assert_eq!(
        r.json("zigbee2mqtt/bridge/state"),
        json!({"state": "online"})
    );
    let will = r.b.will();
    assert_eq!(
        (
            will.topic.as_str(),
            will.payload.as_slice(),
            will.qos,
            will.retain
        ),
        (
            "zigbee2mqtt/bridge/state",
            &br#"{"state":"offline"}"#[..],
            1,
            true
        )
    );
    let info = r.json("zigbee2mqtt/bridge/info");
    assert!(info["permit_join_end"].is_null());
    golden("bridge_info", &info);
    // Only the coordinator so far.
    assert_eq!(
        r.json("zigbee2mqtt/bridge/devices"),
        json!([crate::z2m::coordinator_device("00124b0000000001")])
    );
    assert_eq!(
        r.b.subscriptions(),
        [
            "+/set",
            "+/set/+",
            "+/get",
            "+/get/+",
            "+/+/set",
            "+/+/set/+",
            "+/+/get",
            "+/+/get/+",
            "bridge/request/#"
        ]
        .map(|s| format!("zigbee2mqtt/{s}"))
    );
    // A reconnect publishes everything again.
    r.take();
    r.b.on_disconnected();
    assert!(r.b.next_msg().is_none(), "nothing while disconnected");
    r.b.on_connected();
    r.pump();
    assert_eq!(r.out[0].topic, "zigbee2mqtt/bridge/state");
}

#[test]
fn sensor_state_exposes_and_discovery() {
    let mut r = Rig::new(cfg(true));
    r.join(sensor());
    // Zigbee2MQTT's keys and units: °C, %, battery %, voltage in mV.
    assert_eq!(
        r.json("zigbee2mqtt/lumi.weather"),
        json!({"battery": 100, "humidity": 55.2, "linkquality": 200, "rssi": -55, "temperature": 21.37, "voltage": 3000})
    );
    assert_eq!(
        r.json("zigbee2mqtt/lumi.weather/availability"),
        json!({"state": "online"})
    );
    assert!(
        r.last("zigbee2mqtt/lumi.weather/availability")
            .unwrap()
            .retain
    );
    // The IEEE address named the device until the interview read its
    // model; its retained topics are cleared.
    assert!(
        cleared(&r).contains(&"zigbee2mqtt/0x00158d0001000001/availability"),
        "{:?}",
        cleared(&r)
    );
    let events: Vec<J> = r
        .out
        .iter()
        .filter(|m| m.topic == "zigbee2mqtt/bridge/event")
        .map(|m| serde_json::from_slice(&m.payload).unwrap())
        .collect();
    assert_eq!(
        events[0],
        json!({"type": "device_joined", "data": {"friendly_name": "0x00158d0001000001", "ieee_address": "0x00158d0001000001"}})
    );
    assert_eq!(events[1]["data"]["status"], "started");
    let done = events
        .iter()
        .find(|e| e["data"]["status"] == "successful")
        .expect("interview successful");
    assert_eq!(done["data"]["friendly_name"], "lumi.weather");
    assert_eq!(done["data"]["supported"], false);
    golden("sensor_device", &device_entry(&r, "lumi.weather"));
    let disc = r.json("homeassistant/sensor/0x00158d0001000001/temperature/config");
    golden("sensor_discovery_temperature", &disc);
    // A report: the whole state again, with the new value.
    r.take();
    let s = r.sim.dev(SENSOR).clone();
    let rep = s.report(1, 0x0402, None, &[(0, TypedValue::int(ZclType::I16, 2250))]);
    r.hub.on_aps_indication(rep, r.now);
    r.pump();
    assert_eq!(r.json("zigbee2mqtt/lumi.weather")["temperature"], 22.5);
    assert_eq!(r.topics(), vec!["zigbee2mqtt/lumi.weather"]);
    // /set of a sensor value: logged on bridge/logging.
    r.mqtt("zigbee2mqtt/lumi.weather/set", r#"{"temperature": 5}"#);
    let log = r.json("zigbee2mqtt/bridge/logging");
    assert_eq!(log["level"], "error");
    assert!(
        log["message"].as_str().unwrap().contains("read-only"),
        "{log}"
    );
}

#[test]
fn light_state_set_and_discovery() {
    let mut r = Rig::new(cfg(true));
    r.join(light());
    let name = "zigbee2mqtt/TRADFRI bulb E27 WS opal 980lm";
    assert_eq!(
        r.json(name),
        json!({"state": "ON", "brightness": 254, "color_temp": 370, "color_mode": "color_temp", "linkquality": 200, "rssi": -55})
    );
    let dev = device_entry(&r, "TRADFRI bulb E27 WS opal 980lm");
    let light = dev["definition"]["exposes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "light")
        .cloned()
        .unwrap();
    golden("light_expose", &light);
    golden(
        "light_discovery",
        &r.json("homeassistant/light/0x00158d0001000002/light/config"),
    );
    // One Zigbee2MQTT payload, one engine command: Move to Level (with
    // On/Off) 200, then Move to Color Temperature 300 mired; transition
    // 1 s = 10 tenths.
    r.sent_aps();
    r.mqtt(
        &format!("{name}/set"),
        r#"{"state": "ON", "brightness": 200, "color_temp": 300, "transition": 1}"#,
    );
    let aps = r.sent_aps();
    assert_eq!(
        aps.iter().map(|a| a.1).collect::<Vec<_>>(),
        vec![0x0008, 0x0300],
        "{aps:?}"
    );
    assert_eq!(&aps[0].2[3..], &[200, 10, 0]);
    assert_eq!(&aps[1].2[3..], &[0x2C, 0x01, 10, 0]);
    let st = r.json(name);
    assert_eq!(
        (
            st["state"].clone(),
            st["brightness"].clone(),
            st["color_temp"].clone()
        ),
        (json!("ON"), json!(200), json!(300))
    );
    // Plain values on property topics, OFF, TOGGLE, brightness 0.
    r.mqtt(&format!("{name}/set/state"), "OFF");
    assert_eq!(ops(&r.sent_aps()), vec![(1, 0x0006, 0x00)]);
    assert_eq!(r.json(name)["state"], "OFF");
    r.mqtt(&format!("{name}/set"), "TOGGLE");
    assert_eq!(ops(&r.sent_aps()), vec![(1, 0x0006, 0x02)]);
    r.mqtt(&format!("{name}/set"), r#"{"brightness": 0}"#);
    assert_eq!(ops(&r.sent_aps()), vec![(1, 0x0006, 0x00)]);
    // Unknown property and device: logged on bridge/logging.
    r.mqtt(&format!("{name}/set"), r#"{"colour": 1}"#);
    assert!(
        r.json("zigbee2mqtt/bridge/logging")["message"]
            .as_str()
            .unwrap()
            .contains("no property")
    );
    r.mqtt("zigbee2mqtt/nobody/set", r#"{"state": "ON"}"#);
    assert_eq!(
        r.json("zigbee2mqtt/bridge/logging")["message"],
        "Entity 'nobody' is unknown"
    );
    // By IEEE address as well; /get reads the attributes.
    r.mqtt("zigbee2mqtt/0x00158d0001000002/set", r#"{"state": "ON"}"#);
    assert_eq!(ops(&r.sent_aps()), vec![(1, 0x0006, 0x01)]);
    r.mqtt(&format!("{name}/get"), r#"{"state": ""}"#);
    assert!(
        r.sent_aps()
            .iter()
            .all(|a| a.2[2] == 0x00 && a.2[0] & 0x03 == 0),
        "Read Attributes"
    );
}

#[test]
fn endpoints_get_suffixes() {
    let mut r = Rig::new(cfg(true));
    r.join(plug2());
    assert_eq!(
        r.json("zigbee2mqtt/relay2"),
        json!({"state_1": "OFF", "state_2": "OFF", "linkquality": 200, "rssi": -55})
    );
    let ex = device_entry(&r, "relay2")["definition"]["exposes"].clone();
    let sw: Vec<&J> = ex
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "switch")
        .collect();
    assert_eq!(sw.len(), 2);
    assert_eq!(sw[1]["endpoint"], "2");
    assert_eq!(sw[1]["features"][0]["property"], "state_2");
    assert_eq!(sw[1]["features"][0]["endpoint"], "2");
    r.sent_aps();
    r.mqtt("zigbee2mqtt/relay2/set", r#"{"state_2": "ON"}"#);
    assert_eq!(ops(&r.sent_aps()), vec![(2, 0x0006, 0x01)]);
    // The endpoint topic form.
    r.mqtt("zigbee2mqtt/relay2/1/set", r#"{"state": "ON"}"#);
    assert_eq!(ops(&r.sent_aps()), vec![(1, 0x0006, 0x01)]);
    assert!(
        r.last("homeassistant/switch/0x00158d0001000004/switch_2/config")
            .is_some(),
        "{:?}",
        r.topics()
    );
}

#[test]
fn tuya_trv_climate() {
    let mut r = Rig::with(
        cfg(true),
        Box::new(hub_app::builtin_catalog()),
        Limits::default(),
    );
    r.join(trv());
    let s = r.sim.dev(TRV).clone();
    let value = |v: i32| v.to_be_bytes().to_vec();
    let f = s.tuya_frame(
        tuya_cmd::DATA_REPORT,
        1,
        &[
            (2, 4, vec![1]),
            (3, 4, vec![0]),
            (4, 2, value(215)),
            (5, 2, value(198)),
        ],
    );
    r.hub.on_aps_indication(f, r.now);
    r.pump();
    let st = r.json("zigbee2mqtt/ME167");
    assert_eq!(st["system_mode"], "heat");
    assert_eq!(st["current_heating_setpoint"], 21.5);
    assert_eq!(st["local_temperature"], 19.8);
    assert_eq!(st["running_state"], "heat");
    let dev = device_entry(&r, "ME167");
    assert_eq!(dev["supported"], true);
    assert_eq!(dev["definition"]["vendor"], "AVATTO");
    let climate = dev["definition"]["exposes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "climate")
        .cloned()
        .unwrap();
    golden("trv_climate_expose", &climate);
    golden(
        "trv_climate_discovery",
        &r.json("homeassistant/climate/0xa4c138aabbccddee/climate/config"),
    );
    r.sent_aps();
    r.mqtt(
        "zigbee2mqtt/ME167/set",
        r#"{"current_heating_setpoint": 22, "system_mode": "auto"}"#,
    );
    let tuya: Vec<Vec<u8>> = r
        .sent_aps()
        .into_iter()
        .filter(|a| a.1 == 0xEF00)
        .map(|a| a.2)
        .collect();
    assert_eq!(tuya.len(), 2, "{tuya:?}");
}

#[test]
fn bridge_requests() {
    let mut r = Rig::new(cfg(true));
    r.join(sensor());
    r.join(light());
    r.take();
    // permit_join (2.x and 1.x forms), with a transaction.
    r.mqtt(
        "zigbee2mqtt/bridge/request/permit_join",
        r#"{"time": 60, "transaction": "a1"}"#,
    );
    assert_eq!(
        r.json("zigbee2mqtt/bridge/response/permit_join"),
        json!({"data": {"time": 60}, "status": "ok", "transaction": "a1"})
    );
    assert_eq!(r.permit_joins().last(), Some(&(60, None)));
    r.mqtt(
        "zigbee2mqtt/bridge/request/permit_join",
        r#"{"value": false}"#,
    );
    assert_eq!(
        r.json("zigbee2mqtt/bridge/response/permit_join"),
        json!({"data": {"time": 0, "value": false}, "status": "ok"})
    );
    r.mqtt(
        "zigbee2mqtt/bridge/request/permit_join",
        r#"{"time": 30, "device": "TRADFRI bulb E27 WS opal 980lm"}"#,
    );
    assert_eq!(r.permit_joins().last(), Some(&(30, Some(0x1002))));
    // health_check, unknown requests.
    r.mqtt("zigbee2mqtt/bridge/request/health_check", "");
    assert_eq!(
        r.json("zigbee2mqtt/bridge/response/health_check"),
        json!({"data": {"healthy": true}, "status": "ok"})
    );
    r.mqtt("zigbee2mqtt/bridge/request/restart", "");
    assert_eq!(
        r.json("zigbee2mqtt/bridge/response/restart")["status"],
        "error"
    );
    // Rename: the hub device is renamed, the old topics are cleared, the
    // state follows on the new topic, discovery configs (same unique_id)
    // are published again.
    r.take();
    r.mqtt(
        "zigbee2mqtt/bridge/request/device/rename",
        r#"{"from": "lumi.weather", "to": "kitchen/climate", "transaction": 7}"#,
    );
    assert_eq!(
        r.json("zigbee2mqtt/bridge/response/device/rename"),
        json!({"data": {"from": "lumi.weather", "to": "kitchen/climate", "homeassistant_rename": false}, "status": "ok", "transaction": 7})
    );
    assert_eq!(
        r.hub
            .device("zb:00158d0001000001")
            .unwrap()
            .user_name
            .as_deref(),
        Some("kitchen/climate")
    );
    assert_eq!(
        cleared(&r),
        vec![
            "zigbee2mqtt/lumi.weather",
            "zigbee2mqtt/lumi.weather/availability"
        ]
    );
    assert_eq!(r.json("zigbee2mqtt/kitchen/climate")["temperature"], 21.37);
    let disc = r.json("homeassistant/sensor/0x00158d0001000001/temperature/config");
    assert_eq!(disc["state_topic"], "zigbee2mqtt/kitchen/climate");
    assert_eq!(
        disc["unique_id"],
        "0x00158d0001000001_temperature_zigbee2mqtt"
    );
    assert_eq!(
        device_entry(&r, "kitchen/climate")["ieee_address"],
        "0x00158d0001000001"
    );
    // A slash in the name: commands still find it.
    r.sent_aps();
    r.mqtt("zigbee2mqtt/kitchen/climate/get/temperature", "");
    assert!(
        r.sent_aps().iter().any(|a| a.1 == 0x0402),
        "a read of the temperature"
    );
    // Taken, invalid names, unknown devices.
    r.mqtt(
        "zigbee2mqtt/bridge/request/device/rename",
        r#"{"from": "kitchen/climate", "to": "TRADFRI bulb E27 WS opal 980lm"}"#,
    );
    assert_eq!(
        r.json("zigbee2mqtt/bridge/response/device/rename")["error"],
        "friendly_name 'TRADFRI bulb E27 WS opal 980lm' is already in use"
    );
    r.mqtt(
        "zigbee2mqtt/bridge/request/device/rename",
        r#"{"from": "kitchen/climate", "to": "lamp/1"}"#,
    );
    assert_eq!(
        r.json("zigbee2mqtt/bridge/response/device/rename")["status"],
        "error"
    );
    r.mqtt(
        "zigbee2mqtt/bridge/request/device/rename",
        r#"{"from": "nobody", "to": "x"}"#,
    );
    assert_eq!(
        r.json("zigbee2mqtt/bridge/response/device/rename")["error"],
        "Device 'nobody' does not exist"
    );
    // Remove: response, leave event, retained topics and configs cleared.
    r.take();
    r.mqtt(
        "zigbee2mqtt/bridge/request/device/remove",
        r#"{"id": "kitchen/climate", "force": true}"#,
    );
    assert_eq!(
        r.json("zigbee2mqtt/bridge/response/device/remove"),
        json!({"data": {"id": "kitchen/climate", "block": false, "force": true}, "status": "ok"})
    );
    assert_eq!(
        r.json("zigbee2mqtt/bridge/event"),
        json!({"type": "device_leave", "data": {"friendly_name": "kitchen/climate", "ieee_address": "0x00158d0001000001"}})
    );
    assert!(
        cleared(&r).contains(&"homeassistant/sensor/0x00158d0001000001/temperature/config"),
        "{:?}",
        cleared(&r)
    );
    assert!(cleared(&r).contains(&"zigbee2mqtt/kitchen/climate"));
    assert_eq!(
        r.json("zigbee2mqtt/bridge/devices")
            .as_array()
            .unwrap()
            .len(),
        2,
        "coordinator and the light"
    );
}

#[test]
fn action_events() {
    let mut r = Rig::new(cfg(true));
    // A remote: On/Off client cluster → generic `action` event.
    let ieee = 0x00158d0001000009;
    let remote = SimDevice::new(ieee, 0x1009, LogicalType::EndDevice, true, 0x1234)
        .endpoint(1, 0x0104, 0x0820, &[0x0000, 0x0001], &[0x0006])
        .basic("ACME", "remote1", 3);
    r.join(remote);
    let dev = device_entry(&r, "remote1");
    let action = dev["definition"]["exposes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "action")
        .cloned()
        .expect("action expose");
    assert_eq!(action["type"], "enum");
    assert_eq!(action["access"], 1);
    assert!(
        action["values"].as_array().unwrap().contains(&json!("on")),
        "{action}"
    );
    let ev = r.json("homeassistant/event/0x00158d0001000009/action/config");
    assert_eq!(ev["state_topic"], "zigbee2mqtt/remote1/action");
    assert_eq!(ev["value_template"], "{ \"event_type\": \"{{ value }}\" }");
    assert_eq!(ev["event_types"], action["values"]);
    r.take();
    let s = r.sim.dev(ieee).clone();
    let mut ind = s.zcl(
        1,
        0x0006,
        hub_devices::zcl::FrameType::Cluster,
        None,
        3,
        0x01,
        Vec::new(),
    );
    ind.payload[0] &= !0x08; // client → server, as a remote sends it
    r.hub.on_aps_indication(ind, r.now);
    r.pump();
    let states: Vec<J> = r
        .out
        .iter()
        .filter(|m| m.topic == "zigbee2mqtt/remote1")
        .map(|m| serde_json::from_slice(&m.payload).unwrap())
        .collect();
    assert_eq!(states[0]["action"], "on", "{states:?}");
    assert_eq!(r.last("zigbee2mqtt/remote1/action").unwrap().payload, b"on");
    // The action is not kept in the state.
    assert!(states.last().unwrap().get("action").is_none(), "{states:?}");
}

#[test]
fn queue_and_devices_are_bounded() {
    let limits = Limits {
        queue_bytes: 1024,
        devices_bytes: 600,
    };
    let mut r = Rig::with(cfg(false), Box::new(MemoryCatalog::default()), limits);
    r.join(sensor());
    r.join(light());
    // bridge/devices over its limit: published without exposes.
    let devs = r.json("zigbee2mqtt/bridge/devices");
    assert!(
        devs.as_array()
            .unwrap()
            .iter()
            .skip(1)
            .all(|d| d["definition"]["exposes"] == json!([])),
        "{devs}"
    );
    // Log messages flood a queue nobody drains: bounded, counted.
    for _ in 0..200 {
        r.b.on_message("zigbee2mqtt/nobody/set", b"{}");
    }
    assert!(r.b.queued_bytes() <= 2 * 1024, "{}", r.b.queued_bytes());
    assert!(r.b.stats.dropped > 0);
}

#[test]
fn availability_follows_the_device() {
    let mut r = Rig::new(cfg(false));
    r.join(light());
    r.take();
    let id = "zb:00158d0001000002";
    let mut rec = r.hub.device(id).unwrap().record();
    rec["available"] = false.into();
    r.b.on_api(&json!({"type": "event", "id": 2, "seq": 99, "event": {"topic": "device_updated", "device": rec}}).to_string());
    r.pump();
    let a = r
        .last("zigbee2mqtt/TRADFRI bulb E27 WS opal 980lm/availability")
        .expect("availability");
    assert_eq!(
        (a.payload.as_slice(), a.qos, a.retain),
        (&br#"{"state":"offline"}"#[..], 1, true)
    );
}

#[test]
fn snapshot_after_a_lost_session_replaces_the_model() {
    // A device removed while the bridge's session was gone is removed with
    // its topics when the new session's snapshot arrives.
    let mut r = Rig::new(cfg(false));
    r.join(sensor());
    r.join(light());
    r.take();
    r.hub
        .remove_device("zb:00158d0001000001", true, r.now)
        .unwrap();
    let _ = r.hub.drain_events();
    r.b.api_reset();
    let (sid, hello) = r.api.connect();
    r.sid = sid;
    r.b.on_api(&hello);
    for f in r
        .api
        .handle_text(&mut r.hub, sid, r#"{"type":"auth","token":"t"}"#, r.now)
    {
        r.b.on_api(&f);
    }
    r.pump();
    assert_eq!(r.b.devices(), 1);
    assert!(
        r.out
            .iter()
            .any(|m| m.topic == "zigbee2mqtt/lumi.weather" && m.payload.is_empty()),
        "{:?}",
        r.topics()
    );
}

/// Every field of `want` is in `got` with the same value (objects
/// recursively, arrays of objects matched by `name`); `description` and
/// `presets` are not compared.
fn contains(got: &J, want: &J, path: &str) {
    match (got, want) {
        (J::Object(g), J::Object(w)) => {
            for (k, wv) in w {
                if k == "description" || k == "presets" {
                    continue;
                }
                let gv = g
                    .get(k)
                    .unwrap_or_else(|| panic!("{path}.{k} missing in {got}"));
                contains(gv, wv, &format!("{path}.{k}"));
            }
        }
        (J::Array(g), J::Array(w)) if w.iter().all(|x| x.get("name").is_some()) => {
            for wv in w {
                let gv = g
                    .iter()
                    .find(|x| x["name"] == wv["name"])
                    .unwrap_or_else(|| panic!("{path}: no {} in {got}", wv["name"]));
                contains(gv, wv, &format!("{path}[{}]", wv["name"]));
            }
        }
        _ => assert_eq!(got, want, "{path}"),
    }
}

#[test]
fn documents_match_zigbee2mqtt_references() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/z2m_reference.json");
    let z: J = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut r = Rig::with(
        cfg(true),
        Box::new(hub_app::builtin_catalog()),
        Limits::default(),
    );
    r.join(sensor());
    r.join(light());
    r.join(plug());
    contains(
        &r.json("zigbee2mqtt/bridge/state"),
        &z["bridge_state"],
        "bridge_state",
    );
    contains(
        &r.json("zigbee2mqtt/lumi.weather/availability"),
        &z["availability"],
        "availability",
    );
    let st = r.json("zigbee2mqtt/lumi.weather");
    for k in z["sensor_state_keys"].as_array().unwrap() {
        assert!(st[k.as_str().unwrap()].is_number(), "{k} in {st}");
    }
    let sensor = device_entry(&r, "lumi.weather");
    contains(&sensor, &z["device_entry"], "device_entry");
    contains(
        &sensor["definition"]["exposes"],
        &z["sensor_exposes"],
        "sensor_exposes",
    );
    let light = device_entry(&r, "TRADFRI bulb E27 WS opal 980lm");
    let lx = light["definition"]["exposes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "light")
        .unwrap()
        .clone();
    contains(&lx, &z["light_expose"], "light_expose");
    contains(
        &r.json("zigbee2mqtt/TRADFRI bulb E27 WS opal 980lm"),
        &z["light_state"],
        "light_state",
    );
    let plug = device_entry(&r, "SP 120");
    let sx = plug["definition"]["exposes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["type"] == "switch")
        .unwrap()
        .clone();
    contains(&sx, &z["switch_expose"], "switch_expose");
    contains(
        &r.json("homeassistant/sensor/0x00158d0001000001/temperature/config"),
        &z["ha_sensor_temperature"],
        "ha_sensor_temperature",
    );
    contains(
        &r.json("homeassistant/light/0x00158d0001000002/light/config"),
        &z["ha_light"],
        "ha_light",
    );
    contains(
        &r.json("homeassistant/switch/0x00158d0001000003/switch/config"),
        &z["ha_switch"],
        "ha_switch",
    );
    r.mqtt("zigbee2mqtt/bridge/request/permit_join", r#"{"time": 254}"#);
    contains(
        &r.json("zigbee2mqtt/bridge/response/permit_join"),
        &z["permit_join_response"],
        "permit_join_response",
    );
    r.mqtt(
        "zigbee2mqtt/bridge/request/device/rename",
        r#"{"from": "SP 120", "to": "kitchen plug"}"#,
    );
    contains(
        &r.json("zigbee2mqtt/bridge/response/device/rename"),
        &z["rename_response"],
        "rename_response",
    );
    r.mqtt(
        "zigbee2mqtt/bridge/request/device/remove",
        r#"{"id": "kitchen plug"}"#,
    );
    contains(
        &r.json("zigbee2mqtt/bridge/response/device/remove"),
        &z["remove_response"],
        "remove_response",
    );
    r.mqtt("zigbee2mqtt/bridge/request/health_check", "{}");
    contains(
        &r.json("zigbee2mqtt/bridge/response/health_check"),
        &z["health_check_response"],
        "health_check_response",
    );
}

#[test]
fn hub_version_skips_the_board_suffix() {
    use crate::bridge::hub_version;
    assert_eq!(
        hub_version("zigbee-rs-hub 0.3.0 (SONOFF Dongle Max (Dongle-M))"),
        "0.3.0"
    );
    assert_eq!(hub_version("zigbee-rs-hub 0.1.8"), "0.1.8");
    assert_eq!(hub_version("zigbee-rs-hub"), "");
}
