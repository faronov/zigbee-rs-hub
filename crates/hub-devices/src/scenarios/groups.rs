//! Groups, scenes and bindings (14.9) over the device simulator: two
//! lights, a sleepy remote.

use serde_json::{Value as J, json};

use super::*;
use crate::api::ApiServer;
use crate::testutil::sim::{Sim, SimDevice};
use crate::transport::{ApsDst, Sent};
use crate::zcl::{TypedValue, ZclFrame, ZclType, ZclValue};
use crate::zdo::{BindDst, LogicalType};

const L1: u64 = 0x0017880100aa0001;
const L2: u64 = 0x0017880100aa0002;
const REMOTE: u64 = 0x0017880100aa0003;
const ID1: &str = "zb:0017880100aa0001";
const ID2: &str = "zb:0017880100aa0002";
const IDR: &str = "zb:0017880100aa0003";
const GROUP_LIGHT: &str = "grp:0001:0:light";

fn color_light() -> SimDevice {
    SimDevice::new(L1, 0x2001, LogicalType::Router, true, 0x100B)
        .endpoint(
            11,
            0x0104,
            0x010D,
            &[0x0000, 0x0003, 0x0004, 0x0005, 0x0006, 0x0008, 0x0300],
            &[],
        )
        .basic("ACME", "Bulb E27", 0x01)
        .attr(
            11,
            6,
            0,
            TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false))),
        )
        .attr(11, 8, 0, TypedValue::uint(ZclType::U8, 100))
        .attr(11, 0x0300, 0x0007, TypedValue::uint(ZclType::U16, 370))
        .attr(11, 0x0300, 0x0008, TypedValue::uint(ZclType::ENUM8, 2))
        .attr(11, 0x0300, 0x400A, TypedValue::uint(ZclType::MAP16, 0x10))
        .attr(11, 0x0300, 0x400B, TypedValue::uint(ZclType::U16, 153))
        .attr(11, 0x0300, 0x400C, TypedValue::uint(ZclType::U16, 454))
}

fn dimmer() -> SimDevice {
    SimDevice::new(L2, 0x2002, LogicalType::Router, true, 0x100B)
        .endpoint(
            1,
            0x0104,
            0x0101,
            &[0x0000, 0x0003, 0x0004, 0x0005, 0x0006, 0x0008],
            &[],
        )
        .basic("ACME", "Dimmer", 0x01)
        .attr(
            1,
            6,
            0,
            TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false))),
        )
        .attr(1, 8, 0, TypedValue::uint(ZclType::U8, 50))
}

fn remote() -> SimDevice {
    SimDevice::new(REMOTE, 0x3003, LogicalType::EndDevice, false, 0x100B)
        .endpoint(
            1,
            0x0104,
            0x0820,
            &[0x0000, 0x0001, 0x0003],
            &[0x0006, 0x0008],
        )
        .basic("ACME", "Remote", 0x03)
}

fn setup() -> (Hub<MockTransport>, Sim) {
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    sim.devices.extend([color_light(), dimmer(), remote()]);
    for d in [L1, L2, REMOTE] {
        sim.join(&mut hub, d, T0);
    }
    hub.drain_events();
    sim.log.clear();
    (hub, sim)
}

fn group_aps(sim: &Sim, group: u16) -> Vec<(u16, ZclFrame)> {
    sim.aps()
        .into_iter()
        .filter(|a| a.dst == ApsDst::Group(group))
        .map(|a| (a.cluster, ZclFrame::parse(&a.payload).unwrap()))
        .collect()
}

fn kinds(events: &[HubEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            HubEvent::Groups { kind, .. } => Some(kind.clone()),
            _ => None,
        })
        .collect()
}

fn kitchen(hub: &mut Hub<MockTransport>, sim: &mut Sim, t: u64) {
    let g = hub.group_create("Kitchen", None, true, t).unwrap();
    assert_eq!(g["id"], 1);
    assert!(
        hub.transport.sent.contains(&Sent::Group(1, 1)),
        "hub endpoint joins the group"
    );
    hub.group_add_member(1, ID1, 11, t).unwrap();
    hub.group_add_member(1, ID2, 1, t).unwrap();
    sim.run(hub, t);
    hub.tick(t + 10);
    sim.run(hub, t + 10);
}

