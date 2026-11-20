//! ZDO (ZDP) requests and responses the interview and configure need
//! (Zigbee R22 section 2.4.3 / 2.4.4). All ZDO frames are profile 0x0000,
//! endpoint 0; the first payload byte is the ZDO transaction sequence number.

use crate::zcl::types::{CodecError, Reader};

/// ZDO cluster IDs (requests; responses are `| 0x8000`).
#[allow(missing_docs)]
pub mod cluster {
    pub const NODE_DESC_REQ: u16 = 0x0002;
    pub const SIMPLE_DESC_REQ: u16 = 0x0004;
    pub const ACTIVE_EP_REQ: u16 = 0x0005;
    pub const DEVICE_ANNCE: u16 = 0x0013;
    pub const BIND_REQ: u16 = 0x0021;
    pub const UNBIND_REQ: u16 = 0x0022;
    pub const MGMT_LEAVE_REQ: u16 = 0x0034;
    pub const NODE_DESC_RSP: u16 = 0x8002;
    pub const SIMPLE_DESC_RSP: u16 = 0x8004;
    pub const ACTIVE_EP_RSP: u16 = 0x8005;
    pub const BIND_RSP: u16 = 0x8021;
    pub const UNBIND_RSP: u16 = 0x8022;
    pub const MGMT_LEAVE_RSP: u16 = 0x8034;
}

/// ZDP status `SUCCESS`.
pub const ZDP_SUCCESS: u8 = 0x00;
/// ZDP status `NOT_SUPPORTED`.
pub const ZDP_NOT_SUPPORTED: u8 = 0x84;

/// Logical device type from the Node descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogicalType {
    /// Coordinator.
    Coordinator,
    /// Router.
    Router,
    /// End device.
    EndDevice,
    /// Reserved value.
    Unknown,
}

/// The Node descriptor fields the hub keeps (R22 2.3.2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NodeDescriptor {
    /// Logical type.
    pub logical_type: LogicalType,
    /// MAC capability flags.
    pub mac_capabilities: u8,
    /// Manufacturer code.
    pub manufacturer_code: u16,
    /// Maximum NSDU size.
    pub max_buffer_size: u8,
    /// Maximum incoming transfer size.
    pub max_incoming_transfer: u16,
    /// Server mask (bits 9-15: stack compliance revision).
    pub server_mask: u16,
    /// Maximum outgoing transfer size.
    pub max_outgoing_transfer: u16,
}

impl NodeDescriptor {
    /// MAC capability bit 3, receiver on when idle.
    pub fn rx_on_when_idle(&self) -> bool {
        self.mac_capabilities & 0x08 != 0
    }
    /// MAC capability bit 2, mains powered.
    pub fn mains_powered(&self) -> bool {
        self.mac_capabilities & 0x04 != 0
    }

    /// Parses the 13-byte descriptor.
    pub fn parse(b: &[u8]) -> Result<Self, CodecError> {
        let mut r = Reader::new(b);
        let b0 = r.u8()?;
        let _b1 = r.u8()?;
        let mac_capabilities = r.u8()?;
        let manufacturer_code = r.u16()?;
        let max_buffer_size = r.u8()?;
        let max_incoming_transfer = r.u16()?;
        let server_mask = r.u16()?;
        let max_outgoing_transfer = r.u16()?;
        Ok(Self {
            logical_type: match b0 & 0x07 {
                0 => LogicalType::Coordinator,
                1 => LogicalType::Router,
                2 => LogicalType::EndDevice,
                _ => LogicalType::Unknown,
            },
            mac_capabilities,
            manufacturer_code,
            max_buffer_size,
            max_incoming_transfer,
            server_mask,
            max_outgoing_transfer,
        })
    }

    /// Encodes the 13-byte descriptor (simulator side).
    pub fn encode(&self) -> Vec<u8> {
        let lt = match self.logical_type {
            LogicalType::Coordinator => 0,
            LogicalType::Router => 1,
            LogicalType::EndDevice => 2,
            LogicalType::Unknown => 7,
        };
        let mut out = vec![lt, 0x40, self.mac_capabilities];
        out.extend_from_slice(&self.manufacturer_code.to_le_bytes());
        out.push(self.max_buffer_size);
        out.extend_from_slice(&self.max_incoming_transfer.to_le_bytes());
        out.extend_from_slice(&self.server_mask.to_le_bytes());
        out.extend_from_slice(&self.max_outgoing_transfer.to_le_bytes());
        out.push(0x00);
        out
    }
}

/// A Simple descriptor (R22 2.3.2.5).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SimpleDescriptor {
    /// Endpoint.
    pub endpoint: u8,
    /// Application profile.
    pub profile: u16,
    /// Application device ID.
    pub device_id: u16,
    /// Device version (4 bits).
    pub version: u8,
    /// Input (server) clusters.
    pub in_clusters: Vec<u16>,
    /// Output (client) clusters.
    pub out_clusters: Vec<u16>,
}

