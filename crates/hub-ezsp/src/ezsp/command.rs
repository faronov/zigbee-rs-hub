//! Typed EZSP commands, responses and callbacks of the core stage.
//!
//! Parameter layouts follow the UG100 successor (Simplicity SDK EZSP
//! Reference Guide, docs.silabs.com 9.1.2) §4 "Configuration Frames", §6
//! "Networking Frames", §8 "Messaging Frames", §10 "Trust Center Frames",
//! §5 "Utilities Frames" for EZSP ≥ 14, and the generated
//! `command-functions.h` / `ezsp-frame-utilities.c` of Gecko SDK 4.4.6 for
//! EZSP 13. Frame IDs are identical in EZSP 13–18 (`ezsp-enum.h`).
//!
//! Every type encodes and decodes in both directions so the simulator can
//! play the NCP and tests can round-trip.

use super::types::*;
use super::version::EzspVersion;
use super::wire::{DecodeError, Reader, Writer};

/// Frame IDs (16-bit, extended format).
pub mod id {
    pub const VERSION: u16 = 0x0000;
    pub const ADD_ENDPOINT: u16 = 0x0002;
    pub const NOP: u16 = 0x0005;
    pub const CALLBACK: u16 = 0x0006;
    pub const NO_CALLBACKS: u16 = 0x0007;
    pub const NETWORK_INIT: u16 = 0x0017;
    pub const NETWORK_STATE: u16 = 0x0018;
    pub const STACK_STATUS_HANDLER: u16 = 0x0019;
    pub const PERMIT_JOINING: u16 = 0x0022;
    pub const CHILD_JOIN_HANDLER: u16 = 0x0023;
    /// `trustCenterJoinHandler` in EZSP 13, renamed
    /// `trustCenterPostJoinHandler` in EZSP ≥ 14 with the same ID and
    /// layout.
    pub const TRUST_CENTER_JOIN_HANDLER: u16 = 0x0024;
    pub const GET_EUI64: u16 = 0x0026;
    pub const GET_NODE_ID: u16 = 0x0027;
    pub const GET_NETWORK_PARAMETERS: u16 = 0x0028;
    pub const MAXIMUM_PAYLOAD_LENGTH: u16 = 0x0033;
    pub const SEND_UNICAST: u16 = 0x0034;
    pub const SEND_BROADCAST: u16 = 0x0036;
    pub const SEND_MULTICAST: u16 = 0x0038;
    pub const MESSAGE_SENT_HANDLER: u16 = 0x003F;
    pub const INCOMING_MESSAGE_HANDLER: u16 = 0x0045;
    pub const GET_CONFIGURATION_VALUE: u16 = 0x0052;
    pub const SET_CONFIGURATION_VALUE: u16 = 0x0053;
    pub const INVALID_COMMAND: u16 = 0x0058;
    pub const GET_MULTICAST_TABLE_ENTRY: u16 = 0x0063;
    pub const SET_MULTICAST_TABLE_ENTRY: u16 = 0x0064;
    pub const GET_VALUE: u16 = 0x00AA;
    pub const SET_VALUE: u16 = 0x00AB;
}

/// `EMBER_NULL_NODE_ID` / `SL_ZIGBEE_NULL_NODE_ID`: no alias.
pub const NULL_NODE_ID: NodeId = 0xFFFF;
/// `RX_ON_WHEN_IDLE_BROADCAST_ADDRESS`.
pub const RX_ON_WHEN_IDLE_BROADCAST: NodeId = 0xFFFD;

/// A command could not be encoded for the negotiated version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    /// EZSP 13 message tags are 8 bits.
    TagTooWide(u16),
    /// Payload or value longer than its `uint8_t` length field.
    TooLong(usize),
    /// More clusters than the `uint8_t` count fields allow.
    TooManyClusters,
    /// Version-specific fields built for another version.
    WrongLayout(&'static str),
    /// A status variant of another version (NCP side).
    StatusWidth,
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::TagTooWide(t) => {
                write!(f, "message tag {t} does not fit EZSP 13's 8-bit tag")
            }
            EncodeError::TooLong(n) => write!(f, "{n} bytes exceed a uint8_t length"),
            EncodeError::TooManyClusters => write!(f, "more than 255 clusters"),
            EncodeError::WrongLayout(what) => {
                write!(f, "{what} fields do not match the negotiated EZSP version")
            }
            EncodeError::StatusWidth => {
                write!(f, "status type does not match the negotiated EZSP version")
            }
        }
    }
}

impl std::error::Error for EncodeError {}

