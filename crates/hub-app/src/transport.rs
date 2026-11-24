//! The engine's [`ZigbeeTransport`] on the ZRHP host core.
//!
//! Requests get a [`RequestId`] at once; their outcomes are queued here and
//! handed to the engine ([`hub_devices::Hub::on_outcome`]) by the app:
//!
//! - APS data goes out through [`hub_ncp::Client::start`] without blocking
//!   (requests are held across an NCP resume by the driver), each waiting
//!   for its `EVT_APS_DATA_CONFIRM`; the outcomes are collected on
//!   [`NcpTransport::poll`] in the 1 s tick ([`NcpTransport::take_done`]):
//!   counters, and delivery failures, on which the engine fails a command
//!   at once with the Zigbee status instead of after its response timeout.
//! - GP, group, removal and permit-join requests are rare and answered by
//!   the NCP itself. They are still blocking calls here (refused at once
//!   when the NCP session is not ready); the app passes their outcomes on
//!   as soon as the engine returns ([`NcpTransport::take_control`]).

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use hub_devices::transport::{
    ApsDst, CoordError, Outcome, Progress, Reply, Request, RequestId, Terminal, ZigbeeTransport, legacy_zb_status,
    zrhp_not_delivered,
};
use hub_ncp::coord::coord_error;
use hub_ncp::driver::{CallOpts, Confirm, Error, Ticket};
use hub_ncp::{Hub as Ncp, SessionState};
use log::debug;
use zigbee_zrhp::msg::ids;
use zigbee_zrhp::types::{Ieee, Status};

use crate::convert;

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
    /// Accepted, but no confirm within [`CONFIRM_TIMEOUT`].
    pub unconfirmed: u64,
}

/// How long an accepted request waits for its confirm. The NCP holds a
/// sleepy child's frame until its next expected poll; past this the
/// outcome is only counted (the engine's response timeout decides).
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);

/// The transport.
pub struct NcpTransport {
    ncp: Ncp,
    /// IEEE → NWK of known devices (for `NWK_REMOVE_DEVICE`).
    pub nwk: BTreeMap<u64, u16>,
    inflight: Vec<(Ticket, RequestId, ApsDst)>,
    /// Outcomes of finished APS requests, for the tick.
    done: Vec<(RequestId, Terminal)>,
    /// Outcomes of control requests, for the app as soon as it is back.
    control: VecDeque<(RequestId, Outcome)>,
    next_id: u32,
    /// APS outcome counters.
    pub counters: ApsCounters,
}

/// A host core error, worded as the engine has always shown it.
fn transport_error(e: Error) -> CoordError {
    match e {
        Error::Status(Status::Busy) => CoordError::Busy,
        Error::Status(s) => CoordError::described(coord_error(e), format!("ZRHP status 0x{:02X}", s.to_u8())),
        e => CoordError::described(coord_error(e), "NCP link down"),
    }
}

/// The outcome of a control request the NCP answered with `r`.
fn control_outcome(r: Result<(), Error>) -> Terminal {
    match r {
        Ok(()) => Terminal::Done(Reply::None),
        Err(e @ Error::Status(_)) => Terminal::Refused(transport_error(e)),
        Err(e) => Terminal::Unknown(transport_error(e)),
    }
}

impl NcpTransport {
    /// A transport on `ncp`.
    pub fn new(ncp: Ncp) -> Self {
        Self {
            ncp,
            nwk: BTreeMap::new(),
            inflight: Vec::new(),
            done: Vec::new(),
            control: VecDeque::new(),
            next_id: 0,
            counters: ApsCounters::default(),
        }
    }

    fn ready(&self) -> Result<(), CoordError> {
        match self.ncp.client().state() {
            SessionState::Ready => Ok(()),
            _ => Err(CoordError::LinkDown),
        }
    }

    fn id(&mut self) -> RequestId {
        self.next_id = self.next_id.wrapping_add(1);
        RequestId(self.next_id)
    }

    /// Queues the outcome of control request `id`.
    fn answered(&mut self, id: RequestId, t: Terminal) -> RequestId {
        self.control.push_back((id, t.into()));
        id
    }

    /// Collects finished APS requests.
    pub fn poll(&mut self) {
        if self.inflight.is_empty() {
            return;
        }
        // zigbee-rs NCP images up to 0.1.1 number APS statuses off by one.
        let legacy_codes = self.ncp.client().session().is_some_and(|s| legacy_zb_status(&s.negotiated.fw));
        let (counters, done) = (&mut self.counters, &mut self.done);
        self.inflight.retain(|(t, id, dst)| {
            let Some(r) = t.try_wait() else { return true };
            let terminal = match r {
                Ok(o) => match (o.reply.status(), o.confirm.as_ref().and_then(|c| c.payload.get(2..4))) {
                    (Status::Success, Some(&[0, _])) => {
                        counters.accepted += 1;
                        counters.delivered += 1;
                        Terminal::Delivered
                    }
                    (Status::Success, Some(&[_, zb])) => {
                        debug!("APS_DATA_REQUEST tid {} to {dst:?}: not delivered, zb_status 0x{zb:02X}", t.tid);
                        counters.accepted += 1;
                        counters.not_delivered += 1;
                        zrhp_not_delivered(zb, legacy_codes)
                    }
                    (Status::Success, _) => {
                        counters.accepted += 1;
                        Terminal::Sent
                    }
                    (s, _) => {
                        debug!("APS_DATA_REQUEST tid {}: {s:?}", t.tid);
                        if s == Status::Busy {
                            counters.busy += 1;
                        } else {
                            counters.failed += 1;
                        }
                        Terminal::Refused(transport_error(Error::Status(s)))
                    }
                },
                // Accepted but no confirm in time (a long-polling child).
                Err(Error::Timeout) => {
                    counters.accepted += 1;
                    counters.unconfirmed += 1;
                    Terminal::Unknown(CoordError::Timeout)
                }
                Err(e) => {
                    debug!("APS_DATA_REQUEST tid {}: {e}", t.tid);
                    counters.failed += 1;
                    Terminal::Unknown(coord_error(e))
                }
            };
            done.push((*id, terminal));
            false
        });
    }

