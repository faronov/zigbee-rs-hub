//! The engine's [`ZigbeeTransport`] on a coordinator backend
//! ([`Coordinator`]).
//!
//! Requests get a [`RequestId`] from the backend at once, which never
//! waits. Their outcomes come back as [`CoordEvent`](hub_coord::CoordEvent)s
//! the app hands to [`CoordTransport::outcome`]:
//!
//! - APS outcomes are counted ([`ApsCounters`], [`count`]) and kept for the
//!   1 s tick ([`CoordTransport::take_done`]), where the engine fails a
//!   command at once on a delivery failure instead of after its response
//!   timeout.
//! - Outcomes of control requests (GP, group membership, removal, permit
//!   join) are passed on as soon as the app has them
//!   ([`CoordTransport::take_control`]).
//! - The app's own requests (the device inventory) come back to the app.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use hub_coord::{CoordStatus, Coordinator, NetState, NetworkAdmin};
use hub_devices::transport::{CoordError, Outcome, Request, RequestId, Terminal, ZigbeeTransport};

/// Outcome counters of APS requests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApsCounters {
    /// Accepted by the NCP.
    pub accepted: u64,
    /// Refused with `BUSY`.
    pub busy: u64,
    /// Refused otherwise, or lost in a reset.
    pub failed: u64,
    /// Confirmed delivered (APS ACK for acknowledged unicasts).
    pub delivered: u64,
    /// Confirmed not delivered (no APS ACK, no route, …).
    pub not_delivered: u64,
    /// Accepted, but no confirm in time.
    pub unconfirmed: u64,
}

/// Counts the outcome of an APS request (design §4):
///
/// | Outcome | Counters |
/// |---|---|
/// | `Delivered` | accepted, delivered |
/// | `NotDelivered` | accepted, not_delivered |
/// | `Sent` | accepted |
/// | `Refused(Busy)` | busy |
/// | `Refused(_)` | failed |
/// | `Unknown(Timeout)` | accepted, unconfirmed |
/// | `Unknown(_)` | failed |
/// | `Cancelled` | — |
pub fn count(c: &mut ApsCounters, t: &Terminal) {
    match t {
        Terminal::Delivered => {
            c.accepted += 1;
            c.delivered += 1;
        }
        Terminal::NotDelivered(_) => {
            c.accepted += 1;
            c.not_delivered += 1;
        }
        Terminal::Sent => c.accepted += 1,
        Terminal::Refused(CoordError::Busy) => c.busy += 1,
        Terminal::Refused(_) => c.failed += 1,
        Terminal::Unknown(CoordError::Timeout) => {
            c.accepted += 1;
            c.unconfirmed += 1;
        }
        Terminal::Unknown(_) => c.failed += 1,
        Terminal::Cancelled | Terminal::Done(_) => {}
    }
}

/// Removes `id` from `ids`; `false` when it was not there.
fn take(ids: &mut Vec<RequestId>, id: RequestId) -> bool {
    let i = ids.iter().position(|x| *x == id);
    i.map(|i| ids.swap_remove(i)).is_some()
}

/// The transport.
pub struct CoordTransport {
    coord: Box<dyn Coordinator>,
    admin: Arc<dyn NetworkAdmin>,
    /// IEEE → NWK of known devices (the address a removal names).
    pub nwk: BTreeMap<u64, u16>,
    /// The engine's APS requests in flight (`Vec`s: far less code than
    /// B-tree sets on the hub).
    aps: Vec<RequestId>,
    /// The engine's control requests in flight.
    controls: Vec<RequestId>,
    /// The app's own requests in flight.
    own: Vec<RequestId>,
    /// Control requests the engine submitted so far.
    submitted: u64,
    /// Outcomes of finished APS requests, for the tick.
    done: Vec<(RequestId, Terminal)>,
    /// Outcomes of control requests, for the app as soon as it is back.
    control: VecDeque<(RequestId, Outcome)>,
    /// APS outcome counters.
    pub counters: ApsCounters,
}

impl CoordTransport {
    /// A transport on `coord`; `admin` tells the TX power the operator set.
    pub fn new(coord: Box<dyn Coordinator>, admin: Arc<dyn NetworkAdmin>) -> Self {
        Self {
            coord,
            admin,
            nwk: BTreeMap::new(),
            aps: Vec::new(),
            controls: Vec::new(),
            own: Vec::new(),
            submitted: 0,
            done: Vec::new(),
            control: VecDeque::new(),
            counters: ApsCounters::default(),
        }
    }