/// `addEndpoint` parameters.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Endpoint {
    pub endpoint: u8,
    pub profile_id: u16,
    pub device_id: u16,
    /// Device version and flags (`appFlags`, `deviceVersion` in EZSP 13).
    pub app_flags: u8,
    pub input_clusters: Vec<u16>,
    pub output_clusters: Vec<u16>,
}

/// Version-dependent fields of `sendBroadcast`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BroadcastExtra {
    /// EZSP 13: none.
    V13,
    /// EZSP ≥ 14: `alias` ([`NULL_NODE_ID`] for none) and its NWK sequence.
    V14 { alias: NodeId, nwk_sequence: u8 },
}

impl BroadcastExtra {
    /// No alias, in the layout of `v`.
    pub fn none(v: EzspVersion) -> BroadcastExtra {
        if v.alias_fields() {
            BroadcastExtra::V14 {
                alias: NULL_NODE_ID,
                nwk_sequence: 0,
            }
        } else {
            BroadcastExtra::V13
        }
    }
}

/// Version-dependent fields of `sendMulticast`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MulticastExtra {
    /// EZSP 13: hops non-members forward the message (7+ = infinite).
    V13 { nonmember_radius: u8 },
    /// EZSP ≥ 14.
    V14 {
        broadcast_addr: u16,
        alias: NodeId,
        nwk_sequence: u8,
    },
}

impl MulticastExtra {
    /// No alias, in the layout of `v`.
    // UNVERIFIED: the meaning of `broadcastAddr` (EZSP ≥ 14) is not
    // documented (UG100 successor §8 repeats the nonmemberRadius text);
    // 0xFFFD follows the Zigbee-compliant multicast destination named under
    // SEND_MULTICASTS_TO_SLEEPY_ADDRESS (UG100 successor §2).
    pub fn none(v: EzspVersion, nonmember_radius: u8) -> MulticastExtra {
        if v.alias_fields() {
            MulticastExtra::V14 {
                broadcast_addr: RX_ON_WHEN_IDLE_BROADCAST,
                alias: NULL_NODE_ID,
                nwk_sequence: 0,
            }
        } else {
            MulticastExtra::V13 { nonmember_radius }
        }
    }
}

/// Host → NCP commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Must be the first command after reset; see the session for the
    /// legacy framing of the first exchange.
    Version {
        desired: u8,
    },
    GetConfigurationValue {
        id: ConfigId,
    },
    SetConfigurationValue {
        id: ConfigId,
        value: u16,
    },
    GetValue {
        id: ValueId,
    },
    SetValue {
        id: ValueId,
        value: Vec<u8>,
    },
    AddEndpoint(Endpoint),
    NetworkInit {
        bitmask: NetworkInitBitmask,
    },
    NetworkState,
    GetNetworkParameters,
    GetEui64,
    GetNodeId,
    /// 0 closes, 0xFF opens indefinitely, else seconds.
    PermitJoining {
        duration: u8,
    },
    SendUnicast {
        kind: OutgoingMessageType,
        index_or_destination: u16,
        aps: ApsFrame,
        tag: u16,
        message: Vec<u8>,
    },
    SendBroadcast {
        destination: NodeId,
        extra: BroadcastExtra,
        aps: ApsFrame,
        radius: u8,
        tag: u16,
        message: Vec<u8>,
    },
    SendMulticast {
        aps: ApsFrame,
        hops: u8,
        extra: MulticastExtra,
        tag: u16,
        message: Vec<u8>,
    },
    SetMulticastTableEntry {
        index: u8,
        entry: MulticastTableEntry,
    },
    GetMulticastTableEntry {
        index: u8,
    },
    MaximumPayloadLength,
    Nop,
    /// Any other frame; parameters are passed through.
    Raw {
        frame_id: u16,
        params: Vec<u8>,
    },
}

fn tag(w: &mut Writer, v: EzspVersion, tag: u16) -> Result<(), EncodeError> {
    if v.wide_message_tag() {
        w.u16(tag);
    } else {
        w.u8(u8::try_from(tag).map_err(|_| EncodeError::TagTooWide(tag))?);
    }
    Ok(())
}

fn read_tag(r: &mut Reader<'_>, v: EzspVersion) -> Result<u16, DecodeError> {
    if v.wide_message_tag() {
        r.u16()
    } else {
        r.u8().map(u16::from)
    }
}

fn payload(w: &mut Writer, m: &[u8]) -> Result<(), EncodeError> {
    if m.len() > 255 {
        return Err(EncodeError::TooLong(m.len()));
    }
    w.len_prefixed(m);
    Ok(())
}

