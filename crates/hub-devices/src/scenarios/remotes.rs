//! Remotes and buttons: their client clusters bound to the hub endpoint
//! and listed with their bindings; IKEA remotes on group-only firmware
//! bound to the bind group the hub endpoint joins (`hub::ikea`).

use serde_json::json;

use super::*;
use crate::testutil::sim::{Sim, SimDevice};
use crate::transport::{ApsIndication, Sent};
use crate::zcl::frame::{Direction, ZclHeader};
use crate::zcl::{FrameType, TypedValue, ZclFrame, ZclType, ZclValue};
use crate::zdo::cluster::BIND_REQ;
use crate::zdo::{BindDst, LogicalType};

const BUTTON: u64 = 0x00124b0022aa0001;
const BUTTON_ID: &str = "zb:00124b0022aa0001";
const REMOTE: u64 = 0x000b57fffe1524aa;
const REMOTE_ID: &str = "zb:000b57fffe1524aa";
/// `HubConfig::default()`'s coordinator and hub endpoint.
const HUB: BindDst = BindDst::Ieee(0x00124b0000000001, 1);

/// A SONOFF SNZB-01 (eWeLink WB01): sleepy, On/Off and Identify clients.
fn snzb01() -> SimDevice {
    SimDevice::new(BUTTON, 0x7001, LogicalType::EndDevice, false, 0x0000)
        .endpoint(
            1,
            0x0104,
            0x0000,
            &[0x0000, 0x0003, 0x0001],
            &[0x0006, 0x0003],
        )
        .basic("eWeLink", "WB01", 0x03)
}

/// An IKEA E1524/E1810 with firmware `sw`.
fn e1524(sw: &str) -> SimDevice {
    SimDevice::new(REMOTE, 0x7002, LogicalType::EndDevice, false, 0x117C)
        .endpoint(
            1,
            0x0104,
            0x0820,
            &[0x0000, 0x0001, 0x0003, 0x0009, 0x0B05, 0x1000],
            &[0x0003, 0x0004, 0x0005, 0x0006, 0x0008, 0x0019, 0x1000],
        )
        .basic("IKEA of Sweden", "TRADFRI remote control", 0x03)
        .attr(
            1,
            0x0000,
            0x4000,
            TypedValue::new(ZclType::CHAR_STR, ZclValue::String(sw.into())),
        )
}

/// A cluster command the device sends (client to server), to the hub
/// endpoint or to `group`.
fn press(s: &SimDevice, cluster: u16, cmd: u8, group: Option<u16>) -> ApsIndication {
    let f = ZclFrame {
        header: ZclHeader {
            frame_type: FrameType::Cluster,
            mfr: None,
            direction: Direction::ClientToServer,
            disable_default_response: false,
            tsn: 0x42,
            cmd,
        },
        payload: Vec::new(),
    };
    let mut ind = s.ind(1, 0x0104, cluster, f.encode());
    if group.is_some() {
        ind.group = group;
        ind.src_ieee = None;
    }
    ind
}

fn joined(dev: SimDevice) -> (Hub<MockTransport>, Sim) {
    let mut hub = hub_with(&[]);
    let mut sim = Sim::default();
    let ieee = dev.ieee;
    sim.devices.push(dev);
    sim.join(&mut hub, ieee, T0);
    (hub, sim)
}

fn table(hub: &Hub<MockTransport>, id: &str) -> Vec<J> {
    hub.bindings_list(Some(id))
        .first()
        .map(|b| b["table"].as_array().unwrap().clone())
        .unwrap_or_default()
}