impl SimpleDescriptor {
    /// Parses the descriptor body.
    pub fn parse(b: &[u8]) -> Result<Self, CodecError> {
        let mut r = Reader::new(b);
        let endpoint = r.u8()?;
        let profile = r.u16()?;
        let device_id = r.u16()?;
        let version = r.u8()? & 0x0F;
        let n = r.u8()?;
        let in_clusters = (0..n).map(|_| r.u16()).collect::<Result<_, _>>()?;
        let n = r.u8()?;
        let out_clusters = (0..n).map(|_| r.u16()).collect::<Result<_, _>>()?;
        Ok(Self { endpoint, profile, device_id, version, in_clusters, out_clusters })
    }

    /// Encodes the descriptor body.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![self.endpoint];
        out.extend_from_slice(&self.profile.to_le_bytes());
        out.extend_from_slice(&self.device_id.to_le_bytes());
        out.push(self.version);
        out.push(self.in_clusters.len() as u8);
        out.extend(self.in_clusters.iter().flat_map(|c| c.to_le_bytes()));
        out.push(self.out_clusters.len() as u8);
        out.extend(self.out_clusters.iter().flat_map(|c| c.to_le_bytes()));
        out
    }
}

/// Bind destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum BindDst {
    /// Address mode 0x01: a group.
    Group(u16),
    /// Address mode 0x03: IEEE + endpoint.
    Ieee(u64, u8),
}

/// `NWK addr` request payload: `tsn, nwk` (Node_Desc_req, Active_EP_req).
pub fn nwk_req(tsn: u8, nwk: u16) -> Vec<u8> {
    let mut out = vec![tsn];
    out.extend_from_slice(&nwk.to_le_bytes());
    out
}

/// Simple_Desc_req payload.
pub fn simple_desc_req(tsn: u8, nwk: u16, ep: u8) -> Vec<u8> {
    let mut out = nwk_req(tsn, nwk);
    out.push(ep);
    out
}

/// Bind_req / Unbind_req payload.
pub fn bind_req(tsn: u8, src_ieee: u64, src_ep: u8, cluster: u16, dst: BindDst) -> Vec<u8> {
    let mut out = vec![tsn];
    out.extend_from_slice(&src_ieee.to_le_bytes());
    out.push(src_ep);
    out.extend_from_slice(&cluster.to_le_bytes());
    match dst {
        BindDst::Group(g) => {
            out.push(0x01);
            out.extend_from_slice(&g.to_le_bytes());
        }
        BindDst::Ieee(ieee, ep) => {
            out.push(0x03);
            out.extend_from_slice(&ieee.to_le_bytes());
            out.push(ep);
        }
    }
    out
}

/// Parsed Bind_req (simulator side).
pub fn parse_bind_req(p: &[u8]) -> Result<(u8, u64, u8, u16, BindDst), CodecError> {
    let mut r = Reader::new(p);
    let tsn = r.u8()?;
    let ieee = r.u64()?;
    let ep = r.u8()?;
    let cl = r.u16()?;
    let dst = match r.u8()? {
        0x01 => BindDst::Group(r.u16()?),
        0x03 => BindDst::Ieee(r.u64()?, r.u8()?),
        _ => return Err(CodecError::Invalid),
    };
    Ok((tsn, ieee, ep, cl, dst))
}

/// Mgmt_Leave_req payload (rejoin false, remove children false).
pub fn mgmt_leave_req(tsn: u8, ieee: u64) -> Vec<u8> {
    let mut out = vec![tsn];
    out.extend_from_slice(&ieee.to_le_bytes());
    out.push(0x00);
    out
}

/// A parsed ZDO response or announcement.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZdoMessage {
    /// Node_Desc_rsp.
    NodeDesc { tsn: u8, status: u8, nwk: u16, desc: Option<NodeDescriptor> },
    /// Active_EP_rsp.
    ActiveEp { tsn: u8, status: u8, nwk: u16, endpoints: Vec<u8> },
    /// Simple_Desc_rsp.
    SimpleDesc { tsn: u8, status: u8, nwk: u16, desc: Option<SimpleDescriptor> },
    /// Bind_rsp / Unbind_rsp / Mgmt_Leave_rsp and other status-only
    /// responses.
    Status { cluster: u16, tsn: u8, status: u8 },
    /// Device_annce.
    DeviceAnnce { tsn: u8, nwk: u16, ieee: u64, capability: u8 },
    /// Anything else.
    Other { cluster: u16, tsn: u8 },
}

