//! The narrow boundary between the engine and the coordinator backend:
//! ticketed requests out ([`Request`]: APS data, GP requests, group
//! membership, device removal, permit join), outcomes and indications in.
//!
//! The engine is sans-IO: it never blocks. [`ZigbeeTransport::submit`]
//! returns a [`RequestId`] at once (or refuses the request); what happens
//! to the request follows through [`crate::hub::Hub::on_outcome`]: at most
//! one [`Progress`] (`Accepted`, with the ZCL TSN of a remote GP request),
//! then exactly one [`Terminal`]. Everything else that arrives (APS
//! indications, GP events, joins) is fed into [`crate::hub::Hub`] methods
//! with the current time. Responses are matched by ZCL/ZDO transaction
//! sequence numbers the engine allocates itself.

use crate::gp::types::GpRequest;
pub use hub_coord::{
    ApsDst, ApsIndication, ApsRequest, CoordError, Progress, Reply, Request, RequestId, Terminal,
    TxOptions,
};

/// What [`crate::hub::Hub::on_outcome`] takes: a request's progress or its
/// final outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Intermediate outcome (at most once, before the terminal one).
    Progress(Progress),
    /// Final outcome (exactly once).
    Done(Terminal),
}

impl From<Progress> for Outcome {
    fn from(p: Progress) -> Self {
        Self::Progress(p)
    }
}

impl From<Terminal> for Outcome {
    fn from(t: Terminal) -> Self {
        Self::Done(t)
    }
}

/// What the engine needs from the coordinator backend.
pub trait ZigbeeTransport {
    /// Queues a request: its ID at once, its outcome later through
    /// [`crate::hub::Hub::on_outcome`]; `Err`: refused at once, nothing
    /// was sent. Every request but [`Request::Aps`] is a barrier: nothing
    /// submitted later reaches the NCP before it completes.
    fn submit(&mut self, req: Request) -> Result<RequestId, CoordError>;

    /// An outcome the transport decided itself in [`Self::submit`], which
    /// the engine applies right after the request that produced it (the
    /// mock's automatic completion). Backends hand their outcomes to
    /// [`crate::hub::Hub::on_outcome`] instead and return `None`.
    fn completed(&mut self) -> Option<(RequestId, Outcome)> {
        None
    }

    /// Network and NCP facts for `network/info` (a JSON object; `None`:
    /// the host core does not know them).
    fn network_info(&mut self) -> Option<serde_json::Value> {
        None
    }
}

/// Something the mock recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sent {
    /// APS.
    Aps(ApsRequest),
    /// GP request with the TSN the mock returned.
    Gp(GpRequest, Option<u8>),
    /// Group membership.
    Group(u8, u16),
    /// Group membership removed.
    GroupRemove(u8, u16),
    /// Device removal.
    Remove(u64),
    /// Permit join.
    PermitJoin(u8, Option<u16>),
}

/// Recording transport for host tests.
///
/// By default it completes every control request at once (GP remote
/// requests are `Accepted` with the next [`Self::next_gp_tsn`]), as a
/// coordinator that answers before the engine sends anything else; APS
/// requests get no outcome unless a test passes one to
/// [`crate::hub::Hub::on_outcome`]. With [`Self::manual`], nothing
/// completes on its own: the requests wait in [`Self::open`].
#[derive(Debug, Default)]
pub struct MockTransport {
    /// Everything sent, in order.
    pub sent: Vec<Sent>,
    /// Next ZCL TSN returned for remote GP requests.
    pub next_gp_tsn: u8,
    /// Refuse every APS and GP request with this error.
    pub fail: Option<CoordError>,
    /// What `network_info` returns.
    pub network: Option<serde_json::Value>,
    /// Complete nothing automatically.
    pub manual: bool,
    /// [`Self::manual`]: the requests submitted, for the test to complete.
    pub open: Vec<(RequestId, Request)>,
    next_id: u32,
    done: std::collections::VecDeque<(RequestId, Outcome)>,
}

impl MockTransport {
    /// Takes the recorded traffic.
    pub fn take(&mut self) -> Vec<Sent> {
        std::mem::take(&mut self.sent)
    }
    /// The APS requests sent so far.
    pub fn aps(&self) -> Vec<&ApsRequest> {
        self.sent
            .iter()
            .filter_map(|s| match s {
                Sent::Aps(a) => Some(a),
                _ => None,
            })
            .collect()
    }
    /// The ID of the last request submitted.
    pub fn last_id(&self) -> Option<RequestId> {
        self.next_id.checked_sub(1).map(RequestId)
    }
}

impl ZigbeeTransport for MockTransport {
    fn submit(&mut self, req: Request) -> Result<RequestId, CoordError> {
        if let (Some(e), Request::Aps(_) | Request::Gp(_)) = (&self.fail, &req) {
            return Err(e.clone());
        }
        let id = RequestId(self.next_id);
        self.next_id += 1;
        let mut tsn = None;
        self.sent.push(match &req {
            Request::Aps(a) => Sent::Aps(a.clone()),
            Request::Gp(g) => {
                tsn = g.is_remote().then(|| {
                    let t = self.next_gp_tsn;
                    self.next_gp_tsn = self.next_gp_tsn.wrapping_add(1);
                    t
                });
                Sent::Gp(g.clone(), tsn)
            }
            Request::Group {
                endpoint,
                group,
                add: true,
            } => Sent::Group(*endpoint, *group),
            Request::Group {
                endpoint, group, ..
            } => Sent::GroupRemove(*endpoint, *group),
            Request::RemoveDevice { ieee, .. } => Sent::Remove(*ieee),
            Request::PermitJoin { duration_s, target } => Sent::PermitJoin(*duration_s, *target),
            Request::DeviceInventory => return Err(CoordError::Unsupported),
        });
        if self.manual {
            self.open.push((id, req));
        } else if !matches!(req, Request::Aps(_)) {
            if tsn.is_some() {
                let p = Progress::Accepted { zcl_tsn: tsn };
                self.done.push_back((id, p.into()));
            }
            self.done
                .push_back((id, Terminal::Done(Reply::None).into()));
        }
        Ok(id)
    }

    fn completed(&mut self) -> Option<(RequestId, Outcome)> {
        self.done.pop_front()
    }

    fn network_info(&mut self) -> Option<serde_json::Value> {
        self.network.clone()
    }
}
