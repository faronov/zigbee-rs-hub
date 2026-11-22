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

/// An `APS_DATA_REQUEST` the NCP could not deliver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApsFailure {
    /// The request as the engine sent it.
    pub req: ApsRequest,
    /// `EVT_APS_DATA_CONFIRM.zb_status` (no APS ACK, no route, …); `None`:
    /// the NCP refused the request (queue full, network down).
    pub zb_status: Option<u8>,
    /// The NCP numbers APS statuses as zigbee-rs NCP images up to 0.1.1
    /// ([`zb_status_text`]).
    pub legacy_codes: bool,
}

/// What an `EVT_APS_DATA_CONFIRM.zb_status` means (host protocol 7.3, the
/// APSDE-DATA.confirm status of R22 Table 2-27 plus NWK route failures
/// passed through). `legacy`: zigbee-rs NCP images up to 0.1.1, whose APS
/// numbering lacked INVALID_GROUP: 0xA6 was NO_ACK on some paths and 0xA8
/// a route failure (0xA7 stayed NO_ACK after the ACK retries).
pub fn zb_status_text(zb_status: u8, legacy: bool) -> Option<&'static str> {
    Some(match (zb_status, legacy) {
        (0xA6 | 0xA7, true) | (0xA7, false) => "no APS ACK",
        (0xA8, true) | (0xD0, _) => "no route to the device",
        (0xA9, true) | (0xAE, false) => "NCP table full",
        (0xA0, _) => "payload too long",
        (0xA8, false) => "no bound device",
        (0xA9, false) => "no short address for the IEEE address",
        (0xAD, _) => "APS security failed (no link key)",
        (0xD1, _) => "route error (the next hop did not take the frame)",
        _ => return None,
    })
}

/// `true` for a zigbee-rs NCP firmware version up to 0.1.1 (the version is
/// the last word of `fw`, e.g. `0.1.1` or `sim-ncp 0.1.0`).
pub fn legacy_zb_status(fw: &str) -> bool {
    let v: Vec<u32> = fw
        .rsplit(' ')
        .next()
        .unwrap_or("")
        .split(['.', '-', '+'])
        .take(3)
        .map_while(|x| x.parse().ok())
        .collect();
    v.len() == 3 && (v[0], v[1], v[2]) <= (0, 1, 1)
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

    /// `NWK_GROUP_MEMBERSHIP` remove for one hub endpoint.
    fn group_remove(&mut self, endpoint: u8, group: u16) -> Result<(), TransportError> {
        let _ = (endpoint, group);
        Err(TransportError::Status(0x05))
    }

    /// Removes a device from the network (`NWK_REMOVE_DEVICE`).
    fn remove_device(&mut self, ieee: u64) -> Result<(), TransportError>;

    /// `NWK_PERMIT_JOIN` for `duration_s` (0 closes), on one router or all.
    fn permit_join(&mut self, duration_s: u8, target: Option<u16>) -> Result<(), TransportError>;

    /// Network and NCP facts for `network/info` (a JSON object; `None`:
    /// the host core does not know them).
    fn network_info(&mut self) -> Option<serde_json::Value> {
        None
    }

    /// Requests that failed since the last call (`EVT_APS_DATA_CONFIRM`
    /// with a failure, or a refusal after [`Self::aps_send`] returned).
    fn take_failures(&mut self) -> Vec<ApsFailure> {
        Vec::new()
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
#[derive(Debug, Default)]
pub struct MockTransport {
    /// Everything sent, in order.
    pub sent: Vec<Sent>,
    /// Next ZCL TSN returned for remote GP requests.
    pub next_gp_tsn: u8,
    /// Fail every send with this error.
    pub fail: Option<TransportError>,
    /// What `network_info` returns.
    pub network: Option<serde_json::Value>,
    /// Delivery failures `take_failures` returns.
    pub failures: Vec<ApsFailure>,
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

    fn group_remove(&mut self, endpoint: u8, group: u16) -> Result<(), TransportError> {
        self.sent.push(Sent::GroupRemove(endpoint, group));
        Ok(())
    }

    fn remove_device(&mut self, ieee: u64) -> Result<(), TransportError> {
        self.sent.push(Sent::Remove(ieee));
        Ok(())
    }

    fn permit_join(&mut self, duration_s: u8, target: Option<u16>) -> Result<(), TransportError> {
        self.sent.push(Sent::PermitJoin(duration_s, target));
        Ok(())
    }

    fn network_info(&mut self) -> Option<serde_json::Value> {
        self.network.clone()
    }

    fn take_failures(&mut self) -> Vec<ApsFailure> {
        std::mem::take(&mut self.failures)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zb_status_texts_spec_and_legacy() {
        assert_eq!(zb_status_text(0xA7, false), Some("no APS ACK"));
        assert_eq!(zb_status_text(0xD0, false), Some("no route to the device"));
        assert_eq!(
            zb_status_text(0xD1, false).map(|t| t.starts_with("route error")),
            Some(true)
        );
        assert_eq!(zb_status_text(0xA8, false), Some("no bound device"));
        assert_eq!(
            zb_status_text(0xA9, false).map(|t| t.starts_with("no short")),
            Some(true)
        );
        assert_eq!(zb_status_text(0xAE, false), Some("NCP table full"));
        // zigbee-rs NCP <= 0.1.1 numbering.
        assert_eq!(zb_status_text(0xA6, true), Some("no APS ACK"));
        assert_eq!(zb_status_text(0xA7, true), Some("no APS ACK"));
        assert_eq!(zb_status_text(0xA8, true), Some("no route to the device"));
        assert_eq!(zb_status_text(0xA9, true), Some("NCP table full"));
        assert_eq!(zb_status_text(0xA6, false), None);
        assert_eq!(zb_status_text(0x42, false), None);
    }

    #[test]
    fn legacy_numbering_by_ncp_version() {
        for (fw, legacy) in [
            ("0.1.1", true),
            ("0.1.0", true),
            ("sim-ncp 0.1.0", true),
            ("0.1.2", false),
            ("0.2.0-rc.1", false),
            ("1.0.0", false),
            ("unknown", false),
        ] {
            assert_eq!(legacy_zb_status(fw), legacy, "{fw}");
        }
    }
}