impl Command {
    /// 16-bit frame ID.
    pub fn frame_id(&self) -> u16 {
        match self {
            Command::Version { .. } => id::VERSION,
            Command::GetConfigurationValue { .. } => id::GET_CONFIGURATION_VALUE,
            Command::SetConfigurationValue { .. } => id::SET_CONFIGURATION_VALUE,
            Command::GetValue { .. } => id::GET_VALUE,
            Command::SetValue { .. } => id::SET_VALUE,
            Command::AddEndpoint(_) => id::ADD_ENDPOINT,
            Command::NetworkInit { .. } => id::NETWORK_INIT,
            Command::NetworkState => id::NETWORK_STATE,
            Command::GetNetworkParameters => id::GET_NETWORK_PARAMETERS,
            Command::GetEui64 => id::GET_EUI64,
            Command::GetNodeId => id::GET_NODE_ID,
            Command::PermitJoining { .. } => id::PERMIT_JOINING,
            Command::SendUnicast { .. } => id::SEND_UNICAST,
            Command::SendBroadcast { .. } => id::SEND_BROADCAST,
            Command::SendMulticast { .. } => id::SEND_MULTICAST,
            Command::SetMulticastTableEntry { .. } => id::SET_MULTICAST_TABLE_ENTRY,
            Command::GetMulticastTableEntry { .. } => id::GET_MULTICAST_TABLE_ENTRY,
            Command::MaximumPayloadLength => id::MAXIMUM_PAYLOAD_LENGTH,
            Command::Nop => id::NOP,
            Command::Raw { frame_id, .. } => *frame_id,
        }
    }

    /// Encodes the parameters for `v`.
    pub fn encode_params(&self, v: EzspVersion) -> Result<Vec<u8>, EncodeError> {
        let mut w = Writer::new();
        match self {
            Command::Version { desired } => w.u8(*desired),
            Command::GetConfigurationValue { id } => w.u8(id.0),
            Command::SetConfigurationValue { id, value } => {
                w.u8(id.0);
                w.u16(*value);
            }
            Command::GetValue { id } => w.u8(id.0),
            Command::SetValue { id, value } => {
                w.u8(id.0);
                payload(&mut w, value)?;
            }
            Command::AddEndpoint(e) => {
                let ni = u8::try_from(e.input_clusters.len())
                    .map_err(|_| EncodeError::TooManyClusters)?;
                let no = u8::try_from(e.output_clusters.len())
                    .map_err(|_| EncodeError::TooManyClusters)?;
                w.u8(e.endpoint);
                w.u16(e.profile_id);
                w.u16(e.device_id);
                w.u8(e.app_flags);
                w.u8(ni);
                w.u8(no);
                for c in e.input_clusters.iter().chain(&e.output_clusters) {
                    w.u16(*c);
                }
            }
            Command::NetworkInit { bitmask } => w.u16(bitmask.0),
            Command::NetworkState
            | Command::GetNetworkParameters
            | Command::GetEui64
            | Command::GetNodeId
            | Command::MaximumPayloadLength
            | Command::Nop => {}
            Command::PermitJoining { duration } => w.u8(*duration),
            Command::SendUnicast {
                kind,
                index_or_destination,
                aps,
                tag: t,
                message,
            } => {
                w.u8(kind.0);
                w.u16(*index_or_destination);
                aps.write(&mut w);
                tag(&mut w, v, *t)?;
                payload(&mut w, message)?;
            }
            Command::SendBroadcast {
                destination,
                extra,
                aps,
                radius,
                tag: t,
                message,
            } => {
                match (extra, v.alias_fields()) {
                    (BroadcastExtra::V13, false) => w.u16(*destination),
                    (
                        BroadcastExtra::V14 {
                            alias,
                            nwk_sequence,
                        },
                        true,
                    ) => {
                        w.u16(*alias);
                        w.u16(*destination);
                        w.u8(*nwk_sequence);
                    }
                    _ => return Err(EncodeError::WrongLayout("sendBroadcast")),
                }
                aps.write(&mut w);
                w.u8(*radius);
                tag(&mut w, v, *t)?;
                payload(&mut w, message)?;
            }
            Command::SendMulticast {
                aps,
                hops,
                extra,
                tag: t,
                message,
            } => {
                aps.write(&mut w);
                w.u8(*hops);
                match (extra, v.alias_fields()) {
                    (MulticastExtra::V13 { nonmember_radius }, false) => w.u8(*nonmember_radius),
                    (
                        MulticastExtra::V14 {
                            broadcast_addr,
                            alias,
                            nwk_sequence,
                        },
                        true,
                    ) => {
                        w.u16(*broadcast_addr);
                        w.u16(*alias);
                        w.u8(*nwk_sequence);
                    }
                    _ => return Err(EncodeError::WrongLayout("sendMulticast")),
                }
                tag(&mut w, v, *t)?;
                payload(&mut w, message)?;
            }
            Command::SetMulticastTableEntry { index, entry } => {
                w.u8(*index);
                entry.write(&mut w);
            }
            Command::GetMulticastTableEntry { index } => w.u8(*index),
            Command::Raw { params, .. } => w.bytes(params),
        }
        Ok(w.into_inner())
    }