    /// The backend's status snapshot.
    pub fn status(&self) -> CoordStatus {
        self.coord.status()
    }

    /// A request of the app itself, not the engine's: its outcome comes
    /// back from [`Self::outcome`].
    pub fn submit_own(&mut self, req: Request) -> Result<RequestId, CoordError> {
        let id = self.coord.submit(req, None)?;
        self.own.push(id);
        Ok(id)
    }

    /// An outcome from the backend: kept for the engine, or returned when
    /// the request is the app's own.
    pub fn outcome(&mut self, id: RequestId, o: Outcome) -> Option<Outcome> {
        let done = matches!(o, Outcome::Done(_));
        if self.own.contains(&id) {
            if done {
                take(&mut self.own, id);
            }
            return Some(o);
        }
        match o {
            Outcome::Done(t) if take(&mut self.aps, id) => {
                count(&mut self.counters, &t);
                self.done.push((id, t));
            }
            o => {
                if done {
                    take(&mut self.controls, id);
                }
                self.control.push_back((id, o));
            }
        }
        None
    }

    /// Control requests of the engine wait for their outcome.
    pub fn controls_open(&self) -> bool {
        !self.controls.is_empty()
    }

    /// How many control requests the engine has submitted.
    pub fn controls_submitted(&self) -> u64 {
        self.submitted
    }

    /// Outcomes of the APS requests finished since the last call, in the
    /// order they finished.
    pub fn take_done(&mut self) -> Vec<(RequestId, Terminal)> {
        std::mem::take(&mut self.done)
    }

    /// The next outcome of a control request.
    pub fn take_control(&mut self) -> Option<(RequestId, Outcome)> {
        self.control.pop_front()
    }
}

impl ZigbeeTransport for CoordTransport {
    fn submit(&mut self, req: Request) -> Result<RequestId, CoordError> {
        let req = match req {
            // The app reads the NCP's devices itself (`import_ncp_devices`).
            Request::DeviceInventory => return Err(CoordError::Unsupported),
            // The NCP removes by the address the transport knows, as before.
            Request::RemoveDevice { ieee, .. } => Request::RemoveDevice { ieee, nwk: self.nwk.get(&ieee).copied() },
            req => req,
        };
        let aps = matches!(req, Request::Aps(_));
        let id = self.coord.submit(req, None)?;
        if aps {
            self.aps.push(id);
        } else {
            self.controls.push(id);
            self.submitted += 1;
        }
        Ok(id)
    }

