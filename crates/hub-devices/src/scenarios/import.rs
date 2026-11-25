//! Devices imported from another coordinator (a Zigbee2MQTT or ZHA
//! migration): they appear at once with their entities, nothing is sent
//! until they are heard from (mains devices are probed one at a time), and
//! the configure runs again only when the coordinator IEEE changed.

use super::*;
use crate::device::InterviewStep;
use crate::hub::import::*;
use crate::testutil::sim::{Sim, SimDevice};
use crate::transport::{ApsRequest, Sent};
use crate::zcl::{TypedValue, ZclType, ZclValue};
use crate::zdo::LogicalType;

const PLUG: u64 = 0x5A52_5300_0000_0001;
const SENSOR: u64 = 0x00158d0001a2b3c4;

fn plug(ieee: u64, nwk: u16) -> SimDevice {
    SimDevice::new(ieee, nwk, LogicalType::Router, true, 0x1234)
        .endpoint(1, 0x0104, 0x0051, &[0x0000, 0x0003, 0x0006], &[])
        .basic("zigbee-rs", "SIM-PLUG", 1)
        .attr(
            1,
            0x0006,
            0x0000,
            TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(true))),
        )
}

fn sensor() -> SimDevice {
    SimDevice::new(SENSOR, 0x4321, LogicalType::EndDevice, false, 0x1037)
        .endpoint(
            1,
            0x0104,
            0x0302,
            &[0x0000, 0x0001, 0x0003, 0x0402],
            &[0x0019],
        )
        .basic("ACME", "TH-1", 0x03)
        .attr(1, 0x0402, 0x0000, TypedValue::int(ZclType::I16, 2137))
}

/// What another coordinator's interview of `dev` recorded.
fn imported(dev: &SimDevice) -> ImportedDevice {
    let mut sim = Sim::default();
    sim.devices.push(dev.clone());
    let mut src = hub_with(&[]);
    sim.join(&mut src, dev.ieee, T0);
    let d = src.devices().next().unwrap();
    ImportedDevice {
        ieee: dev.ieee,
        nwk: dev.nwk,
        name: Some(format!(
            "{} at the source",
            d.fp.model_id.clone().unwrap_or_default()
        )),
        fp: d.fp.clone(),
        node_desc: d.node_desc,
        attrs: Vec::new(),
    }
}

fn aps(hub: &mut Hub<crate::transport::MockTransport>) -> Vec<ApsRequest> {
    hub.transport
        .take()
        .into_iter()
        .filter_map(|s| match s {
            Sent::Aps(a) => Some(a),
            _ => None,
        })
        .collect()
}

fn binds(sim: &Sim) -> usize {
    sim.aps()
        .iter()
        .filter(|a| a.profile == 0 && a.cluster == 0x0021)
        .count()
}

#[test]
fn an_imported_device_appears_at_once_and_is_silent_until_heard_from() {
    let p = plug(PLUG, 0x5A01);
    let mut hub = hub_with(&[]);
    let id = "zb:5a52530000000001";
    assert_eq!(
        hub.import_device(&imported(&p), IMPORT_VERIFY, T0),
        Ok(ImportAction::New)
    );
    let d = hub.device(id).unwrap();
    assert_eq!(d.record()["interview"], "imported");
    assert_eq!(d.record()["name"], "SIM-PLUG at the source");
    assert!(
        d.entities.iter().any(|e| e.uid.ends_with(":1:switch")),
        "entities from the fingerprint"
    );
    let events = hub.drain_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, HubEvent::DeviceAdded(i) if i == id))
    );
    assert!(aps(&mut hub).is_empty(), "nothing sent at import");
}

#[test]
fn a_mains_device_is_probed_and_verified_without_reconfigure() {
    let p = plug(PLUG, 0x5A01);
    let mut sim = Sim::default();
    sim.devices.push(p.clone());
    let mut hub = hub_with(&[]);
    hub.import_device(&imported(&p), IMPORT_VERIFY, T0).unwrap();
    hub.tick(T0 + 1000);
    // The probe (Basic DateCode/SWBuildID) is the only request.
    let sent = hub.transport.sent.clone();
    assert_eq!(sent.len(), 1, "{sent:?}");
    sim.run(&mut hub, T0 + 1000);
    let d = hub.device("zb:5a52530000000001").unwrap();
    assert_eq!(d.imported, 0, "verified by its answer");
    assert_eq!(d.record()["interview"], "complete");
    assert!(
        d.configure.hash.is_some(),
        "the previous coordinator's configure is taken as done"
    );
    assert_eq!(binds(&sim), 0, "same coordinator IEEE: no binds");
    // The unknown on/off state was read.
    let st = d
        .entities
        .iter()
        .find(|e| e.uid.ends_with(":1:switch"))
        .unwrap();
    assert_eq!(st.state.value, crate::expr::Value::Bool(true));
    // A restart keeps it verified.
    let snap = hub.snapshot_device("zb:5a52530000000001").unwrap();
    assert_eq!(snap.imported, 0);
}