    /// NCP side: decodes a command. `Ok(None)` for a frame ID outside the
    /// typed set.
    pub fn decode(
        frame_id: u16,
        v: EzspVersion,
        params: &[u8],
    ) -> Result<Option<Command>, DecodeError> {
        let mut r = Reader::new(params);
        let cmd = match frame_id {
            id::VERSION => Command::Version { desired: r.u8()? },
            id::GET_CONFIGURATION_VALUE => Command::GetConfigurationValue {
                id: ConfigId(r.u8()?),
            },
            id::SET_CONFIGURATION_VALUE => Command::SetConfigurationValue {
                id: ConfigId(r.u8()?),
                value: r.u16()?,
            },
            id::GET_VALUE => Command::GetValue {
                id: ValueId(r.u8()?),
            },
            id::SET_VALUE => Command::SetValue {
                id: ValueId(r.u8()?),
                value: r.len_prefixed()?,
            },
            id::ADD_ENDPOINT => {
                let endpoint = r.u8()?;
                let profile_id = r.u16()?;
                let device_id = r.u16()?;
                let app_flags = r.u8()?;
                let ni = r.u8()?;
                let no = r.u8()?;
                let input_clusters = (0..ni).map(|_| r.u16()).collect::<Result<_, _>>()?;
                let output_clusters = (0..no).map(|_| r.u16()).collect::<Result<_, _>>()?;
                Command::AddEndpoint(Endpoint {
                    endpoint,
                    profile_id,
                    device_id,
                    app_flags,
                    input_clusters,
                    output_clusters,
                })
            }
            id::NETWORK_INIT => Command::NetworkInit {
                bitmask: NetworkInitBitmask(r.u16()?),
            },
            id::NETWORK_STATE => Command::NetworkState,
            id::GET_NETWORK_PARAMETERS => Command::GetNetworkParameters,
            id::GET_EUI64 => Command::GetEui64,
            id::GET_NODE_ID => Command::GetNodeId,
            id::PERMIT_JOINING => Command::PermitJoining { duration: r.u8()? },
            id::SEND_UNICAST => Command::SendUnicast {
                kind: OutgoingMessageType(r.u8()?),
                index_or_destination: r.u16()?,
                aps: ApsFrame::read(&mut r)?,
                tag: read_tag(&mut r, v)?,
                message: r.len_prefixed()?,
            },
            id::SEND_BROADCAST => {
                let (destination, extra) = if v.alias_fields() {
                    let alias = r.u16()?;
                    let destination = r.u16()?;
                    (
                        destination,
                        BroadcastExtra::V14 {
                            alias,
                            nwk_sequence: r.u8()?,
                        },
                    )
                } else {
                    (r.u16()?, BroadcastExtra::V13)
                };
                Command::SendBroadcast {
                    destination,
                    extra,
                    aps: ApsFrame::read(&mut r)?,
                    radius: r.u8()?,
                    tag: read_tag(&mut r, v)?,
                    message: r.len_prefixed()?,
                }
            }
            id::SEND_MULTICAST => {
                let aps = ApsFrame::read(&mut r)?;
                let hops = r.u8()?;
                let extra = if v.alias_fields() {
                    MulticastExtra::V14 {
                        broadcast_addr: r.u16()?,
                        alias: r.u16()?,
                        nwk_sequence: r.u8()?,
                    }
                } else {
                    MulticastExtra::V13 {
                        nonmember_radius: r.u8()?,
                    }
                };
                Command::SendMulticast {
                    aps,
                    hops,
                    extra,
                    tag: read_tag(&mut r, v)?,
                    message: r.len_prefixed()?,
                }
            }
            id::SET_MULTICAST_TABLE_ENTRY => Command::SetMulticastTableEntry {
                index: r.u8()?,
                entry: MulticastTableEntry::read(&mut r)?,
            },
            id::GET_MULTICAST_TABLE_ENTRY => Command::GetMulticastTableEntry { index: r.u8()? },
            id::MAXIMUM_PAYLOAD_LENGTH => Command::MaximumPayloadLength,
            id::NOP => Command::Nop,
            _ => return Ok(None),
        };
        r.finish()?;
        Ok(Some(cmd))
    }
}