    fn network_info(&mut self) -> Option<serde_json::Value> {
        Some(network_info(&self.coord.status(), self.admin.tx_power_setting()))
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `network/info`: the backend's view of the network and the NCP
/// (`tx_power_requested`: what the operator set). EUI-64 values are
/// big-endian hex, as in device records.
pub fn network_info(st: &CoordStatus, tx_power_requested: Option<i8>) -> serde_json::Value {
    use serde_json::json;
    let be = |b: &[u8; 8]| {
        let mut r = *b;
        r.reverse();
        hex(&r)
    };
    let mut o = json!({
        "state": match st.network_state { NetState::Up => "up", NetState::FormedDown => "formed_down", NetState::None => "none" },
        "session": format!("{:?}", st.session).to_lowercase(),
        "ownership": st.ownership.as_str(),
        "permit_join_s": st.permit_join_s,
    });
    if let Some(n) = &st.network {
        o["pan_id"] = n.pan_id.into();
        o["ext_pan_id"] = be(&n.ext_pan_id).into();
        o["channel"] = n.channel.into();
        o["nwk_update_id"] = n.nwk_update_id.into();
        if let Some(p) = n.tx_power {
            o["tx_power"] = p.into();
        }
    }
    // Re-applied after every NCP restart.
    if let Some(p) = tx_power_requested {
        o["tx_power_requested"] = p.into();
    }
    if let Some(s) = &st.ncp {
        let mut ncp_o = json!({
            "fw": s.fw,
            "protocol": format!("{}.{}", s.protocol.0, s.protocol.1),
            "chip": s.chip,
            "baud": s.baud,
            "flow_control": s.flow_control,
        });
        // What SYS_GET_INFO told, when the NCP answered it.
        if let Some(ieee) = &s.ieee {
            ncp_o["ieee"] = be(ieee).into();
            ncp_o["flash_kib"] = (s.flash_size / 1024).into();
            ncp_o["board"] = s.board.clone().into();
            ncp_o["product"] = s.product.clone().into();
        }
        o["ncp"] = ncp_o;
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use hub_ncp::driver::{Error, Outcome as Answer};
    use hub_ncp::zrhp::msg::{Kind, ids};
    use hub_ncp::zrhp::types::Status;
    use hub_ncp::{Message, coord::aps_terminal};

    fn reply(status: Status) -> Message {
        Message { kind: Kind::Response, msg_id: ids::APS_DATA_REQUEST, tid: 7, payload: vec![status.to_u8()] }
    }

    fn confirm(status: u8, zb_status: u8) -> Option<Message> {
        let payload = vec![7, 0, status, zb_status, 1, 0x34, 0x12, 1, 10, 0];
        Some(Message { kind: Kind::Event, msg_id: ids::EVT_APS_DATA_CONFIRM, tid: 1, payload })
    }

    /// Every branch of the host core's APS outcomes, its terminal and the
    /// counters it moves (design §4).
    #[test]
    fn aps_outcomes_count_as_pinned() {
        let ok = |confirm| Ok(Answer { reply: reply(Status::Success), confirm });
        let refused = |s| Ok(Answer { reply: reply(s), confirm: None });
        let c = |accepted, busy, failed, delivered, not_delivered, unconfirmed| ApsCounters {
            accepted,
            busy,
            failed,
            delivered,
            not_delivered,
            unconfirmed,
        };
        let failed = c(0, 0, 1, 0, 0, 0);
        let rows: Vec<(Result<Answer, Error>, &str, ApsCounters)> = vec![
            (ok(confirm(0, 0)), "Delivered", c(1, 0, 0, 1, 0, 0)),
            (ok(confirm(1, 0xD0)), "NotDelivered", c(1, 0, 0, 0, 1, 0)),
            (ok(None), "Sent", c(1, 0, 0, 0, 0, 0)),
            (refused(Status::Busy), "Refused(Busy)", c(0, 1, 0, 0, 0, 0)),
            (refused(Status::InvalidParameter), "Refused", failed),
            (refused(Status::InProgress), "Refused", failed),
            (Err(Error::Timeout), "Unknown(Timeout)", c(1, 0, 0, 0, 0, 1)),
            (Err(Error::LinkReset), "Unknown", failed),
            (Err(Error::Rejected(2)), "Unknown", failed),
            (Err(Error::Codec), "Unknown", failed),
            (Err(Error::Closed), "Unknown", failed),
            // The driver's own refusal of a payload too large.
            (Err(Error::Status(Status::TooLarge)), "Unknown", failed),
        ];
        for (r, want, counters) in rows {
            let t = aps_terminal(r, false);
            let name = match &t {
                Terminal::Delivered => "Delivered",
                Terminal::NotDelivered(_) => "NotDelivered",
                Terminal::Sent => "Sent",
                Terminal::Refused(CoordError::Busy) => "Refused(Busy)",
                Terminal::Refused(_) => "Refused",
                Terminal::Unknown(CoordError::Timeout) => "Unknown(Timeout)",
                Terminal::Unknown(_) => "Unknown",
                t => panic!("{t:?}"),
            };
            let mut got = ApsCounters::default();
            count(&mut got, &t);
            assert_eq!((name, got), (want, counters), "{t:?}");
        }
        // Texts as the engine has always shown them.
        let t = aps_terminal(ok(confirm(1, 0xD0)), false);
        assert_eq!(t, hub_ncp::coord::zrhp_not_delivered(0xD0, false));
        let Terminal::NotDelivered(e) = t else { unreachable!() };
        assert_eq!(e.to_string(), "no route to the device");
        let Terminal::Refused(e) = aps_terminal(refused(Status::InvalidParameter), false) else { unreachable!() };
        assert_eq!(e.to_string(), "ZRHP status 0x03");
    }
}