#[test]
fn group_members_entity_aggregate_and_groupcast() {
    let (mut hub, mut sim) = setup();
    // No such group.
    assert_eq!(
        hub.group_add_member(1, ID1, 11, T0).unwrap_err().code,
        "not_found"
    );
    kitchen(&mut hub, &mut sim, T0 + 1000);
    assert!(sim.dev(L1).groups.contains(&(11, 1)));
    assert!(sim.dev(L2).groups.contains(&(1, 1)));
    let rec = &hub.groups_list()[0];
    assert_eq!(rec["members"][0]["state"], "ok");
    assert_eq!(rec["members"][1]["state"], "ok");
    assert_eq!(rec["entity"], GROUP_LIGHT);
    assert_eq!(
        hub.group_add_member(1, IDR, 1, T0).unwrap_err().code,
        "unsupported"
    );

    let evs = hub.drain_events();
    assert!(evs.contains(&HubEvent::DeviceAdded("grp:0001".into())));
    assert!(evs.contains(&HubEvent::EntityAdded(GROUP_LIGHT.into())));
    let dev = hub.zg_device_record("grp:0001").unwrap();
    assert_eq!(dev["name"], "Kitchen");
    assert_eq!(dev["available"], true);
    // The representative is the colour light.
    let ent = hub.zg_entity_record(GROUP_LIGHT).unwrap();
    assert_eq!(ent["platform"], "light");
    assert_eq!(ent["device"], "grp:0001");
    assert_eq!(
        ent["capabilities"]["supported_color_modes"],
        json!(["color_temp"])
    );
    let st = &hub.zg_states()[0];
    assert_eq!(st["state"], "off");
    assert_eq!(st["attributes"]["brightness"], 75);

    // A command is one groupcast; the members follow optimistically.
    sim.log.clear();
    let ack = hub
        .entity_command(
            GROUP_LIGHT,
            "turn_on",
            &json!({"brightness": 200, "transition": 0.5}),
            T0 + 2000,
        )
        .unwrap();
    assert_eq!(ack.status, "ok");
    sim.run(&mut hub, T0 + 2000);
    let sent = group_aps(&sim, 1);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, 0x0008);
    assert_eq!(sent[0].1.header.cmd, 0x04);
    assert!(sent[0].1.header.disable_default_response);
    assert_eq!(sent[0].1.payload, vec![200, 5, 0]);
    let evs = hub.drain_events();
    let gs = states(&evs, GROUP_LIGHT);
    let last = gs.last().expect("group state published");
    assert_eq!(last["state"], "on");
    assert_eq!(last["attributes"]["brightness"], 200);
    assert_eq!(state(&hub, &format!("{ID2}:1:light")), "on");

    // A member's report moves the aggregate.
    let ind = sim.dev(L2).report(
        1,
        6,
        None,
        &[(
            0,
            TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(false))),
        )],
    );
    hub.on_aps_indication(ind, T0 + 3000);
    let evs = hub.drain_events();
    assert!(
        states(&evs, GROUP_LIGHT).is_empty(),
        "unchanged: one member is still on"
    );
    assert_eq!(hub.zg_states()[0]["state"], "on");

    sim.log.clear();
    hub.entity_command(GROUP_LIGHT, "turn_off", &json!({}), T0 + 4000)
        .unwrap();
    sim.run(&mut hub, T0 + 4000);
    assert_eq!(group_aps(&sim, 1)[0].1.header.cmd, 0x00);
    assert!(matches!(
        sim.dev(L1).attrs[&(11, 6, 0, None)].value,
        ZclValue::Bool(Some(false))
    ));
    assert_eq!(
        states(&hub.drain_events(), GROUP_LIGHT).last().unwrap()["state"],
        "off"
    );

    // Identify: a groupcast as well.
    hub.zg_request(
        "groups/identify",
        &json!({"group": 1, "time_s": 5}),
        T0 + 5000,
    )
    .unwrap();
    assert_eq!(hub.transport.aps().last().unwrap().cluster, 0x0003);
}