/// NCP → host responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Version {
        protocol_version: u8,
        stack_type: u8,
        stack_version: super::version::StackVersion,
    },
    GetConfigurationValue {
        status: Status,
        value: u16,
    },
    SetConfigurationValue {
        status: Status,
    },
    GetValue {
        status: Status,
        value: Vec<u8>,
    },
    SetValue {
        status: Status,
    },
    AddEndpoint {
        status: Status,
    },
    /// `NOT_JOINED` when no network is stored.
    NetworkInit {
        status: Status,
    },
    NetworkState {
        state: NetworkStatus,
    },
    GetNetworkParameters {
        status: Status,
        node_type: NodeType,
        parameters: NetworkParameters,
    },
    GetEui64 {
        eui64: Eui64,
    },
    GetNodeId {
        node_id: NodeId,
    },
    PermitJoining {
        status: Status,
    },
    /// `sequence`: APS counter the message will use.
    SendUnicast {
        status: Status,
        sequence: u8,
    },
    SendBroadcast {
        status: Status,
        sequence: u8,
    },
    SendMulticast {
        status: Status,
        sequence: u8,
    },
    SetMulticastTableEntry {
        status: Status,
    },
    GetMulticastTableEntry {
        status: Status,
        entry: MulticastTableEntry,
    },
    MaximumPayloadLength {
        aps_length: u8,
    },
    Nop,
    /// `invalidCommand` (0x0058): the NCP rejected the command.
    InvalidCommand {
        reason: EzspStatus,
    },
    /// Response to [`Command::Raw`].
    Raw {
        frame_id: u16,
        params: Vec<u8>,
    },
}

impl Response {
    /// Frame ID the NCP uses for this response.
    pub fn frame_id(&self) -> u16 {
        match self {
            Response::Version { .. } => id::VERSION,
            Response::GetConfigurationValue { .. } => id::GET_CONFIGURATION_VALUE,
            Response::SetConfigurationValue { .. } => id::SET_CONFIGURATION_VALUE,
            Response::GetValue { .. } => id::GET_VALUE,
            Response::SetValue { .. } => id::SET_VALUE,
            Response::AddEndpoint { .. } => id::ADD_ENDPOINT,
            Response::NetworkInit { .. } => id::NETWORK_INIT,
            Response::NetworkState { .. } => id::NETWORK_STATE,
            Response::GetNetworkParameters { .. } => id::GET_NETWORK_PARAMETERS,
            Response::GetEui64 { .. } => id::GET_EUI64,
            Response::GetNodeId { .. } => id::GET_NODE_ID,
            Response::PermitJoining { .. } => id::PERMIT_JOINING,
            Response::SendUnicast { .. } => id::SEND_UNICAST,
            Response::SendBroadcast { .. } => id::SEND_BROADCAST,
            Response::SendMulticast { .. } => id::SEND_MULTICAST,
            Response::SetMulticastTableEntry { .. } => id::SET_MULTICAST_TABLE_ENTRY,
            Response::GetMulticastTableEntry { .. } => id::GET_MULTICAST_TABLE_ENTRY,
            Response::MaximumPayloadLength { .. } => id::MAXIMUM_PAYLOAD_LENGTH,
            Response::Nop => id::NOP,
            Response::InvalidCommand { .. } => id::INVALID_COMMAND,
            Response::Raw { frame_id, .. } => *frame_id,
        }
    }

