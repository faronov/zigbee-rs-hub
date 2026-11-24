//! APS data out and in: the engine's `APS_DATA_REQUEST` and
//! `EVT_APS_DATA_INDICATION` (moved from `hub_devices::transport`, which
//! re-exports them).

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