/// A button's On/Off client cluster is bound to the hub endpoint at
/// configure and listed with its bindings (the web UI shows it); its
/// presses fire its `action` events.
#[test]
fn a_button_s_client_cluster_is_bound_to_the_hub_and_listed() {
    let (mut hub, mut sim) = joined(snzb01());
    assert!(sim.dev(BUTTON).bind_table.contains(&(1, 6, HUB)));
    assert_eq!(
        table(&hub, BUTTON_ID),
        [json!({"endpoint": 1, "cluster": 6, "target": {"hub": true, "endpoint": 1}})]
    );
    hub.drain_events();
    let s = sim.dev(BUTTON).clone();
    hub.on_aps_indication(press(&s, 6, 0x02, None), T0 + 60_000);
    let uid = format!("{BUTTON_ID}:1:action");
    assert_eq!(fired(&hub.drain_events(), &uid)[0].0, "toggle");
}

/// zhc's `ikeaConfigureRemote`: a TRADFRI remote before 2.3.075 binds to
/// a group only. The hub endpoint joins 0x0000 and the bind group 901,
/// On/Off is bound to 901 (listed), no client cluster to the hub; the
/// remote's groupcasts fire its events. The memberships come back at the
/// next NCP session.
#[test]
fn an_ikea_remote_on_group_only_firmware_binds_to_the_bind_group() {
    let (mut hub, mut sim) = joined(e1524("2.3.014"));
    for g in [0x0000, 901] {
        assert!(sim.log.contains(&Sent::Group(1, g)), "{g}");
    }
    let bt = &sim.dev(REMOTE).bind_table;
    assert!(bt.contains(&(1, 6, BindDst::Group(901))), "{bt:?}");
    assert!(!bt.iter().any(|b| b.2 == HUB), "{bt:?}");
    assert_eq!(
        table(&hub, REMOTE_ID),
        [json!({"endpoint": 1, "cluster": 6, "target": {"group": 901}})]
    );
    hub.drain_events();
    let s = sim.dev(REMOTE).clone();
    hub.on_aps_indication(press(&s, 6, 0x02, Some(901)), T0 + 60_000);
    let uid = format!("{REMOTE_ID}:1:action");
    assert_eq!(fired(&hub.drain_events(), &uid)[0].0, "toggle");

    hub.transport.sent.clear();
    assert_eq!(hub.groups_resync(T0 + 70_000), 2);
    for g in [0x0000, 901] {
        assert!(hub.transport.sent.contains(&Sent::Group(1, g)), "{g}");
    }
}

/// A remote that slept through its configure: the bind to the group waits
/// until it is heard. While unbound it sends to group 0x0000, which the
/// hub endpoint joined, so a button press sends the bind.
#[test]
fn an_unbound_ikea_remote_s_press_sends_the_waiting_bind() {
    let mut dev = e1524("2.3.014");
    dev.zdo_silent.insert(BIND_REQ);
    let (mut hub, mut sim) = joined(dev);
    for k in 1..=2u64 {
        hub.tick(T0 + k * 30_000);
        sim.run(&mut hub, T0 + k * 30_000);
    }
    assert!(sim.dev(REMOTE).bind_table.is_empty());
    assert!(sim.log.contains(&Sent::Group(1, 0x0000)));
    sim.dev(REMOTE).zdo_silent.clear();
    let s = sim.dev(REMOTE).clone();
    let t = T0 + 120_000;
    hub.on_aps_indication(press(&s, 6, 0x02, Some(0x0000)), t);
    sim.run(&mut hub, t);
    assert!(
        sim.dev(REMOTE)
            .bind_table
            .contains(&(1, 6, BindDst::Group(901)))
    );
}

/// Firmware 2.3.075 and later binds to the hub endpoint (zhc binds On/Off
/// to the coordinator): no bind group.
#[test]
fn newer_ikea_firmware_binds_to_the_hub() {
    let (hub, mut sim) = joined(e1524("2.3.080"));
    assert!(sim.dev(REMOTE).bind_table.contains(&(1, 6, HUB)));
    assert!(
        table(&hub, REMOTE_ID).contains(
            &json!({"endpoint": 1, "cluster": 6, "target": {"hub": true, "endpoint": 1}})
        )
    );
    assert!(!sim.log.iter().any(|s| matches!(s, Sent::Group(..))));
}
