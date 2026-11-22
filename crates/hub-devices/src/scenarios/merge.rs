//! A catalog definition that arrives after the device joined (Slacky-DIY
//! TS0201-z-SlD, two endpoints, generic patches plus settings): entity
//! names, endpoint suffixes, one identify, stale entities removed.

use super::*;
use crate::model::LayerSource;
use crate::testutil::sim::{Sim, SimDevice};
use crate::testutil::yaml::load_definition;
use crate::zcl::{TypedValue, ZclType, ZclValue};
use crate::zdo::LogicalType;

const IEEE: u64 = 0xa4c1_38ee_1126_da72;
const ID: &str = "zb:a4c138ee1126da72";

fn sensor() -> SimDevice {
    SimDevice::new(IEEE, 0x3338, LogicalType::EndDevice, false, 0x1141)
        .endpoint(
            1,
            0x0104,
            0x0302,
            &[0x0000, 0x0001, 0x0003, 0x0402, 0x0405, 0x0007],
            &[0x0003, 0x0006, 0x0019],
        )
        .endpoint(2, 0x0104, 0x0302, &[0x0003, 0x0007], &[0x0003, 0x0006])
        .basic("Slacky-DIY", "TS0201-z-SlD", 3)
        .attr(1, 0x0402, 0x0000, TypedValue::int(ZclType::I16, 2137))
        .attr(1, 0x0405, 0x0000, TypedValue::uint(ZclType::U16, 4510))
        .attr(1, 0x0402, 0xF004, TypedValue::int(ZclType::I16, 3000))
        .attr(1, 0x0001, 0x0020, TypedValue::uint(ZclType::U8, 30))
        .attr(1, 0x0001, 0x0021, TypedValue::uint(ZclType::U8, 180))
}

fn definition() -> crate::model::Definition {
    let src = include_str!("fixtures/slacky-diy-ts0201-z-sld.yaml");
    load_definition(src, LayerSource::Catalog).unwrap()
}

fn names(hub: &Hub<MockTransport>) -> Vec<(String, String)> {
    let d = hub.device(ID).unwrap();
    d.entities
        .iter()
        .filter(|e| !e.orphaned)
        .map(|e| {
            let r = e.record(&d.id);
            (
                e.uid.clone(),
                r["name"]
                    .as_str()
                    .unwrap_or(r["key"].as_str().unwrap())
                    .to_string(),
            )
        })
        .collect()
}

fn uids(hub: &Hub<MockTransport>) -> Vec<String> {
    names(hub).into_iter().map(|(u, _)| u).collect()
}

fn name_of(hub: &Hub<MockTransport>, key: &str) -> Option<String> {
    names(hub)
        .into_iter()
        .find(|(u, _)| u.ends_with(&format!(":{key}")))
        .map(|(_, n)| n)
}

const ASLEEP: u64 = T0 + 600_000;

/// Joined with generic entities, then the catalog arrives while the
/// device sleeps.
fn joined_then_catalog() -> (Hub<MockTransport>, Sim, Vec<serde_json::Value>) {
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    sim.devices.push(sensor());
    sim.join(&mut hub, IEEE, T0);
    hub.drain_events();
    let mut cat = MemoryCatalog::default();
    cat.insert(definition());
    let rep = hub.set_catalog(Box::new(cat), ASLEEP);
    sim.run(&mut hub, ASLEEP);
    (hub, sim, rep)
}