    /// The status field, for responses that have one.
    pub fn status(&self) -> Option<Status> {
        match self {
            Response::GetConfigurationValue { status, .. }
            | Response::SetConfigurationValue { status }
            | Response::GetValue { status, .. }
            | Response::SetValue { status }
            | Response::AddEndpoint { status }
            | Response::NetworkInit { status }
            | Response::GetNetworkParameters { status, .. }
            | Response::PermitJoining { status }
            | Response::SendUnicast { status, .. }
            | Response::SendBroadcast { status, .. }
            | Response::SendMulticast { status, .. }
            | Response::SetMulticastTableEntry { status }
            | Response::GetMulticastTableEntry { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// Host side: decodes the response with frame ID `frame_id`.
    pub fn decode(frame_id: u16, v: EzspVersion, params: &[u8]) -> Result<Response, DecodeError> {
        use StatusFamily::{Ember, Ezsp};
        let mut r = Reader::new(params);
        let st = |r: &mut Reader<'_>, f| Status::read(r, v, f);
        let resp = match frame_id {
            id::VERSION => Response::Version {
                protocol_version: r.u8()?,
                stack_type: r.u8()?,
                stack_version: super::version::StackVersion(r.u16()?),
            },
            id::GET_CONFIGURATION_VALUE => Response::GetConfigurationValue {
                status: st(&mut r, Ezsp)?,
                value: r.u16()?,
            },
            id::SET_CONFIGURATION_VALUE => Response::SetConfigurationValue {
                status: st(&mut r, Ezsp)?,
            },
            id::GET_VALUE => Response::GetValue {
                status: st(&mut r, Ezsp)?,
                value: r.len_prefixed()?,
            },
            id::SET_VALUE => Response::SetValue {
                status: st(&mut r, Ezsp)?,
            },
            id::ADD_ENDPOINT => Response::AddEndpoint {
                status: st(&mut r, Ezsp)?,
            },
            id::NETWORK_INIT => Response::NetworkInit {
                status: st(&mut r, Ember)?,
            },
            id::NETWORK_STATE => Response::NetworkState {
                state: NetworkStatus(r.u8()?),
            },
            id::GET_NETWORK_PARAMETERS => Response::GetNetworkParameters {
                status: st(&mut r, Ember)?,
                node_type: NodeType(r.u8()?),
                parameters: NetworkParameters::read(&mut r)?,
            },
            id::GET_EUI64 => Response::GetEui64 {
                eui64: Eui64(r.array()?),
            },
            id::GET_NODE_ID => Response::GetNodeId { node_id: r.u16()? },
            id::PERMIT_JOINING => Response::PermitJoining {
                status: st(&mut r, Ember)?,
            },
            id::SEND_UNICAST => Response::SendUnicast {
                status: st(&mut r, Ember)?,
                sequence: r.u8()?,
            },
            id::SEND_BROADCAST => Response::SendBroadcast {
                status: st(&mut r, Ember)?,
                sequence: r.u8()?,
            },
            id::SEND_MULTICAST => Response::SendMulticast {
                status: st(&mut r, Ember)?,
                sequence: r.u8()?,
            },
            id::SET_MULTICAST_TABLE_ENTRY => Response::SetMulticastTableEntry {
                status: st(&mut r, Ember)?,
            },
            id::GET_MULTICAST_TABLE_ENTRY => Response::GetMulticastTableEntry {
                status: st(&mut r, Ember)?,
                entry: MulticastTableEntry::read(&mut r)?,
            },
            id::MAXIMUM_PAYLOAD_LENGTH => Response::MaximumPayloadLength {
                aps_length: r.u8()?,
            },
            id::NOP => Response::Nop,
            id::INVALID_COMMAND => Response::InvalidCommand {
                reason: EzspStatus(r.u8()?),
            },
            _ => {
                return Ok(Response::Raw {
                    frame_id,
                    params: params.to_vec(),
                });
            }
        };
        r.finish()?;
        Ok(resp)
    }

    /// NCP side: encodes the parameters for `v`.
    pub fn encode_params(&self, v: EzspVersion) -> Result<Vec<u8>, EncodeError> {
        use StatusFamily::{Ember, Ezsp};
        let mut w = Writer::new();
        let st = |w: &mut Writer, s: &Status, f| {
            if !s.matches(v, f) {
                return Err(EncodeError::StatusWidth);
            }
            s.write(w);
            Ok(())
        };
        match self {
            Response::Version {
                protocol_version,
                stack_type,
                stack_version,
            } => {
                w.u8(*protocol_version);
                w.u8(*stack_type);
                w.u16(stack_version.0);
            }
            Response::GetConfigurationValue { status, value } => {
                st(&mut w, status, Ezsp)?;
                w.u16(*value);
            }
            Response::SetConfigurationValue { status }
            | Response::SetValue { status }
            | Response::AddEndpoint { status } => st(&mut w, status, Ezsp)?,
            Response::GetValue { status, value } => {
                st(&mut w, status, Ezsp)?;
                payload(&mut w, value)?;
            }
            Response::NetworkInit { status }
            | Response::PermitJoining { status }
            | Response::SetMulticastTableEntry { status } => st(&mut w, status, Ember)?,
            Response::NetworkState { state } => w.u8(state.0),
            Response::GetNetworkParameters {
                status,
                node_type,
                parameters,
            } => {
                st(&mut w, status, Ember)?;
                w.u8(node_type.0);
                parameters.write(&mut w);
            }
            Response::GetEui64 { eui64 } => w.bytes(&eui64.0),
            Response::GetNodeId { node_id } => w.u16(*node_id),
            Response::SendUnicast { status, sequence }
            | Response::SendBroadcast { status, sequence }
            | Response::SendMulticast { status, sequence } => {
                st(&mut w, status, Ember)?;
                w.u8(*sequence);
            }
            Response::GetMulticastTableEntry { status, entry } => {
                st(&mut w, status, Ember)?;
                entry.write(&mut w);
            }
            Response::MaximumPayloadLength { aps_length } => w.u8(*aps_length),
            Response::Nop => {}
            Response::InvalidCommand { reason } => w.u8(reason.0),
            Response::Raw { params, .. } => w.bytes(params),
        }
        Ok(w.into_inner())
    }
}

/// NCP → host callbacks (asynchronous on UART by default: UG100 successor
/// §2 "UART Interface Callbacks").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Callback {
    /// `stackStatusHandler`: NETWORK_UP, NETWORK_DOWN, NETWORK_OPENED, …
    StackStatus { status: Status },
    /// `messageSentHandler`. `status` is success only if an ACK was
    /// received from the destination (UG100 successor §8); for broadcasts
    /// and multicasts it reports submission. `message` is empty unless
    /// the messageContentsInCallback policy asks for it.
    MessageSent {
        status: Status,
        kind: OutgoingMessageType,
        index_or_destination: u16,
        aps: ApsFrame,
        tag: u16,
        message: Vec<u8>,
    },
    /// `incomingMessageHandler`.
    IncomingMessage {
        kind: IncomingMessageType,
        aps: ApsFrame,
        info: RxPacketInfo,
        message: Vec<u8>,
    },
    /// `trustCenterJoinHandler` (EZSP 13) / `trustCenterPostJoinHandler`
    /// (EZSP ≥ 14).
    TrustCenterJoin {
        new_node_id: NodeId,
        new_node_eui64: Eui64,
        status: DeviceUpdate,
        policy_decision: JoinDecision,
        parent_of_new_node_id: NodeId,
    },
    /// `childJoinHandler`.
    ChildJoin {
        index: u8,
        joining: bool,
        child_id: NodeId,
        child_eui64: Eui64,
        child_type: NodeType,
    },
    /// Any other callback.
    Unknown { frame_id: u16, params: Vec<u8> },
}

impl Callback {
    pub fn frame_id(&self) -> u16 {
        match self {
            Callback::StackStatus { .. } => id::STACK_STATUS_HANDLER,
            Callback::MessageSent { .. } => id::MESSAGE_SENT_HANDLER,
            Callback::IncomingMessage { .. } => id::INCOMING_MESSAGE_HANDLER,
            Callback::TrustCenterJoin { .. } => id::TRUST_CENTER_JOIN_HANDLER,
            Callback::ChildJoin { .. } => id::CHILD_JOIN_HANDLER,
            Callback::Unknown { frame_id, .. } => *frame_id,
        }
    }

    /// Host side.
    pub fn decode(frame_id: u16, v: EzspVersion, params: &[u8]) -> Result<Callback, DecodeError> {
        let mut r = Reader::new(params);
        let cb = match frame_id {
            id::STACK_STATUS_HANDLER => Callback::StackStatus {
                status: Status::read(&mut r, v, StatusFamily::Ember)?,
            },
            id::MESSAGE_SENT_HANDLER if v.uses_sl_status() => {
                let status = Status::read(&mut r, v, StatusFamily::Ember)?;
                Callback::MessageSent {
                    status,
                    kind: OutgoingMessageType(r.u8()?),
                    index_or_destination: r.u16()?,
                    aps: ApsFrame::read(&mut r)?,
                    tag: read_tag(&mut r, v)?,
                    message: r.len_prefixed()?,
                }
            }
            id::MESSAGE_SENT_HANDLER => {
                let kind = OutgoingMessageType(r.u8()?);
                let index_or_destination = r.u16()?;
                let aps = ApsFrame::read(&mut r)?;
                let tag = read_tag(&mut r, v)?;
                let status = Status::read(&mut r, v, StatusFamily::Ember)?;
                Callback::MessageSent {
                    status,
                    kind,
                    index_or_destination,
                    aps,
                    tag,
                    message: r.len_prefixed()?,
                }
            }
            id::INCOMING_MESSAGE_HANDLER => {
                let kind = IncomingMessageType(r.u8()?);
                let aps = ApsFrame::read(&mut r)?;
                let info = if v.alias_fields() {
                    RxPacketInfo {
                        sender: r.u16()?,
                        sender_eui64: Some(Eui64(r.array()?)),
                        binding_index: r.u8()?,
                        address_index: r.u8()?,
                        last_hop_lqi: r.u8()?,
                        last_hop_rssi: r.i8()?,
                        last_hop_timestamp: Some(r.u32()?),
                    }
                } else {
                    let last_hop_lqi = r.u8()?;
                    let last_hop_rssi = r.i8()?;
                    RxPacketInfo {
                        sender: r.u16()?,
                        sender_eui64: None,
                        binding_index: r.u8()?,
                        address_index: r.u8()?,
                        last_hop_lqi,
                        last_hop_rssi,
                        last_hop_timestamp: None,
                    }
                };
                Callback::IncomingMessage {
                    kind,
                    aps,
                    info,
                    message: r.len_prefixed()?,
                }
            }
            id::TRUST_CENTER_JOIN_HANDLER => Callback::TrustCenterJoin {
                new_node_id: r.u16()?,
                new_node_eui64: Eui64(r.array()?),
                status: DeviceUpdate(r.u8()?),
                policy_decision: JoinDecision(r.u8()?),
                parent_of_new_node_id: r.u16()?,
            },
            id::CHILD_JOIN_HANDLER => Callback::ChildJoin {
                index: r.u8()?,
                joining: r.bool()?,
                child_id: r.u16()?,
                child_eui64: Eui64(r.array()?),
                child_type: NodeType(r.u8()?),
            },
            _ => {
                return Ok(Callback::Unknown {
                    frame_id,
                    params: params.to_vec(),
                });
            }
        };
        r.finish()?;
        Ok(cb)
    }

    /// NCP side.
    pub fn encode_params(&self, v: EzspVersion) -> Result<Vec<u8>, EncodeError> {
        let mut w = Writer::new();
        let status = |w: &mut Writer, s: &Status| {
            if !s.matches(v, StatusFamily::Ember) {
                return Err(EncodeError::StatusWidth);
            }
            s.write(w);
            Ok(())
        };
        match self {
            Callback::StackStatus { status: s } => status(&mut w, s)?,
            Callback::MessageSent {
                status: s,
                kind,
                index_or_destination,
                aps,
                tag: t,
                message,
            } => {
                if v.uses_sl_status() {
                    status(&mut w, s)?;
                }
                w.u8(kind.0);
                w.u16(*index_or_destination);
                aps.write(&mut w);
                tag(&mut w, v, *t)?;
                if !v.uses_sl_status() {
                    status(&mut w, s)?;
                }
                payload(&mut w, message)?;
            }
            Callback::IncomingMessage {
                kind,
                aps,
                info,
                message,
            } => {
                w.u8(kind.0);
                aps.write(&mut w);
                if v.alias_fields() {
                    let (Some(eui), Some(ts)) = (info.sender_eui64, info.last_hop_timestamp) else {
                        return Err(EncodeError::WrongLayout(
                            "incomingMessageHandler packet info",
                        ));
                    };
                    w.u16(info.sender);
                    w.bytes(&eui.0);
                    w.u8(info.binding_index);
                    w.u8(info.address_index);
                    w.u8(info.last_hop_lqi);
                    w.u8(info.last_hop_rssi as u8);
                    w.u32(ts);
                } else {
                    if info.sender_eui64.is_some() || info.last_hop_timestamp.is_some() {
                        return Err(EncodeError::WrongLayout(
                            "incomingMessageHandler packet info",
                        ));
                    }
                    w.u8(info.last_hop_lqi);
                    w.u8(info.last_hop_rssi as u8);
                    w.u16(info.sender);
                    w.u8(info.binding_index);
                    w.u8(info.address_index);
                }
                payload(&mut w, message)?;
            }
            Callback::TrustCenterJoin {
                new_node_id,
                new_node_eui64,
                status: s,
                policy_decision,
                parent_of_new_node_id,
            } => {
                w.u16(*new_node_id);
                w.bytes(&new_node_eui64.0);
                w.u8(s.0);
                w.u8(policy_decision.0);
                w.u16(*parent_of_new_node_id);
            }
            Callback::ChildJoin {
                index,
                joining,
                child_id,
                child_eui64,
                child_type,
            } => {
                w.u8(*index);
                w.bool(*joining);
                w.u16(*child_id);
                w.bytes(&child_eui64.0);
                w.u8(child_type.0);
            }
            Callback::Unknown { params, .. } => w.bytes(params),
        }
        Ok(w.into_inner())
    }
}