#[test]
fn scenes_store_recall_view_remove() {
    let (mut hub, mut sim) = setup();
    kitchen(&mut hub, &mut sim, T0 + 1000);
    hub.entity_command(
        GROUP_LIGHT,
        "turn_on",
        &json!({"brightness": 180}),
        T0 + 2000,
    )
    .unwrap();
    sim.run(&mut hub, T0 + 2000);
    let on = || TypedValue::new(ZclType::BOOL, ZclValue::Bool(Some(true)));
    sim.dev(L1).attrs.insert((11, 6, 0, None), on());
    sim.dev(L2).attrs.insert((1, 6, 0, None), on());
    sim.dev(L1)
        .attrs
        .insert((11, 8, 0, None), TypedValue::uint(ZclType::U8, 180));
    sim.dev(L2)
        .attrs
        .insert((1, 8, 0, None), TypedValue::uint(ZclType::U8, 180));

    let g = hub
        .scene_store(1, None, Some("Evening"), T0 + 3000)
        .unwrap();
    assert_eq!(g["scenes"][0]["id"], 1);
    assert_eq!(g["scenes"][0]["results"][format!("{ID1}/11")], "pending");
    sim.run(&mut hub, T0 + 3000);
    let rec = &hub.groups_list()[0];
    assert_eq!(rec["scenes"][0]["results"][format!("{ID1}/11")], "ok");
    assert_eq!(rec["scenes"][0]["results"][format!("{ID2}/1")], "ok");
    assert_eq!(sim.dev(L1).scenes[&(11, 1, 1)], (true, 180));
    let evs = hub.drain_events();
    assert!(evs.contains(&HubEvent::EntityAdded("grp:0001:0:scene_1".into())));
    let scene = hub.zg_entity_record("grp:0001:0:scene_1").unwrap();
    assert_eq!(scene["platform"], "scene");
    assert_eq!(scene["name"], "Evening");

    // Off, then recall: a groupcast, then the members are read again.
    hub.entity_command(GROUP_LIGHT, "turn_off", &json!({}), T0 + 4000)
        .unwrap();
    sim.run(&mut hub, T0 + 4000);
    sim.log.clear();
    let ack = hub
        .entity_command("grp:0001:0:scene_1", "activate", &json!({}), T0 + 5000)
        .unwrap();
    assert_eq!(ack.status, "ok");
    sim.run(&mut hub, T0 + 5000);
    let sent = group_aps(&sim, 1);
    assert_eq!((sent[0].0, sent[0].1.header.cmd), (0x0005, 0x05));
    assert_eq!(sent[0].1.payload, vec![1, 0, 1]);
    hub.drain_events();
    hub.tick(T0 + 7000);
    sim.run(&mut hub, T0 + 7000);
    assert_eq!(state(&hub, &format!("{ID1}:11:light")), "on");
    assert_eq!(
        states(&hub.drain_events(), GROUP_LIGHT).last().unwrap()["state"],
        "on"
    );

    // View on one member: the extension field sets.
    hub.zg_request(
        "scenes/view",
        &json!({"group": 1, "scene": 1, "device": ID2, "endpoint": 1}),
        T0 + 8000,
    )
    .unwrap();
    sim.run(&mut hub, T0 + 8000);
    let view = hub
        .drain_events()
        .into_iter()
        .find_map(|e| match e {
            HubEvent::Groups { kind, body } if kind == "scene_view" => Some(body),
            _ => None,
        })
        .expect("scene_view");
    assert_eq!(view["status"], "SUCCESS");
    assert_eq!(
        view["extension_fields"][1],
        json!({"cluster": 8, "data": "b4"})
    );

    // Remove: from every member, and the entity goes.
    hub.scene_remove(1, 1, T0 + 9000).unwrap();
    sim.run(&mut hub, T0 + 9000);
    assert!(sim.dev(L1).scenes.is_empty() && sim.dev(L2).scenes.is_empty());
    assert!(
        hub.drain_events()
            .contains(&HubEvent::EntityRemoved("grp:0001:0:scene_1".into()))
    );
    assert_eq!(
        hub.scene_recall(1, 1, None, T0).unwrap_err().code,
        "not_found"
    );
}

#[test]
fn delete_group_leaves_members_and_hub_membership() {
    let (mut hub, mut sim) = setup();
    kitchen(&mut hub, &mut sim, T0 + 1000);
    hub.drain_events();
    hub.group_delete(1, false, T0 + 1500).unwrap();
    assert!(hub.transport.sent.contains(&Sent::GroupRemove(1, 1)));
    assert_eq!(hub.groups_list()[0]["deleting"], true);
    let evs = hub.drain_events();
    assert!(evs.contains(&HubEvent::EntityRemoved(GROUP_LIGHT.into())));
    assert!(evs.contains(&HubEvent::DeviceRemoved("grp:0001".into())));
    hub.tick(T0 + 2000);
    sim.run(&mut hub, T0 + 2000);
    assert!(sim.dev(L1).groups.is_empty() && sim.dev(L2).groups.is_empty());
    assert!(hub.groups_list().is_empty());
    assert!(kinds(&hub.drain_events()).contains(&"group_removed".to_string()));
    // The next group reuses the ID.
    assert_eq!(hub.group_create("Hall", None, false, T0).unwrap()["id"], 1);
}

