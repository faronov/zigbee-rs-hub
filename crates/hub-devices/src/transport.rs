//! The narrow boundary between the engine and the ZRHP host core
//! (branch `phase1`): APS data out, indications in, GP requests and
//! events, group membership.
//!
//! The engine is sans-IO: it never blocks. Outgoing traffic goes through
//! [`ZigbeeTransport`]; everything that arrives (APS indications, GP
//! events, joins) is fed into [`crate::hub::Hub`] methods with the current
//! time. Responses are matched by ZCL/ZDO transaction sequence numbers the
//! engine allocates itself.

use crate::gp::types::GpRequest;

/// APS destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ApsDst {
    /// NWK short address.
    Nwk(u16),
    /// IEEE address.
    Ieee(u64),
    /// Group.
    Group(u16),
}

/// `APS_DATA_REQUEST.tx_options` subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct TxOptions {
    /// APS acknowledgement.
    pub ack: bool,
    /// Bit 5, extended timeout (sleepy destinations).
    pub extended_timeout: bool,
}

/// One `APS_DATA_REQUEST`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApsRequest {
    /// Destination.
    pub dst: ApsDst,
    /// Destination endpoint (ignored for groups).
    pub dst_ep: u8,
    /// Source endpoint (the hub's endpoint 1; 0 for ZDO; 242 for GP).
    pub src_ep: u8,
    /// Profile.
    pub profile: u16,
    /// Cluster.
    pub cluster: u16,
    /// ZCL/ZDO payload.
    pub payload: Vec<u8>,
    /// Options.
    pub options: TxOptions,
}

/// One `EVT_APS_DATA_INDICATION`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApsIndication {
    /// Source NWK.
    pub src_nwk: u16,
    /// Source IEEE when the NCP knows it.
    pub src_ieee: Option<u64>,
    /// Source endpoint.
    pub src_ep: u8,
    /// Destination endpoint.
    pub dst_ep: u8,
    /// Group the frame was sent to.
    pub group: Option<u16>,
    /// Profile.
    pub profile: u16,
    /// Cluster.
    pub cluster: u16,
    /// Payload.
    pub payload: Vec<u8>,
    /// LQI.
    pub lqi: u8,
    /// RSSI.
    pub rssi: i8,
}

/// Transport failure (the request was not queued).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// The NCP link is down.
    LinkDown,
    /// Queue full / busy.
    Busy,
    /// Any other error with a ZRHP status.
    Status(u8),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LinkDown => write!(f, "NCP link down"),
            Self::Busy => write!(f, "busy"),
            Self::Status(s) => write!(f, "ZRHP status 0x{s:02X}"),
        }
    }
}

impl std::error::Error for TransportError {}

/// What the engine needs from the host core.
pub trait ZigbeeTransport {
    /// Queues an `APS_DATA_REQUEST` (ZCL or ZDO; ZDO on profile 0,
    /// endpoint 0).
    fn aps_send(&mut self, req: ApsRequest) -> Result<(), TransportError>;

    /// Sends a GP request. Remote provisioning requests (0x0A08–0x0A0D)
    /// return the ZCL TSN the NCP used (`GpRemoteRsp.zcl_tsn`), which the
    /// matching `EVT_GP_REMOTE_*` carries; other requests return `None`.
    fn gp_request(&mut self, req: GpRequest) -> Result<Option<u8>, TransportError>;

    /// `NWK_GROUP_MEMBERSHIP` add for one hub endpoint.
    fn group_add(&mut self, endpoint: u8, group: u16) -> Result<(), TransportError>;

    /// Removes a device from the network (`NWK_REMOVE_DEVICE`).
    fn remove_device(&mut self, ieee: u64) -> Result<(), TransportError>;
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
    /// Device removal.
    Remove(u64),
}

/// Recording transport for host tests.
#[derive(Debug, Default)]
pub struct MockTransport {
    /// Everything sent, in order.
    pub sent: Vec<Sent>,
    /// Next ZCL TSN returned for remote GP requests.
    pub next_gp_tsn: u8,
    /// Fail every send with this error.
    pub fail: Option<TransportError>,
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
}

impl ZigbeeTransport for MockTransport {
    fn aps_send(&mut self, req: ApsRequest) -> Result<(), TransportError> {
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        self.sent.push(Sent::Aps(req));
        Ok(())
    }

    fn gp_request(&mut self, req: GpRequest) -> Result<Option<u8>, TransportError> {
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        let tsn = req.is_remote().then(|| {
            let t = self.next_gp_tsn;
            self.next_gp_tsn = self.next_gp_tsn.wrapping_add(1);
            t
        });
        self.sent.push(Sent::Gp(req, tsn));
        Ok(tsn)
    }

    fn group_add(&mut self, endpoint: u8, group: u16) -> Result<(), TransportError> {
        self.sent.push(Sent::Group(endpoint, group));
        Ok(())
    }

    fn remove_device(&mut self, ieee: u64) -> Result<(), TransportError> {
        self.sent.push(Sent::Remove(ieee));
        Ok(())
    }
}