#[allow(missing_docs)]
impl ZdoMessage {
    /// Parses a ZDO frame received on `cluster`.
    pub fn parse(cluster_id: u16, p: &[u8]) -> Result<Self, CodecError> {
        let mut r = Reader::new(p);
        let tsn = r.u8()?;
        Ok(match cluster_id {
            cluster::NODE_DESC_RSP => {
                let status = r.u8()?;
                let nwk = r.u16()?;
                let desc = if status == ZDP_SUCCESS { Some(NodeDescriptor::parse(r.rest())?) } else { None };
                Self::NodeDesc { tsn, status, nwk, desc }
            }
            cluster::ACTIVE_EP_RSP => {
                let status = r.u8()?;
                let nwk = r.u16()?;
                let endpoints = if status == ZDP_SUCCESS {
                    let n = r.u8()?;
                    r.take(usize::from(n))?.to_vec()
                } else {
                    Vec::new()
                };
                Self::ActiveEp { tsn, status, nwk, endpoints }
            }
            cluster::SIMPLE_DESC_RSP => {
                let status = r.u8()?;
                let nwk = r.u16()?;
                let desc = if status == ZDP_SUCCESS {
                    let len = r.u8()?;
                    Some(SimpleDescriptor::parse(r.take(usize::from(len))?)?)
                } else {
                    None
                };
                Self::SimpleDesc { tsn, status, nwk, desc }
            }
            cluster::DEVICE_ANNCE => {
                let nwk = r.u16()?;
                let ieee = r.u64()?;
                let capability = r.u8()?;
                Self::DeviceAnnce { tsn, nwk, ieee, capability }
            }
            c if c & 0x8000 != 0 && r.remaining() >= 1 => Self::Status { cluster: c, tsn, status: r.u8()? },
            c => Self::Other { cluster: c, tsn },
        })
    }

    pub fn tsn(&self) -> u8 {
        match *self {
            Self::NodeDesc { tsn, .. }
            | Self::ActiveEp { tsn, .. }
            | Self::SimpleDesc { tsn, .. }
            | Self::Status { tsn, .. }
            | Self::DeviceAnnce { tsn, .. }
            | Self::Other { tsn, .. } => tsn,
        }
    }
}

/// Node_Desc_rsp payload (simulator side).
pub fn node_desc_rsp(tsn: u8, nwk: u16, d: &NodeDescriptor) -> Vec<u8> {
    let mut out = vec![tsn, ZDP_SUCCESS];
    out.extend_from_slice(&nwk.to_le_bytes());
    out.extend_from_slice(&d.encode());
    out
}

/// Active_EP_rsp payload (simulator side).
pub fn active_ep_rsp(tsn: u8, nwk: u16, eps: &[u8]) -> Vec<u8> {
    let mut out = vec![tsn, ZDP_SUCCESS];
    out.extend_from_slice(&nwk.to_le_bytes());
    out.push(eps.len() as u8);
    out.extend_from_slice(eps);
    out
}

/// Simple_Desc_rsp payload (simulator side).
pub fn simple_desc_rsp(tsn: u8, nwk: u16, d: &SimpleDescriptor) -> Vec<u8> {
    let body = d.encode();
    let mut out = vec![tsn, ZDP_SUCCESS];
    out.extend_from_slice(&nwk.to_le_bytes());
    out.push(body.len() as u8);
    out.extend_from_slice(&body);
    out
}

/// Device_annce payload.
pub fn device_annce(tsn: u8, nwk: u16, ieee: u64, capability: u8) -> Vec<u8> {
    let mut out = vec![tsn];
    out.extend_from_slice(&nwk.to_le_bytes());
    out.extend_from_slice(&ieee.to_le_bytes());
    out.push(capability);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_round_trip() {
        let nd = NodeDescriptor {
            logical_type: LogicalType::EndDevice,
            mac_capabilities: 0x80,
            manufacturer_code: 0x115F,
            max_buffer_size: 0x7F,
            max_incoming_transfer: 100,
            server_mask: 0x2C00,
            max_outgoing_transfer: 100,
        };
        let p = node_desc_rsp(7, 0x1234, &nd);
        assert_eq!(
            ZdoMessage::parse(cluster::NODE_DESC_RSP, &p).unwrap(),
            ZdoMessage::NodeDesc { tsn: 7, status: 0, nwk: 0x1234, desc: Some(nd) }
        );
        assert!(!nd.rx_on_when_idle());
        let sd = SimpleDescriptor {
            endpoint: 1,
            profile: 0x0104,
            device_id: 0x0302,
            version: 1,
            in_clusters: vec![0x0000, 0x0402],
            out_clusters: vec![0x0019],
        };
        let p = simple_desc_rsp(8, 0x1234, &sd);
        assert_eq!(
            ZdoMessage::parse(cluster::SIMPLE_DESC_RSP, &p).unwrap(),
            ZdoMessage::SimpleDesc { tsn: 8, status: 0, nwk: 0x1234, desc: Some(sd) }
        );
        let b = bind_req(9, 0x00158d0001a2b3c4, 1, 0x0402, BindDst::Ieee(0xAABB, 1));
        assert_eq!(parse_bind_req(&b).unwrap(), (9, 0x00158d0001a2b3c4, 1, 0x0402, BindDst::Ieee(0xAABB, 1)));
        assert_eq!(
            ZdoMessage::parse(cluster::BIND_RSP, &[9, 0]).unwrap(),
            ZdoMessage::Status { cluster: cluster::BIND_RSP, tsn: 9, status: 0 }
        );
    }
}
