//! The engine's [`ZigbeeTransport`] on the ZRHP host core.
//!
//! APS data goes out through [`hub_ncp::Client::start`] without blocking
//! (requests are held across an NCP resume by the driver), each waiting for
//! its `EVT_APS_DATA_CONFIRM`; the outcomes are collected on
//! [`NcpTransport::poll`]: counters, and failures for the engine
//! ([`ZigbeeTransport::take_failures`]), which then fails a command at once
//! with the Zigbee status instead of after its response timeout. GP,
//! group, removal and permit-join requests are rare and answered by the NCP
//! itself, so they are blocking calls, refused at once when the NCP session
//! is not ready (the engine retries or reports the error).

use std::collections::BTreeMap;
use std::time::Duration;

use hub_devices::gp::types::GpRequest;
use hub_devices::transport::{ApsFailure, ApsRequest, TransportError, ZigbeeTransport};
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
    inflight: Vec<(Ticket, ApsRequest)>,
    failures: Vec<ApsFailure>,
    /// APS outcome counters.
    pub counters: ApsCounters,
}

fn transport_error(e: Error) -> TransportError {
    match e {
        Error::Status(Status::Busy) => TransportError::Busy,
        Error::Status(s) => TransportError::Status(s.to_u8()),
        _ => TransportError::LinkDown,
    }
}

impl NcpTransport {
    /// A transport on `ncp`.
    pub fn new(ncp: Ncp) -> Self {
        Self { ncp, nwk: BTreeMap::new(), inflight: Vec::new(), failures: Vec::new(), counters: ApsCounters::default() }
    }

    fn ready(&self) -> Result<(), TransportError> {
        match self.ncp.client().state() {
            SessionState::Ready => Ok(()),
            _ => Err(TransportError::LinkDown),
        }
    }

    /// Collects finished APS requests.
    pub fn poll(&mut self) {
        let (counters, failures) = (&mut self.counters, &mut self.failures);
        self.inflight.retain(|(t, req)| {
            let Some(r) = t.try_wait() else { return true };
            let failed = match r {
                Ok(o) => match (o.reply.status(), o.confirm.as_ref().and_then(|c| c.payload.get(2..4))) {
                    (Status::Success, Some(&[0, _])) => {
                        counters.accepted += 1;
                        counters.delivered += 1;
                        None
                    }
                    (Status::Success, Some(&[_, zb])) => {
                        debug!("APS_DATA_REQUEST tid {} to {:?}: not delivered, zb_status 0x{zb:02X}", t.tid, req.dst);
                        counters.accepted += 1;
                        counters.not_delivered += 1;
                        Some(Some(zb))
                    }
                    (Status::Success, _) => {
                        counters.accepted += 1;
                        None
                    }
                    (s, _) => {
                        debug!("APS_DATA_REQUEST tid {}: {s:?}", t.tid);
                        if s == Status::Busy {
                            counters.busy += 1;
                        } else {
                            counters.failed += 1;
                        }
                        Some(None)
                    }
                },
                // Accepted but no confirm in time (a long-polling child).
                Err(Error::Timeout) => {
                    counters.accepted += 1;
                    counters.unconfirmed += 1;
                    None
                }
                Err(e) => {
                    debug!("APS_DATA_REQUEST tid {}: {e}", t.tid);
                    counters.failed += 1;
                    None
                }
            };
            if let Some(zb_status) = failed {
                failures.push(ApsFailure { req: req.clone(), zb_status });
            }
            false
        });
    }
}

impl ZigbeeTransport for NcpTransport {
    fn aps_send(&mut self, req: ApsRequest) -> Result<(), TransportError> {
        if matches!(self.ncp.client().state(), SessionState::Stopped | SessionState::Incompatible) {
            return Err(TransportError::LinkDown);
        }
        let payload = convert::aps_request(&req).map_err(transport_error)?;
        let opts = CallOpts { confirm: Some(Confirm::aps(CONFIRM_TIMEOUT)), ..CallOpts::default() };
        let ticket = self.ncp.client().start(ids::APS_DATA_REQUEST, payload, opts).map_err(transport_error)?;
        self.inflight.push((ticket, req));
        Ok(())
    }

    fn take_failures(&mut self) -> Vec<ApsFailure> {
        std::mem::take(&mut self.failures)
    }

    fn gp_request(&mut self, req: GpRequest) -> Result<Option<u8>, TransportError> {
        self.ready()?;
        let (id, payload) = convert::gp_request(&req).map_err(transport_error)?;
        let out = self.ncp.client().call_raw(id, &payload, CallOpts::default()).map_err(transport_error)?;
        Ok(if req.is_remote() { out.reply.payload.get(1).copied() } else { None })
    }

    fn group_add(&mut self, endpoint: u8, group: u16) -> Result<(), TransportError> {
        self.ready()?;
        self.ncp.group_membership(1, group, endpoint).map_err(transport_error)
    }

    fn remove_device(&mut self, ieee: u64) -> Result<(), TransportError> {
        self.ready()?;
        let nwk = self.nwk.get(&ieee).copied().unwrap_or(0xFFFF);
        self.ncp.remove_device(nwk, Ieee::from_u64(ieee).0, 0).map_err(transport_error)
    }

    fn permit_join(&mut self, duration_s: u8, target: Option<u16>) -> Result<(), TransportError> {
        self.ready()?;
        self.ncp.permit_join_on(duration_s, target).map_err(transport_error)
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
