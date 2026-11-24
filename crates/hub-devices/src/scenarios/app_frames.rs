//! The application's clusters (hub-app's OTA server, 0x0019): device
//! commands go to the application, and its frames are requests of the
//! ticketed transport whose outcomes go back to it, not to the engine.

use super::*;
use crate::hub::{AppOutcome, zigbee_id};
use crate::testutil::sim::{Sim, SimDevice};
use crate::transport::{CoordError, Request, Terminal};
use crate::zcl::{Direction, FrameType, ZclFrame, ZclHeader};
use crate::zdo::LogicalType;

const OTA: u16 = 0x0019;
const PLUG: u64 = 0x0011_2233_4455_6601;
const SENSOR: u64 = 0x0011_2233_4455_6602;

fn ota_client(ieee: u64, nwk: u16, router: bool) -> SimDevice {
    let ty = if router {
        LogicalType::Router
    } else {
        LogicalType::EndDevice
    };
    SimDevice::new(ieee, nwk, ty, router, 0x1002)
        .endpoint(1, 0x0104, 0x0051, &[0x0000], &[OTA])
        .basic("ACME", if router { "Plug" } else { "Sensor" }, 1)
}

fn frame(direction: Direction, tsn: u8, cmd: u8, payload: Vec<u8>) -> ZclFrame {
    ZclFrame {
        header: ZclHeader {
            frame_type: FrameType::Cluster,
            mfr: None,
            direction,
            disable_default_response: direction == Direction::ServerToClient,
            tsn,
            cmd,
        },
        payload,
    }
}

/// An Image Block Response (status ABORT) of the server.
fn block_rsp(tsn: u8) -> ZclFrame {
    frame(Direction::ServerToClient, tsn, 0x05, vec![0x95])
}

/// A delivery failure as a backend reports it.
fn not_delivered() -> Terminal {
    Terminal::NotDelivered(CoordError::Other("no APS ACK".into()))
}

/// The OTA frames submitted and not completed, by request.
fn open_ota(hub: &Hub<MockTransport>) -> Vec<crate::transport::RequestId> {
    hub.transport
        .open
        .iter()
        .filter(|(_, r)| matches!(r, Request::Aps(a) if a.cluster == OTA))
        .map(|(id, _)| *id)
        .collect()
}

#[test]
fn app_frames_are_requests_whose_outcomes_go_to_the_app() {
    let mut hub = hub_with(&[]);
    hub.app_clusters.push(OTA);
    let mut sim = Sim::default();
    sim.devices.push(ota_client(PLUG, 0x1001, true));
    sim.devices.push(ota_client(SENSOR, 0x1002, false));
    sim.join(&mut hub, PLUG, T0);
    sim.join(&mut hub, SENSOR, T0);
    let (plug, sensor) = (zigbee_id(PLUG), zigbee_id(SENSOR));
    hub.transport.take();
    hub.transport.manual = true;
    hub.drain_events();

    // A Query Next Image asking for a Default Response: the application's.
    let q = frame(Direction::ClientToServer, 0x21, 0x01, vec![0; 9]).encode();
    hub.on_aps_indication(sim.dev(PLUG).ind(1, 0x0104, OTA, q.clone()), T0 + 1000);
    let f = hub.take_app_frames();
    assert_eq!(f.len(), 1);
    assert_eq!(
        (f[0].device.as_str(), f[0].cluster, f[0].header.cmd),
        (plug.as_str(), OTA, 0x01)
    );
    assert!(
        open_ota(&hub).is_empty(),
        "no Default Response from the engine"
    );

    // Submitted frames have IDs; their outcomes go to the application and
    // the engine retries nothing.
    let a = hub
        .send_app_zcl(&plug, 1, OTA, block_rsp(0x21), T0 + 1000)
        .unwrap()
        .expect("sent");
    let b = hub
        .send_app_zcl(&plug, 1, OTA, block_rsp(0x22), T0 + 1000)
        .unwrap()
        .expect("sent");
    assert_eq!(open_ota(&hub), [a, b]);
    hub.on_outcome(b, not_delivered().into(), T0 + 1100);
    hub.on_outcome(a, Terminal::Delivered.into(), T0 + 1100);
    hub.tick(T0 + 1100);
    let outcome = |id, terminal| AppOutcome {
        device: plug.clone(),
        cluster: OTA,
        id: Some(id),
        terminal,
    };
    assert_eq!(
        hub.take_app_outcomes(),
        [outcome(b, not_delivered()), outcome(a, Terminal::Delivered)]
    );
    assert_eq!(open_ota(&hub), [a, b], "nothing re-sent");
    hub.transport.open.clear();

    // Refused at once: nothing sent, no outcome later.
    hub.transport.fail = Some(CoordError::Busy);
    assert_eq!(
        hub.send_app_zcl(&plug, 1, OTA, block_rsp(0x23), T0 + 1200),
        Err(CoordError::Busy)
    );
    hub.transport.fail = None;
    assert!(hub.take_app_outcomes().is_empty());

    // A sleepy device: queued until it is heard, then submitted; the
    // outcome carries the ID it got then.
    let later = T0 + 120_000;
    assert_eq!(
        hub.send_app_zcl(&sensor, 1, OTA, block_rsp(0x24), later),
        Ok(None)
    );
    assert!(open_ota(&hub).is_empty());
    hub.on_aps_indication(sim.dev(SENSOR).ind(1, 0x0104, OTA, q), later + 10);
    let [c] = open_ota(&hub)[..] else {
        panic!("one frame flushed")
    };
    hub.on_outcome(c, Terminal::Unknown(CoordError::Timeout).into(), later + 20);
    let out = hub.take_app_outcomes();
    assert_eq!(
        (out[0].device.as_str(), out[0].id),
        (sensor.as_str(), Some(c))
    );
    hub.transport.open.clear();

    // An outcome that never comes is reported unknown after ten minutes.
    let d = hub
        .send_app_zcl(&plug, 1, OTA, block_rsp(0x25), later)
        .unwrap()
        .unwrap();
    hub.tick(later + 9 * 60_000);
    assert!(hub.take_app_outcomes().is_empty());
    hub.tick(later + 10 * 60_000 + 1);
    assert_eq!(
        hub.take_app_outcomes(),
        [outcome(d, Terminal::Unknown(CoordError::Timeout))]
    );
}