/// The hub endpoint joins no more groups than the coordinator's group
/// table holds; groups without it are not limited.
#[test]
fn the_hub_joins_no_more_groups_than_the_coordinator_table_holds() {
    let (mut hub, _sim) = setup();
    hub.cfg.hub_groups_max = Some(2);
    hub.group_create("A", None, true, T0).unwrap();
    hub.group_create("B", None, true, T0).unwrap();
    let e = hub.group_create("C", None, true, T0).unwrap_err();
    assert_eq!(e.code, "no_space", "{e:?}");
    assert!(e.message.contains("holds 2 groups"), "{}", e.message);
    assert_eq!(hub.groups_list().len(), 2, "nothing created");
    let c = hub.group_create("C", None, false, T0).unwrap()["id"]
        .as_u64()
        .unwrap() as u16;
    assert_eq!(
        hub.group_update(c, None, Some(true), T0).unwrap_err().code,
        "no_space"
    );
    // The hub leaves A: room for C.
    hub.group_update(1, None, Some(false), T0).unwrap();
    hub.group_update(c, Some("C2"), Some(true), T0).unwrap();
    assert!(hub.transport.sent.contains(&Sent::Group(1, c)));
}

#[test]
fn membership_read_reconciles_and_failed_add() {
    let (mut hub, mut sim) = setup();
    sim.dev(L2).groups.insert((1, 0x0007));
    sim.dev(L1).group_capacity = 0;
    hub.group_create("Existing", Some(7), false, T0).unwrap();
    hub.group_add_member(7, ID1, 11, T0).unwrap();
    sim.run(&mut hub, T0);
    let m = &hub.groups_list()[0]["members"][0];
    assert_eq!(m["state"], "failed");
    assert_eq!(m["error"], "INSUFFICIENT_SPACE (table full)");
    // A member the device already has (Add Group answers SUCCESS).
    hub.group_add_member(7, ID2, 1, T0).unwrap();
    hub.zg_request("groups/membership", &json!({"device": ID2}), T0)
        .unwrap();
    sim.run(&mut hub, T0);
    let evs = hub.drain_events();
    let body = evs
        .iter()
        .find_map(|e| match e {
            HubEvent::Groups { kind, body } if kind == "group_membership" => Some(body.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(body["groups"], json!([7]));
    assert_eq!(hub.groups_list()[0]["members"][1]["state"], "ok");
    // A failed member is forgotten at once.
    hub.group_remove_member(7, ID1, 11, false, T0).unwrap();
    assert_eq!(hub.groups_list()[0]["members"].as_array().unwrap().len(), 1);
    assert_eq!(
        hub.group_create("x", Some(7), true, T0).unwrap_err().code,
        "invalid_value"
    );
    assert_eq!(
        hub.group_create("x", Some(0xFFF8), true, T0)
            .unwrap_err()
            .code,
        "invalid_value"
    );
}

/// `devices/remove` of a sleepy device: the Mgmt_Leave_req (rejoin false)
/// goes out at once (its parent holds it until the next poll), not into
/// the device's queue, which goes with the device; then the coordinator's
/// removal.
#[test]
fn a_removed_sleepy_device_is_asked_to_leave_at_once() {
    let (mut hub, mut sim) = setup();
    let t = T0 + 120_000; // the remote's awake window after joining is over
    hub.remove_device(IDR, false, t).unwrap();
    sim.run(&mut hub, t);
    let leave: Vec<_> = sim
        .aps()
        .into_iter()
        .filter(|a| a.profile == 0 && a.cluster == 0x0034)
        .collect();
    assert_eq!(leave.len(), 1, "{:?}", sim.log);
    assert_eq!(
        leave[0].payload[1..10],
        [&REMOTE.to_le_bytes()[..], &[0x00]].concat()[..],
        "rejoin false"
    );
    assert!(leave[0].options.extended_timeout);
    assert!(
        sim.log
            .iter()
            .any(|s| matches!(s, Sent::Remove(i) if *i == REMOTE))
    );
    assert!(hub.device(IDR).is_none());
}

#[test]
fn bindings_on_a_sleepy_remote_wait_for_it() {
    let (mut hub, mut sim) = setup();
    let t = T0 + 120_000; // the remote's awake window after joining is over
    let r = hub
        .zg_request(
            "bindings/create",
            &json!({"device": IDR, "endpoint": 1, "clusters": [6, 8], "target": {"device": ID1, "endpoint": 11}}),
            t,
        )
        .unwrap();
    assert_eq!(r["pending"].as_array().unwrap().len(), 2);
    assert_eq!(r["pending"][0]["state"], "queued");
    sim.run(&mut hub, t);
    assert!(
        sim.dev(REMOTE)
            .bind_table
            .iter()
            .all(|b| b.2 != BindDst::Ieee(L1, 11)),
        "not sent yet"
    );
    // A cluster the endpoint does not have.
    let e = hub
        .zg_request(
            "bindings/create",
            &json!({"device": IDR, "endpoint": 1, "cluster": 0x0300, "target": {"group": 1}}),
            t,
        )
        .unwrap_err();
    assert_eq!(e.code, "invalid_value");

    // The remote talks (a button press): the queue goes out.
    let ind = sim.dev(REMOTE).zcl(
        1,
        6,
        crate::zcl::FrameType::Cluster,
        None,
        0x10,
        0x02,
        vec![],
    );
    hub.on_aps_indication(ind, t + 1000);
    sim.run(&mut hub, t + 1000);
    assert!(
        sim.dev(REMOTE)
            .bind_table
            .contains(&(1, 6, BindDst::Ieee(L1, 11)))
    );
    assert!(
        sim.dev(REMOTE)
            .bind_table
            .contains(&(1, 8, BindDst::Ieee(L1, 11)))
    );
    let b = &hub.bindings_list(Some(IDR))[0];
    assert!(b["pending"].as_array().unwrap().is_empty());
    // Its configure's binds to the hub (On/Off, Level), then these two.
    assert_eq!(b["table"].as_array().unwrap().len(), 4);
    assert_eq!(b["table"][0]["target"], json!({"hub": true, "endpoint": 1}));
    assert_eq!(
        b["table"][2]["target"],
        json!({"device": ID1, "endpoint": 11})
    );

    // Read the table back (Mgmt_Bind pages of two), with a group binding.
    sim.dev(REMOTE).bind_table.push((1, 6, BindDst::Group(1)));
    hub.bindings_read(IDR, t + 2000).unwrap();
    sim.run(&mut hub, t + 2000);
    let b = &hub.bindings_list(Some(IDR))[0];
    assert_eq!(b["reading"], false);
    let n = sim.dev(REMOTE).bind_table.len();
    assert_eq!(b["total"], n);
    assert_eq!(b["table"].as_array().unwrap().len(), n);
    assert!(
        b["table"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["target"] == json!({"group": 1}))
    );

    // Unbind one cluster.
    hub.zg_request(
        "bindings/delete",
        &json!({"device": IDR, "endpoint": 1, "cluster": 8, "target": {"device": ID1, "endpoint": 11}}),
        t + 3000,
    )
    .unwrap();
    sim.run(&mut hub, t + 3000);
    assert!(
        !sim.dev(REMOTE)
            .bind_table
            .contains(&(1, 8, BindDst::Ieee(L1, 11)))
    );
    assert!(kinds(&hub.drain_events()).contains(&"bindings_updated".to_string()));
}

#[test]
fn persisted_groups_resend_what_was_not_confirmed() {
    let (mut hub, mut sim) = setup();
    hub.group_create("Kitchen", None, true, T0).unwrap();
    hub.group_add_member(1, ID1, 11, T0).unwrap();
    sim.run(&mut hub, T0);
    sim.dev(L2).responsive = false;
    hub.group_add_member(1, ID2, 1, T0).unwrap();
    hub.zg_request(
        "bindings/create",
        &json!({"device": IDR, "endpoint": 1, "cluster": 6, "target": {"group": 1}}),
        T0 + 120_000,
    )
    .unwrap();
    assert!(hub.zg_take_dirty());
    let records = hub.zg_export();
    assert_eq!(
        records.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
        ["grp/0001", &format!("bnd/{IDR}")]
    );

    // A restarted hub: devices from their snapshots, then the records.
    let mut hub2 = hub_with(&[]);
    for id in [ID1, ID2, IDR] {
        let s = hub.snapshot_device(id).unwrap();
        hub2.restore_device(s, None, T0 + 200_000).unwrap();
    }
    for (k, v) in &records {
        hub2.zg_import(k, v).unwrap();
    }
    assert_eq!(hub2.groups_resync(T0 + 200_000), 1);
    assert!(hub2.transport.sent.contains(&Sent::Group(1, 1)));
    sim.dev(L2).responsive = true;
    sim.log.clear();
    hub2.tick(T0 + 200_000);
    sim.run(&mut hub2, T0 + 200_000);
    // Only the unconfirmed member is asked again; the binding waits for
    // the sleepy remote.
    let adds: Vec<u16> = sim
        .aps()
        .iter()
        .filter(|a| a.cluster == 0x0004)
        .map(|a| match a.dst {
            ApsDst::Nwk(n) => n,
            _ => 0,
        })
        .collect();
    assert_eq!(adds, [0x2002]);
    let g = &hub2.groups_list()[0];
    assert_eq!(g["members"][1]["state"], "ok");
    assert_eq!(
        hub2.bindings_list(Some(IDR))[0]["pending"][0]["state"],
        "queued"
    );
    assert!(hub2.zg_entity_record(GROUP_LIGHT).is_some());
}

#[test]
fn api_requests_snapshot_and_group_devices() {
    let (mut hub, mut sim) = setup();
    let mut api = ApiServer::new("hub", "t");
    let (s, _) = api.connect();
    api.handle_text(&mut hub, s, r#"{"type":"auth","token":"t"}"#, T0);
    let call = |api: &mut ApiServer, hub: &mut Hub<MockTransport>, body: J| -> J {
        let out = api.handle_text(hub, s, &body.to_string(), T0 + 1000);
        serde_json::from_str(&out[0]).unwrap()
    };
    let r = call(
        &mut api,
        &mut hub,
        json!({"id": 1, "type": "groups/create", "name": "Kitchen"}),
    );
    assert_eq!(r["result"]["id"], 1);
    let r = call(
        &mut api,
        &mut hub,
        json!({"id": 2, "type": "groups/add_member", "group": "0x0001", "device": ID1, "endpoint": 11}),
    );
    assert_eq!(r["success"], true, "{r}");
    sim.run(&mut hub, T0 + 1000);
    let r = call(&mut api, &mut hub, json!({"id": 3, "type": "groups/bogus"}));
    assert_eq!(r["error"]["code"], "invalid_format");
    let r = call(
        &mut api,
        &mut hub,
        json!({"id": 4, "type": "groups/update", "group": 99, "name": "x"}),
    );
    assert_eq!(r["error"]["code"], "not_found");

    // Group devices and entities are listed with the others.
    let r = call(&mut api, &mut hub, json!({"id": 5, "type": "devices/list"}));
    assert!(
        r["result"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["id"] == "grp:0001")
    );
    let r = call(
        &mut api,
        &mut hub,
        json!({"id": 6, "type": "entities/list", "device": "grp:0001"}),
    );
    assert_eq!(r["result"][0]["uid"], GROUP_LIGHT);
    let r = call(
        &mut api,
        &mut hub,
        json!({"id": 7, "type": "entities/states", "uids": [GROUP_LIGHT]}),
    );
    assert_eq!(r["result"][0]["state"], "off");
    let r = call(
        &mut api,
        &mut hub,
        json!({"id": 8, "type": "entities/command", "uid": GROUP_LIGHT, "command": "turn_on", "params": {}}),
    );
    assert_eq!(r["result"]["status"], "ok");

    // Snapshot of the groups topic, then events.
    let out = api.handle_text(
        &mut hub,
        s,
        r#"{"id": 9, "type": "subscribe", "topics": ["groups", "devices", "entities"]}"#,
        T0,
    );
    let snap: J = serde_json::from_str(&out[1]).unwrap();
    assert_eq!(snap["event"]["groups"][0]["name"], "Kitchen");
    assert!(
        snap["event"]["devices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["id"] == "grp:0001")
    );
    assert!(
        snap["event"]["entities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["uid"] == GROUP_LIGHT)
    );
    let _ = call(
        &mut api,
        &mut hub,
        json!({"id": 10, "type": "groups/update", "group": 1, "name": "Cuisine"}),
    );
    let frames = api.pump(&mut hub);
    let evs: Vec<J> = frames
        .iter()
        .map(|(_, f)| serde_json::from_str(f).unwrap())
        .collect();
    assert!(evs.iter().any(
        |e| e["event"]["topic"] == "group_updated" && e["event"]["group"]["name"] == "Cuisine"
    ));
    assert!(
        evs.iter().any(|e| e["event"]["topic"] == "device_updated"
            && e["event"]["device"]["name"] == "Cuisine")
    );

    // HA's "delete device" on the group device deletes the group.
    let r = call(
        &mut api,
        &mut hub,
        json!({"id": 11, "type": "devices/remove", "device": "grp:0001"}),
    );
    assert_eq!(r["success"], true);
    sim.run(&mut hub, T0 + 2000);
    assert!(hub.groups_list().is_empty());
}

#[test]
fn configure_group_step_follows_the_membership_outcome() {
    use crate::device::InterviewStep;
    use crate::model::LayerSource;
    use crate::testutil::yaml::load_definition;
    use crate::transport::{CoordError, Reply, Request, Terminal};
    let def = "zdef: 1\nid: test/gr1\nrevision: 1\nvendor: Test\nmodel: GR1\n\
               match:\n  - {model: GR1}\nconfigure:\n  - group: {group: 0x0042}\n";
    let mut cat = MemoryCatalog::default();
    cat.insert(load_definition(def, LayerSource::Catalog).unwrap());
    let mut hub = hub_with(&[]);
    hub.set_catalog(Box::new(cat), T0);
    hub.transport.manual = true;
    let mut sim = Sim::default();
    sim.devices.push(
        SimDevice::new(REMOTE, 0x3001, LogicalType::Router, true, 0x1234)
            .endpoint(1, 0x0104, 0x0820, &[0x0000], &[0x0006])
            .basic("Test", "GR1", 0x01),
    );
    sim.join(&mut hub, REMOTE, T0);
    let group_req = |hub: &mut Hub<MockTransport>| {
        let i = hub
            .transport
            .open
            .iter()
            .position(|(_, r)| matches!(r, Request::Group { group: 0x0042, .. }))
            .expect("membership request");
        hub.transport.open.remove(i).0
    };
    let key = "group:1:0042";
    // The step waits for the NCP's answer.
    let id = group_req(&mut hub);
    let d = hub.device(IDR).unwrap();
    assert_eq!(d.interview, InterviewStep::Configure);
    assert!(!d.diag.configure.contains_key(key));
    hub.on_outcome(id, Terminal::Refused(CoordError::Busy).into(), T0);
    let d = hub.device(IDR).unwrap();
    assert_eq!(d.diag.configure[key], "FAILURE");
    assert!(d.warnings.contains(&"configure_failed".to_string()));
    // Retried after the back-off; done once the NCP took it.
    hub.tick(T0 + 61_000);
    let id = group_req(&mut hub);
    hub.on_outcome(id, Terminal::Done(Reply::None).into(), T0 + 61_000);
    let d = hub.device(IDR).unwrap();
    assert_eq!(d.diag.configure[key], "SUCCESS");
    assert!(d.warnings.is_empty(), "{:?}", d.warnings);
    assert_eq!(d.interview, InterviewStep::Done);
}

/// The `bindings_updated` events' pending states of the remote.
fn pending_states(events: &[HubEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            HubEvent::Groups { kind, body }
                if kind == "bindings_updated" && body["device"] == IDR =>
            {
                Some(
                    body["pending"]
                        .as_array()?
                        .iter()
                        .map(|p| p["state"].as_str().unwrap_or("").to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                )
            }
            _ => None,
        })
        .collect()
}

fn bind_to_l1(hub: &mut Hub<MockTransport>, t: u64) {
    let r = hub
        .zg_request(
            "bindings/create",
            &json!({"device": IDR, "endpoint": 1, "cluster": 6, "target": {"device": ID1, "endpoint": 11}}),
            t,
        )
        .unwrap();
    assert_eq!(r["pending"][0]["state"], "queued");
}

/// Any uplink of a sleepy remote counts as awake: its Device_annce sends
/// what waits for it (the record says `pending` at once, no longer
/// `queued`), and so does a command it sends to a group.
#[test]
fn a_sleepy_remote_s_queue_goes_out_on_any_uplink() {
    let (mut hub, mut sim) = setup();
    let t = T0 + 120_000;
    bind_to_l1(&mut hub, t);
    hub.drain_events();
    let s = sim.dev(REMOTE).clone();
    let annce = crate::zdo::device_annce(1, s.nwk, REMOTE, s.node.mac_capabilities);
    hub.on_aps_indication(s.ind(0, 0, 0x0013, annce), t + 1000);
    assert_eq!(pending_states(&hub.drain_events()), ["pending"]);
    sim.run(&mut hub, t + 1000);
    assert!(
        sim.dev(REMOTE)
            .bind_table
            .contains(&(1, 6, BindDst::Ieee(L1, 11)))
    );

    // Asleep again; a binding table read waits, a groupcast press sends it.
    sim.dev(REMOTE).bind_table.clear();
    let t = t + 60_000;
    hub.bindings_read(IDR, t).unwrap();
    sim.run(&mut hub, t);
    assert_eq!(hub.bindings_list(Some(IDR))[0]["reading"], true);
    let mut press = s.zcl(
        1,
        6,
        crate::zcl::FrameType::Cluster,
        None,
        0x11,
        0x02,
        vec![],
    );
    press.group = Some(0x0001);
    press.src_ieee = None;
    hub.on_aps_indication(press, t + 1000);
    sim.run(&mut hub, t + 1000);
    assert_eq!(hub.bindings_list(Some(IDR))[0]["reading"], false);
}

/// A poll the NCP reports (ZRHP `EVT_CHILD_POLL`) wakes a sleepy remote
/// as an uplink does: what waits for it goes out at once, to the short
/// address it polled from. A poll of an unknown device changes nothing,
/// and polls publish no link quality (they have none).
#[test]
fn a_sleepy_remote_s_poll_sends_its_queue() {
    let (mut hub, mut sim) = setup();
    let t = T0 + 120_000;
    let nwk = sim.dev(REMOTE).nwk;
    hub.on_child_poll(Some(REMOTE), nwk, t - 20_000);
    hub.drain_events();
    hub.on_child_poll(Some(REMOTE), nwk, t - 19_000);
    assert!(hub.drain_events().is_empty());
    bind_to_l1(&mut hub, t);
    hub.drain_events();
    hub.on_child_poll(Some(0x0017880100aa00ff), 0x7777, t + 500);
    assert!(hub.transport.aps().is_empty());
    assert!(pending_states(&hub.drain_events()).is_empty());

    sim.dev(REMOTE).nwk = 0x4444;
    hub.on_child_poll(Some(REMOTE), 0x4444, t + 1000);
    assert_eq!(pending_states(&hub.drain_events()), ["pending"]);
    assert_eq!(hub.device(IDR).unwrap().nwk(), Some(0x4444));
    sim.run(&mut hub, t + 1000);
    assert!(
        sim.dev(REMOTE)
            .bind_table
            .contains(&(1, 6, BindDst::Ieee(L1, 11)))
    );

    // Asleep again: a binding table read waits; a poll known only by its
    // short address sends it.
    sim.dev(REMOTE).bind_table.clear();
    let t = t + 60_000;
    hub.bindings_read(IDR, t).unwrap();
    sim.run(&mut hub, t);
    assert_eq!(hub.bindings_list(Some(IDR))[0]["reading"], true);
    hub.on_child_poll(None, 0x4444, t + 1000);
    sim.run(&mut hub, t + 1000);
    assert_eq!(hub.bindings_list(Some(IDR))[0]["reading"], false);
}

/// A remote that rejoined under another short address gets what waited
/// for it there: after the trust center's join, and after a frame that
/// shows the new address with its IEEE (also inside the ZDO requests
/// about it).
#[test]
fn a_new_nwk_address_readdresses_the_queue() {
    use crate::zdo::cluster::NODE_DESC_REQ;
    let (mut hub, mut sim) = setup();
    let t = T0 + 120_000;
    bind_to_l1(&mut hub, t);
    sim.dev(REMOTE).nwk = 0x4444;
    hub.on_device_joined(REMOTE, 0x4444, t + 1000);
    sim.run(&mut hub, t + 1000);
    assert!(
        sim.dev(REMOTE)
            .bind_table
            .contains(&(1, 6, BindDst::Ieee(L1, 11)))
    );
    assert_eq!(hub.device(IDR).unwrap().nwk(), Some(0x4444));

    // An interview request parked for it, then it is heard from elsewhere.
    sim.dev(REMOTE).responsive = false;
    let t = t + 60_000;
    hub.reinterview(IDR, t).unwrap();
    hub.tick(t + 21_000);
    sim.run(&mut hub, t + 21_000);
    sim.log.clear();
    sim.dev(REMOTE).nwk = 0x5555;
    let s = sim.dev(REMOTE).clone();
    hub.on_aps_indication(s.report(1, 1, None, &[]), t + 30_000);
    let nd: Vec<_> = hub
        .transport
        .aps()
        .into_iter()
        .filter(|a| a.profile == 0 && a.cluster == NODE_DESC_REQ)
        .cloned()
        .collect();
    assert_eq!(nd.len(), 1, "{nd:?}");
    assert_eq!(nd[0].dst, ApsDst::Nwk(0x5555));
    assert_eq!(nd[0].payload[1..3], 0x5555u16.to_le_bytes());
    assert_eq!(hub.device(IDR).unwrap().nwk(), Some(0x5555));
}

/// A request a sleepy remote never answers fails after its tries (each
/// sent when it was heard from): the binding table read ends with an
/// error instead of `reading` for ever, and a bind fails.
#[test]
fn a_sleepy_remote_s_requests_fail_after_their_tries() {
    let (mut hub, mut sim) = setup();
    sim.dev(REMOTE).responsive = false;
    let t = T0 + 120_000;
    bind_to_l1(&mut hub, t);
    hub.bindings_read(IDR, t).unwrap();
    let s = sim.dev(REMOTE).clone();
    for k in 0..5u64 {
        let tk = t + k * 60_000;
        hub.on_aps_indication(s.report(1, 1, None, &[]), tk + 1000);
        hub.tick(tk + 30_000);
        sim.run(&mut hub, tk + 30_000);
    }
    let sent = sim
        .aps()
        .iter()
        .filter(|a| a.profile == 0 && a.cluster == crate::zdo::cluster::BIND_REQ)
        .count();
    assert_eq!(sent, 3);
    let b = &hub.bindings_list(Some(IDR))[0];
    assert_eq!(b["reading"], false);
    assert!(b["error"].is_string(), "{b}");
    assert_eq!(b["pending"][0]["state"], "failed");
}
