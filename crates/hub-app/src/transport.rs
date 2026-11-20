//! The engine's [`ZigbeeTransport`] on the ZRHP host core.
//!
//! APS data goes out through [`hub_ncp::Client::start`] without blocking
//! (requests are held across an NCP resume by the driver); the outcomes are
//! collected on [`NcpTransport::poll`] for the failure counters. GP,
//! group, removal and permit-join requests are rare and answered by the NCP
//! itself, so they are blocking calls, refused at once when the NCP session
//! is not ready (the engine retries or reports the error).

use std::collections::BTreeMap;

use hub_devices::gp::types::GpRequest;
use hub_devices::transport::{ApsRequest, TransportError, ZigbeeTransport};
use hub_ncp::driver::{CallOpts, Error, Ticket};
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
    /// Refused otherwise, timed out or lost in a reset.
    pub failed: u64,
}

/// The transport.
pub struct NcpTransport {
    ncp: Ncp,
    /// IEEE → NWK of known devices (for `NWK_REMOVE_DEVICE`).
    pub nwk: BTreeMap<u64, u16>,
    inflight: Vec<Ticket>,
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
        Self { ncp, nwk: BTreeMap::new(), inflight: Vec::new(), counters: ApsCounters::default() }
    }

    fn ready(&self) -> Result<(), TransportError> {
        match self.ncp.client().state() {
            SessionState::Ready => Ok(()),
            _ => Err(TransportError::LinkDown),
        }
    }

    /// Collects finished APS requests.
    pub fn poll(&mut self) {
        let counters = &mut self.counters;
        self.inflight.retain(|t| match t.try_wait() {
            None => true,
            Some(Ok(o)) => {
                match o.reply.status() {
                    Status::Success => counters.accepted += 1,
                    Status::Busy => counters.busy += 1,
                    s => {
                        debug!("APS_DATA_REQUEST tid {}: {s:?}", t.tid);
                        counters.failed += 1;
                    }
                }
                false
            }
            Some(Err(e)) => {
                debug!("APS_DATA_REQUEST tid {}: {e}", t.tid);
                counters.failed += 1;
                false
            }
        });
    }
}

impl ZigbeeTransport for NcpTransport {
    fn aps_send(&mut self, req: ApsRequest) -> Result<(), TransportError> {
        if matches!(self.ncp.client().state(), SessionState::Stopped | SessionState::Incompatible) {
            return Err(TransportError::LinkDown);
        }
        let payload = convert::aps_request(&req).map_err(transport_error)?;
        let ticket = self.ncp.client().start(ids::APS_DATA_REQUEST, payload, CallOpts::default()).map_err(transport_error)?;
        self.inflight.push(ticket);
        Ok(())
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
}