#[test]
fn catalog_names_have_no_endpoint_suffix_and_one_identify() {
    let (hub, _, rep) = joined_then_catalog();
    assert_eq!(rep.len(), 1);
    let all = names(&hub);
    for (uid, name) in &all {
        assert!(
            !name.ends_with(" 1") && !name.ends_with(" 2"),
            "{uid}: {name}"
        );
    }
    let identify: Vec<_> = uids(&hub)
        .into_iter()
        .filter(|u| u.ends_with(":identify"))
        .collect();
    assert_eq!(identify, [format!("{ID}:1:identify")]);
    // Settings from the definition, the generic sensors kept once.
    for k in [
        "temperature",
        "humidity",
        "battery",
        "battery_voltage",
        "high_temperature",
        "humidity_actions",
    ] {
        assert_eq!(
            uids(&hub)
                .iter()
                .filter(|u| u.ends_with(&format!(":{k}")))
                .count(),
            1,
            "{k}"
        );
    }
    assert_eq!(
        name_of(&hub, "temperature_actions"),
        Some("temperature_actions".into())
    );
    // The herdsman model, for device pictures.
    let rec = hub.device(ID).unwrap().record();
    assert_eq!(rec["definition_model"], "TS0201-z-SlD");
}

#[test]
fn settings_are_read_when_the_sleepy_device_wakes() {
    let (mut hub, mut sim, _) = joined_then_catalog();
    let high = format!("{ID}:1:high_temperature");
    assert_eq!(
        hub.state_of(&high).unwrap()["state"],
        serde_json::Value::Null
    );
    // Its next frame (a report) delivers the queued reads.
    let ind = sim.dev(IEEE).report(
        1,
        0x0402,
        None,
        &[(0x0000, TypedValue::int(ZclType::I16, 2140))],
    );
    hub.on_aps_indication(ind, ASLEEP + 60_000);
    sim.run(&mut hub, ASLEEP + 60_000);
    assert_eq!(
        hub.state_of(&high).unwrap()["state"],
        serde_json::json!(30.0)
    );
}

#[test]
fn removing_the_definition_removes_its_entities() {
    let (mut hub, mut sim, _) = joined_then_catalog();
    hub.drain_events();
    let rep = hub.set_catalog(Box::<MemoryCatalog>::default(), ASLEEP + 1000);
    sim.run(&mut hub, ASLEEP + 1000);
    assert_eq!(rep.len(), 1);
    let ev = hub.drain_events();
    let removed: Vec<_> = ev
        .iter()
        .filter_map(|e| match e {
            HubEvent::EntityRemoved(u) => Some(u.clone()),
            _ => None,
        })
        .collect();
    assert!(
        removed.contains(&format!("{ID}:1:high_temperature")),
        "{removed:?}"
    );
    assert!(hub.entity(&format!("{ID}:1:high_temperature")).is_none());
    assert!(hub.entity(&format!("{ID}:1:temperature")).is_some());
    assert!(hub.device(ID).unwrap().entities.iter().all(|e| !e.orphaned));
}

#[test]
fn a_key_on_two_endpoints_keeps_endpoint_names() {
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    sim.devices.push(
        SimDevice::new(0x1122, 0x4455, LogicalType::Router, true, 0x1234)
            .endpoint(1, 0x0104, 0x0100, &[0x0000, 0x0003, 0x0006], &[])
            .endpoint(2, 0x0104, 0x0100, &[0x0003, 0x0006], &[])
            .basic("Acme", "TwoGang", 1)
            .attr(
                1,
                0x0006,
                0x0000,
                TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(true))),
            )
            .attr(
                2,
                0x0006,
                0x0000,
                TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false))),
            ),
    );
    sim.join(&mut hub, 0x1122, T0);
    let d = hub.device("zb:0000000000001122").unwrap();
    let n: Vec<_> = d
        .entities
        .iter()
        .map(|e| (e.uid.clone(), e.record(&d.id)["name"].clone()))
        .collect();
    let get = |u: &str| {
        n.iter()
            .find(|(x, _)| x.ends_with(u))
            .map(|(_, v)| v.clone())
    };
    assert_eq!(get(":1:light"), Some(serde_json::json!("light 1")), "{n:?}");
    assert_eq!(get(":2:light"), Some(serde_json::json!("light 2")));
    assert_eq!(
        n.iter().filter(|(u, _)| u.ends_with(":identify")).count(),
        1
    );
}