    /// Outcomes of the APS requests [`Self::poll`] found finished, in the
    /// order they were sent.
    pub fn take_done(&mut self) -> Vec<(RequestId, Terminal)> {
        std::mem::take(&mut self.done)
    }

    /// The next outcome of a control request.
    pub fn take_control(&mut self) -> Option<(RequestId, Outcome)> {
        self.control.pop_front()
    }
}

impl ZigbeeTransport for NcpTransport {
    fn submit(&mut self, req: Request) -> Result<RequestId, CoordError> {
        match req {
            Request::Aps(req) => {
                if matches!(self.ncp.client().state(), SessionState::Stopped | SessionState::Incompatible) {
                    return Err(CoordError::LinkDown);
                }
                let payload = convert::aps_request(&req).map_err(transport_error)?;
                let opts = CallOpts { confirm: Some(Confirm::aps(CONFIRM_TIMEOUT)), ..CallOpts::default() };
                let ticket = self.ncp.client().start(ids::APS_DATA_REQUEST, payload, opts).map_err(transport_error)?;
                let id = self.id();
                self.inflight.push((ticket, id, req.dst));
                Ok(id)
            }
            Request::Gp(req) => {
                self.ready()?;
                let (msg_id, payload) = convert::gp_request(&req).map_err(transport_error)?;
                let r = self.ncp.client().call_raw(msg_id, &payload, CallOpts::default());
                let id = self.id();
                let r = r.map(|out| {
                    // The ZCL TSN the NCP used, which its EVT_GP_REMOTE_* carries.
                    if req.is_remote() {
                        let p = Progress::Accepted { zcl_tsn: out.reply.payload.get(1).copied() };
                        self.control.push_back((id, p.into()));
                    }
                });
                Ok(self.answered(id, control_outcome(r)))
            }
            Request::Group { endpoint, group, add } => {
                self.ready()?;
                let r = self.ncp.group_membership(if add { 1 } else { 2 }, group, endpoint);
                let id = self.id();
                Ok(self.answered(id, control_outcome(r)))
            }
            // The NCP removes by the address the transport knows, as before.
            Request::RemoveDevice { ieee, .. } => {
                self.ready()?;
                let nwk = self.nwk.get(&ieee).copied().unwrap_or(0xFFFF);
                let r = self.ncp.remove_device(nwk, Ieee::from_u64(ieee).0, 0);
                let id = self.id();
                Ok(self.answered(id, control_outcome(r)))
            }
            Request::PermitJoin { duration_s, target } => {
                self.ready()?;
                let r = self.ncp.permit_join_on(duration_s, target);
                let id = self.id();
                Ok(self.answered(id, control_outcome(r)))
            }
            // The app reads the NCP's devices itself (`import_ncp_devices`).
            Request::DeviceInventory => Err(CoordError::Unsupported),
        }
    }

    fn network_info(&mut self) -> Option<serde_json::Value> {
        Some(network_info(&self.ncp))
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `network/info`: the host core's view of the network and the NCP. EUI-64
/// values are big-endian hex, as in device records.
pub fn network_info(ncp: &Ncp) -> serde_json::Value {
    use serde_json::json;
    let st = ncp.status();
    let be = |b: &[u8; 8]| {
        let mut r = *b;
        r.reverse();
        hex(&r)
    };
    let mut o = json!({
        "state": match st.network_state { 2 => "up", 1 => "formed_down", _ => "none" },
        "session": format!("{:?}", st.session).to_lowercase(),
        "ownership": match &st.ownership {
            hub_ncp::Ownership::Unknown => "unknown",
            hub_ncp::Ownership::NoNetwork => "no_network",
            hub_ncp::Ownership::Owned => "owned",
            hub_ncp::Ownership::Lost => "lost",
            hub_ncp::Ownership::Mismatch(_) => "mismatch",
            hub_ncp::Ownership::Observed => "observed",
        },
        "permit_join_s": ncp.permit_join_remaining(),
    });
    if let Some(n) = &st.network {
        o["pan_id"] = n.pan_id.into();
        o["ext_pan_id"] = be(&n.ext_pan_id).into();
        o["channel"] = n.channel.into();
        o["nwk_update_id"] = n.nwk_update_id.into();
        // 0x7F: unknown (spec 9.3, NWK_GET_INFO before the power is set).
        if n.tx_power != 0x7F {
            o["tx_power"] = n.tx_power.into();
        }
    }
    // What the operator set (re-applied after every NCP restart).
    if let Some(p) = ncp.stored_tx_power() {
        o["tx_power_requested"] = p.into();
    }
    if let Some(s) = &st.ncp {
        let mut ncp_o = json!({
            "fw": s.negotiated.fw,
            "protocol": format!("{}.{}", s.negotiated.major, s.negotiated.minor),
            "chip": s.negotiated.chip,
            "baud": s.line.baud,
            "flow_control": s.line.flow,
        });
        if let Some(i) = &s.info {
            ncp_o["ieee"] = be(&i.ieee_active).into();
            ncp_o["flash_kib"] = (i.flash_size / 1024).into();
            ncp_o["board"] = i.board.clone().into();
            ncp_o["product"] = i.product.clone().into();
        }
        o["ncp"] = ncp_o;
    }
    o
}