#[test]
fn another_coordinator_ieee_configures_again_two_at_a_time() {
    let mut sim = Sim::default();
    let mut hub = hub_with(&[]);
    for i in 0..5u64 {
        let p = plug(PLUG + i, 0x5A01 + i as u16);
        hub.import_device(&imported(&p), IMPORT_RECONFIGURE, T0)
            .unwrap();
        sim.devices.push(p);
    }
    let _ = hub.drain_events();
    // Every device talks at once (reports): two are configured, the others wait.
    for i in 0..5u64 {
        let dev = sim.dev(PLUG + i).clone();
        let report = dev.report(
            1,
            0x0006,
            None,
            &[(
                0x0000,
                TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false))),
            )],
        );
        hub.on_aps_indication(report, T0 + 10);
    }
    let configuring = || hub_configuring(&hub);
    assert_eq!(configuring(), IMPORT_SLOTS);
    assert_eq!(hub.import_queue.len(), 5 - IMPORT_SLOTS);
    let mut t = T0 + 10;
    for _ in 0..20 {
        sim.run(&mut hub, t);
        t += 1000;
        hub.tick(t);
        if hub.imported_pending() == 0 {
            break;
        }
    }
    assert_eq!(hub.imported_pending(), 0);
    assert!(
        binds(&sim) >= 5,
        "a bind per device to the new coordinator IEEE"
    );
    for i in 0..5u64 {
        let d = hub.device(&crate::hub::zigbee_id(PLUG + i)).unwrap();
        assert_eq!((d.interview, d.imported), (InterviewStep::Done, 0));
    }
}

#[test]
fn a_sleepy_device_tries_again_at_its_next_wake_when_the_slots_are_busy() {
    let mut sim = Sim::default();
    let mut hub = hub_with(&[]);
    for i in 0..2u64 {
        let p = plug(PLUG + i, 0x5A01 + i as u16);
        hub.import_device(&imported(&p), IMPORT_RECONFIGURE, T0)
            .unwrap();
        sim.devices.push(p);
    }
    let s = sensor();
    hub.import_device(&imported(&s), IMPORT_RECONFIGURE, T0)
        .unwrap();
    sim.devices.push(s.clone());
    let _ = hub.drain_events();
    for i in 0..2u64 {
        let report = sim.dev(PLUG + i).report(
            1,
            0x0006,
            None,
            &[(
                0x0000,
                TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false))),
            )],
        );
        hub.on_aps_indication(report, T0 + 10);
    }
    let temp = |v| {
        s.report(
            1,
            0x0402,
            None,
            &[(0x0000, TypedValue::int(ZclType::I16, v))],
        )
    };
    hub.on_aps_indication(temp(2000), T0 + 20);
    assert!(
        !hub.transport
            .sent
            .iter()
            .any(|x| matches!(x, Sent::Aps(a) if a.dst == crate::transport::ApsDst::Nwk(0x4321))),
        "its new attributes do not start its configure either"
    );
    assert_eq!(hub_configuring(&hub), IMPORT_SLOTS);
    assert!(
        hub.import_queue.is_empty(),
        "a sleepy device is not queued: it would be asleep when popped"
    );
    let id = "zb:00158d0001a2b3c4";
    let d = hub.device(id).unwrap();
    assert_eq!(
        (d.interview, d.import_contacted),
        (InterviewStep::Done, false)
    );
    let mut t = T0 + 20;
    for _ in 0..20 {
        sim.run(&mut hub, t);
        t += 1000;
        hub.tick(t);
        if hub_configuring(&hub) == 0 {
            break;
        }
    }
    assert_eq!(hub.imported_pending(), 1, "only the sensor is left");
    // Its next report starts its configure.
    t += 600_000;
    hub.on_aps_indication(temp(2100), t);
    assert_eq!(hub.device(id).unwrap().interview, InterviewStep::Configure);
    for _ in 0..20 {
        sim.run(&mut hub, t);
        t += 1000;
        hub.tick(t);
        if hub.imported_pending() == 0 {
            break;
        }
    }
    assert_eq!(hub.imported_pending(), 0);
}

fn hub_configuring(hub: &Hub<crate::transport::MockTransport>) -> usize {
    hub.devices()
        .filter(|d| d.imported != 0 && d.interview == InterviewStep::Configure)
        .count()
}

#[test]
fn mains_probes_are_spaced_and_sleepy_devices_wait_for_their_wake() {
    let mut hub = hub_with(&[]);
    for i in 0..3u64 {
        hub.import_device(
            &imported(&plug(PLUG + i, 0x5A01 + i as u16)),
            IMPORT_VERIFY,
            T0,
        )
        .unwrap();
    }
    let s = sensor();
    hub.import_device(&imported(&s), IMPORT_VERIFY, T0).unwrap();
    let _ = hub.transport.take();
    let mut probes = Vec::new();
    for k in 0..10u64 {
        hub.tick(T0 + k * 1000);
        for a in aps(&mut hub) {
            // A probe's own retries are not new probes.
            if !probes
                .iter()
                .any(|p: &(u64, crate::transport::ApsDst)| p.1 == a.dst)
            {
                probes.push((k, a.dst));
            }
        }
    }
    // Three plugs at 0, 3 and 6 s (none answers here); the sensor never.
    assert_eq!(
        probes.iter().map(|p| p.0).collect::<Vec<_>>(),
        vec![0, 3, 6],
        "{probes:?}"
    );
    assert!(
        !probes
            .iter()
            .any(|p| p.1 == crate::transport::ApsDst::Nwk(0x4321))
    );
    // Its report verifies it.
    let report = s.report(
        1,
        0x0402,
        None,
        &[(0x0000, TypedValue::int(ZclType::I16, 2000))],
    );
    hub.on_aps_indication(report, T0 + 20_000);
    assert_eq!(hub.device("zb:00158d0001a2b3c4").unwrap().imported, 0);
}

#[test]
fn a_record_without_endpoints_is_interviewed_when_heard_from() {
    let p = plug(PLUG, 0x5A01);
    let mut d = imported(&p);
    d.fp.endpoints.clear();
    let mut sim = Sim::default();
    sim.devices.push(p.clone());
    let mut hub = hub_with(&[]);
    hub.import_device(&d, IMPORT_VERIFY, T0).unwrap();
    assert_eq!(
        hub.device("zb:5a52530000000001").unwrap().record()["interview"],
        "imported"
    );
    let report = p.report(
        1,
        0x0006,
        None,
        &[(
            0x0000,
            TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(true))),
        )],
    );
    hub.on_aps_indication(report, T0 + 10);
    sim.run(&mut hub, T0 + 10);
    let dev = hub.device("zb:5a52530000000001").unwrap();
    assert_eq!((dev.interview, dev.imported), (InterviewStep::Done, 0));
    assert_eq!(dev.fp.endpoints.len(), 1, "interviewed");
    assert_eq!(dev.user_name.as_deref(), Some("SIM-PLUG at the source"));
}

#[test]
fn a_restart_mid_reconfigure_verifies_again() {
    let p = plug(PLUG, 0x5A01);
    let mut hub = hub_with(&[]);
    hub.import_device(&imported(&p), IMPORT_RECONFIGURE, T0)
        .unwrap();
    let report = p.report(
        1,
        0x0006,
        None,
        &[(
            0x0000,
            TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(true))),
        )],
    );
    hub.on_aps_indication(report, T0 + 10);
    let id = "zb:5a52530000000001";
    assert_eq!(hub.device(id).unwrap().interview, InterviewStep::Configure);
    let snap = hub.snapshot_device(id).unwrap();
    assert!(
        snap.interviewed && snap.imported == IMPORT_RECONFIGURE,
        "{snap:?}"
    );
    let mut again = hub_with(&[]);
    again.restore_device(snap, None, T0 + 100).unwrap();
    let d = again.device(id).unwrap();
    assert_eq!(
        (d.interview, d.imported, d.import_contacted),
        (InterviewStep::Done, IMPORT_RECONFIGURE, false)
    );
    assert_eq!(d.record()["interview"], "imported");
}

#[test]
fn imported_groups_join_the_hubs_without_device_traffic() {
    let mut hub = hub_with(&[]);
    for i in 0..2u64 {
        hub.import_device(
            &imported(&plug(PLUG + i, 0x5A01 + i as u16)),
            IMPORT_VERIFY,
            T0,
        )
        .unwrap();
    }
    let _ = hub.transport.take();
    let members = serde_json::json!([
        {"device": "zb:5a52530000000001", "endpoint": 1, "state": "ok"},
        {"device": "zb:5a52530000000002", "endpoint": 1, "state": "ok"}
    ]);
    let groups = [serde_json::json!({"id": 0x1001, "name": "Living room", "members": members})];
    assert_eq!(hub.zg_merge_groups(&groups, T0), 1);
    let g = hub.zg.groups.iter().find(|g| g.id == 0x1001).unwrap();
    assert_eq!(
        (g.name.as_str(), g.members.len(), g.hub_member),
        ("Living room", 2, true)
    );
    let sent = hub.transport.take();
    assert!(
        sent.iter().all(|s| !matches!(s, Sent::Aps(_))),
        "the members hold the group already: {sent:?}"
    );
    assert!(
        sent.iter().any(|s| matches!(s, Sent::Group(1, 0x1001))),
        "the hub endpoint joins it"
    );
    // The same import again changes nothing; a group the hub has keeps its name.
    assert_eq!(hub.zg_merge_groups(&groups, T0 + 1), 0);
    let renamed = [serde_json::json!({"id": 0x1001, "name": "Other", "members": []})];
    assert_eq!(hub.zg_merge_groups(&renamed, T0 + 2), 0);
    assert_eq!(hub.zg.groups[0].name, "Living room");
}
